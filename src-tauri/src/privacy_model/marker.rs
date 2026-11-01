// src-tauri/src/privacy_model/marker.rs
//
// The verified marker (items.id=678): a small JSON file next to the model that
// records "this exact file (size, modification time) hashed to the pinned
// SHA-256". With a valid marker the model is trusted with two stat calls; hashing
// 1.6 GB happens once after a download or sideload, and again only when the
// file's size or modification time changes.
//
// WHAT IT IS NOT: a security boundary. Anyone who can write the data directory
// can write the model and the marker. It guards against a corrupt, partial,
// wrong or replaced file, not against a local attacker; the gap between this
// check and the filter opening the file is not closed either.
//
// MEASURED (items.id=678, 2026-11-01): `sha256sum` of the real file, warm page
// cache, took 0.85 s on the development machine.

use std::io::{self, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::pinned_asset::{hex32, Expect};

const MARKER_VERSION: u32 = 1;
/// A marker is a few dozen bytes; anything bigger is not one.
const MARKER_MAX_BYTES: u64 = 4096;
const HASH_BUFFER: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub version: u32,
    pub size: u64,
    /// Modification time of the model file, nanoseconds since the Unix epoch.
    pub mtime_ns: u64,
    /// Lower-case hex SHA-256 the file was verified against.
    pub sha256: String,
}

/// Modification time as nanoseconds since the epoch; None if the filesystem
/// does not report one or it is before the epoch.
pub fn mtime_ns(meta: &std::fs::Metadata) -> Option<u64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_nanos()).ok()
}

/// The marker at `path`, or None if it is missing, oversized or not valid JSON
/// of the expected shape.
pub fn read_marker(path: &Path) -> Option<Marker> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MARKER_MAX_BYTES {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write the marker through a temporary file and a rename, so a crash never
/// leaves half a marker.
pub fn write_marker(path: &Path, marker: &Marker) -> io::Result<()> {
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let body = serde_json::to_vec(marker).map_err(io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// What the cheap check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// Right size, and a marker that matches size, modification time and the
    /// pinned hash.
    Valid,
    /// No model file.
    Missing,
    /// A file of the wrong size: not the model, no hash needed to know.
    WrongSize,
    /// Right size but no usable marker: must be hashed.
    Stale,
}

/// Two stat calls and a small read. Never hashes.
pub fn check(model: &Path, marker_path: &Path, expect: &Expect) -> Check {
    let Ok(meta) = std::fs::metadata(model) else {
        return Check::Missing;
    };
    if !meta.is_file() {
        return Check::Missing;
    }
    if meta.len() != expect.size {
        return Check::WrongSize;
    }
    let Some(marker) = read_marker(marker_path) else {
        return Check::Stale;
    };
    let current = mtime_ns(&meta);
    if marker.version == MARKER_VERSION
        && marker.size == meta.len()
        && marker.sha256 == hex32(&expect.sha256)
        && current == Some(marker.mtime_ns)
    {
        Check::Valid
    } else {
        Check::Stale
    }
}

/// SHA-256 of a file, streamed.
pub fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buf[..n]);
    }
}

/// Record a marker for a model whose content was already verified (the
/// downloader hashes as it streams). One stat, no hashing.
pub fn record(model: &Path, marker_path: &Path, expect: &Expect) -> io::Result<()> {
    let meta = std::fs::metadata(model)?;
    if meta.len() != expect.size {
        return Err(io::Error::other("model size differs from the pin"));
    }
    let mtime_ns = mtime_ns(&meta)
        .ok_or_else(|| io::Error::other("the filesystem reports no modification time"))?;
    write_marker(
        marker_path,
        &Marker {
            version: MARKER_VERSION,
            size: meta.len(),
            mtime_ns,
            sha256: hex32(&expect.sha256),
        },
    )
}

fn remove_quietly(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("privacy_model: could not remove {path:?}: {e}"),
    }
}

/// What `verify_existing` decided about the file at the final path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// The file is the pinned model and a valid marker records it.
    Verified,
    /// There was no file.
    Missing,
    /// A file was there and was not the pinned model; it has been deleted
    /// (with its marker). The name is QR's, and the space is needed to fetch
    /// the right file.
    Rejected,
}

/// Check the file at the final path, hashing it only when the marker cannot
/// vouch for it. `hash` is a parameter so tests can count calls. Blocking:
/// callers run it on the blocking pool.
pub fn verify_existing(
    model: &Path,
    marker_path: &Path,
    expect: &Expect,
    hash: &dyn Fn(&Path) -> io::Result<[u8; 32]>,
) -> io::Result<Verified> {
    match check(model, marker_path, expect) {
        Check::Valid => Ok(Verified::Verified),
        Check::Missing => Ok(Verified::Missing),
        Check::WrongSize => {
            remove_quietly(model);
            remove_quietly(marker_path);
            Ok(Verified::Rejected)
        }
        Check::Stale => {
            let before = std::fs::metadata(model)?;
            let digest = hash(model)?;
            let after = std::fs::metadata(model)?;
            if before.len() != after.len() || mtime_ns(&before) != mtime_ns(&after) {
                return Err(io::Error::other(
                    "the model file changed while it was being verified",
                ));
            }
            if digest != expect.sha256 {
                remove_quietly(model);
                remove_quietly(marker_path);
                return Ok(Verified::Rejected);
            }
            record(model, marker_path, expect)?;
            Ok(Verified::Verified)
        }
    }
}
