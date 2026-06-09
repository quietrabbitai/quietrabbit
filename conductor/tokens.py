# conductor/tokens.py
# System token definitions and StepDefinition dataclass.
# SYSTEM_TOKENS: reserved variable names injected by the Conductor.
# These cannot be used as output_var names in .focus files.
# StepDefinition: the internal representation of a single focus step,
# populated during Phase 1 LOAD from the .focus file.
# Immutable after construction — frozen=True enforced, dict fields
# wrapped in MappingProxyType to prevent content mutation.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   path_context → focus_context
#   space_context → life_context

from __future__ import annotations

from dataclasses import dataclass, field
from types import MappingProxyType
from typing import Literal, Mapping


# Tokens injected by the Conductor into prompt templates.
# Declared in guide/.focus files using {token_name} syntax.
# Defined as frozenset — immutable, hashable, O(1) lookup.
SYSTEM_TOKENS: frozenset[str] = frozenset({
    "user_input",       # the user's current request
    "life_context",     # life-level shared context
    "voice_profile",    # assembled voice profile for this step
    "previous_output",  # output_var from the immediately preceding step
    "focus_context",    # focus-level metadata (name, description)
})


@dataclass(frozen=True)
class StepDefinition:
    """
    Internal representation of a single step from a .focus file.
    Populated during Phase 1 LOAD. Immutable after construction.
    frozen=True prevents field reassignment. MappingProxyType prevents
    content mutation of dict fields.
    """
    step_id: str
    display_name: str
    guide_id: str
    task_type: str                  # must match task_types.yaml — validated at LOAD
    routing_tier: int               # 1, 2, or 3
    step_type: Literal[
        "generate", "voice_transform", "post_process"
    ] = "generate"
    output_var: str | None = None
    prompt_template: str = ""
    field_requirements: Mapping[
        str, Literal["recommended", "optional", "not_needed"]
    ] = field(default_factory=dict)
    options_override: Mapping[str, object] = field(default_factory=dict)

    def __post_init__(self):
        # Wrap mutable dict fields in MappingProxyType.
        # frozen=True prevents field reassignment but not dict content mutation —
        # MappingProxyType closes that gap.
        object.__setattr__(
            self, "field_requirements",
            MappingProxyType(dict(self.field_requirements))
        )
        object.__setattr__(
            self, "options_override",
            MappingProxyType(dict(self.options_override))
        )


def validate_step(step: StepDefinition) -> list[str]:
    """
    Validate a StepDefinition after LOAD. Returns list of error strings.
    Empty list = valid. Called by lifecycle.py during Phase 1 LOAD.

    Checks:
    - output_var does not collide with a SYSTEM_TOKEN
    - step_type is a known value
    - routing_tier is 1, 2, or 3
    """
    errors = []

    if step.output_var and step.output_var in SYSTEM_TOKENS:
        errors.append(
            f"Step '{step.step_id}': output_var '{step.output_var}' "
            f"collides with a system token. "
            f"System tokens: {sorted(SYSTEM_TOKENS)}"
        )

    if step.step_type not in ("generate", "voice_transform", "post_process"):
        errors.append(
            f"Step '{step.step_id}': unknown step_type '{step.step_type}'. "
            f"Must be: generate | voice_transform | post_process"
        )

    if step.routing_tier not in (1, 2, 3):
        errors.append(
            f"Step '{step.step_id}': routing_tier must be 1, 2, or 3. "
            f"Got: {step.routing_tier}"
        )

    return errors
