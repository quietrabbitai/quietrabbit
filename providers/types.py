# providers/types.py
# Core data types for all provider interactions.
# Used by ollama_client.py, tier2 providers, and the Conductor.
# GenerateRequest.stream is always resolved by StepExecutor —
# callers must not set it directly.

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal


@dataclass
class GenerateOptions:
    temperature: float
    top_p: float
    num_ctx: int
    num_predict: int = 2048


@dataclass
class GenerateRequest:
    model: str
    prompt: str
    task_type: str
    stream: bool | None = None      # StepExecutor resolves — callers must not set
    options: GenerateOptions | None = None


@dataclass
class ChatMessage:
    role: Literal['system', 'user', 'assistant']
    content: str


@dataclass
class GenerateResponse:
    content: str
    model: str
    prompt_token_count: int
    output_token_count: int
    latency_ms: float
    completion_status: Literal['complete', 'streaming', 'cancelled'] = 'complete'
    # 'streaming' only used in Release 2


@dataclass
class ProviderHealth:
    provider: str
    status: Literal['available', 'degraded', 'unavailable']
    checked_at: str                 # ISO 8601 UTC string via now()
    error: str | None = None
    available_models: list[str] = field(default_factory=list)


@dataclass
class ContextWindowStatus:
    status: Literal['ok', 'warn', 'exceeded']
    token_estimate: int = 0
    context_window: int = 0
    usage_fraction: float = 0.0
    plain_language: str | None = None
    recommended_action: Literal['compact_then_escalate'] | None = None


@dataclass
class ModelfileVersion:
    model_id: str
    expected_version: str
    applied_version: str | None     # None = Modelfile not yet applied
    is_current: bool


@dataclass
class EvaluationTask:
    task_type: str
    prompt: str
    expected_format: Literal['prose', 'structured_output', 'code_block', 'short_answer']
    latency_target_ms: float


@dataclass
class EvaluationResult:
    model_id: str
    task_type: str
    latency_ms: float
    format_compliant: bool
    score: float                    # (latency_score * 0.40) + (format * 0.60)
    hardware_factor: float = 1.0    # for persistence to model_hardware_scores
    seeded_score: float = 0.0       # raw score before hardware factor applied
