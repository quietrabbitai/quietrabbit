# providers/tier2_base.py
# Abstract base class for all Tier 2 external providers.
# Concrete implementations: providers/groq.py, providers/mistral.py (future).
#
# CONTRACT:
# - All Tier 2 providers receive abstracted field values only.
#   Raw personal field values never appear in prompts routed here.
#   The disclosure buffer enforces this upstream in executor.py Step 8.
# - generate() is the primary interface. Called by StepExecutor Step 10
#   when execution_tier >= 2.
# - Disclosure log write failure is fatal before generate() is called —
#   DisclosureLogWriteError halts the run. This class never writes the log.
# - Key retrieval is the concrete implementation's responsibility.
#   The base class does not prescribe key source — this allows Layer 6
#   (env vars) and Layer 8 (InMemoryKeyRegistry) to share the same interface.
# - All provider errors must be mapped to QRAPIError subclasses (F10).
#   Callers must never see raw httpx/requests exceptions.
# - Stateless single-request completion model only.
#   No tools, function calling, retrieval, or multi-step pipelines.
#   Hybrid provider patterns are Release 2+.
#
# HONEST FREE-TIER FRAMING (CLAUDE.md):
# Groq: US-based, free tier, faster iteration.
# Mistral: EU/GDPR, paid, stronger privacy guarantees.
# No prescribed default — user chooses at install time.
# This module makes no recommendation.

from __future__ import annotations

from abc import ABC, abstractmethod

from providers.errors import UnknownProviderError
from providers.types import GenerateRequest, GenerateResponse, ProviderHealth


class Tier2Provider(ABC):
    """
    Abstract base for Tier 2 external providers.
    Stateless single-request completion model — no tools, no memory,
    no multi-turn state. All state management is in TaskTrack (executor).
    """

    @property
    @abstractmethod
    def provider_id(self) -> str:
        """
        Short stable identifier. Used in disclosure_log.provider column,
        model prefix validation, and error messages.
        Examples: "groq", "mistral"
        Must match the prefix used in StepExecutor model IDs
        (e.g. "groq:llama-3.1-8b-instant").
        """
        ...

    @property
    @abstractmethod
    def display_name(self) -> str:
        """Human-readable name for user-facing messages."""
        ...

    @abstractmethod
    def generate(self, request: GenerateRequest) -> GenerateResponse:
        """
        Send a generation request to the external provider.
        Returns GenerateResponse on success.

        Privacy contract: the prompt in request contains abstracted field
        values only — enforced upstream by executor.py Step 8 (disclosure
        buffer). This method does not perform privacy validation; that is
        Gate1's responsibility. Concrete implementations may enforce
        provider-specific input constraints (context length, content policy).

        Key retrieval: the concrete implementation is responsible for
        obtaining the API key. The base class prescribes no source —
        Layer 6 uses env vars; Layer 8 uses InMemoryKeyRegistry.

        Error mapping (required — callers must never see raw exceptions):
          MissingAPIKeyError       — key absent from source
          InvalidAPIKeyError       — 401 from provider
          ProviderRateLimitError   — 429 (retryable via executor retry loop)
          ProviderTimeoutError     — timeout (retryable)
          ProviderUnavailableError — connection error (retryable)
          ProviderError            — unexpected HTTP status (terminal)
        """
        ...

    @abstractmethod
    def health_check(self) -> ProviderHealth:
        """
        Check provider availability.
        Must complete within 3 seconds. Must never raise.
        Timeout     → status="unavailable"
        Partial failure → status="degraded"
        Any exception → ProviderHealth(status="unavailable", error=str(e))
        """
        ...

    def model_id_from_request(self, request: GenerateRequest) -> str:
        """
        Extract the bare model name from a GenerateRequest.model string.
        Format: "provider_id:model_name" (e.g. "groq:llama-3.1-8b-instant").
        Validates that the prefix matches self.provider_id.
        Raises UnknownProviderError if prefix is absent or mismatched —
        prevents silent misrouting of requests to the wrong provider.
        """
        if ":" not in request.model:
            raise UnknownProviderError(
                provider=request.model,
                plain_language=(
                    f"Model ID '{request.model}' is missing a provider prefix. "
                    f"Expected format: '{self.provider_id}:model-name'. [Get help]"
                ),
            )
        prefix, model_name = request.model.split(":", 1)
        if prefix != self.provider_id:
            raise UnknownProviderError(
                provider=prefix,
                plain_language=(
                    f"Model prefix '{prefix}' does not match provider "
                    f"'{self.provider_id}'. Check path routing configuration. "
                    "[Get help]"
                ),
            )
        return model_name
