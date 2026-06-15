# providers/errors.py
# All QR exceptions extend QRAPIError with a plain_language field.
# plain_language is always the user-facing message — plain, non-technical, actionable.
# All errors map to failure modes F1-F10 defined in the architecture (Section 6.5).

from __future__ import annotations


class QRAPIError(Exception):
    """
    Base class for all QR errors.
    plain_language: user-facing message — must pass the Plain Language Rule.
    step_id / focus_id: optional context for diagnostics export.
    """

    def __init__(
        self,
        plain_language: str,
        step_id: str | None = None,
        focus_id: str | None = None,
        **kwargs,
    ):
        self.plain_language = plain_language
        self.step_id = step_id
        self.focus_id = focus_id
        super().__init__(plain_language)


# -- F1: Ollama unavailable ---------------------------------------------------

class OllamaUnavailableError(QRAPIError):
    """Ollama host unreachable (connection refused/DNS failure).
    Triggers: retry, offer Tier 2, STOP if Tier 1 only space."""
    pass


class OllamaTimeoutError(QRAPIError):
    """Ollama connected but did not respond within timeout."""
    pass


class OllamaGenerationError(QRAPIError):
    """Ollama returned an unexpected HTTP error status."""

    def __init__(self, status_code: int, plain_language: str, **kwargs):
        self.status_code = status_code
        super().__init__(plain_language, **kwargs)


class OllamaInvalidRequestError(QRAPIError):
    """Ollama rejected the request as malformed (e.g. unknown model, bad payload).
    Distinct from connectivity failures — do not retry or offer Tier 2."""
    pass


# -- F2: Quality below floor --------------------------------------------------

class QualityBelowFloorError(QRAPIError):
    """Output quality score below QR_QUALITY_FLOOR threshold."""
    pass


# -- F3: Context window -------------------------------------------------------

class ContextWindowExceededError(QRAPIError):
    """Prompt exceeds model context window. Compact or escalate."""
    pass


class ContextCompactionError(QRAPIError):
    """Raised when direct local context compaction fails."""
    pass


# -- F4: Privacy Guardian hard block ------------------------------------------

class PrivacyGateBlockedError(QRAPIError):
    """Privacy Guardian hard block (PG_GATE_1/PG_GATE_4). STOP, offer alternatives."""
    pass


class ContentPromotionBlockedError(QRAPIError):
    """PG_GATE_3 blocked cross-tier content promotion.
    Content sensitivity exceeds target tier ceiling."""
    pass


class DisclosureLogWriteError(QRAPIError):
    """
    Disclosure log write failed when execution_tier > 1.
    Personal data cannot cross an external tier boundary without a
    permanent audit record — the path run must halt (F_SYSTEM).
    ADR-012: 'If we can't record the disclosure, we can't disclose.'
    Non-fatal at Tier 1 (process counter incremented instead).
    """
    pass


class FloorConsentRequiredError(QRAPIError):
    """
    Floor Consent Gate fired (ADR-012 Amendment 3).

    The user's privacy_default_tier is lower than the external abstraction
    floor (abstraction_tier >= 2 for any Tier 2+ call). At least one field
    would produce a different outcome at the floor vs the user's stated
    preference. The run is paused — no external call has been made.

    This is a recoverable pause, not a failure. FailureHandler maps it to
    action='await_floor_consent'. The run resumes or restarts based on
    the user's consent choice.

    User-facing notification (approved UX copy):
    'To maintain privacy while using [Focus Name], Quiet Rabbit modified
    the following fields for external use: ...'
    Choices: [Continue — use modified values] [Use original values locally] [Cancel]

    floor_clamped_fields: field names whose Gate1 outcome changed because
        of floor clamping (raw_abstraction vs abstraction_tier).
    approved_fields: {field_name: abstracted_value} — what would be sent
        if the user chooses 'Continue'. Shown to user for transparency.
    focus_display_name: human-readable focus name for the UX copy.
    """

    def __init__(
        self,
        floor_clamped_fields: list[str],
        approved_fields: dict[str, str],
        focus_display_name: str,
        plain_language: str,
        **kwargs,
    ):
        self.floor_clamped_fields = floor_clamped_fields
        self.approved_fields = approved_fields
        self.focus_display_name = focus_display_name
        super().__init__(plain_language, **kwargs)


