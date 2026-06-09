# providers/evaluation.py
# Level 1 Evaluation Harness.
# Measures model latency and format compliance across task types.
# Results written to model_hardware_scores in scores.db.
# Runs at startup (Layer 1) and on-demand for recalibration.
#
# Task types must match task_types.yaml — any task type added to the
# taxonomy should have a corresponding EvaluationTask here.

from __future__ import annotations

import json
import re

from providers.ollama_client import generate
from providers.types import (
    GenerateRequest,
    GenerateOptions,
    EvaluationTask,
    EvaluationResult,
)
from providers.utils import get_data_root, now, open_db


# -- Release 1 model IDs ------------------------------------------------------

RELEASE_1_MODELS = [
    "llama3.2:3b",      # fast model — quick_response, summarization
    "llama3.1:8b",      # primary reasoning and writing model
    "qwen2.5:7b",       # code specialist — Tech Support path
]


# -- Default evaluation tasks per task type -----------------------------------

DEFAULT_EVALUATION_TASKS: dict[str, EvaluationTask] = {
    "summarization": EvaluationTask(
        task_type="summarization",
        prompt=(
            "Summarize the following in one sentence: "
            "Quiet Rabbit is a self-hosted AI platform that runs entirely "
            "on your own hardware, keeping your personal data private."
        ),
        expected_format="prose",
        latency_target_ms=2000.0,
    ),
    "structured_output": EvaluationTask(
        task_type="structured_output",
        prompt=(
            'Return a JSON object with two keys: "status" set to "ok" '
            'and "message" set to "evaluation complete". '
            "Return JSON only, no explanation."
        ),
        expected_format="structured_output",
        latency_target_ms=2000.0,
    ),
    "code": EvaluationTask(
        task_type="code",
        prompt=(
            "Write a Python function called add(a, b) that returns the "
            "sum of two numbers. Include a docstring."
        ),
        expected_format="code_block",
        latency_target_ms=4000.0,
    ),
    "reasoning": EvaluationTask(
        task_type="reasoning",
        prompt=(
            "A user has a document that is 5,000 words long. "
            "A model can process 4,000 words at a time. "
            "What is the minimum number of passes required to process "
            "the full document? Explain briefly."
        ),
        expected_format="prose",
        latency_target_ms=4000.0,
    ),
    "research": EvaluationTask(
        task_type="research",
        prompt=(
            "List three key privacy considerations for a self-hosted "
            "AI system. Be specific and concise."
        ),
        expected_format="prose",
        latency_target_ms=4000.0,
    ),
    "long_context": EvaluationTask(
        task_type="long_context",
        prompt=(
            "The following is a multi-part document. Read it carefully "
            "and answer the question at the end.\n\n"
            + ("Quiet Rabbit is a self-hosted AI platform. " * 200)
            + "\n\nQuestion: What is Quiet Rabbit? Answer in one sentence."
        ),
        expected_format="prose",
        latency_target_ms=8000.0,
    ),
    "creative_writing": EvaluationTask(
        task_type="creative_writing",
        prompt=(
            "Write a two-sentence product tagline for a privacy-focused "
            "personal AI assistant."
        ),
        expected_format="prose",
        latency_target_ms=3000.0,
    ),
    "quick_response": EvaluationTask(
        task_type="quick_response",
        prompt="What is the capital of France? Answer in one word.",
        expected_format="short_answer",
        latency_target_ms=1000.0,
    ),
}


# -- Format compliance checks -------------------------------------------------

def _is_valid_json(content: str) -> bool:
    """Check JSON compliance. Uses removeprefix/removesuffix for reliable
    markdown fence stripping — avoids lstrip() character-set pitfall."""
    clean = content.strip()
    if clean.startswith("```json"):
        clean = clean.removeprefix("```json")
    elif clean.startswith("```"):
        clean = clean.removeprefix("```")
    if clean.endswith("```"):
        clean = clean.removesuffix("```")
    clean = clean.strip()
    try:
        json.loads(clean)
        return True
    except (json.JSONDecodeError, ValueError):
        return False


