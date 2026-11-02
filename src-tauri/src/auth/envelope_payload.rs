// src-tauri/src/auth/envelope_payload.rs
//
// The plaintext inside a group invitation or key-rotation envelope
// (items.id=661, decisions.id=862).
//
// WHY A STRUCT, NOT THE BARE KEY: both envelopes used to hold only the group's
// 32-byte symmetric key, and the group's display name and the sender's label
// sat next to the ciphertext in plaintext columns of the unencrypted shared.db.
// Those are user-authored text (a group called "Oncology support" is the
// domain information contextual separation protects), so they travel INSIDE
// the encrypted payload now and the plaintext row keeps only ids, status and
// timestamps. A recipient who is not logged in cannot read them; that is the
// intended consequence.
//
// FORMAT (binary, length-prefixed, so the key never sits in a heap String and
// items.id=660's canonical signed form can extend it):
//
//   byte 0       format version (FORMAT_VERSION = 1)
//   byte 1       kind: 1 = invitation, 2 = rotation
//   bytes 2..34  the group's 32-byte symmetric key
//   invitation:  u16 BE length + UTF-8 group_display_name,
//                u16 BE length + UTF-8 sender_label
//   rotation:    u16 BE length + UTF-8 sender_label
//
// STRICT PARSE, fail closed: the version and kind must match, every length
// must fit, text must be valid UTF-8 within MAX_TEXT_BYTES, and no byte may be
// left over. A bare 32-byte key (the old format) can never parse: byte 0 would
// have to be 1 and byte 1 a kind, and the length would be wrong. There is no
// compatibility path, there are no shipped users (decisions.id=857).
//
// EXTENSION POINT for items.id=659 and 660: they add the signer's public key,
// the roster snapshot and the generation counter as new FIELDS of a new
// FORMAT_VERSION, not a second envelope.

use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::auth::kdf::MASTER_KEY_LEN;

pub const FORMAT_VERSION: u8 = 1;
const KIND_INVITATION: u8 = 1;
const KIND_ROTATION: u8 = 2;

/// Longest group name or sender label accepted, in UTF-8 bytes.
pub const MAX_TEXT_BYTES: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EnvelopePayloadError {
    #[error("envelope payload is truncated")]
    Truncated,
    #[error("envelope payload has unsupported format version {0}")]
    UnsupportedVersion(u8),
    #[error("envelope payload is the wrong kind")]
    WrongKind,
    #[error("envelope payload has trailing bytes")]
    TrailingBytes,
    #[error("envelope payload text is not valid UTF-8")]
    InvalidUtf8,
    #[error("envelope payload text is longer than {MAX_TEXT_BYTES} bytes")]
    TextTooLong,
}

/// What a group invitation carries. The key is wiped on drop, as
/// UnlockedGroupKey's is; the text is not secret key material, so it is skipped.
/// A type with a Drop cannot be partially moved out of, so callers clone the
/// name and label they keep.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct InvitationPayload {
    pub group_key: [u8; MASTER_KEY_LEN],
    #[zeroize(skip)]
    pub group_display_name: String,
    #[zeroize(skip)]
    pub sender_label: String,
}

/// What a key rotation carries. Wiped on drop like InvitationPayload.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct RotationPayload {
    pub group_key: [u8; MASTER_KEY_LEN],
    #[zeroize(skip)]
    pub sender_label: String,
}

// A derived Debug would print the key, so these print it as redacted.
impl std::fmt::Debug for InvitationPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvitationPayload")
            .field("group_key", &"<redacted>")
            .field("group_display_name", &self.group_display_name)
            .field("sender_label", &self.sender_label)
            .finish()
    }
}

impl std::fmt::Debug for RotationPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotationPayload")
            .field("group_key", &"<redacted>")
            .field("sender_label", &self.sender_label)
            .finish()
    }
}

