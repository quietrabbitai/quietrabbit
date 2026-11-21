---
paths:
  - "src-tauri/src/auth/**"
  - "src-tauri/src/commands/auth.rs"
  - "src-tauri/src/commands/key_rotation.rs"
---

## Key Architectural Decisions

- Account creation order (items.id=614, 630): an account row is written only after its first per-account databases (integration_keys.db, cloud_chat_cookies.db) exist, and a failed first-run bootstrap removes users/<user_id>/ and leaves no row. A later login recreates a missing first database under the same data key.

- Recovery key: BIP39 mnemonic of 256-bit recovery entropy, independent of the data key. The app stores only the derived recovery public key and the data key sealed to it; every password change, restore and new recovery phrase rotates the data key. See AUTH_MULTIUSER_ARCHITECTURE.md Sections 4.0 and 5.1.
