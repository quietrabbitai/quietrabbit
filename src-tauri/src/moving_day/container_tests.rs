use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;

/// Test-only passwords.
const PW: &str = "correct horse battery staple";
const FIXTURE_PW: &str = "fixture passphrase for Z\u{fc}rich";

/// A cheap work factor for tests about everything except the work factor.
const CHEAP: u8 = 10;

const PAYLOAD: &[u8] = include_bytes!("../../tests/fixtures/moving_day/payload.txt");
const OURS: &[u8] = include_bytes!("../../tests/fixtures/moving_day/ours_v1.bin");
const STOCK: &[u8] = include_bytes!("../../tests/fixtures/moving_day/stock_age_v1.bin");
const PUBLIC_KEY: &[u8] = include_bytes!("../../tests/fixtures/moving_day/public_key.bin");

const PREAMBLE: &[u8] = b"QRMEMORY 1\n";
const CHUNK: usize = 64 * 1024;
const ENCRYPTED_CHUNK: usize = CHUNK + 16;

fn sealed(plaintext: &[u8]) -> Vec<u8> {
    seal_at(plaintext, PW, CHEAP).expect("seal")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("needle")
}

/// Where the age header ends: just past the newline of the `--- <mac>` line.
fn header_end(file: &[u8]) -> usize {
    let mac = find(file, b"\n--- ") + 1;
    mac + find(&file[mac..], b"\n") + 1
}

fn assert_failed(file: &[u8], what: &str) {
    assert_eq!(
        open_capped(file, PW, MAX_WORK_FACTOR).map(|p| p.len()),
        Err(OpenError::PasswordOrDamaged),
        "{what}"
    );
}

#[test]
fn round_trips_every_chunk_shape() {
    for len in [0, 1, CHUNK, CHUNK + 1, 3 * CHUNK + 7] {
        let plaintext = pattern(len);
        let file = sealed(&plaintext);
        let opened = open(&file, PW).expect("open");
        assert!(opened.as_slice() == plaintext, "length {len}");
    }
}

#[test]
fn seal_writes_the_preamble_and_one_scrypt_stanza_at_the_written_factor() {
    let file = seal(b"x", PW).expect("seal");
    let end = header_end(&file);
    let header = std::str::from_utf8(&file[..end]).expect("text header");
    let lines: Vec<&str> = header.lines().collect();
    assert_eq!(lines[0], "QRMEMORY 1");
    assert_eq!(lines[1], "age-encryption.org/v1");
    let stanza: Vec<&str> = lines[2].split(' ').collect();
    assert_eq!((stanza[0], stanza[1]), ("->", "scrypt"));
    assert_eq!(stanza[3], "18", "the written work factor");
    assert_eq!(
        header.lines().filter(|l| l.starts_with("-> ")).count(),
        1,
        "exactly one stanza"
    );
    assert!(end - PREAMBLE.len() < 200, "header length {end}");
    assert_eq!(open(&file, PW).expect("open").as_slice(), b"x");
}

#[test]
fn two_files_of_the_same_payload_differ() {
    assert_ne!(sealed(b"same"), sealed(b"same"));
}

#[test]
fn the_work_factors_are_the_ruled_ones() {
    assert_eq!(WRITE_WORK_FACTOR, 18);
    assert_eq!(MAX_WORK_FACTOR, 19);
    // `open` passes the ruled limit, and the limit reaches age. Behaviour is
    // shown below with a low limit, so no test runs a large derivation.
    let source = include_str!("container.rs");
    assert!(source.contains("open_capped(file, password, MAX_WORK_FACTOR)"));
    assert!(source.contains("identity.set_max_work_factor(max_work_factor);"));
    assert!(source.contains("seal_at(plaintext, password, WRITE_WORK_FACTOR)"));
}

#[test]
fn a_work_factor_above_the_limit_is_refused() {
    let file = seal_at(b"payload", PW, 13).expect("seal");
    assert!(open_capped(&file, PW, 13).is_ok());
    assert_eq!(
        open_capped(&file, PW, 12).map(|p| p.len()),
        Err(OpenError::PasswordOrDamaged)
    );
}

