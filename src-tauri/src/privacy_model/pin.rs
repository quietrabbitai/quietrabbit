// src-tauri/src/privacy_model/pin.rs
//
// The pinned Privacy Filter model (items.id=678, decisions.id=864). Compiled
// in, never read from the environment or a file at run time.
//
// ONE SOURCE OF TRUTH: .github/workflows/privacy-model-release.yml reads
// FILE_NAME, SIZE, SHA256_HEX, UPSTREAM_REVISION, RELEASE_TAG and UPSTREAM_REPO
// out of this file with sed, so each stays on one line of the form
// `[pub ]const NAME: type = value;`. Change them together, deliberately: a new
// pin needs a new release tag, and the tag is baked into every shipped app.
//
// The upstream repository is third-party and mutable, so the pin names a
// revision (a commit), and the hash decides what is accepted whichever source
// served the bytes.

/// The model file's name, in the models directory, in the release asset and
/// in the upstream repository.
pub const FILE_NAME: &str = "privacy-filter-q8.gguf";

/// Exact size in bytes.
pub const SIZE: u64 = 1_637_572_192;

/// SHA-256 of the file, lower-case hex.
pub const SHA256_HEX: &str = "80efc1803eda7c095a79741d2008c07e2e0a57b01bac8825fbeb448fd097998c";

/// The Hugging Face revision (commit) of LocalAI-io/privacy-filter-GGUF that
/// holds exactly this file.
pub const UPSTREAM_REVISION: &str = "935d86882f4b5dc6e0eef05aae48ae5ba0a82a7a";

/// The GitHub release tag that holds the QR-published copy of the asset.
pub const RELEASE_TAG: &str = "privacy-filter-q8-935d868";

/// Free space required beyond the bytes still to be fetched.
pub const DISK_MARGIN: u64 = 256 * 1024 * 1024;

const RELEASE_REPO: &str = "quietrabbitai/quietrabbit";
const UPSTREAM_REPO: &str = "LocalAI-io/privacy-filter-GGUF";

/// Source 1: the QR release asset. Answers 404 until the release is published.
pub fn release_asset_url() -> String {
    format!("https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_TAG}/{FILE_NAME}")
}

/// Source 2: the upstream revision.
pub fn upstream_url() -> String {
    format!("https://huggingface.co/{UPSTREAM_REPO}/resolve/{UPSTREAM_REVISION}/{FILE_NAME}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_a_valid_64_digit_hex_and_parses() {
        assert_eq!(SHA256_HEX.len(), 64);
        assert_eq!(SHA256_HEX, SHA256_HEX.to_ascii_lowercase());
        let e = crate::pinned_asset::Expect::from_hex(SIZE, SHA256_HEX).expect("valid hex");
        assert_eq!(crate::pinned_asset::hex32(&e.sha256), SHA256_HEX);
    }

    #[test]
    fn the_revision_is_a_full_commit_hash_and_the_tag_names_its_prefix() {
        assert_eq!(UPSTREAM_REVISION.len(), 40);
        assert!(UPSTREAM_REVISION.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(RELEASE_TAG.ends_with(&UPSTREAM_REVISION[..7]));
    }

    #[test]
    fn both_sources_are_https_and_name_the_pinned_file() {
        let release = release_asset_url();
        let upstream = upstream_url();
        assert!(release.starts_with("https://github.com/quietrabbitai/quietrabbit/releases/"));
        assert!(release.contains(RELEASE_TAG) && release.ends_with(FILE_NAME));
        assert!(upstream.starts_with("https://huggingface.co/LocalAI-io/"));
        assert!(upstream.contains(UPSTREAM_REVISION) && upstream.ends_with(FILE_NAME));
        for url in [&release, &upstream] {
            assert!(!url.contains('?'), "no query string rides on a request");
        }
    }
}
