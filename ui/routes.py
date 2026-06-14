# ui/routes.py
# Layer 0: /health and /diagnostics endpoints
# Layer 3: /run (submit), /status/<focus_run_id> (poll), /output/<focus_run_id> (display)
# Layer 6: /consent/<focus_run_id> (Floor Consent Gate UI — dev mode only)
#
# Layer 3 auth: single hardcoded dev user and life for smoke testing only.
# Real auth (InMemoryKeyRegistry, Flask-Login, per-user keys) wired in Layer 8.
# All three Layer 3 endpoints are gated behind a DEV_MODE guard — they will not
# activate in production (QR_ENV != 'development') until Layer 8 auth lands.
#
# UI is minimal HTML — enough to submit a prompt, poll for completion, and
# display output. No styling. Sole purpose: prove end-to-end pipeline works.
#
# D5-152 (floor consent preference scoping):
# floor_consent_preference stored as scoped dict in personas.extra_metadata in
# shared.db (open_instance_db) — NOT outputs.db (no personas table there).
# Consent binds to abstraction_tier, not focus_id or provider_id.
# Schema: {"mode": "modified", "abstraction_tier": N,
#           "consent_timestamp": "...", "consent_version": "1"}
# lifecycle._execute_step() validates abstraction_tier match before honoring.
#
# Updated as part of Phase A codebase rename (D6-224, D6-225):
#   _DEV_SPACE_ID → _DEV_PERSONA_ID, "dev-space" → "dev-life"
#   QR_INTERVIEW_SPACE_ID → QR_INTERVIEW_LIFE_ID env var
#   PathRun → FocusRun import
#   path_run_id → focus_run_id in routes and HTML
#   path_runs → focus_runs in SQL
#   spaces table → lives table
#   get_path_run_status → get_focus_run_status
#   get_output_for_run signature updated (life_id param)
#   open_personal_db / open_outputs_db: space_id → life_id
#   Quick Draft → Quick Ask in UI text
# Updated as part of Phase C Persona model migration (D6-298):
#   _DEV_LIFE_ID → _DEV_PERSONA_ID
#   FocusRun, output_store, consent calls: life_id → persona_id
#   disclosure_log INSERT: life_id column → persona_id
#   Floor consent: FROM lives / UPDATE lives → personas
#   D5-152: lives.extra_metadata → personas.extra_metadata

from __future__ import annotations

import os
import threading
from pathlib import Path

from flask import Flask, jsonify, request, render_template_string

from providers.utils import get_data_root, now
from conductor.concurrency import ConductorScheduler, PathPriority


# -- Dev mode constants (Layer 3 smoke test only) ----------------------------

_DEV_MODE = os.environ.get("QR_ENV", "production") == "development"
_DEV_USER_ID = "dev-user"
_DEV_PERSONA_ID = "dev-life"

_DEV_KEY_HEX = os.environ.get("QR_DEV_KEY_HEX", "")
if _DEV_MODE and not _DEV_KEY_HEX:
    raise RuntimeError(
        "QR_DEV_KEY_HEX must be set in .env when QR_ENV=development. "
        "A random key on each restart makes encrypted DBs permanently unreadable. "
        "Add a 64-char hex string to your .env file and to the environment: "
        "section of docker-compose.yml.\n"
        "Example: QR_DEV_KEY_HEX="
        "0000000000000000000000000000000000000000000000000000000000000000"
    )


# -- Minimal smoke test HTML --------------------------------------------------

_SUBMIT_FORM = """<!DOCTYPE html>
<html>
<head><title>Quiet Rabbit — Dev Smoke Test</title></head>
<body>
<h2>Quiet Rabbit — Dev Smoke Test</h2>
<form method="POST" action="/run">
  <label for="focus_id">Focus:</label><br>
  <select name="focus_id" id="focus_id">
    <option value="quick-ask">Quick Ask</option>
    <option value="research-and-buy">Research &amp; Buy</option>
  </select><br><br>
  <textarea name="prompt" rows="6" cols="60"
    placeholder="Type your prompt here..."></textarea><br><br>
  <input type="submit" value="Run">
</form>
{% if run_id %}
<p>Run started: <code>{{ run_id }}</code></p>
<p><a href="/status/{{ run_id }}">Check status (JSON)</a></p>
{% endif %}
</body>
</html>"""

