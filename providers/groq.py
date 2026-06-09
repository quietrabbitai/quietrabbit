# providers/groq.py
# Groq Tier 2 provider — implements Tier2Provider for Groq API.
#
# Model: llama-3.1-8b-instant (drafting — fast, good quality, free tier)
# Provider: Groq (US-based, free tier available)
# API: https://api.groq.com/openai/v1 (OpenAI-compatible chat/completions)
#
# KEY RETRIEVAL (Layer 6 dev bridge):
# API key read from GROQ_API_KEY environment variable.
# WARNING: API keys must not be added to .env — see .env.example.
# For dev testing: add GROQ_API_KEY to the environment: section of
# docker-compose.yml directly (same pattern as QR_DEV_KEY_HEX).
# Layer 8: replaced by integration_keys.db retrieval via InMemoryKeyRegistry.
#
# HONEST FRAMING (CLAUDE.md):
# Groq is US-based. Free tier available. Data processed in the US.
# Users with EU data residency requirements should use Mistral instead.
# This provider makes no recommendation — user chooses at install time.
#
# ERROR MAPPING:
# All requests exceptions mapped to QRAPIError subclasses before returning.
# Callers (StepExecutor) never see raw requests exceptions.
# ProviderTimeoutError, ProviderRateLimitError, ProviderUnavailableError
#   → retryable (executor retry loop handles via F10)
# InvalidAPIKeyError, MissingAPIKeyError, ProviderError
#   → terminal (executor stops, maps to F10 await_user or stop)

from __future__ import annotations

import time

import requests

from providers.errors import (
    InvalidAPIKeyError,
    MissingAPIKeyError,
    ProviderError,
    ProviderRateLimitError,
    ProviderTimeoutError,
    ProviderUnavailableError,
)
from providers.tier2_base import Tier2Provider
from providers.types import GenerateRequest, GenerateResponse, ProviderHealth
from providers.utils import now

import os

GROQ_API_BASE = "https://api.groq.com/openai/v1"
GROQ_TIMEOUT_SECONDS = 30.0
GROQ_HEALTH_TIMEOUT_SECONDS = 3.0


