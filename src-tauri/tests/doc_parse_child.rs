// src-tauri/tests/doc_parse_child.rs
//
// items.id=837 (decisions.id=927): the PDF/DOCX text parser runs in a child
// process of the app binary under a memory cap, a CPU limit and a wall-clock
// kill. These tests drive the real parent code (`extract_in_child`) against:
//   - the real app binary in parse mode, with hostile files generated here at
//     test time (a flate-bomb PDF, a nested-q PDF, an XML-bomb DOCX), each small
//     on disk and far over the memory cap once parsed;
//   - small /bin/sh scripts standing in for the child, to pin the kill, wait,
//     cap, environment, descriptor and limit behaviour that a parser cannot
//     show on demand.
// Linux only, like the limits (Windows and macOS are items.id=838).
#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use flate2::write::{DeflateEncoder, ZlibEncoder};
use flate2::{Compression, Crc};
use quietrabbit_lib::doc_parse::{
    extract_in_child, DocumentKind, ParseCommand, ParseFailure, ParseLimits,
};

const APP: &str = env!("CARGO_BIN_EXE_quietrabbit");

/// The real children each peak at over a gibibyte; one at a time.
static HEAVY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The wall clock ingest uses (EXTRACTION_TIMEOUT, 20 s).
const WALL: Duration = Duration::from_secs(20);

fn app_command(kind: DocumentKind) -> ParseCommand {
    ParseCommand::executable(PathBuf::from(APP), kind)
}

fn sh(script: &str) -> ParseCommand {
    ParseCommand {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
    }
}

fn sh_with_arg(script: &str, arg: &std::path::Path) -> ParseCommand {
    let mut c = sh(script);
    c.args.push(arg.into());
    c
}

fn limits(wall: Duration, max_output: usize) -> ParseLimits {
    ParseLimits {
        wall,
        max_output,
        ..ParseLimits::production(wall)
    }
}

// ---- fixtures, generated here ---------------------------------------------

/// A one-page PDF whose only content stream is `stream_bytes` (already
/// compressed when `flate`), with byte-exact xref offsets.
fn pdf_with_stream(stream_bytes: &[u8], flate: bool) -> Vec<u8> {
    let filter = if flate { " /Filter /FlateDecode" } else { "" };
    let mut objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> \
          /MediaBox [0 0 200 200] /Contents 5 0 R >>"
            .to_vec(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut stream = format!("<< /Length {}{filter} >>\nstream\n", stream_bytes.len()).into_bytes();
    stream.extend_from_slice(stream_bytes);
    stream.extend_from_slice(b"\nendstream");
    objects.push(stream);

    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(obj);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f\r\n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n\r\n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn hello_pdf() -> Vec<u8> {
    pdf_with_stream(b"BT /F1 24 Tf 10 100 Td (Hello World) Tj ET", false)
}

fn zlib_of_repeats(chunk: &[u8], times: usize) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    for _ in 0..times {
        enc.write_all(chunk).unwrap();
    }
    enc.finish().unwrap()
}

/// A content stream of `2 GiB` of spaces: about 2 MB deflated.
fn flate_bomb_pdf() -> Vec<u8> {
    let chunk = vec![b' '; 1 << 20];
    pdf_with_stream(&zlib_of_repeats(&chunk, 2048), true)
}

/// Five million nested `q` operators: about 10 KB deflated.
fn nested_q_pdf() -> Vec<u8> {
    let content = b"q ".repeat(5_000_000);
    pdf_with_stream(&zlib_of_repeats(&content, 1), true)
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;
const DOC_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
const DOC_TAIL: &str = "</w:body></w:document>";

/// A minimal zip with one entry per `(name, raw deflate stream, crc32,
/// uncompressed size)`. Hand-built so a bomb entry can be declared at its true
/// size without ever holding it in memory.
fn zip_of(entries: &[(&str, Vec<u8>, u32, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, crc, size) in entries {
        let offset = out.len() as u32;
        let method: u16 = 8;
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0u16.to_le_bytes()); // flags
        local.extend_from_slice(&method.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // time
        local.extend_from_slice(&0x21u16.to_le_bytes()); // date
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // extra
        local.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&local);
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // made by
        central.extend_from_slice(&20u16.to_le_bytes()); // needed
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x21u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]); // extra, comment, disk, internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // disk numbers
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A raw-deflate entry made of `parts` (each `(bytes, repeat count)`).
fn deflated(parts: &[(&[u8], usize)]) -> (Vec<u8>, u32, u32) {
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
    let mut crc = Crc::new();
    for (bytes, times) in parts {
        for _ in 0..*times {
            enc.write_all(bytes).unwrap();
            crc.update(bytes);
        }
    }
    (enc.finish().unwrap(), crc.sum(), crc.amount())
}

fn docx_of(body: &[(&[u8], usize)]) -> Vec<u8> {
    let mut parts: Vec<(&[u8], usize)> = vec![(DOC_HEAD.as_bytes(), 1)];
    parts.extend_from_slice(body);
    parts.push((DOC_TAIL.as_bytes(), 1));
    let (doc, doc_crc, doc_size) = deflated(&parts);
    let (ct, ct_crc, ct_size) = deflated(&[(CONTENT_TYPES.as_bytes(), 1)]);
    let (rels, rels_crc, rels_size) = deflated(&[(ROOT_RELS.as_bytes(), 1)]);
    zip_of(&[
        ("[Content_Types].xml", ct, ct_crc, ct_size),
        ("_rels/.rels", rels, rels_crc, rels_size),
        ("word/document.xml", doc, doc_crc, doc_size),
    ])
}

fn hello_docx() -> Vec<u8> {
    docx_of(&[(b"<w:p><w:r><w:t>Hello Docx</w:t></w:r></w:p>".as_slice(), 1)])
}

/// 512 MiB of paragraphs: well over the cap once parsed, about 0.6 MB zipped.
fn xml_bomb_docx() -> Vec<u8> {
    let para = b"<w:p><w:r><w:t>Hello docx bomb paragraph</w:t></w:r></w:p>".repeat(1 << 14);
    let times = (512usize << 20) / para.len();
    docx_of(&[(para.as_slice(), times)])
}

// ---- helpers ---------------------------------------------------------------

/// True once `pid` is gone or only a zombie awaiting its parent.
fn process_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    }
}