_OUTPUT_PAGE = """<!DOCTYPE html>
<html>
<head><title>Quiet Rabbit — Output</title></head>
<body>
<h2>Output</h2>
<p><strong>Run:</strong> <code>{{ run_id }}</code></p>
<p><strong>Status:</strong> {{ status }}</p>
<hr>
<pre style="white-space: pre-wrap;">{{ content | e }}</pre>
<br>
<a href="/">New draft</a>
</body>
</html>"""

_CONSENT_PAGE = """<!DOCTYPE html>
<html>
<head><title>Quiet Rabbit — Privacy Review</title></head>
<body>
<h2>Privacy Review</h2>
<p>To maintain privacy while using <strong>{{ focus_display_name }}</strong>,
Quiet Rabbit modified the following fields for external use:</p>
<table border="1" cellpadding="6" cellspacing="0">
  <tr><th>Field</th><th>Will be sent as</th></tr>
  {% for field_name, abstracted_value in clamped_fields %}
  <tr><td>{{ field_name }}</td><td><em>{{ abstracted_value }}</em></td></tr>
  {% endfor %}
</table>
<br>
<form method="POST" action="/consent/{{ run_id }}">
  <p>
    <label>
      <input type="checkbox" name="remember" value="1">
      Remember my choice for this Persona
    </label>
  </p>
  <button name="choice" value="continue">Continue &mdash; use modified values</button>
  &nbsp;
  <button name="choice" value="local">Use original values locally</button>
  &nbsp;
  <button name="choice" value="cancel">Cancel</button>
</form>
</body>
</html>"""

_CONSENT_RESULT_PAGE = """<!DOCTYPE html>
<html>
<head><title>Quiet Rabbit — Consent Recorded</title></head>
<body>
<h2>{{ heading }}</h2>
<p>{{ message }}</p>
{% if run_id %}
<p><a href="/status/{{ run_id }}">Check run status</a></p>
{% endif %}
<p><a href="/">New draft</a></p>
</body>
</html>"""


# -- Health check helper ------------------------------------------------------

def _check_data_root_writable(data_root: Path) -> bool:
    if not data_root.exists():
        return False
    try:
        sentinel = data_root / ".qr_health_check"
        sentinel.write_text("ok")
        sentinel.unlink()
        return True
    except OSError:
        return False


# -- App factory --------------------------------------------------------------

