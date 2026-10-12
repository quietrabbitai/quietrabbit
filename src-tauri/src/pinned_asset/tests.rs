// src-tauri/src/pinned_asset/tests.rs
//
// Loopback only: no real host is ever contacted.

use std::io::{Read, Write};
use std::net::TcpListener;

use super::*;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};

fn body() -> Vec<u8> {
    (0..5000u32).flat_map(|i| i.to_le_bytes()).collect()
}

fn expect_of(bytes: &[u8]) -> Expect {
    Expect {
        size: bytes.len() as u64,
        sha256: sha256_of(bytes),
    }
}

fn fetcher() -> PinnedFetcher {
    PinnedFetcher::for_loopback_tests(&["127.0.0.1"])
}

async fn run(
    f: &PinnedFetcher,
    sources: &[&str],
    expect: &Expect,
    dest: &Path,
) -> Result<(), FetchError> {
    f.fetch_to_file(
        sources,
        expect,
        dest,
        &CancellationToken::new(),
        &mut |_, _| {},
    )
    .await
}

#[test]
fn hex_round_trips_and_rejects_bad_input() {
    let d = sha256_of(b"abc");
    let h = hex32(&d);
    assert_eq!(h.len(), 64);
    assert_eq!(Expect::from_hex(3, &h).unwrap().sha256, d);
    assert_eq!(Expect::from_hex(3, &h.to_uppercase()).unwrap().sha256, d);
    assert!(Expect::from_hex(3, "abcd").is_none());
    assert!(Expect::from_hex(3, &"zz".repeat(32)).is_none());
}

#[tokio::test]
async fn a_matching_body_is_placed_atomically_and_progress_is_reported() {
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("nested").join("asset.bin");
    let mut seen: Vec<(u64, u64)> = Vec::new();
    fetcher()
        .fetch_to_file(
            &[&format!("{}/a", server.base())],
            &expect_of(&data),
            &dest,
            &CancellationToken::new(),
            &mut |c, t| seen.push((c, t)),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!part_path(&dest).exists(), "no partial file is left behind");
    let last = seen.last().copied().unwrap();
    assert_eq!(last, (data.len() as u64, data.len() as u64));
}

#[tokio::test]
async fn requests_carry_no_identifying_headers_or_query() {
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    run(
        &fetcher(),
        &[&format!("{}/releases/x.bin", server.base())],
        &expect_of(&data),
        &dest,
    )
    .await
    .unwrap();
    let req = &server.captured()[0];
    assert_eq!(req.path, "/releases/x.bin", "no query string");
    assert!(req.header("user-agent").is_none(), "no User-Agent");
    assert!(req.header("authorization").is_none());
    assert!(req.header("cookie").is_none());
}

