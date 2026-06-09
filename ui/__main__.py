# ui/__main__.py
# Entry point for `python -m ui`
# Development server for Layer 0 verification.
# Production: entrypoint evolves to full Conductor init as layers are built.

from ui.routes import app

if __name__ == "__main__":
    app.run(host="0.0.0.0", port=3000, debug=False)
