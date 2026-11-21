---
paths:
  - "src-tauri/src/privacy_model/**"
  - "src-tauri/src/pinned_asset/**"
  - "src-tauri/src/commands/privacy_model.rs"
  - ".github/workflows/privacy-model-release.yml"
  - "src-tauri/build.rs"
---

## Privacy model (items.id=678, decisions.id=864)
The Privacy Filter model (OpenAI privacy-filter, LocalAI-io GGUF q8, Apache-2.0)
is a required component, not bundled in the installer. src-tauri/src/privacy_model/:
pin.rs (size 1,637,572,192, SHA-256 80efc180...98c, upstream revision 935d8688...,
release tag privacy-filter-q8-935d868), paths.rs, marker.rs, install.rs, runtime.rs.

- Where: <data_root>/models/privacy-filter-q8.gguf, with a `.verified` marker
  (size, mtime, hash) beside it. On Linux the data root is
  ~/.local/share/ai.quietrabbit.app (the Tauri identifier), NOT
  ~/.local/share/quietrabbit. The dir also holds scores.db: only those two exact
  names are touched, nothing is listed or scanned.
- When: finish_login calls privacy_model::runtime::ensure_started() at EVERY
  login (not awaited, single flight, no account state: it keeps running through a
  lock or logout; a source scan pins that). It starts only in a build with the
  Privacy Filter library linked (cfg privacy_filter_available) and only for the
  installed location. Order of a run: stat the marker (valid: just load), hash a
  file of the right size that has no valid marker (a wrong file is deleted),
  free-disk check, fetch (QR release asset first, upstream revision second, both
  checked against the same size and hash by pinned_asset, Range resume into
  `.part`), record the marker, then privacy_filter::reinitialize() (no restart).
  A file of the pinned size and hash placed at the exact path is accepted without
  any download (offline sideload); click retry or log in again afterwards.
- A release build loads the model only while the marker is valid
  (loadable_model_path); QR_PRIVACY_FILTER_MODEL and the $HOME legacy path exist
  in DEBUG builds only. A PF-enabled debug run (PRIVACY_FILTER_LIB_DIR set)
  WITHOUT QR_PRIVACY_FILTER_MODEL starts a 1.6 GB download at the first login,
  because the dev model is not under the data root. Set the variable (or put the
  file at the data-root path) before running `cargo tauri dev` with the library.
- pf_load returns a NON-NULL context carrying an error for a missing or corrupt
  model; load_decision treats that as failure and frees it. The instance is a
  ReloadableSlot: loaded once and never replaced; a failed load is latched until
  reinitialize(); "no verified model yet" is not latched.
- Status feed: PrivacyFilterStatus (ready, downloading{percent}, verifying,
  unavailable{reason}); reasons not_compiled_in, model_unavailable,
  download_failed, low_disk, verification_failed. Progress is on `task-progress`
  with kind `privacy_model` (total is the pinned size); retry is the IPC command
  retry_privacy_model_setup. Frontend copy is placeholders only (Chat-BRAND,
  items.id=839).
- Release asset: .github/workflows/privacy-model-release.yml, manual only. With
  publish off it downloads from the pinned revision, checks size and hash against
  pin.rs and keeps a 1-day artifact; with publish on it creates a DRAFT release
  (--latest=false) that Jason publishes by hand. Until it exists the first source
  answers 404 and the upstream revision serves the file.
- Measured: hashing the 1.6 GB file takes about 0.85 s warm (sha256sum); sha2 is
  built with opt-level 3 in the dev profile because it took 43 s at opt-level 0.
- Test the real library and model by hand (CI builds without the library):
  cd src-tauri && TMPDIR=$HOME/.cache/qr-678-tmp CARGO_TARGET_DIR=$HOME/.cache/qr-618-target-pf PRIVACY_FILTER_LIB_DIR=$HOME/privacy-filter.cpp/build/release-portable cargo test --lib privacy_filter::tests::pf_enabled
  (never build under /tmp, a 16 GB tmpfs).
- Unverified: the GitHub release redirect host until the asset is published; any
  Windows or macOS behaviour (app_data_dir values, FFI paths, mtime granularity,
  free-space probe; items.id=700, 701). Accepted: no automatic in-session retry
  (a retry is a login or the retry command); two processes on one data directory
  are not coordinated (the release single-instance guard covers shipped builds).
