// src-tauri/src/pinned_asset/mod.rs
//
// Pinned, hash-verified asset fetch (items.id=713; shared with items.id=678,
// decisions.id=864 and 875 point 6). One module for "download this exact file":
// the caller supplies the expected size and SHA-256, the module streams the
// body to a `.part` file next to the destination, hashes it as it arrives,
// resumes an interrupted transfer with an HTTP Range request, and only renames
// the file into place once size and hash match. A mismatch deletes the partial
// file and tries the next source.
//
// WHERE THE EXPECTATION COMES FROM is the caller's business. items.id=678
// compiles the privacy-model size and hash into the binary. The provider
// catalog (provider_catalog/) takes them from a manifest whose minisign
// signature it has already verified. This module never decides what to trust.
//
// WHAT IS HERE, AND WHAT IS NOT. Here: the HTTP policy (HTTPS only, a bounded
// redirect chain that may only land on named hosts), size caps, streaming hash,
// resume, atomic placement, cancellation, a progress callback. NOT here (they
// belong to items.id=678): free-disk-space check, the verified marker, sideload,
// the login trigger, re-initialising the privacy filter, the status events.
//
// NETWORK DISCIPLINE. Nothing in this module runs by itself. The only callers
// are click-driven commands (a source-scan test in provider_catalog pins that
// for the catalog). A request carries no query string, no identifier, no app or
// catalog version and no user content. No User-Agent is set.
//
// REDIRECTS. A GitHub release download answers 302 to a CDN host, so redirects
// cannot be off as in providers/hosted_client.rs. They are limited instead:
// at most MAX_REDIRECTS hops, every hop HTTPS, and every target host must be on
// the caller's list. GitHub: `release-assets.githubusercontent.com`, one hop
// (checked by Chat-PM 2026-10-11). Any Hugging Face host is UNVERIFIED and is
// not in GITHUB_RELEASE_REDIRECT_HOSTS; items.id=678 owns that check.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{header, redirect, Client, StatusCode};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests;

/// Most redirects one fetch may follow.
pub const MAX_REDIRECTS: usize = 5;

/// Hosts a GitHub release download may be redirected to.
pub const GITHUB_RELEASE_REDIRECT_HOSTS: &[&str] = &["release-assets.githubusercontent.com"];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A body that stalls for this long fails (the transfer as a whole has no cap:
/// a 1.6 GB model on a slow line is legitimate).
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Total cap for the small in-memory fetches (manifest, signature).
const SMALL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const HASH_CHUNK: usize = 64 * 1024;

/// What the finished file must be, exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    pub size: u64,
    pub sha256: [u8; 32],
}

impl Expect {
    /// Parse a 64-character hex digest (either case).
    pub fn from_hex(size: u64, hex: &str) -> Option<Self> {
        Some(Self {
            size,
            sha256: decode_hex32(hex)?,
        })
    }
}

fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

/// Lower-case hex of a SHA-256 digest.
pub fn hex32(digest: &[u8; 32]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    /// No connection, a reset, a stalled body, a TLS failure. The partial
    /// file (if any) is kept so the next attempt resumes.
    #[error("network: {0}")]
    Network(String),
    #[error("http status {0}")]
    Status(u16),
    /// A redirect left the allowed hosts, left HTTPS, or went too deep.
    #[error("redirect refused")]
    RedirectRefused,
    #[error("size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("hash mismatch")]
    HashMismatch,
    #[error("response larger than the allowed {0} bytes")]
    TooLarge(u64),
    #[error("io: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
    #[error("no sources given")]
    NoSources,
}

fn io_err(e: std::io::Error) -> FetchError {
    FetchError::Io(e.to_string())
}

fn map_reqwest(e: reqwest::Error) -> FetchError {
    if e.is_redirect() {
        FetchError::RedirectRefused
    } else {
        // reqwest's Display can include the URL, which is fine (no secrets
        // ever ride in these URLs: no query string, no key).
        FetchError::Network(e.to_string())
    }
}

fn redirect_policy(allowed_hosts: Vec<String>, allow_http: bool) -> redirect::Policy {
    redirect::Policy::custom(move |attempt| {
        // `previous` holds the original URL plus every hop already followed,
        // so this attempt is hop number `previous().len()`.
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let scheme_ok = url.scheme() == "https" || (allow_http && url.scheme() == "http");
        let host_ok = url
            .host_str()
            .is_some_and(|h| allowed_hosts.iter().any(|a| a == h));
        if scheme_ok && host_ok {
            attempt.follow()
        } else {
            attempt.error("redirect target not allowed")
        }
    })
}

pub struct PinnedFetcher {
    client: Client,
}

impl PinnedFetcher {
    /// Production fetcher: HTTPS only, redirects only to `redirect_hosts`.
    pub fn new(redirect_hosts: &[&str]) -> Self {
        let client = Client::builder()
            .https_only(true)
            .redirect(redirect_policy(
                redirect_hosts.iter().map(|h| (*h).to_owned()).collect(),
                false,
            ))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .expect("pinned_asset: reqwest client must build");
        Self { client }
    }

