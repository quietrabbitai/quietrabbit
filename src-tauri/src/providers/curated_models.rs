// src-tauri/src/providers/curated_models.rs
//
// The curated set of local model tags QR's bundled Ollama sidecar may pull
// (items.id=856; decisions.id=840).
//
// TRUST BOUNDARY. QR must never load a model it did not pull from a curated,
// human-reviewed list, resolved against registry.ollama.ai and never against a
// user-suppliable registry host. A tag such as `host/name:tag` makes Ollama
// resolve against `host`, so the check is an EXACT allowlist, not a search for
// bad prefixes: a tag is acceptable only if it is byte-for-byte one of the
// entries below. No trimming, no case folding, no parsing.
//
// WHY A COMPILED-IN CONSTANT and not the seeded `providers` rows. The rows
// live in shared.db, which is unencrypted. Whoever can write that file can set
// `local_model_tag` to anything, and could add a matching row as well, so a
// check against the rows would not stop them. This list is part of the binary.
//
// KEEPING IT IN STEP. Every entry needs a seeded `providers` row
// (schema/shared_024.sql and later migrations) and every seeded local_model row
// needs an entry here; `seeded_local_model_rows_match_the_curated_set` fails
// the build when they drift in either direction. items.id=578 (curating more
// models) adds one line here per model. This list is deliberately NOT
// `evaluation::RELEASE_1_MODELS`: that is an evaluation list, and tying the
// trust boundary to it would widen the boundary whenever a model is added
// for evaluation.

/// Tags QR may pull. Exact match only; see the module comment.
pub const CURATED_LOCAL_MODEL_TAGS: &[&str] = &["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"];

/// Whether `tag` is exactly one of [`CURATED_LOCAL_MODEL_TAGS`].
///
/// Everything else is refused: any registry host or path prefix
/// (`registry.ollama.ai/library/...`, `library/...`, `hf.co/...`,
/// `host:5000/name:tag`), a digest (`@sha256:...`), a different case, any
/// leading or trailing whitespace, an untagged name, a cloud tag, an empty
/// string and every tag nobody has curated.
pub fn is_curated_local_model_tag(tag: &str) -> bool {
    CURATED_LOCAL_MODEL_TAGS.contains(&tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curated_tag_is_accepted() {
        assert!(!CURATED_LOCAL_MODEL_TAGS.is_empty());
        for tag in CURATED_LOCAL_MODEL_TAGS {
            assert!(is_curated_local_model_tag(tag), "{tag}");
        }
        for tag in ["llama3.2:3b", "llama3.1:8b", "qwen2.5:7b"] {
            assert!(is_curated_local_model_tag(tag), "{tag}");
        }
    }

    /// Host and path prefixes are what decisions.id=840 is about: none may
    /// pass, however the tag after the prefix looks.
    #[test]
    fn host_and_path_prefixes_are_rejected() {
        for tag in [
            "registry.ollama.ai/library/llama3.2:3b",
            "https://registry.ollama.ai/library/llama3.2:3b",
            "http://evil.example/llama3.2:3b",
            "library/llama3.2:3b",
            "/llama3.2:3b",
            "./llama3.2:3b",
            "../llama3.2:3b",
            "hf.co/someone/model:latest",
            "evil.example/llama3.2:3b",
            "evil.example:5000/library/llama3.2:3b",
            "127.0.0.1:11434/llama3.2:3b",
            "localhost/llama3.2:3b",
            "user@evil.example/llama3.2:3b",
            "evil.example/ns/qwen2.5:7b",
            "ns/llama3.1:8b",
            "\\\\evil\\share\\llama3.1:8b",
        ] {
            assert!(!is_curated_local_model_tag(tag), "{tag}");
        }
    }

    #[test]
    fn uncurated_and_malformed_tags_are_rejected() {
        for tag in [
            "llama3.3:70b",
            "llama3.2:1b",
            "llama3.2",
            "llama3.2:latest",
            "qwen2.5",
            "gpt-oss:20b",
            "gpt-oss:cloud",
            "llama3.2:3b-cloud",
            "LLAMA3.2:3B",
            "Llama3.2:3b",
            " llama3.2:3b",
            "llama3.2:3b ",
            "llama3.2:3b\n",
            "\tllama3.2:3b",
            "llama3.2:3b\0",
            "llama3.2:3b@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "llama3.2:3b,llama3.1:8b",
            "llama3.2:3b/evil",
            "\"llama3.2:3b\"",
            "",
            ":",
            "llama3.2:",
            ":3b",
        ] {
            assert!(!is_curated_local_model_tag(tag), "{tag:?}");
        }
    }

    /// The constant and the seeded `providers` rows must agree in both
    /// directions (items.id=578 will add to both). Reads the rows through the
    /// real migrations, the same way the provider_store seeding test does.
    #[tokio::test]
    async fn seeded_local_model_rows_match_the_curated_set() {
        use sqlx::{sqlite::SqliteConnectOptions, ConnectOptions, Row};

        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        crate::persistence::migrations::run_migrations(&mut conn, "shared", None)
            .await
            .expect("run shared migrations");

        let rows = sqlx::query(
            "SELECT local_model_tag FROM providers \
             WHERE provider_type = 'local_model' ORDER BY local_model_tag",
        )
        .fetch_all(&mut conn)
        .await
        .expect("read seeded local_model rows");

        let mut seeded: Vec<String> = rows
            .iter()
            .map(|r| {
                r.try_get::<Option<String>, _>("local_model_tag")
                    .expect("local_model_tag column")
                    .expect("a seeded local_model row must carry a local_model_tag")
            })
            .collect();
        seeded.sort();
        let mut curated: Vec<String> = CURATED_LOCAL_MODEL_TAGS
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        curated.sort();

        assert_eq!(
            seeded, curated,
            "seeded local_model rows and CURATED_LOCAL_MODEL_TAGS must list the same tags"
        );
        for tag in &seeded {
            assert!(is_curated_local_model_tag(tag), "{tag}");
        }
    }
}