#[test]
fn the_preamble_decides_not_ours_and_newer_before_any_password() {
    let good = sealed(b"payload");
    let age_only = &good[PREAMBLE.len()..];
    let not_ours: &[&[u8]] = &[
        b"",
        b"QRMEMORY",
        b"QRMEMORY ",
        b"QRMEMORY 1",
        b"QRMEMORY \n",
        b"QRMEMORY 0\n",
        b"QRMEMORY 01\n",
        b"QRMEMORY x\n",
        b"QRMEMORY 1x\n",
        b"QRMEMORY -1\n",
        b"QRMEMORY  1\n",
        b"QRMEMORY 1 \n",
        b"QRMEMORY 1\r\n",
        b"QRMEMORY 1000000000\n",
        b"QRMEMORY 99999999999999999999999999\n",
        b"qrmemory 1\n",
        b" QRMEMORY 1\n",
        b"PK\x03\x04 some other file",
        age_only,
    ];
    for file in not_ours {
        let shown = String::from_utf8_lossy(&file[..file.len().min(24)]);
        assert_eq!(probe(file), Err(OpenError::NotOurs), "probe {shown:?}");
        assert_eq!(
            open(file, PW).map(|p| p.len()),
            Err(OpenError::NotOurs),
            "open {shown:?}"
        );
    }
    for version in ["2", "10", "999999999"] {
        let mut file = format!("QRMEMORY {version}\n").into_bytes();
        file.extend_from_slice(age_only);
        assert_eq!(probe(&file), Err(OpenError::NewerVersion), "{version}");
        assert_eq!(
            open(&file, PW).map(|p| p.len()),
            Err(OpenError::NewerVersion),
            "{version}"
        );
    }
    assert_eq!(probe(&good), Ok(()));
    // probe reads the preamble only: what follows is not its business.
    assert_eq!(probe(PREAMBLE), Ok(()));
}

#[test]
fn a_wrong_password_does_not_open() {
    let file = sealed(b"payload");
    for wrong in [
        "",
        "correct horse battery stapl",
        "Correct horse battery staple",
    ] {
        assert_eq!(
            open(&file, wrong).map(|p| p.len()),
            Err(OpenError::PasswordOrDamaged),
            "{wrong:?}"
        );
    }
}

#[test]
fn one_changed_byte_anywhere_does_not_open() {
    let file = sealed(&pattern(CHUNK + 100));
    let end = header_end(&file);
    let salt = find(&file, b"-> scrypt ") + 12;
    let wrapped_key = find(&file, b" 10\n") + 6;
    let mac = find(&file, b"\n--- ") + 8;
    let nonce = end + 3;
    let first_chunk = end + 16 + 100;
    let last_chunk = file.len() - 40;
    assert!(open(&file, PW).is_ok());
    for (at, what) in [
        (salt, "salt"),
        (wrapped_key, "wrapped file key"),
        (mac, "header MAC"),
        (nonce, "nonce"),
        (first_chunk, "first chunk"),
        (end + 16 + ENCRYPTED_CHUNK - 1, "first chunk tag"),
        (last_chunk, "last chunk"),
        (file.len() - 1, "last byte"),
    ] {
        let mut changed = file.clone();
        // Stay inside base64 in the text header.
        changed[at] = if at >= end {
            changed[at] ^ 0x01
        } else if changed[at] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert_failed(&changed, what);
    }
}

#[test]
fn a_file_cut_short_does_not_open() {
    let file = sealed(&pattern(2 * CHUNK + 100));
    let end = header_end(&file);
    let payload = end + 16;
    for (len, what) in [
        (PREAMBLE.len(), "preamble only"),
        (end - 10, "inside the header"),
        (end, "header only"),
        (end + 7, "inside the nonce"),
        (payload, "no chunk"),
        (payload + 100, "inside the first chunk"),
        (payload + ENCRYPTED_CHUNK, "after the first chunk"),
        (payload + 2 * ENCRYPTED_CHUNK, "after the second chunk"),
        (file.len() - 1, "one byte short"),
    ] {
        assert_failed(&file[..len], what);
    }
    // The same at a payload that ends exactly on a chunk boundary.
    let file = sealed(&pattern(2 * CHUNK));
    let payload = header_end(&file) + 16;
    assert!(open(&file, PW).is_ok());
    assert_failed(&file[..payload + ENCRYPTED_CHUNK], "one whole chunk of two");
}

#[test]
fn bytes_added_at_the_end_do_not_open() {
    for len in [100, CHUNK, 2 * CHUNK] {
        let mut file = sealed(&pattern(len));
        file.push(0);
        assert_failed(&file, &format!("one byte after {len}"));
        file.extend_from_slice(&[7u8; 200]);
        assert_failed(&file, &format!("201 bytes after {len}"));
    }
}

