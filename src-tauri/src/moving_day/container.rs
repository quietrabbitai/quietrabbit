//! The Moving Day file container (items.id=966, decisions.id=971 J1, 972):
//! one preamble line, `QRMEMORY 1\n`, then a binary age v1 file in passphrase
//! mode with exactly one scrypt stanza.
//!
//! Bytes in, bytes out. Nothing here touches a file, a database, the account or
//! its data key: the file password is the only thing that opens a file, and
//! neither the data key, the account password nor the recovery phrase does. No
//! password policy lives here either (the caller applies it).
//!
//! THE PREAMBLE is `QRMEMORY`, one space, a decimal version from 1 with no
//! leading zero, then `\n`. It is what lets [`probe`] say "not ours" and "made
//! by a newer version" before any password; bare age can say neither. It is NOT
//! authenticated, so changing it can only change which message appears: the
//! payload carries the format version again, inside the encryption, and that one
//! governs. Dropping the first line leaves a file the stock `age` tool opens.
//!
//! THE PASSWORD is NFKC-normalised (`kdf::normalise_password`, the account
//! rule) on write and on open. age does not normalise, so a password that is not
//! NFKC-stable, typed as is into the `age` tool, does not open the file.
//!
//! ONLY age's passphrase (scrypt) functions are used. Two `deny.toml` ignores
//! depend on that, and a source scan in the tests pins it.
//!
//! [`seal`] and [`open`] derive a key with scrypt (about 256 MiB at the written
//! work factor): call them on the blocking pool. [`probe`] is cheap.

use std::io::{Read, Write};
use std::iter;

use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::kdf;

/// The preamble's marker, with the space that ends it.
const MARKER: &[u8] = b"QRMEMORY ";

/// The one format version this build writes and reads.
const VERSION: &[u8] = b"1";

/// A version number longer than this is not one of ours.
const MAX_VERSION_DIGITS: usize = 9;

/// The scrypt work factor every file is written with (N = 2^18). Fixed: age's
/// own default aims at one second on the WRITING device, which would give a slow
/// machine a weak file. The same value the stock `age` tool writes.
const WRITE_WORK_FACTOR: u8 = 18;

/// The largest work factor a file may ask of [`open`] (decisions.id=972). A
/// hostile header cannot demand more: 19 is about 512 MiB. age's default limit
/// follows the speed of the reading device instead.
const MAX_WORK_FACTOR: u8 = 19;

/// The age header must end within this many bytes, checked BEFORE age reads it.
/// age 0.12.1 parses the header again from its start after every new line, so
/// the cost grows with the square of its length: measured in the test profile on
/// the development machine, a header that never ends took 1.6 s at 64 KiB, 25 s
/// at 256 KiB and 404 s at 1 MiB, so the file limit alone does not bound it. A
/// passphrase header is about 150 bytes.
const MAX_HEADER_BYTES: usize = 4 * 1024;

/// The largest file [`open`] reads and [`seal`] writes.
pub const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;

/// The largest payload [`seal`] accepts: the file limit less far more than the
/// preamble, the age header and a 16-byte tag for each 64 KiB chunk can take.
pub const MAX_PLAINTEXT_BYTES: usize = MAX_FILE_BYTES - 64 * 1024;

/// Why a file did not open. With success, these are the four states of the
/// password screen. No variant carries anything: not the file's bytes, not a
/// library error, nothing about the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OpenError {
    /// No well-formed preamble: not a file Quiet Rabbit made.
    #[error("not a Quiet Rabbit file")]
    NotOurs,
    /// A well-formed preamble with a version above this build's.
    #[error("the file was made by a newer version")]
    NewerVersion,
    /// Everything else after a version 1 preamble. A wrong password and a
    /// damaged file are deliberately one answer.
    #[error("the password did not open the file, or the file is damaged")]
    PasswordOrDamaged,
}

/// Why no file was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SealError {
    #[error("no password was given")]
    EmptyPassword,
    #[error("there is too much to put in one file")]
    TooLarge,
    #[error("the file could not be encrypted")]
    Failed,
}

/// Look at the preamble only: is this one of our files, of a version this build
/// reads? Needs no password and does no key work. `Ok` says nothing about the
/// rest of the file.
pub fn probe(file: &[u8]) -> Result<(), OpenError> {
    strip_preamble(file).map(drop)
}

/// Encrypt `plaintext` under `password` into a complete file.
pub fn seal(plaintext: &[u8], password: &str) -> Result<Vec<u8>, SealError> {
    seal_at(plaintext, password, WRITE_WORK_FACTOR)
}