#[tokio::test]
async fn a_hash_mismatch_deletes_the_file_and_the_next_source_is_tried() {
    let good = body();
    let mut bad = good.clone();
    bad[10] ^= 0xff;
    let first = LoopbackServer::start(vec![Canned::bytes(200, bad)]);
    let second = LoopbackServer::start(vec![Canned::bytes(200, good.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    run(
        &fetcher(),
        &[&first.base(), &second.base()],
        &expect_of(&good),
        &dest,
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), good);
    assert_eq!(first.connections(), 1);
    assert_eq!(second.connections(), 1);
}

#[tokio::test]
async fn a_hash_mismatch_on_every_source_leaves_nothing() {
    let good = body();
    let mut bad = good.clone();
    bad[0] ^= 1;
    let server = LoopbackServer::start(vec![Canned::bytes(200, bad)]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let err = run(&fetcher(), &[&server.base()], &expect_of(&good), &dest)
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::HashMismatch);
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn a_body_of_the_wrong_size_is_refused_before_it_is_read() {
    let good = body();
    let mut longer = good.clone();
    longer.extend_from_slice(b"extra");
    let server = LoopbackServer::start(vec![Canned::bytes(200, longer.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let err = run(&fetcher(), &[&server.base()], &expect_of(&good), &dest)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        FetchError::SizeMismatch {
            expected: good.len() as u64,
            got: longer.len() as u64
        }
    );
    assert!(!dest.exists());
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn an_interrupted_transfer_resumes_with_a_range_request() {
    let data = body();
    let split = 7000usize;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    std::fs::write(part_path(&dest), &data[..split]).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(206, data[split..].to_vec())
        .with_header(
            "Content-Range",
            &format!("bytes {split}-{}/{}", data.len() - 1, data.len()),
        )]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let req = &server.captured()[0];
    assert_eq!(
        req.header("range"),
        Some(format!("bytes={split}-").as_str())
    );
}

#[tokio::test]
async fn a_wrong_content_range_is_refused_and_the_partial_file_dropped() {
    let data = body();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    std::fs::write(part_path(&dest), &data[..100]).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(206, data[500..].to_vec())
        .with_header("Content-Range", "bytes 500-19999/20000")]);
    let err = run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap_err();
    assert!(matches!(err, FetchError::Network(_)));
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn a_server_that_ignores_range_restarts_from_zero() {
    let data = body();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    std::fs::write(part_path(&dest), &data[..300]).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn a_416_answer_restarts_from_zero_once() {
    let data = body();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    std::fs::write(part_path(&dest), &data[..300]).unwrap();
    let server = LoopbackServer::start(vec![
        Canned::bytes(416, Vec::new()),
        Canned::bytes(200, data.clone()),
    ]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(server.connections(), 2);
}

#[tokio::test]
async fn a_partial_file_longer_than_expected_is_discarded() {
    let data = body();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let mut too_long = data.clone();
    too_long.extend_from_slice(b"junk");
    std::fs::write(part_path(&dest), &too_long).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn a_complete_partial_file_is_verified_without_any_request() {
    let data = body();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    std::fs::write(part_path(&dest), &data).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(500, Vec::new())]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn a_dropped_connection_keeps_the_partial_file_for_the_next_attempt() {
    let data = body();
    let cut = 3000usize;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let payload = data.clone();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&payload[..cut]);
            // Closes with the body incomplete.
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let err = run(
        &fetcher(),
        &[&format!("http://127.0.0.1:{port}/x")],
        &expect_of(&data),
        &dest,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, FetchError::Network(_)), "got {err:?}");
    assert!(!dest.exists());
    assert_eq!(
        std::fs::metadata(part_path(&dest)).unwrap().len(),
        cut as u64
    );

    // The next attempt resumes where it stopped.
    let server = LoopbackServer::start(vec![Canned::bytes(206, data[cut..].to_vec()).with_header(
        "Content-Range",
        &format!("bytes {cut}-{}/{}", data.len() - 1, data.len()),
    )]);
    run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[tokio::test]
async fn an_http_error_status_is_reported() {
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(404, Vec::new())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let err = run(&fetcher(), &[&server.base()], &expect_of(&data), &dest)
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::Status(404));
}

#[tokio::test]
async fn no_sources_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = run(
        &fetcher(),
        &[],
        &expect_of(b"x"),
        &dir.path().join("asset.bin"),
    )
    .await
    .unwrap_err();
    assert_eq!(err, FetchError::NoSources);
}

#[tokio::test]
async fn a_cancelled_fetch_stops_and_keeps_nothing_it_did_not_write() {
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let token = CancellationToken::new();
    token.cancel();
    let err = fetcher()
        .fetch_to_file(
            &[&server.base()],
            &expect_of(&data),
            &dest,
            &token,
            &mut |_, _| {},
        )
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::Cancelled);
    assert!(!dest.exists());
}

// -- redirects ---------------------------------------------------------------

#[tokio::test]
async fn a_redirect_to_an_allowed_host_is_followed() {
    let data = body();
    let target = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let origin =
        LoopbackServer::start(vec![Canned::bytes(302, Vec::new())
            .with_header("Location", &format!("{}/real", target.base()))]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    run(&fetcher(), &[&origin.base()], &expect_of(&data), &dest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(target.captured()[0].path, "/real");
}

#[tokio::test]
async fn a_redirect_to_a_host_off_the_list_is_refused_without_connecting() {
    let data = body();
    let elsewhere = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    // `localhost` is the same machine but not the listed host name.
    let off_list = elsewhere.base().replace("127.0.0.1", "localhost");
    let origin = LoopbackServer::start(vec![
        Canned::bytes(302, Vec::new()).with_header("Location", &off_list)
    ]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("asset.bin");
    let err = run(&fetcher(), &[&origin.base()], &expect_of(&data), &dest)
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::RedirectRefused);
    assert_eq!(elsewhere.connections(), 0);
}

#[tokio::test]
async fn the_production_policy_refuses_a_redirect_to_plain_http() {
    // Same list, but the production policy: http is never an allowed scheme.
    let policy_client = PinnedFetcher {
        client: Client::builder()
            .no_proxy()
            .redirect(redirect_policy(vec!["127.0.0.1".to_owned()], false))
            .build()
            .unwrap(),
    };
    let target = LoopbackServer::start(vec![Canned::bytes(200, b"x".to_vec())]);
    let origin = LoopbackServer::start(vec![
        Canned::bytes(302, Vec::new()).with_header("Location", &format!("{}/p", target.base()))
    ]);
    let err = policy_client
        .fetch_small(&origin.base(), 1024)
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::RedirectRefused);
    assert_eq!(target.connections(), 0);
}

#[tokio::test]
async fn a_redirect_loop_stops_after_the_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/again\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(head.as_bytes());
        }
    });
    let err = fetcher()
        .fetch_small(&format!("http://127.0.0.1:{port}/start"), 1024)
        .await
        .unwrap_err();
    assert_eq!(err, FetchError::RedirectRefused);
    // The first request plus MAX_REDIRECTS followed hops.
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        MAX_REDIRECTS + 1
    );
}

// -- small fetch -------------------------------------------------------------

#[tokio::test]
async fn fetch_small_returns_the_body_within_the_cap() {
    let server = LoopbackServer::start(vec![Canned::bytes(200, b"manifest".to_vec())]);
    let got = fetcher().fetch_small(&server.base(), 1024).await.unwrap();
    assert_eq!(got, b"manifest");
}

#[tokio::test]
async fn fetch_small_refuses_a_body_over_the_cap_and_a_bad_status() {
    let server = LoopbackServer::start(vec![Canned::bytes(200, vec![b'a'; 2048])]);
    assert_eq!(
        fetcher()
            .fetch_small(&server.base(), 1024)
            .await
            .unwrap_err(),
        FetchError::TooLarge(1024)
    );
    let missing = LoopbackServer::start(vec![Canned::bytes(404, Vec::new())]);
    assert_eq!(
        fetcher()
            .fetch_small(&missing.base(), 1024)
            .await
            .unwrap_err(),
        FetchError::Status(404)
    );
}

#[test]
fn the_production_constructor_builds() {
    let _ = PinnedFetcher::new(GITHUB_RELEASE_REDIRECT_HOSTS);
}