async fn wait_gone(pid: i32) -> bool {
    for _ in 0..100 {
        if process_gone(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn read_pid(path: &std::path::Path) -> Option<i32> {
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(pid) = s.trim().parse() {
                return Some(pid);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

async fn hostile(kind: DocumentKind, bytes: Vec<u8>) {
    let _one_at_a_time = HEAVY.lock().await;
    let started = Instant::now();
    let result = extract_in_child(&app_command(kind), &bytes, ParseLimits::production(WALL)).await;
    let took = started.elapsed();
    assert!(took < WALL, "took {took:?}, the wall clock is {WALL:?}");
    match result {
        // Killed by a signal: the memory cap's abort. A clean refusal (Exit)
        // would also leave no text, but would not show the cap works, so the
        // valid-file round trips above are what keep these fixtures honest.
        Err(ParseFailure::Signal(_)) => {}
        other => panic!("expected the memory cap to kill the child, got {other:?}"),
    }
}

// ---- the real child --------------------------------------------------------

#[tokio::test]
async fn a_valid_pdf_round_trips_through_the_app_binary() {
    let _one_at_a_time = HEAVY.lock().await;
    let text = extract_in_child(
        &app_command(DocumentKind::Pdf),
        &hello_pdf(),
        ParseLimits::production(WALL),
    )
    .await
    .expect("a valid PDF must give text");
    assert!(text.contains("Hello"), "got {text:?}");
}

#[tokio::test]
async fn a_valid_docx_round_trips_through_the_app_binary() {
    let _one_at_a_time = HEAVY.lock().await;
    let text = extract_in_child(
        &app_command(DocumentKind::Docx),
        &hello_docx(),
        ParseLimits::production(WALL),
    )
    .await
    .expect("a valid DOCX must give text");
    assert!(text.contains("Hello Docx"), "got {text:?}");
}

#[tokio::test]
async fn garbage_is_a_clean_failure_not_text() {
    let _one_at_a_time = HEAVY.lock().await;
    for kind in [DocumentKind::Pdf, DocumentKind::Docx] {
        let result = extract_in_child(
            &app_command(kind),
            b"this is not a document at all",
            ParseLimits::production(WALL),
        )
        .await;
        assert!(
            matches!(result, Err(ParseFailure::Exit(_))),
            "{kind:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_flate_bomb_pdf_is_stopped_by_the_memory_cap() {
    hostile(DocumentKind::Pdf, flate_bomb_pdf()).await;
}

#[tokio::test]
async fn a_nested_q_pdf_is_stopped_by_the_memory_cap() {
    hostile(DocumentKind::Pdf, nested_q_pdf()).await;
}

#[tokio::test]
async fn an_xml_bomb_docx_is_stopped_by_the_memory_cap() {
    hostile(DocumentKind::Docx, xml_bomb_docx()).await;
}

// ---- the parent's own behaviour, with a script for a child -----------------

#[tokio::test]
async fn text_from_the_child_is_returned() {
    let got = extract_in_child(
        &sh("cat >/dev/null; printf hello"),
        b"x",
        limits(WALL, 1000),
    )
    .await;
    assert_eq!(got, Ok("hello".to_owned()));
}

#[tokio::test]
async fn the_child_receives_exactly_the_bytes_on_stdin() {
    let got = extract_in_child(&sh("cat"), b"round trip", limits(WALL, 1000)).await;
    assert_eq!(got, Ok("round trip".to_owned()));
}

#[tokio::test]
async fn a_spawn_failure_is_a_spawn_failure_not_a_signal() {
    let command = ParseCommand {
        program: PathBuf::from("/nonexistent/qr-parse"),
        args: Vec::new(),
    };
    let got = extract_in_child(&command, b"x", limits(WALL, 1000)).await;
    assert!(matches!(got, Err(ParseFailure::Spawn(_))), "{got:?}");
}

#[tokio::test]
async fn a_nonzero_exit_is_a_failure() {
    let got = extract_in_child(
        &sh("cat >/dev/null; printf partial; exit 3"),
        b"x",
        limits(WALL, 1000),
    )
    .await;
    assert_eq!(got, Err(ParseFailure::Exit(3)));
}

#[tokio::test]
async fn non_utf8_output_is_a_failure() {
    let got = extract_in_child(
        &sh("cat >/dev/null; printf '\\377\\376'"),
        b"x",
        limits(WALL, 1000),
    )
    .await;
    assert_eq!(got, Err(ParseFailure::NotUtf8));
}

#[tokio::test]
async fn output_over_the_cap_is_oversize_and_the_child_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    // Never stops writing: only the parent's kill ends it.
    let command = sh_with_arg("echo $$ > \"$0\"; exec yes aaaaaaaa", &pidfile);
    let started = Instant::now();
    let got = extract_in_child(&command, b"x", limits(WALL, 1000)).await;
    assert_eq!(got, Err(ParseFailure::Oversize));
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid = read_pid(&pidfile).await.expect("the child wrote its pid");
    assert!(wait_gone(pid).await, "the oversize child is still running");
}

#[tokio::test]
async fn a_hung_child_is_killed_at_the_wall_clock() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let command = sh_with_arg("echo $$ > \"$0\"; exec sleep 60", &pidfile);
    let started = Instant::now();
    let got = extract_in_child(&command, b"x", limits(Duration::from_secs(1), 1000)).await;
    let took = started.elapsed();
    assert_eq!(got, Err(ParseFailure::Timeout));
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(5),
        "took {took:?}"
    );
    let pid = read_pid(&pidfile).await.expect("the child wrote its pid");
    assert!(wait_gone(pid).await, "the hung child is still running");
}

#[tokio::test]
async fn the_whole_process_group_dies_not_just_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild");
    let command = sh_with_arg("sleep 60 & echo $! > \"$0\"; wait", &pidfile);
    let got = extract_in_child(&command, b"x", limits(Duration::from_secs(1), 1000)).await;
    assert_eq!(got, Err(ParseFailure::Timeout));
    let pid = read_pid(&pidfile).await.expect("the grandchild's pid");
    assert!(
        wait_gone(pid).await,
        "the grandchild survived the group kill"
    );
}

#[tokio::test]
async fn dropping_the_future_kills_and_reaps_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let command = sh_with_arg("echo $$ > \"$0\"; exec sleep 60", &pidfile);
    let task =
        tokio::spawn(async move { extract_in_child(&command, b"x", limits(WALL, 1000)).await });
    let pid = read_pid(&pidfile).await.expect("the child wrote its pid");
    assert!(!process_gone(pid), "the child should be running");
    task.abort();
    let _ = task.await;
    assert!(
        wait_gone(pid).await,
        "a cancelled extraction left its child running"
    );
}

#[tokio::test]
async fn the_child_gets_the_limits_and_no_environment() {
    // SAFETY-adjacent: a name no other test or code reads.
    std::env::set_var("QR_TEST_DOC_PARSE_SECRET", "hunter2");
    let l = ParseLimits {
        memory_bytes: 3 * 1024 * 1024 * 1024,
        cpu_secs: 7,
        wall: WALL,
        max_output: 1000,
    };
    let script = "cat >/dev/null; ulimit -v; ulimit -c; ulimit -t; \
                  if [ -n \"${QR_TEST_DOC_PARSE_SECRET+x}\" ]; then echo leaked; else echo clean; fi; pwd";
    let got = extract_in_child(&sh(script), b"x", l).await.unwrap();
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(lines, ["3145728", "0", "7", "clean", "/"], "got {got:?}");
}

#[tokio::test]
async fn descriptors_above_stderr_do_not_reach_the_child() {
    let file = std::fs::File::open("/etc/hostname").unwrap();
    // dup2 to a high number clears close-on-exec: the worst case, an inherited
    // descriptor the app forgot to mark.
    let leaked = 211;
    // SAFETY: dup2 onto a descriptor number this test owns.
    assert_eq!(unsafe { libc::dup2(file.as_raw_fd(), leaked) }, leaked);
    let got = extract_in_child(
        &sh("cat >/dev/null; if [ -e /proc/self/fd/211 ]; then echo leaked; else echo clean; fi"),
        b"x",
        limits(WALL, 1000),
    )
    .await;
    // SAFETY: closes the descriptor created above.
    unsafe { libc::close(leaked) };
    assert_eq!(got, Ok("clean\n".to_owned()));
}