fn push_text(out: &mut Vec<u8>, text: &str) -> Result<(), EnvelopePayloadError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(EnvelopePayloadError::TextTooLong);
    }
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

/// Encode an invitation. The result holds the group key, so it is zeroized on
/// drop.
pub fn encode_invitation(
    payload: &InvitationPayload,
) -> Result<Zeroizing<Vec<u8>>, EnvelopePayloadError> {
    let mut out = Zeroizing::new(Vec::with_capacity(
        2 + MASTER_KEY_LEN + 4 + payload.group_display_name.len() + payload.sender_label.len(),
    ));
    out.push(FORMAT_VERSION);
    out.push(KIND_INVITATION);
    out.extend_from_slice(&payload.group_key);
    push_text(&mut out, &payload.group_display_name)?;
    push_text(&mut out, &payload.sender_label)?;
    Ok(out)
}

pub fn encode_rotation(
    payload: &RotationPayload,
) -> Result<Zeroizing<Vec<u8>>, EnvelopePayloadError> {
    let mut out = Zeroizing::new(Vec::with_capacity(
        2 + MASTER_KEY_LEN + 2 + payload.sender_label.len(),
    ));
    out.push(FORMAT_VERSION);
    out.push(KIND_ROTATION);
    out.extend_from_slice(&payload.group_key);
    push_text(&mut out, &payload.sender_label)?;
    Ok(out)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], EnvelopePayloadError> {
        if self.bytes.len() < n {
            return Err(EnvelopePayloadError::Truncated);
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn text(&mut self) -> Result<String, EnvelopePayloadError> {
        let len_bytes = self.take(2)?;
        let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        if len > MAX_TEXT_BYTES {
            return Err(EnvelopePayloadError::TextTooLong);
        }
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| EnvelopePayloadError::InvalidUtf8)
    }

    fn finish(self) -> Result<(), EnvelopePayloadError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(EnvelopePayloadError::TrailingBytes)
        }
    }
}

fn header_and_key<'a>(
    bytes: &'a [u8],
    expected_kind: u8,
) -> Result<([u8; MASTER_KEY_LEN], Reader<'a>), EnvelopePayloadError> {
    let mut r = Reader { bytes };
    let head = r.take(2)?;
    if head[0] != FORMAT_VERSION {
        return Err(EnvelopePayloadError::UnsupportedVersion(head[0]));
    }
    if head[1] != expected_kind {
        return Err(EnvelopePayloadError::WrongKind);
    }
    let mut key = [0u8; MASTER_KEY_LEN];
    key.copy_from_slice(r.take(MASTER_KEY_LEN)?);
    Ok((key, r))
}

pub fn decode_invitation(bytes: &[u8]) -> Result<InvitationPayload, EnvelopePayloadError> {
    let (group_key, mut r) = header_and_key(bytes, KIND_INVITATION)?;
    let group_display_name = r.text()?;
    let sender_label = r.text()?;
    r.finish()?;
    Ok(InvitationPayload {
        group_key,
        group_display_name,
        sender_label,
    })
}

