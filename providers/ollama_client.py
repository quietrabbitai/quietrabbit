# providers/ollama_client.py
# Ollama Tier 1 client.
# All calls go through generate() or chat() — no bare requests elsewhere.
# Latency is always tracked — never hardcoded to 0.
# stream=False always in Release 1. StepExecutor resolves stream value.

from __future__ import annotations

import json
import os
import re
import time
from pathlib import Path

import requests

from providers.errors import (
    OllamaUnavailableError,
    OllamaTimeoutError,
    OllamaGenerationError,
    OllamaInvalidRequestError,
)
from providers.types import (
    GenerateRequest,
    GenerateResponse,
    GenerateOptions,
    ProviderHealth,
    ContextWindowStatus,
    ModelfileVersion,
    ChatMessage,
)
from providers.utils import now


# -- Configuration ------------------------------------------------------------

def _base_url() -> str:
    host = os.environ.get("OLLAMA_HOST", "host.docker.internal")
    port = os.environ.get("OLLAMA_PORT", "11434")
    return f"http://{host}:{port}"


OLLAMA_TIMEOUT_SECONDS = 120
OLLAMA_CONNECT_TIMEOUT_SECONDS = 5
OLLAMA_MODELFILE_TIMEOUT_SECONDS = 300   # model creation can take longer

# Context window thresholds — both env-tunable
CONTEXT_WARNING_THRESHOLD = float(
    os.environ.get("QR_CONTEXT_WARNING_THRESHOLD", "0.75")
)
CONTEXT_HARD_LIMIT = float(
    os.environ.get("QR_CONTEXT_HARD_LIMIT", "0.95")
)

# Task types that receive a 20% safety buffer in token estimation.
# These types produce denser token output than the 4-char/token heuristic
# assumes, increasing the risk of silent context window overflow.
# creative_writing and prose added in Layer 4 — long-form narrative text
# has higher token density than the heuristic predicts.
# If a new task type is added to task_types.yaml, evaluate whether it
# needs a buffer here. Prefer over-estimation — it triggers compaction
# earlier but never causes a hard failure.
_BUFFERED_TASK_TYPES: frozenset[str] = frozenset({
    "code",
    "research",
    "creative_writing",
    "prose",
})


# -- Health check -------------------------------------------------------------

def check_ollama_health() -> ProviderHealth:
    """
    Check Ollama connectivity and available models.
    Called at startup and every 30s by health monitor.
    Never raises — always returns a ProviderHealth.
    """
    try:
        response = requests.get(
            f"{_base_url()}/api/tags",
            timeout=OLLAMA_CONNECT_TIMEOUT_SECONDS,
        )
        if response.ok:
            available = [
                m["name"] for m in response.json().get("models", [])
            ]
            return ProviderHealth(
                provider="ollama",
                status="available",
                checked_at=now(),
                available_models=available,
            )
        return ProviderHealth(
            provider="ollama",
            status="degraded",
            checked_at=now(),
            error=f"HTTP {response.status_code}",
        )
    except requests.ConnectionError:
        return ProviderHealth(
            provider="ollama",
            status="unavailable",
            checked_at=now(),
            error="connection_refused",
        )
    except requests.Timeout:
        return ProviderHealth(
            provider="ollama",
            status="unavailable",
            checked_at=now(),
            error="timeout",
        )


# -- Token estimation ---------------------------------------------------------

def estimate_token_count(text: str, task_type: str = "") -> int:
    """
    Heuristic token count: ~4 chars per token.
    Applies 20% safety buffer for task types in _BUFFERED_TASK_TYPES
    where the heuristic significantly underestimates token density.
    Known limitation: may underestimate for JSON, non-English text.
    Over-estimation is safe (triggers compaction earlier, never fails hard).
    Under-estimation risk: model hits context limit without warning.
    """
    base = len(text) // 4
    if task_type in _BUFFERED_TASK_TYPES:
        return int(base * 1.20)
    return base


