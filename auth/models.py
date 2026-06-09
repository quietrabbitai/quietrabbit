# auth/models.py
# Flask-Login User model — stub for Layer 0.
# Full implementation: Layer 8 (auth package).
# password_hash is intentionally NOT an attribute on this model.
# Credential lookup returns (User, password_hash) tuple separately.
# See auth/credentials.py (Layer 8) and ARCHITECTURE Section 8.5.

from __future__ import annotations
from flask_login import UserMixin


class User(UserMixin):
    """
    Flask-Login user model.
    password_hash is never stored on the User object — fetched separately
    during credential lookup and discarded after verification.

    Roles: consumer / builder / admin
    Role enforcement is disabled in Release 1 (role_enforcement='disabled'
    in instance_config). Single flag flip activates in Release 2 — no code change.
    """

    def __init__(
        self,
        id: str,
        display_name: str,
        role: str,
        is_primary: bool,
        auth_enabled: bool,
    ):
        self.id = id
        self.display_name = display_name
        self.role = role
        self.is_primary = is_primary
        self.auth_enabled = auth_enabled

    @classmethod
    def from_row(cls, row) -> User:
        return cls(
            id=row["id"],
            display_name=row["display_name"],
            role=row["role"],
            is_primary=bool(row["is_primary"]),
            auth_enabled=bool(row["auth_enabled"]),
        )

    def is_admin(self) -> bool:
        return self.role == "admin"

    def is_builder(self) -> bool:
        """Builder and admin can create/edit paths and specialists."""
        return self.role in ("builder", "admin")

    def is_consumer(self) -> bool:
        """Consumer can run paths and manage their own spaces."""
        return self.role == "consumer"
