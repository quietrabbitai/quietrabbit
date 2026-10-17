# Local patches to docx-rust 0.1.11

Source: crates.io docx-rust 0.1.11 (MIT, https://github.com/cstkingkey/docx-rs).
Only `src/`, `Cargo.toml`, `LICENSE` and `README.md` are vendored.

Why: the published release depends on `zip = "4"`; RUSTSEC-2026-0367/0368/0369
are fixed only in zip >= 9.0.0 (items.id=763). Wired in through
`[patch.crates-io]` in src-tauri/Cargo.toml.

Differences from the published 0.1.11:

1. `Cargo.toml`: `[dependencies.zip]` `version = "4"` -> `"9"`.
2. `src/docx.rs` (`option_read_multiple!` and `option_read_multiple_files!`):
   `zip.file_names()` now yields `Result<Cow<str>, ZipError>` in zip 9, so
   `.map(|x| x.to_string())` became `.filter_map(|x| x.ok()).map(|x| x.to_string())`.

Remove this directory and the patch entry once upstream releases a docx-rust
that depends on zip >= 9 (items.id=765).