    /// Test-only: plain-HTTP loopback client, proxies off, same redirect rules
    /// except that `http` is accepted for the listed hosts.
    #[cfg(test)]
    pub(crate) fn for_loopback_tests(redirect_hosts: &[&str]) -> Self {
        let client = Client::builder()
            .no_proxy()
            .redirect(redirect_policy(
                redirect_hosts.iter().map(|h| (*h).to_owned()).collect(),
                true,
            ))
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(5))
            .build()
            .expect("loopback client");
        Self { client }
    }

    /// Fetch a small body into memory, refusing anything over `max_bytes`.
    /// For the manifest and its signature. No hash: the caller verifies the
    /// signature over exactly these bytes.
    pub async fn fetch_small(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        let resp = self
            .client
            .get(url)
            .timeout(SMALL_TOTAL_TIMEOUT)
            .send()
            .await
            .map_err(map_reqwest)?;
        if resp.status() != StatusCode::OK {
            return Err(FetchError::Status(resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > max_bytes) {
            return Err(FetchError::TooLarge(max_bytes));
        }
        let mut out: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest)?;
            if out.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(FetchError::TooLarge(max_bytes));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Fetch the first source that yields exactly `expect`, into `dest`.
    ///
    /// `progress(current, total)` is called as bytes arrive (the caller
    /// throttles and maps it to task_progress). On a size or hash mismatch the
    /// partial file is deleted and the next source is tried; on a network
    /// failure the partial file is kept, so a later call resumes it, and the
    /// next source is tried. `Cancelled` stops at once and keeps the partial
    /// file. Returns the last error when every source fails.
    pub async fn fetch_to_file(
        &self,
        sources: &[&str],
        expect: &Expect,
        dest: &Path,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<(), FetchError> {
        if sources.is_empty() {
            return Err(FetchError::NoSources);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_err)?;
        }
        let part = part_path(dest);
        let mut last = FetchError::NoSources;
        for source in sources {
            match self
                .try_source(source, expect, dest, &part, cancel, progress)
                .await
            {
                Ok(()) => return Ok(()),
                Err(FetchError::Cancelled) => return Err(FetchError::Cancelled),
                Err(e) => {
                    log::warn!("pinned_asset: source failed: {e}");
                    last = e;
                }
            }
        }
        Err(last)
    }

    async fn try_source(
        &self,
        source: &str,
        expect: &Expect,
        dest: &Path,
        part: &Path,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(u64, u64) + Send),
    ) -> Result<(), FetchError> {
        // One restart is allowed inside a source: a server that answers a
        // Range request with 416, or ignores it, makes us start from zero.
        for attempt in 0..2 {
            let mut have = match tokio::fs::metadata(part).await {
                Ok(m) => m.len(),
                Err(_) => 0,
            };
            if have > expect.size {
                let _ = tokio::fs::remove_file(part).await;
                have = 0;
            }
            let mut hasher = Sha256::new();
            if have > 0 {
                hash_existing(part, &mut hasher).await?;
            }
            if have == expect.size {
                // A complete partial file from an earlier run: no request.
                return finish(hasher, have, expect, dest, part).await;
            }

            let mut req = self.client.get(source);
            if have > 0 {
                req = req.header(header::RANGE, format!("bytes={have}-"));
            }
            let resp = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(FetchError::Cancelled),
                r = req.send() => r.map_err(map_reqwest)?,
            };
            let status = resp.status();

            if status == StatusCode::RANGE_NOT_SATISFIABLE && have > 0 && attempt == 0 {
                let _ = tokio::fs::remove_file(part).await;
                continue;
            }
            let resume = match status {
                StatusCode::PARTIAL_CONTENT if have > 0 => {
                    let ok = resp
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
                    if !ok {
                        let _ = tokio::fs::remove_file(part).await;
                        return Err(FetchError::Network("unexpected Content-Range".to_owned()));
                    }
                    true
                }
                StatusCode::OK => {
                    if let Some(len) = resp.content_length() {
                        if len != expect.size {
                            return Err(FetchError::SizeMismatch {
                                expected: expect.size,
                                got: len,
                            });
                        }
                    }
                    false
                }
                other => return Err(FetchError::Status(other.as_u16())),
            };
            if !resume {
                // A full-body answer: whatever was in the partial file is stale.
                have = 0;
                hasher = Sha256::new();
            }

            let mut file = if resume {
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(part)
                    .await
                    .map_err(io_err)?
            } else {
                tokio::fs::File::create(part).await.map_err(io_err)?
            };

            let mut stream = resp.bytes_stream();
            loop {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(FetchError::Cancelled),
                    n = stream.next() => n,
                };
                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(map_reqwest)?;
                let got = have + chunk.len() as u64;
                if got > expect.size {
                    drop(file);
                    let _ = tokio::fs::remove_file(part).await;
                    return Err(FetchError::SizeMismatch {
                        expected: expect.size,
                        got,
                    });
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await.map_err(io_err)?;
                have = got;
                progress(have, expect.size);
            }
            file.flush().await.map_err(io_err)?;
            file.sync_all().await.map_err(io_err)?;
            drop(file);
            return finish(hasher, have, expect, dest, part).await;
        }
        Err(FetchError::Status(
            StatusCode::RANGE_NOT_SATISFIABLE.as_u16(),
        ))
    }
}

/// `<dest>.part`
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".part");
    dest.with_file_name(name)
}

async fn hash_existing(part: &Path, hasher: &mut Sha256) -> Result<(), FetchError> {
    let mut file = tokio::fs::File::open(part).await.map_err(io_err)?;
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = file.read(&mut buf).await.map_err(io_err)?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
    }
}

async fn finish(
    hasher: Sha256,
    have: u64,
    expect: &Expect,
    dest: &Path,
    part: &Path,
) -> Result<(), FetchError> {
    if have != expect.size {
        let _ = tokio::fs::remove_file(part).await;
        return Err(FetchError::SizeMismatch {
            expected: expect.size,
            got: have,
        });
    }
    let digest: [u8; 32] = hasher.finalize().into();
    if digest != expect.sha256 {
        let _ = tokio::fs::remove_file(part).await;
        return Err(FetchError::HashMismatch);
    }
    tokio::fs::rename(part, dest).await.map_err(io_err)
}

/// SHA-256 of a byte slice.
pub fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}