class GroqProvider(Tier2Provider):
    """
    Groq Tier 2 provider using OpenAI-compatible chat/completions endpoint.
    Stateless — no session, no memory, no tools.
    HTTP transport: requests (consistent with Ollama client stack).
    """

    @property
    def provider_id(self) -> str:
        return "groq"

    @property
    def display_name(self) -> str:
        return "Groq"

    def _get_api_key(self) -> str:
        """
        Retrieve API key from environment.
        Layer 6: reads GROQ_API_KEY env var.
        Layer 8: replace with integration_keys.db retrieval.
        Raises MissingAPIKeyError if absent or empty.
        """
        key = os.environ.get("GROQ_API_KEY", "").strip()
        if not key:
            raise MissingAPIKeyError(
                provider="groq",
                plain_language=(
                    "Groq API key is not configured. "
                    "Add GROQ_API_KEY to your environment to use "
                    "the Writing Assistant. [Get help]"
                ),
            )
        return key

    def generate(self, request: GenerateRequest) -> GenerateResponse:
        """
        Send a chat completion request to Groq.
        Prompt delivered as a single user message — executor Step 8 has
        already assembled all context (voice profile, abstracted fields,
        prior step outputs) into the prompt string. No system message needed.
        Privacy contract: prompt contains abstracted values only (Gate1 +
        disclosure buffer guarantee). This method does not validate privacy.
        """
        api_key = self._get_api_key()
        model_name = self.model_id_from_request(request)

        payload = {
            "model": model_name,
            "messages": [
                {"role": "user", "content": request.prompt}
            ],
            "temperature": (
                request.options.temperature if request.options else 0.5
            ),
            "max_tokens": (
                request.options.num_predict if request.options else 1024
            ),
            "stream": False,
        }

        start_ms = time.monotonic() * 1000

        try:
            response = requests.post(
                f"{GROQ_API_BASE}/chat/completions",
                headers={
                    "Authorization": f"Bearer {api_key}",
                    "Content-Type": "application/json",
                },
                json=payload,
                timeout=GROQ_TIMEOUT_SECONDS,
            )
        except requests.Timeout:
            raise ProviderTimeoutError(
                provider="groq",
                plain_language=(
                    "Groq didn't respond in time. "
                    "[Try again] [Use local AI instead]"
                ),
            )
        except requests.ConnectionError:
            raise ProviderUnavailableError(
                provider="groq",
                plain_language=(
                    "Groq is unreachable. Check your internet connection. "
                    "[Try again] [Use local AI instead]"
                ),
            )
        except requests.RequestException:
            raise ProviderUnavailableError(
                provider="groq",
                plain_language=(
                    "Groq connection failed. "
                    "[Try again] [Use local AI instead]"
                ),
            )

        latency_ms = (time.monotonic() * 1000) - start_ms

        if response.status_code == 401:
            raise InvalidAPIKeyError(
                provider="groq",
                plain_language=(
                    "Groq API key was rejected. Check your key is correct "
                    "and has not expired. [Get help]"
                ),
            )
        if response.status_code == 429:
            raise ProviderRateLimitError(
                provider="groq",
                plain_language=(
                    "Groq rate limit reached. "
                    "[Try again in a moment] [Use local AI instead]"
                ),
            )
        if response.status_code != 200:
            raise ProviderError(
                provider="groq",
                status_code=response.status_code,
                plain_language=(
                    f"Groq returned an unexpected error "
                    f"({response.status_code}). "
                    "[Try again] [Get help]"
                ),
            )

        try:
            data = response.json()
            content = data["choices"][0]["message"]["content"]
            usage = data.get("usage", {})
            prompt_tokens = usage.get("prompt_tokens", 0)
            completion_tokens = usage.get("completion_tokens", 0)
        except (KeyError, IndexError, ValueError):
            raise ProviderError(
                provider="groq",
                status_code=response.status_code,
                plain_language=(
                    "Groq returned an unexpected response format. "
                    "[Try again] [Get help]"
                ),
            )

        return GenerateResponse(
            content=content,
            model=request.model,
            prompt_token_count=prompt_tokens,
            output_token_count=completion_tokens,
            latency_ms=latency_ms,
            completion_status="complete",
        )

    def health_check(self) -> ProviderHealth:
        """
        Check Groq availability via the models endpoint.
        Never raises — all exceptions return ProviderHealth(status=...).
        Timeout → unavailable. 401 → degraded (key issue, not network).
        3-second timeout per base class contract.
        """
        try:
            key = self._get_api_key()
        except MissingAPIKeyError:
            return ProviderHealth(
                provider="groq",
                status="unavailable",
                checked_at=now(),
                error="GROQ_API_KEY not configured",
                available_models=[],
            )

        try:
            response = requests.get(
                f"{GROQ_API_BASE}/models",
                headers={"Authorization": f"Bearer {key}"},
                timeout=GROQ_HEALTH_TIMEOUT_SECONDS,
            )
        except requests.Timeout:
            return ProviderHealth(
                provider="groq",
                status="unavailable",
                checked_at=now(),
                error="health check timed out",
                available_models=[],
            )
        except requests.RequestException as e:
            return ProviderHealth(
                provider="groq",
                status="unavailable",
                checked_at=now(),
                error=str(e),
                available_models=[],
            )

        if response.status_code == 401:
            return ProviderHealth(
                provider="groq",
                status="degraded",
                checked_at=now(),
                error="API key rejected (401)",
                available_models=[],
            )

        if response.status_code != 200:
            return ProviderHealth(
                provider="groq",
                status="degraded",
                checked_at=now(),
                error=f"unexpected status {response.status_code}",
                available_models=[],
            )

        try:
            models = [
                m["id"] for m in response.json().get("data", [])
            ]
        except (KeyError, ValueError):
            models = []

        return ProviderHealth(
            provider="groq",
            status="available",
            checked_at=now(),
            error=None,
            available_models=models,
        )