def check_context_window(
    model_id: str,
    prompt: str,
    task_type: str,
    context_window: int,
) -> ContextWindowStatus:
    """
    Check whether prompt fits within model context window.
    context_window: from routing table model config.
    Returns status and recommended action for StepExecutor.
    """
    if not context_window:
        # Missing model config — fail safe with a clear error state
        return ContextWindowStatus(
            status="exceeded",
            token_estimate=estimate_token_count(prompt, task_type),
            context_window=0,
            usage_fraction=1.0,
            plain_language=(
                "Quiet Rabbit couldn't determine the model's capacity. "
                "[Get help]"
            ),
            recommended_action="compact_then_escalate",
        )

    token_estimate = estimate_token_count(prompt, task_type)
    usage_fraction = token_estimate / context_window

    if usage_fraction >= CONTEXT_HARD_LIMIT:
        return ContextWindowStatus(
            status="exceeded",
            token_estimate=token_estimate,
            context_window=context_window,
            usage_fraction=usage_fraction,
            plain_language=(
                "This is too long for local processing. "
                "[Use an external service] [Shorten the document]"
            ),
            recommended_action="compact_then_escalate",
        )

    if usage_fraction >= CONTEXT_WARNING_THRESHOLD:
        if task_type == "long_context":
            return ContextWindowStatus(
                status="warn",
                token_estimate=token_estimate,
                context_window=context_window,
                usage_fraction=usage_fraction,
                plain_language=(
                    "This document is long. Local processing may miss "
                    "details toward the end. "
                    "[Use an external service] [Continue locally]"
                ),
                recommended_action="compact_then_escalate",
            )
        return ContextWindowStatus(
            status="warn",
            token_estimate=token_estimate,
            context_window=context_window,
            usage_fraction=usage_fraction,
            plain_language=(
                "This is getting long — results may be less complete "
                "toward the end."
            ),
            recommended_action="compact_then_escalate",
        )

    return ContextWindowStatus(
        status="ok",
        token_estimate=token_estimate,
        context_window=context_window,
        usage_fraction=usage_fraction,
    )


# -- Single-turn generation ---------------------------------------------------

def generate(request: GenerateRequest) -> GenerateResponse:
    """
    Primary Tier 1 inference call.
    Latency always tracked — never hardcoded.
    stream=False always in Release 1.
    Raises: OllamaUnavailableError, OllamaTimeoutError,
            OllamaGenerationError, OllamaInvalidRequestError
    """
    options = request.options or GenerateOptions(
        temperature=0.5,
        top_p=0.90,
        num_ctx=2048,
        num_predict=2048,
    )

    payload = {
        "model": request.model,
        "prompt": request.prompt,
        "stream": False,            # Release 1: always False
        "options": {
            "temperature": options.temperature,
            "top_p": options.top_p,
            "num_ctx": options.num_ctx,
            "num_predict": options.num_predict,
        },
    }

    try:
        start = time.monotonic()
        response = requests.post(
            f"{_base_url()}/api/generate",
            json=payload,
            timeout=OLLAMA_TIMEOUT_SECONDS,
        )
        latency_ms = (time.monotonic() - start) * 1000

    except requests.ConnectionError:
        raise OllamaUnavailableError(
            plain_language=(
                "The local AI isn't responding. "
                "[Try again] [Use an external service] [Get help]"
            )
        )
    except requests.Timeout:
        raise OllamaTimeoutError(
            plain_language=(
                "The local AI took too long to respond. "
                "[Try again] [Use an external service]"
            )
        )

    if response.status_code == 400:
        raise OllamaInvalidRequestError(
            plain_language=(
                "The local AI didn't understand the request. "
                "This is likely a configuration issue. [Get help]"
            )
        )
    if not response.ok:
        raise OllamaGenerationError(
            status_code=response.status_code,
            plain_language=(
                "The local AI returned an unexpected response. "
                "[Try again] [Get help]"
            )
        )

    data = response.json()
    return GenerateResponse(
        content=data["response"],
        model=data["model"],
        prompt_token_count=data.get("prompt_eval_count", 0),
        output_token_count=data.get("eval_count", 0),
        latency_ms=latency_ms,
        completion_status="complete",
    )