#[test]
fn a_second_stanza_is_refused() {
    let file = sealed(b"payload");
    let mac = find(&file, b"\n--- ") + 1;
    let mut two = file[..mac].to_vec();
    two.extend_from_slice(b"-> other stanza\nAAAAAAAAAAAAAAAAAAAAAA\n");
    two.extend_from_slice(&file[mac..]);
    assert_failed(&two, "two stanzas");
}

#[test]
fn a_public_key_age_file_is_refused_before_any_key_work() {
    // Made by the stock tool (`age -r`): a valid age file with no passphrase.
    assert_eq!(probe(PUBLIC_KEY), Ok(()));
    assert_failed(PUBLIC_KEY, "public-key file");
    assert!(include_str!("container.rs").contains("if !decryptor.is_scrypt() {"));
}

#[test]
fn the_size_limits_hold_on_both_sides() {
    let too_much = vec![0u8; MAX_PLAINTEXT_BYTES + 1];
    assert_eq!(
        seal_at(&too_much, PW, CHEAP).map(|f| f.len()),
        Err(SealError::TooLarge)
    );
    // The largest payload still fits in a file `open` accepts.
    let largest = pattern(MAX_PLAINTEXT_BYTES);
    let file = sealed(&largest);
    assert!(file.len() <= MAX_FILE_BYTES, "{}", file.len());
    assert!(open(&file, PW).expect("open").as_slice() == largest);

    let mut over = file;
    over.resize(MAX_FILE_BYTES + 1, 0);
    assert_failed(&over, "over the file limit");
    assert!(include_str!("container.rs").contains("if file.len() > MAX_FILE_BYTES ||"));
}

#[test]
fn an_empty_password_seals_nothing() {
    assert_eq!(
        seal(b"payload", "").map(|f| f.len()),
        Err(SealError::EmptyPassword)
    );
}

/// age 0.12.1's header reader costs the square of the header's length. Without
/// the bound, 1 MiB of header took 404 s here and the full file limit would take
/// days. The time limit is very loose: the bounded answer needs no key work.
#[test]
fn a_header_that_never_ends_is_refused_quickly() {
    let mut file = PREAMBLE.to_vec();
    file.extend_from_slice(b"age-encryption.org/v1\n-> scrypt AAAAAAAAAAAAAAAAAAAAAA 10\n");
    let line = format!("{}\n", "A".repeat(64));
    // Long enough to be unmistakably slow unbounded (about 25 s here), short
    // enough that a broken bound still ends the test.
    while file.len() < 256 * 1024 {
        file.extend_from_slice(line.as_bytes());
    }
    let started = Instant::now();
    assert_failed(&file, "endless header");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");

    // The same at the file limit.
    while file.len() + line.len() <= MAX_FILE_BYTES {
        file.extend_from_slice(line.as_bytes());
    }
    assert_failed(&file, "endless header at the file limit");
}

#[test]
fn the_header_bound_is_on_the_closing_line() {
    let fits = |pad: usize, tail: &[u8]| {
        let mut body = vec![b'A'; pad];
        body.extend_from_slice(tail);
        header_is_bounded(&body)
    };
    assert!(fits(10, b"\n--- mac\nrest"));
    assert!(fits(MAX_HEADER_BYTES - 9, b"\n--- mac\n"));
    assert!(
        !fits(MAX_HEADER_BYTES - 8, b"\n--- mac\n"),
        "newline outside"
    );
    assert!(!fits(MAX_HEADER_BYTES, b"\n--- mac\n"), "line outside");
    assert!(!fits(10, b"\n--- mac"), "unfinished line");
    assert!(!fits(10, b"\n---mac\n"));
    assert!(!fits(0, b""));
    let file = sealed(b"x");
    assert!(header_is_bounded(&file[PREAMBLE.len()..]));
}

#[test]
fn the_password_is_normalised_on_both_sides() {
    // (as sealed, other spellings with the same NFKC form)
    let cases = [
        // Full-width letters.
        (
            "passphrase one",
            "\u{ff50}\u{ff41}\u{ff53}\u{ff53}phrase one",
        ),
        // The fi ligature.
        ("final answer", "\u{fb01}nal answer"),
        // Composed and decomposed e acute.
        ("caf\u{e9} au lait", "cafe\u{301} au lait"),
    ];
    for (plain, other) in cases {
        assert_ne!(plain.as_bytes(), other.as_bytes());
        for (seal_with, open_with) in [(plain, other), (other, plain)] {
            let file = seal_at(b"payload", seal_with, CHEAP).expect("seal");
            assert!(
                open(&file, open_with).is_ok(),
                "{seal_with:?} {open_with:?}"
            );
        }
    }
}