def create_app() -> Flask:
    app = Flask(__name__)
    scheduler = ConductorScheduler()

    # -------------------------------------------------------------------------
    # Layer 0 — /health and /diagnostics
    # -------------------------------------------------------------------------

    @app.route("/health")
    def health():
        data_root = get_data_root()
        data_root_ok = _check_data_root_writable(data_root)
        status = "ok" if data_root_ok else "degraded"
        return jsonify({
            "status": status,
            "timestamp": now(),
            "data_root_writable": data_root_ok,
        }), 200 if status == "ok" else 503

    @app.route("/diagnostics")
    def diagnostics():
        from conductor.privacy import get_process_disclosure_log_failures
        data_root = get_data_root()
        data_root_ok = _check_data_root_writable(data_root)
        return jsonify({
            "timestamp": now(),
            "qr_env": os.environ.get("QR_ENV", "production"),
            "qr_network_storage": os.environ.get("QR_NETWORK_STORAGE", "false"),
            "ollama_configured": bool(os.environ.get("OLLAMA_HOST")),
            "data_root_exists": data_root.exists(),
            "data_root_writable": data_root_ok,
            "instance_db_exists": (
                data_root / "instance" / "shared.db"
            ).exists(),
            "privacy_audit_write_failures": get_process_disclosure_log_failures(),
            "layer": 6,
            "note": (
                "Layer 6 — Writing Assistant, Tier 2 routing, "
                "Floor Consent Gate, ADR-012 complete."
            ),
        }), 200

    # -------------------------------------------------------------------------
    # Layer 3 — Quick Ask smoke test endpoints (dev mode only)
    # -------------------------------------------------------------------------

    @app.route("/", methods=["GET"])
    def index():
        if not _DEV_MODE:
            return jsonify({"error": "UI not available in production mode."}), 404
        return render_template_string(_SUBMIT_FORM)

    @app.route("/run", methods=["POST"])
    def run_focus():
        """
        Submit a focus run (Quick Ask or Writing Assistant).
        load() + authorize() synchronous; initialize() through cleanup() background.
        Layer 8: replace dev constants with session-derived values.
        """
        if not _DEV_MODE:
            return jsonify({"error": "Not available in production mode."}), 404

        prompt = request.form.get("prompt", "").strip()
        focus_id = request.form.get("focus_id", "quick-ask").strip()
        if not prompt:
            return render_template_string(_SUBMIT_FORM, run_id=None), 400

        from conductor.lifecycle import FocusRun

        run = FocusRun(
            user_id=_DEV_USER_ID,
            persona_id=_DEV_PERSONA_ID,
            focus_id=focus_id,
            scheduler=scheduler,
            user_input=prompt,
            key_hex=_DEV_KEY_HEX,
        )

        try:
            run.load()
            run.authorize()
        except Exception as e:
            return jsonify({"error": str(e)}), 500

        if not scheduler.acquire_run_slot(
            run.focus_run_id, PathPriority.INTERACTIVE, timeout=2.0
        ):
            return jsonify({
                "error": (
                    "Quiet Rabbit is at capacity. "
                    "Please wait for current runs to finish."
                )
            }), 503

        def _execute_remaining() -> None:
            try:
                run.initialize()
                early = run.execute()
                if early is None:
                    run.output()
            except Exception:
                try:
                    run._write_focus_run_record(status="failed")
                except Exception:
                    pass
            finally:
                try:
                    current = run._get_current_status()
                    final = (
                        "failed"
                        if current in ("running", "initializing")
                        else current
                    )
                    run.cleanup(final_status=final)
                except Exception:
                    pass
                scheduler.release_run_slot(run.focus_run_id)

        thread = threading.Thread(target=_execute_remaining, daemon=True)
        thread.start()

        return render_template_string(_SUBMIT_FORM, run_id=run.focus_run_id)

    @app.route("/status/<focus_run_id>", methods=["GET"])
    def run_status(focus_run_id: str):
        if not _DEV_MODE:
            return jsonify({"error": "Not available in production mode."}), 404

        from persistence.output_store import get_focus_run_status

        status = get_focus_run_status(
            user_id=_DEV_USER_ID,
            persona_id=_DEV_PERSONA_ID,
            key_hex=_DEV_KEY_HEX,
            focus_run_id=focus_run_id,
        )
        if status is None:
            return jsonify({"error": "Run not found."}), 404

        response: dict = {"focus_run_id": focus_run_id, "status": status}
        if status == "awaiting_feedback":
            response["output_url"] = f"/output/{focus_run_id}"
        if status == "awaiting_user":
            response["consent_url"] = f"/consent/{focus_run_id}"

        return jsonify(response), 200

    @app.route("/output/<focus_run_id>", methods=["GET"])
    def run_output(focus_run_id: str):
        if not _DEV_MODE:
            return jsonify({"error": "Not available in production mode."}), 404

        from persistence.output_store import get_output_for_run, get_focus_run_status

        status = get_focus_run_status(
            user_id=_DEV_USER_ID,
            persona_id=_DEV_PERSONA_ID,
            key_hex=_DEV_KEY_HEX,
            focus_run_id=focus_run_id,
        )
        if status is None:
            return jsonify({"error": "Run not found."}), 404

        output = get_output_for_run(
            user_id=_DEV_USER_ID,
            persona_id=_DEV_PERSONA_ID,
            key_hex=_DEV_KEY_HEX,
            focus_run_id=focus_run_id,
        )
        if output is None:
            return render_template_string(
                _OUTPUT_PAGE,
                run_id=focus_run_id,
                status=status,
                content="Output not yet available.",
            )

        return render_template_string(
            _OUTPUT_PAGE,
            run_id=focus_run_id,
            status=status,
            content=output.content,
        )

    # -------------------------------------------------------------------------
    # Layer 6 — Floor Consent Gate (dev mode only)
    # -------------------------------------------------------------------------

    @app.route("/consent/<focus_run_id>", methods=["GET"])
    def consent_get(focus_run_id: str):
        """
        Display the Floor Consent Gate UI for a paused run.
        Layer 8: replace with full React consent component.
        """
        if not _DEV_MODE:
            return jsonify({"error": "Not available in production mode."}), 404

        import json
        from providers.utils import open_outputs_db

        try:
            with open_outputs_db(_DEV_USER_ID, _DEV_PERSONA_ID, _DEV_KEY_HEX) as db:
                row = db.execute(
                    "SELECT status, notes FROM focus_runs WHERE id = ?",
                    [focus_run_id]
                ).fetchone()
        except Exception as e:
            return jsonify({"error": str(e)}), 500

        if not row:
            return jsonify({"error": "Run not found."}), 404

        if row["status"] != "awaiting_user":
            return jsonify({
                "error": f"Run is not awaiting consent (status: {row['status']})."
            }), 400

        try:
            notes = json.loads(row["notes"] or "{}")
            floor_meta = notes.get("floor_consent_meta", {})
            floor_clamped = floor_meta.get("floor_clamped_fields", [])
            approved = floor_meta.get("approved_fields", {})
            focus_display = floor_meta.get("focus_display_name", "this focus")
        except (json.JSONDecodeError, KeyError):
            floor_clamped = []
            approved = {}
            focus_display = "this focus"

        clamped_pairs = [
            (name, approved.get(name, "—")) for name in floor_clamped
        ]

        return render_template_string(
            _CONSENT_PAGE,
            run_id=focus_run_id,
            focus_display_name=focus_display,
            clamped_fields=clamped_pairs,
        )

    @app.route("/consent/<focus_run_id>", methods=["POST"])
    def consent_post(focus_run_id: str):
        """
        Handle the user's Floor Consent Gate decision.

        choice=continue: if remember checked, persist scoped consent to
          personas.extra_metadata in shared.db (open_instance_db).
          Consent schema (D5-152):
            {"mode": "modified", "abstraction_tier": N,
             "consent_timestamp": "...", "consent_version": "1"}
          Consent binds to abstraction_tier — not focus_id or provider_id.

        choice=local: store mode="local" if remember checked.
        choice=cancel: write floor_consent_cancelled, set status=cancelled.

        Layer 6 limitation: "Continue" records the consent but does not
        automatically resume execution — the run must be resubmitted with
        the consent preference stored. Full resume wired in Layer 8.
        """
        if not _DEV_MODE:
            return jsonify({"error": "Not available in production mode."}), 404

        import json
        import uuid
        from providers.utils import open_outputs_db, open_personal_db, open_instance_db

        choice = request.form.get("choice", "cancel")
        remember = request.form.get("remember") == "1"

        try:
            with open_outputs_db(_DEV_USER_ID, _DEV_PERSONA_ID, _DEV_KEY_HEX) as db:
                row = db.execute(
                    "SELECT status, notes FROM focus_runs WHERE id = ?",
                    [focus_run_id]
                ).fetchone()
        except Exception as e:
            return jsonify({"error": str(e)}), 500

        if not row or row["status"] != "awaiting_user":
            return jsonify({"error": "Run not available for consent."}), 400

        try:
            notes = json.loads(row["notes"] or "{}")
        except json.JSONDecodeError:
            notes = {}

        floor_meta = notes.get("floor_consent_meta", {})
        execution_tier = floor_meta.get("execution_tier", 2)
        abstraction_tier = floor_meta.get("abstraction_tier", 2)
        step_id = floor_meta.get("step_id", "unknown")

        if choice == "continue":
            event_type = "floor_consent_given"
            new_status = "initializing"
            heading = "Consent Recorded"
            message = (
                "Quiet Rabbit will use the modified values. "
                "Please resubmit your request to continue."
            )
            consent_record = {
                "mode": "modified",
                "abstraction_tier": abstraction_tier,
                "consent_timestamp": now(),
                "consent_version": "1",
            } if remember else None
        elif choice == "local":
            event_type = "floor_consent_declined_local"
            new_status = "failed"
            heading = "Using Local AI"
            message = (
                "Your run will use the local AI with your original values. "
                "Please resubmit using Quick Ask or a local-only focus."
            )
            consent_record = {
                "mode": "local",
                "abstraction_tier": abstraction_tier,
                "consent_timestamp": now(),
                "consent_version": "1",
            } if remember else None
        else:
            event_type = "floor_consent_cancelled"
            new_status = "cancelled"
            heading = "Run Cancelled"
            message = "Your run has been cancelled. No data was sent externally."
            consent_record = None

        # Write consent event to disclosure_log
        try:
            with open_personal_db(_DEV_USER_ID, _DEV_PERSONA_ID, _DEV_KEY_HEX) as db:
                db.execute(
                    """INSERT INTO disclosure_log
                       (id, user_id, persona_id, focus_run_id, step_id,
                        routing_tier, execution_tier, abstraction_tier,
                        provider, fields_shared, fields_abstracted,
                        fields_withheld, override_declined, declined_at,
                        created_at, extra_metadata)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    [
                        str(uuid.uuid4()),
                        _DEV_USER_ID, _DEV_PERSONA_ID, focus_run_id, step_id,
                        execution_tier, execution_tier, abstraction_tier,
                        None, "[]", "{}", "[]",
                        1 if choice != "continue" else 0,
                        now() if choice != "continue" else None,
                        now(),
                        json.dumps({"event_type": event_type}),
                    ],
                )
        except Exception:
            pass  # non-fatal — consent is recorded in run status

        # Update run status in outputs.db
        try:
            notes["floor_consent_preference"] = consent_record
            with open_outputs_db(_DEV_USER_ID, _DEV_PERSONA_ID, _DEV_KEY_HEX) as db:
                db.execute(
                    "UPDATE focus_runs SET status = ?, notes = ? WHERE id = ?",
                    [new_status, json.dumps(notes), focus_run_id],
                )
        except Exception as e:
            return jsonify({"error": str(e)}), 500

        # Persist scoped consent to lives.extra_metadata in shared.db (D5-152)
        if consent_record is not None:
            try:
                with open_instance_db() as db:
                    life_row = db.execute(
                        "SELECT extra_metadata FROM personas WHERE id = ?",
                        [_DEV_PERSONA_ID]
                    ).fetchone()
                    if life_row:
                        life_meta = json.loads(
                            life_row["extra_metadata"] or "{}"
                        )
                        life_meta["floor_consent_preference"] = consent_record
                        db.execute(
                            "UPDATE personas SET extra_metadata = ? WHERE id = ?",
                            [json.dumps(life_meta), _DEV_PERSONA_ID],
                        )
            except Exception:
                pass  # non-fatal — run notes serve as fallback for this run

        return render_template_string(
            _CONSENT_RESULT_PAGE,
            run_id=focus_run_id if choice == "continue" else None,
            heading=heading,
            message=message,
        )

    return app


app = create_app()