# -- Multi-turn chat ----------------------------------------------------------

def chat(
    messages: list[ChatMessage],
    model: str,
    task_type: str,
    options: GenerateOptions | None = None,
) -> GenerateResponse:
    """
    Multi-turn chat for interview flows, Path Builder, and Privacy
    Guardian disclosure dialogs.
    Latency always tracked — never hardcoded to 0.
    """
    opts = options or GenerateOptions(
        temperature=0.5,
        top_p=0.90,
        num_ctx=2048,
    )

    payload = {
        "model": model,
        "messages": [
            {"role": m.role, "content": m.content}
            for m in messages
        ],
        "stream": False,
        "options": {
            "temperature": opts.temperature,
            "top_p": opts.top_p,
            "num_ctx": opts.num_ctx,
        },
    }

    try:
        start = time.monotonic()
        response = requests.post(
            f"{_base_url()}/api/chat",
            json=payload,
            timeout=OLLAMA_TIMEOUT_SECONDS,
        )
        latency_ms = (time.monotonic() - start) * 1000

    except requests.ConnectionError:
        raise OllamaUnavailableError(
            plain_language=(
                "The local AI isn't responding. "
                "[Try again] [Get help]"
            )
        )
    except requests.Timeout:
        raise OllamaTimeoutError(
            plain_language=(
                "The local AI took too long to respond. [Try again]"
            )
        )

    if not response.ok:
        raise OllamaGenerationError(
            status_code=response.status_code,
            plain_language=(
                "The local AI returned an unexpected response. [Try again]"
            )
        )

    data = response.json()
    return GenerateResponse(
        content=data["message"]["content"],
        model=data.get("model", model),
        prompt_token_count=data.get("prompt_eval_count", 0),
        output_token_count=data.get("eval_count", 0),
        latency_ms=latency_ms,
        completion_status="complete",
    )


# -- Modelfile management -----------------------------------------------------

def get_applied_modelfile_version(model_name: str) -> str | None:
    """
    Read QR-MODELFILE-VERSION comment from the applied Modelfile.
    Returns version string or None if not applied or not found.
    """
    try:
        response = requests.post(
            f"{_base_url()}/api/show",
            json={"name": model_name},
            timeout=10,
        )
        if not response.ok:
            return None
        modelfile = response.json().get("modelfile", "")
        match = re.search(r"# QR-MODELFILE-VERSION: (.+)", modelfile)
        return match.group(1).strip() if match else None
    except Exception:
        return None


def check_modelfile_version(
    model_id: str,
    expected_version: str,
) -> ModelfileVersion:
    """Check whether the applied Modelfile matches the expected version."""
    applied = get_applied_modelfile_version(model_id)
    return ModelfileVersion(
        model_id=model_id,
        expected_version=expected_version,
        applied_version=applied,
        is_current=(applied == expected_version),
    )


def apply_modelfile(model_name: str, modelfile_path: Path) -> bool:
    """
    Apply a Modelfile via Ollama /api/create.
    Validates NDJSON response body — HTTP 200 does not guarantee success.
    Ollama streams NDJSON during creation; checks final line for
    {"status":"success"} before confirming. This is a documented
    NotebookLM bug fix — do not simplify to response.ok check.
    Returns True if applied successfully, False otherwise.
    """
    try:
        response = requests.post(
            f"{_base_url()}/api/create",
            json={
                "name": model_name,
                "modelfile": modelfile_path.read_text(),
            },
            timeout=OLLAMA_MODELFILE_TIMEOUT_SECONDS,
        )
        if not response.ok:
            return False

        # HTTP 200 does not mean success — validate NDJSON stream body.
        lines = [l for l in response.text.strip().splitlines() if l]
        if lines:
            try:
                last = json.loads(lines[-1])
                return last.get("status") == "success"
            except json.JSONDecodeError:
                pass

        return False

    except Exception:
        return False