#[test]
fn nothing_else_is_folded_into_the_password() {
    let file = seal_at(b"payload", " padded ", CHEAP).expect("seal");
    assert!(open(&file, "padded").is_err());
    assert!(open(&file, " padded ").is_ok());
}

/// Fixed files. If either stops opening, files already made stop opening.
#[test]
fn the_fixed_file_from_this_code_still_opens() {
    let opened = open(OURS, FIXTURE_PW).expect("our fixed file");
    assert_eq!(opened.as_slice(), PAYLOAD);
}

#[test]
fn the_fixed_file_from_stock_age_opens() {
    // The preamble line followed by the output of `age -p`.
    let opened = open(STOCK, FIXTURE_PW).expect("the stock-made fixed file");
    assert_eq!(opened.as_slice(), PAYLOAD);
}

#[test]
fn errors_carry_nothing() {
    for e in [
        OpenError::NotOurs,
        OpenError::NewerVersion,
        OpenError::PasswordOrDamaged,
    ] {
        assert_eq!(std::mem::size_of_val(&e), 1);
        // Nothing in the UI says "QR" (decisions.id=889 addendum), and these
        // words may reach a log.
        assert!(!e.to_string().contains("QR"));
    }
    assert_eq!(std::mem::size_of::<SealError>(), 1);
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `age::` path in one file's code, comments left out.
fn age_paths(source: &str) -> Vec<String> {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut found = Vec::new();
    for line in source.lines() {
        let code = line.split("//").next().unwrap_or("");
        for (at, _) in code.match_indices("age::") {
            if code[..at].chars().next_back().is_some_and(ident) {
                continue; // storage::, package:: and the like
            }
            let path: String = code[at..]
                .chars()
                .take_while(|&c| ident(c) || c == ':')
                .collect();
            found.push(path);
        }
        let words = code.trim_start();
        let words = words.strip_prefix("pub ").unwrap_or(words);
        // `use age;`, `use age as x;` and `extern crate age` would let a name
        // reach age without the `age::` spelling.
        for start in ["use age;", "use age as ", "extern crate age"] {
            if words.starts_with(start) {
                found.push(words.to_owned());
            }
        }
    }
    found
}

/// The deny.toml ignores for RUSTSEC-2026-0387 and -0374 rest on this: the one
/// place that uses age is the container, and it uses the passphrase functions
/// only (never a public-key, tagged, plugin or SSH item).
#[test]
fn only_the_passphrase_functions_of_age_are_used() {
    const ALLOWED: [&str; 7] = [
        "age::scrypt::Recipient::new",
        "age::scrypt::Identity::new",
        "age::Encryptor::with_recipients",
        "age::Decryptor::new_buffered",
        "age::Recipient",
        "age::Identity",
        "age::secrecy::SecretString",
    ];
    // `SecretString::from` is the one longer spelling.
    let allowed = |p: &str| ALLOWED.contains(&p) || p == "age::secrecy::SecretString::from";

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests", "examples", "benches"] {
        if root.join(dir).is_dir() {
            rust_files(&root.join(dir), &mut files);
        }
    }
    files.push(root.join("build.rs"));
    let container = root.join("src/moving_day/container.rs");
    let this_file = root.join("src/moving_day/container_tests.rs");
    let mut in_container = 0;
    for file in &files {
        if *file == this_file {
            continue; // names the paths as strings, and is checked below
        }
        let paths = age_paths(&std::fs::read_to_string(file).expect("read"));
        if *file == container {
            for p in &paths {
                assert!(allowed(p), "container.rs uses {p}");
            }
            in_container = paths.len();
        } else {
            assert!(paths.is_empty(), "{} uses age: {paths:?}", file.display());
        }
    }
    assert!(in_container >= ALLOWED.len(), "the scan saw {in_container}");
    // This file uses no age item at all: outside strings it never writes one.
    let own = std::fs::read_to_string(&this_file).expect("read");
    let uses_age = own
        .lines()
        .any(|l| l.trim_start().starts_with("use age") || l.trim_start().starts_with("age::"));
    assert!(!uses_age);

    // The scan itself sees what it is meant to see.
    assert_eq!(
        age_paths("let k = age::x25519::Identity::generate();"),
        ["age::x25519::Identity::generate"]
    );
    assert_eq!(age_paths("use age::{plugin, ssh};"), ["age::"]);
    assert_eq!(age_paths("use age as a;"), ["use age as a;"]);
    assert!(age_paths("storage::open(); // age::ssh").is_empty());
}
