# conductor/compaction.py
# ContextCompactor — direct local model call for context window reduction.
# Called when a prompt approaches the context window limit (F3 warn threshold).
#
# Design constraints (Section 6.6):
# - Direct Ollama call only — no specialist routing, no Librarian
# - No PG gates — compaction input is already TaskTrack content (model output only)
# - routing_table passed explicitly to avoid circular import with taxonomy loader
# - Tier 1 local only — never compacts by sending content to external provider
# - Fails closed: empty output or any generation failure raises ContextCompactionError

from __future__ import annotations

from dataclasses import dataclass

from providers.ollama_client import generate
from providers.types import GenerateRequest, GenerateOptions
from providers.errors import (
    ContextCompactionError,
    OllamaUnavailableError,
    OllamaTimeoutError,
    OllamaGenerationError,
    OllamaInvalidRequestError,
)


COMPACTION_MODEL = "llama3.2:3b"    # fast model — compaction is overhead, not output
COMPACTION_MAX_TOKENS = 512         # summary target — keeps headroom for main prompt
COMPACTION_TEMPERATURE = 0.1        # low temperature — factual compression, not generation
COMPACTION_CONTEXT_WINDOW = 4096    # llama3.2:3b window — tunable here if model changes

COMPACTION_PROMPT_TEMPLATE = (
    "Summarize the following content as concisely as possible, "
    "preserving all key facts, decisions, outputs, variable names, "
    "and identifiers exactly. "
    "Do not add commentary or explanation.\n\n"
    "{content}"
)


@dataclass
class CompactionResult:
    original_length: int    # approximate size metric (chars, not tokens)
    compacted_length: int   # approximate size metric (chars, not tokens)
    compacted_text: str
    compaction_ratio: float  # compacted_length / original_length; <1.0 = reduction
    model_used: str


class ContextCompactor:
    """
    Compacts accumulated TaskTrack content when context window pressure
    is detected (F3 warn threshold: QR_CONTEXT_WARNING_THRESHOLD, default 0.75).

    Called directly from StepExecutor or lifecycle — not routed through
    the specialist system. routing_table is passed in explicitly.

    Usage:
        compactor = ContextCompactor()
        result = compactor.compact(text, routing_table)
    """

    def compact(self, text: str, _routing_table: dict) -> CompactionResult:
        """
        Compact text via direct local model call.
        _routing_table is accepted but unused in Layer 3 — present for
        forward compatibility when routing logic is wired in Layer 4+.

        Fails closed: raises ContextCompactionError on any generation
        failure or empty output. Never returns empty compacted_text.
        """
        if not text or not text.strip():
            return CompactionResult(
                original_length=0,
                compacted_length=0,
                compacted_text="",
                compaction_ratio=1.0,
                model_used=COMPACTION_MODEL,
            )

        prompt = COMPACTION_PROMPT_TEMPLATE.format(content=text.strip())

        try:
            response = generate(GenerateRequest(
                model=COMPACTION_MODEL,
                prompt=prompt,
                task_type="summarization",
                stream=False,
                options=GenerateOptions(
                    temperature=COMPACTION_TEMPERATURE,
                    top_p=0.90,
                    num_ctx=COMPACTION_CONTEXT_WINDOW,
                    num_predict=COMPACTION_MAX_TOKENS,
                ),
            ))
        except (
            OllamaUnavailableError,
            OllamaTimeoutError,
            OllamaGenerationError,
            OllamaInvalidRequestError,
        ) as e:
            raise ContextCompactionError(
                plain_language=(
                    "Quiet Rabbit couldn't compress the context to continue. "
                    "[Shorten your input] or [Start a new run]"
                )
            ) from e

        # Fail closed — empty or whitespace-only output would erase context
        if not response.content or not response.content.strip():
            raise ContextCompactionError(
                plain_language=(
                    "Quiet Rabbit couldn't compress the context to continue. "
                    "[Shorten your input] or [Start a new run]"
                )
            )

        original_len = len(text)
        compacted_len = len(response.content)

        return CompactionResult(
            original_length=original_len,
            compacted_length=compacted_len,
            compacted_text=response.content,
            compaction_ratio=compacted_len / original_len if original_len > 0 else 1.0,
            model_used=COMPACTION_MODEL,
        )
