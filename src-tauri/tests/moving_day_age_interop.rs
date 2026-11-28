//! items.id=966: the Moving Day file against the stock `age` tool, both ways.
//! A user who drops the first line must be able to open the rest with `age -d`,
//! with no Quiet Rabbit code at all.
//!
//! `age` reads a passphrase from a terminal only, so `expect` supplies it. The
//! passwords are test-only, and they reach `expect` through the environment,
//! never the command line.
//!
//! Without `age` or `expect` on PATH the test prints a skip and passes, EXCEPT
//! when `QR_REQUIRE_STOCK_AGE=1`, where it fails. CI sets it, so it cannot pass
//! silently there. Linux only: Windows and macOS are checked at the release gate
//! (decisions.id=972).
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Stdio};

use quietrabbit_lib::moving_day::container;

const PREAMBLE: &[u8] = b"QRMEMORY 1\n";
const PASSWORD_VAR: &str = "QR_TEST_AGE_PASSWORD";

const DECRYPT: &str = r#"
set timeout 120
spawn -noecho age -d -o [lindex $argv 1] [lindex $argv 0]
expect {
    "passphrase" {}
    timeout { exit 90 }
    eof { exit 91 }
}
send -- "$env(QR_TEST_AGE_PASSWORD)\r"
expect {
    timeout { exit 90 }
    eof {}
}
exit [lindex [wait] 3]
"#;

const ENCRYPT: &str = r#"
set timeout 120
spawn -noecho age -p -o [lindex $argv 1] [lindex $argv 0]
expect {
    "Enter passphrase" {}
    timeout { exit 90 }
    eof { exit 91 }
}
send -- "$env(QR_TEST_AGE_PASSWORD)\r"
expect {
    "Confirm passphrase" {}
    timeout { exit 90 }
    eof { exit 91 }
}
send -- "$env(QR_TEST_AGE_PASSWORD)\r"
expect {
    timeout { exit 90 }
    eof {}
}
exit [lindex [wait] 3]
"#;

fn runs(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether the stock tools are here. Panics when they are required and are not.
fn stock_tools() -> bool {
    let present = runs("age", "--version") && runs("expect", "-v");
    if !present {
        assert!(
            std::env::var("QR_REQUIRE_STOCK_AGE").as_deref() != Ok("1"),
            "QR_REQUIRE_STOCK_AGE=1, but `age` or `expect` is not on PATH"
        );
        eprintln!("SKIPPED: `age` or `expect` is not on PATH");
    }
    present
}

/// Run one of the scripts, and say whether `age` succeeded.
fn drive(script: &str, password: &str, input: &Path, output: &Path) -> bool {
    let dir = input.parent().expect("parent");
    let script_path = dir.join("drive.exp");
    std::fs::write(&script_path, script).expect("write script");
    let result = Command::new("expect")
        .arg(&script_path)
        .arg(input)
        .arg(output)
        .env(PASSWORD_VAR, password)
        // The password crosses the terminal as UTF-8.
        .env("LC_ALL", "C.UTF-8")
        .output()
        .expect("run expect");
    if !result.status.success() {
        eprintln!(
            "expect exit {:?}: {}{}",
            result.status.code(),
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    result.status.success()
}

/// More than one 64 KiB chunk, with bytes a terminal would mangle.
fn payload() -> Vec<u8> {
    (0..150_000usize).map(|i| (i % 256) as u8).collect()
}

/// ASCII, and a non-ASCII password already in NFKC form (typed raw into `age`,
/// which does not normalise).
const PASSWORDS: [&str; 2] = [
    "correct horse battery staple",
    "Z\u{fc}rich caf\u{e9} \u{6771}\u{4eac} passphrase",
];

#[test]
fn stock_age_opens_our_file_without_its_first_line() {
    if !stock_tools() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    for (i, password) in PASSWORDS.iter().enumerate() {
        let file = container::seal(&payload(), password).expect("seal");
        let age_part = file.strip_prefix(PREAMBLE).expect("preamble");
        let input = dir.path().join(format!("ours-{i}.age"));
        let output = dir.path().join(format!("ours-{i}.out"));
        std::fs::write(&input, age_part).expect("write");
        assert!(drive(DECRYPT, password, &input, &output), "password {i}");
        assert!(
            std::fs::read(&output).expect("read") == payload(),
            "password {i}"
        );
    }
}

/// The check above can fail: the same run with another password does not open.
#[test]
fn stock_age_refuses_our_file_with_another_password() {
    if !stock_tools() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let file = container::seal(b"payload", PASSWORDS[0]).expect("seal");
    let input = dir.path().join("ours.age");
    let output = dir.path().join("ours.out");
    std::fs::write(&input, file.strip_prefix(PREAMBLE).expect("preamble")).expect("write");
    assert!(!drive(DECRYPT, PASSWORDS[1], &input, &output));
    assert!(std::fs::read(&output).unwrap_or_default() != b"payload");
}

#[test]
fn we_open_what_stock_age_writes_behind_our_first_line() {
    if !stock_tools() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("payload");
    let output = dir.path().join("stock.age");
    std::fs::write(&input, payload()).expect("write");
    assert!(drive(ENCRYPT, PASSWORDS[1], &input, &output));
    let mut file = PREAMBLE.to_vec();
    file.extend(std::fs::read(&output).expect("read"));
    let opened = container::open(&file, PASSWORDS[1]).expect("open");
    assert!(opened.as_slice() == payload());
}