class VoiceProfileContaminationError(QRAPIError):
    """
    Voice profile value scan detected likely personal information before
    prompt assembly.

    Raised at Tier 2+ when a scan detects that a voice profile attribute
    value may contain personal information. At Tier 1, contaminated
    attributes are stripped and execution continues — this error is only
    raised at Tier 2+.

    This is secondary containment. Primary prevention is write-time
    validation in personal_store.py (not yet implemented — D6-326).

    NOTE: If a PrivacyViolationError intermediate base class is introduced
    in future, this error should migrate to it (flagged by ChatGPT review).

    attribute_name: the offending attribute key — never the value.
    contamination_type: classification of the detection signal
        ('personal_field_match' | 'email_pattern' | 'digit_dense').
    execution_tier: the tier at which contamination was detected.
    """

    def __init__(
        self,
        attribute_name: str,
        contamination_type: str,
        execution_tier: int,
        plain_language: str,
        **kwargs,
    ):
        self.attribute_name = attribute_name
        self.contamination_type = contamination_type
        self.execution_tier = execution_tier
        super().__init__(plain_language, **kwargs)


# -- F5: Security Checker flag ------------------------------------------------

class SecurityCheckerFlagError(QRAPIError):
    """Security Checker flagged an artifact. STOP, no retry."""
    pass


# -- F6: Inbound contamination ------------------------------------------------

class InboundContaminationError(QRAPIError):
    """PG_GATE_2 flagged inbound response. Hold for user decision."""
    pass


# -- F7: personal.db unavailable ----------------------------------------------

class PersonalDBNotFoundError(QRAPIError):
    """personal.db file does not exist at expected path. STOP immediately.
    Distinct from decryption failure — file is missing, not unreadable."""
    pass


class PersonalDBDecryptionError(QRAPIError):
    """personal.db exists but SQLCipher PRAGMA key failed.
    Wrong key, corrupted database, or plaintext file where encrypted expected.
    STOP immediately — never proceed with a plaintext fallback."""
    pass


# -- F8: Snapshot write failure -----------------------------------------------

class SnapshotWriteError(QRAPIError):
    """Checkpoint write failed. Degrade to memory-only, suspend checkpointing."""
    pass


# -- F9: Loop detection -------------------------------------------------------

class LoopDetectedError(QRAPIError):
    """Normalized semantic hash matched — execution loop detected. STOP."""
    pass


# -- F10: Tier 2/3 provider errors --------------------------------------------

class MissingAPIKeyError(QRAPIError):
    """API key not found in integration_keys.db."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class InvalidAPIKeyError(QRAPIError):
    """API key rejected by provider (401)."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class ProviderRateLimitError(QRAPIError):
    """Provider rate limit hit (429)."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class ProviderTimeoutError(QRAPIError):
    """Provider took too long to respond."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class ProviderUnavailableError(QRAPIError):
    """Provider unreachable (connection error)."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class ProviderError(QRAPIError):
    """Generic provider error (unexpected HTTP status)."""

    def __init__(self, provider: str, status_code: int, plain_language: str, **kwargs):
        self.provider = provider
        self.status_code = status_code
        super().__init__(plain_language, **kwargs)


class UnknownProviderError(QRAPIError):
    """Provider ID not found in routing table."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


class MissingTier2ConfigError(QRAPIError):
    """No Tier 2 provider configured. User has not completed install interview."""
    pass


class UnknownValidationProvider(QRAPIError):
    """Validation provider ID not found in routing table."""

    def __init__(self, provider: str, plain_language: str, **kwargs):
        self.provider = provider
        super().__init__(plain_language, **kwargs)


# -- Tier routing -------------------------------------------------------------

class TierBoundaryViolationError(QRAPIError):
    """Step routing_tier exceeds space max_permitted_tier.
    Triggers at Step 3 of the 15-step sequence. STOP — never route above ceiling."""

    def __init__(
        self,
        requested_tier: int,
        permitted_tier: int,
        plain_language: str,
        **kwargs,
    ):
        self.requested_tier = requested_tier
        self.permitted_tier = permitted_tier
        super().__init__(plain_language, **kwargs)


# -- Startup and integrity ----------------------------------------------------

class TaxonomyIntegrityError(QRAPIError):
    """SHA-256 manifest verification failed for a taxonomy file.
    In production: fail_fast. In development: warn and continue."""

    def __init__(self, filename: str, plain_language: str, **kwargs):
        self.filename = filename
        super().__init__(plain_language, **kwargs)


class DatabaseMigrationError(QRAPIError):
    """Database migration failed or partially completed.
    auth_enabled must NOT be set to 1 after this error.
    Triggers rollback of any migrated databases."""

    def __init__(self, db_path: str, plain_language: str, **kwargs):
        self.db_path = db_path
        super().__init__(plain_language, **kwargs)


# -- Auth ---------------------------------------------------------------------

class InsecureKeychainError(QRAPIError):
    """Platform keychain backend is insecure (plaintext). Refuse to start."""
    pass