pub fn decode_rotation(bytes: &[u8]) -> Result<RotationPayload, EnvelopePayloadError> {
    let (group_key, mut r) = header_and_key(bytes, KIND_ROTATION)?;
    let sender_label = r.text()?;
    r.finish()?;
    Ok(RotationPayload {
        group_key,
        sender_label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invitation() -> InvitationPayload {
        InvitationPayload {
            group_key: [7u8; MASTER_KEY_LEN],
            group_display_name: "Family Documents \u{1F4C1}".to_owned(),
            sender_label: "Alice".to_owned(),
        }
    }

    #[test]
    fn invitation_round_trips_including_non_ascii_text() {
        let bytes = encode_invitation(&invitation()).unwrap();
        assert_eq!(decode_invitation(&bytes).unwrap(), invitation());
    }

    #[test]
    fn rotation_round_trips_and_empty_text_is_allowed() {
        let p = RotationPayload {
            group_key: [9u8; MASTER_KEY_LEN],
            sender_label: String::new(),
        };
        let bytes = encode_rotation(&p).unwrap();
        assert_eq!(decode_rotation(&bytes).unwrap(), p);
    }

    #[test]
    fn a_bare_32_byte_key_never_parses_as_either_kind() {
        for key in [[0u8; 32], [1u8; 32], [2u8; 32], [0xffu8; 32]] {
            assert!(decode_invitation(&key).is_err());
            assert!(decode_rotation(&key).is_err());
        }
    }

    #[test]
    fn kinds_are_not_interchangeable() {
        let inv = encode_invitation(&invitation()).unwrap();
        assert_eq!(decode_rotation(&inv), Err(EnvelopePayloadError::WrongKind));
        let rot = encode_rotation(&RotationPayload {
            group_key: [1u8; MASTER_KEY_LEN],
            sender_label: "x".to_owned(),
        })
        .unwrap();
        assert_eq!(
            decode_invitation(&rot),
            Err(EnvelopePayloadError::WrongKind)
        );
    }

    #[test]
    fn every_truncation_is_rejected() {
        let bytes = encode_invitation(&invitation()).unwrap();
        for cut in 0..bytes.len() {
            assert!(
                decode_invitation(&bytes[..cut]).is_err(),
                "a payload cut to {cut} bytes must not parse"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode_invitation(&invitation()).unwrap().to_vec();
        bytes.push(0);
        assert_eq!(
            decode_invitation(&bytes),
            Err(EnvelopePayloadError::TrailingBytes)
        );
    }

    #[test]
    fn an_unsupported_version_is_rejected() {
        let mut bytes = encode_invitation(&invitation()).unwrap().to_vec();
        bytes[0] = 2;
        assert_eq!(
            decode_invitation(&bytes),
            Err(EnvelopePayloadError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut bytes = encode_rotation(&RotationPayload {
            group_key: [1u8; MASTER_KEY_LEN],
            sender_label: "ab".to_owned(),
        })
        .unwrap()
        .to_vec();
        let n = bytes.len();
        bytes[n - 1] = 0xff;
        assert_eq!(
            decode_rotation(&bytes),
            Err(EnvelopePayloadError::InvalidUtf8)
        );
    }

    #[test]
    fn over_long_text_is_refused_at_encode_and_at_decode() {
        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        let mut p = invitation();
        p.group_display_name = long.clone();
        assert_eq!(
            encode_invitation(&p).map(|_| ()),
            Err(EnvelopePayloadError::TextTooLong)
        );

        // A hand-built payload that claims a too-long length.
        let mut raw = vec![FORMAT_VERSION, KIND_ROTATION];
        raw.extend_from_slice(&[3u8; MASTER_KEY_LEN]);
        raw.extend_from_slice(&((MAX_TEXT_BYTES + 1) as u16).to_be_bytes());
        raw.extend_from_slice(long.as_bytes());
        assert_eq!(
            decode_rotation(&raw),
            Err(EnvelopePayloadError::TextTooLong)
        );
    }

    #[test]
    fn the_debug_output_never_shows_the_key() {
        let shown = format!("{:?}", invitation());
        assert!(shown.contains("redacted"));
        assert!(!shown.contains("[7, 7"));
    }

    #[test]
    fn zeroizing_a_payload_wipes_the_key_and_keeps_the_text() {
        let mut p = invitation();
        p.zeroize();
        assert_eq!(p.group_key, [0u8; MASTER_KEY_LEN]);
        assert_eq!(p.sender_label, "Alice", "text is skipped, not secret");

        let mut r = RotationPayload {
            group_key: [9u8; MASTER_KEY_LEN],
            sender_label: "x".to_owned(),
        };
        r.zeroize();
        assert_eq!(r.group_key, [0u8; MASTER_KEY_LEN]);
    }
}