/// Decrypt a complete file. The payload is returned only once the whole of it
/// has authenticated, to the last chunk.
pub fn open(file: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>, OpenError> {
    open_capped(file, password, MAX_WORK_FACTOR)
}

/// The bytes after the preamble.
fn strip_preamble(file: &[u8]) -> Result<&[u8], OpenError> {
    let rest = file.strip_prefix(MARKER).ok_or(OpenError::NotOurs)?;
    let end = rest
        .iter()
        .take(MAX_VERSION_DIGITS + 1)
        .position(|&b| b == b'\n')
        .ok_or(OpenError::NotOurs)?;
    let version = &rest[..end];
    let canonical =
        version.first().is_some_and(|&b| b != b'0') && version.iter().all(u8::is_ascii_digit);
    if !canonical {
        return Err(OpenError::NotOurs);
    }
    // Canonical and not "1" is a number above 1.
    if version != VERSION {
        return Err(OpenError::NewerVersion);
    }
    Ok(&rest[end + 1..])
}

/// True when the line that closes an age header (`--- <mac>`) is complete within
/// the first [`MAX_HEADER_BYTES`]. age stops at that line, whether it accepts it
/// or not, so this bounds what it parses.
fn header_is_bounded(body: &[u8]) -> bool {
    let head = &body[..body.len().min(MAX_HEADER_BYTES)];
    head.windows(5)
        .position(|w| w == b"\n--- ")
        .is_some_and(|at| head[at + 5..].contains(&b'\n'))
}

/// The password as age takes it: NFKC, in a string that is zeroized on drop.
fn age_password(password: &str) -> Option<age::secrecy::SecretString> {
    let normalised = kdf::normalise_password(password.as_bytes());
    let text = std::str::from_utf8(normalised.as_bytes()).ok()?;
    // to_owned allocates exactly, so the conversion moves the buffer and leaves
    // no second copy behind.
    Some(age::secrecy::SecretString::from(text.to_owned()))
}

/// [`seal`] with the work factor named, so tests need not pay for the real one.
fn seal_at(plaintext: &[u8], password: &str, work_factor: u8) -> Result<Vec<u8>, SealError> {
    if password.is_empty() {
        return Err(SealError::EmptyPassword);
    }
    if plaintext.len() > MAX_PLAINTEXT_BYTES {
        return Err(SealError::TooLarge);
    }
    let secret = age_password(password).ok_or(SealError::Failed)?;
    let mut recipient = age::scrypt::Recipient::new(secret);
    recipient.set_work_factor(work_factor);
    let encryptor = age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
        .map_err(|_| SealError::Failed)?;

    let mut file = Vec::with_capacity(plaintext.len() + 64 * 1024);
    file.extend_from_slice(MARKER);
    file.extend_from_slice(VERSION);
    file.push(b'\n');
    let mut writer = encryptor
        .wrap_output(&mut file)
        .map_err(|_| SealError::Failed)?;
    writer.write_all(plaintext).map_err(|_| SealError::Failed)?;
    writer.finish().map_err(|_| SealError::Failed)?;
    if file.len() > MAX_FILE_BYTES {
        return Err(SealError::TooLarge);
    }
    Ok(file)
}

/// [`open`] with the accepted work factor named, so tests can show the limit is
/// applied without running a large derivation.
fn open_capped(
    file: &[u8],
    password: &str,
    max_work_factor: u8,
) -> Result<Zeroizing<Vec<u8>>, OpenError> {
    let body = strip_preamble(file)?;
    // Every failure from here on is the same answer.
    let failed = OpenError::PasswordOrDamaged;
    if file.len() > MAX_FILE_BYTES || !header_is_bounded(body) {
        return Err(failed);
    }
    let decryptor = age::Decryptor::new_buffered(body).map_err(|_| failed)?;
    // Before any key work: one scrypt stanza and nothing else.
    if !decryptor.is_scrypt() {
        return Err(failed);
    }
    let mut identity = age::scrypt::Identity::new(age_password(password).ok_or(failed)?);
    identity.set_max_work_factor(max_work_factor);
    let mut reader = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|_| failed)?;
    // The payload is shorter than the body, so sizing the buffer to the body
    // means it should not have to grow and leave a copy behind (not tested). It
    // must be read to the end: only the last chunk shows that the file was not
    // cut short. On any error the buffer is zeroized and dropped.
    let mut plaintext = Zeroizing::new(Vec::with_capacity(body.len()));
    reader.read_to_end(&mut plaintext).map_err(|_| failed)?;
    Ok(plaintext)
}

#[cfg(test)]
#[path = "container_tests.rs"]
mod tests;