def _looks_like_code(content: str) -> bool:
    patterns = [
        r"def \w+\(",
        r"function \w+\(",
        r"class \w+[:{]",
        r"import \w+",
        r"#include",
    ]
    return any(re.search(p, content) for p in patterns)


def check_format_compliance(content: str, expected_format: str) -> bool:
    checks = {
        "prose": lambda c: (
            len(c.strip()) > 0 and not c.strip().startswith("{")
        ),
        "short_answer": lambda c: len(c.strip()) > 0,
        "structured_output": _is_valid_json,
        "code_block": lambda c: "```" in c or _looks_like_code(c),
    }
    check_fn = checks.get(expected_format, lambda c: True)
    return check_fn(content)


# -- Evaluation harness -------------------------------------------------------

class EvaluationHarness:
    """
    Level 1 evaluation: latency + format compliance.
    Score = (latency_score * 0.40) + (format_compliance * 0.60)
    Results persisted to models/scores.db via open_db() wrapper.
    """

    def run_single(
        self,
        model_id: str,
        task: EvaluationTask,
        options: GenerateOptions | None = None,
    ) -> EvaluationResult:
        """Run one evaluation task on one model. Returns EvaluationResult."""
        opts = options or GenerateOptions(
            temperature=0.3,    # low temperature for consistent evaluation
            top_p=0.90,
            num_ctx=2048,
            num_predict=512,
        )

        response = generate(
            GenerateRequest(
                model=model_id,
                prompt=task.prompt,
                task_type=task.task_type,
                options=opts,
            )
        )

        format_ok = check_format_compliance(
            response.content, task.expected_format
        )
        latency_score = min(
            1.0,
            task.latency_target_ms / max(response.latency_ms, 1)
        )
        score = (latency_score * 0.40) + (float(format_ok) * 0.60)

        result = EvaluationResult(
            model_id=model_id,
            task_type=task.task_type,
            latency_ms=response.latency_ms,
            format_compliant=format_ok,
            score=score,
            hardware_factor=min(1.0, latency_score),
            seeded_score=score,
        )

        self._persist_result(result)
        return result

    def run_all(
        self,
        model_ids: list[str] | None = None,
        tasks: dict[str, EvaluationTask] | None = None,
    ) -> list[EvaluationResult]:
        """
        Run all task types against all models.
        Defaults to RELEASE_1_MODELS and DEFAULT_EVALUATION_TASKS.
        Skips models that are unavailable — does not raise.
        """
        models = model_ids or RELEASE_1_MODELS
        task_map = tasks or DEFAULT_EVALUATION_TASKS
        results = []

        for model_id in models:
            for task_type, task in task_map.items():
                try:
                    result = self.run_single(model_id, task)
                    results.append(result)
                    print(
                        f"  {model_id} / {task_type}: "
                        f"score={result.score:.2f} "
                        f"latency={result.latency_ms:.0f}ms "
                        f"format={'OK' if result.format_compliant else 'FAIL'}"
                    )
                except Exception as e:
                    print(f"  {model_id} / {task_type}: SKIP ({e})")

        return results

    def _persist_result(self, result: EvaluationResult) -> None:
        """
        Write result to models/scores.db via open_db() wrapper.
        open_db() ensures correct journal mode for NAS compatibility.
        scores.db is unencrypted — open_db() is the correct opener.
        INSERT OR REPLACE: one row per model_id + task_type.
        """
        data_root = get_data_root()
        db_path = data_root / "models" / "scores.db"

        if not db_path.exists():
            return  # scores.db not initialized yet — skip silently

        with open_db(db_path) as conn:
            conn.execute("""
                INSERT OR REPLACE INTO model_hardware_scores (
                    id, model_id, task_type, latency_ms,
                    format_compliance, hardware_factor,
                    seeded_score, sample_count, recorded_at
                ) VALUES (
                    lower(hex(randomblob(8))),
                    ?, ?, ?, ?, ?, ?, 1, ?
                )
            """, [
                result.model_id,
                result.task_type,
                result.latency_ms,
                float(result.format_compliant),
                result.hardware_factor,
                result.seeded_score,
                now(),
            ])
