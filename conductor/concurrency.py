# conductor/concurrency.py
# ConductorScheduler — resource arbitration for concurrent focus runs.
#
# Enforces two independent ceilings (Architecture Section 6.10):
#   MAX_CONCURRENT_INFERENCE=1  — only one Ollama call at a time (GPU serialization)
#   MAX_CONCURRENT_RUNS         — total simultaneous focus runs (default 3, env-tunable)
#
# Interactive focuses request cooperative yield from background focuses at step boundaries.
# Preemption is cooperative — background runs check should_yield() BEFORE acquiring
# the inference slot for their next step. Active inference calls cannot be interrupted.
#
# Layer 3: scheduling infrastructure only. Lifecycle wires should_yield() checks
# at step boundaries in the same pass as executor wiring.
# Layer 4: replace raw semaphore priority logic with a proper priority queue.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   path_run_id → run_id (parameter name throughout)
#   acquire_path_slot → acquire_run_slot
#   release_path_slot → release_run_slot
#   MAX_CONCURRENT_PATHS → MAX_CONCURRENT_RUNS
#   path_slots → run_slots
#   _active_runs key comments updated

from __future__ import annotations

import os
import threading
from enum import Enum


MAX_CONCURRENT_INFERENCE: int = 1   # hard limit — GPU cannot parallelize safely
MAX_CONCURRENT_RUNS: int = int(os.environ.get("QR_MAX_CONCURRENT_PATHS", "3"))

RUN_SLOT_TIMEOUT: float = float(os.environ.get("QR_PATH_SLOT_TIMEOUT", "30.0"))
INFERENCE_SLOT_TIMEOUT: float = float(os.environ.get("QR_INFERENCE_SLOT_TIMEOUT", "60.0"))


class PathPriority(Enum):
    INTERACTIVE = "interactive"   # user-initiated, foreground
    BACKGROUND = "background"     # scheduled, deferred


class ConductorScheduler:
    """
    Manages two resource ceilings:
      - inference_slot: Semaphore(1) — serializes GPU calls
      - run_slots: Semaphore(MAX_CONCURRENT_RUNS) — caps total active runs

    Interactive runs request cooperative yield from background runs:
      When an interactive request arrives and the inference slot is held by
      a background run, the scheduler records that run as the yield target.
      The background run checks should_yield() before reacquiring the slot
      for its next step, and yields cooperatively if targeted.

    False returns from acquire_*() indicate resource contention.
    Callers are responsible for mapping False to plain-language recovery options
    (e.g. "Quiet Rabbit is busy — your run will start shortly").

    INVARIANT: release_run_slot() must only be called after acquire_run_slot()
    returns True. Callers that acquire conditionally must track success themselves.
    Layer 4 should move to explicit ownership tokens to enforce this structurally.

    interactive_wait_count: tracks queued interactive requests for future
    priority-queue implementation (Layer 4). Currently informational only.
    """

    def __init__(self) -> None:
        self._inference_slot = threading.Semaphore(MAX_CONCURRENT_INFERENCE)
        self._run_slots = threading.Semaphore(MAX_CONCURRENT_RUNS)
        self._lock = threading.Lock()
        self._active_runs: dict[str, PathPriority] = {}
        self._inference_holder: str | None = None
        self._preempt_requested_for: str | None = None  # targeted run_id, not a bool
        self.interactive_wait_count: int = 0            # Layer 4: priority queue signal

    # -- Run slot management --------------------------------------------------

    def acquire_run_slot(
        self,
        run_id: str,
        priority: PathPriority,
        timeout: float = RUN_SLOT_TIMEOUT,
    ) -> bool:
        """
        Acquire a run execution slot before starting a focus run.
        Returns True on success, False on timeout (resource contention).
        Caller maps False to plain-language recovery message.
        """
        acquired = self._run_slots.acquire(timeout=timeout)
        if acquired:
            with self._lock:
                self._active_runs[run_id] = priority
        return acquired

    def release_run_slot(self, run_id: str) -> None:
        """
        Release a run execution slot at cleanup.
        Safe to call on unregistered run_id (no-op).
        Must only be called after acquire_run_slot() returned True.
        """
        with self._lock:
            if run_id in self._active_runs:
                del self._active_runs[run_id]
                self._run_slots.release()

    # -- Inference slot management --------------------------------------------

    def acquire_inference_slot(
        self,
        run_id: str,
        priority: PathPriority,
        timeout: float = INFERENCE_SLOT_TIMEOUT,
    ) -> bool:
        """
        Acquire the inference slot before calling Ollama.
        Interactive runs target the current background holder for cooperative yield.
        Returns True on success, False on timeout (resource contention).
        Caller maps False to plain-language recovery message.
        """
        with self._lock:
            if priority == PathPriority.INTERACTIVE:
                self.interactive_wait_count += 1
                # Target the current holder for cooperative yield if it is background
                if (
                    self._inference_holder is not None
                    and self._active_runs.get(self._inference_holder)
                    == PathPriority.BACKGROUND
                ):
                    self._preempt_requested_for = self._inference_holder

        acquired = self._inference_slot.acquire(timeout=timeout)

        with self._lock:
            if acquired:
                self._inference_holder = run_id
                # Only clear preemption signal when an interactive run acquires.
                # Background runs acquiring the slot leave the signal intact so
                # that a waiting interactive run can still trigger yield on the
                # next background reacquisition.
                if priority == PathPriority.INTERACTIVE:
                    self._preempt_requested_for = None
                    self.interactive_wait_count = max(0, self.interactive_wait_count - 1)
            else:
                # Timeout path — undo the increment from the arrival block
                # so interactive_wait_count stays accurate.
                if priority == PathPriority.INTERACTIVE:
                    self.interactive_wait_count = max(0, self.interactive_wait_count - 1)

        return acquired

    def release_inference_slot(self, run_id: str) -> None:
        """
        Release the inference slot after an Ollama call completes.
        Must be called in a finally block to guarantee release.
        Non-holders are silently ignored — semaphore is not released.
        Clears stale preemption target if this holder was being targeted,
        preventing spurious yield signals on future background acquisitions.
        """
        should_release = False
        with self._lock:
            if self._inference_holder == run_id:
                self._inference_holder = None
                # Clear preemption target — holder has released naturally.
                if self._preempt_requested_for == run_id:
                    self._preempt_requested_for = None
                should_release = True
        if should_release:
            self._inference_slot.release()

    # -- Cooperative yield check (called at step boundaries) ------------------

    def should_yield(self, run_id: str) -> bool:
        """
        Returns True if this background run should yield before reacquiring
        the inference slot for its next step.
        Called by background runs at each step boundary, BEFORE acquire_inference_slot().
        Interactive runs should never call this — they do not yield.
        """
        with self._lock:
            if self._active_runs.get(run_id) != PathPriority.BACKGROUND:
                return False
            return self._preempt_requested_for == run_id

    # -- Diagnostics ----------------------------------------------------------

    def active_run_count(self) -> int:
        """Current number of active focus runs."""
        with self._lock:
            return len(self._active_runs)

    def inference_slot_holder(self) -> str | None:
        """run_id currently holding the inference slot, or None."""
        with self._lock:
            return self._inference_holder
