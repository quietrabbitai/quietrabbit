// src-tauri/src/privacy_model/tests.rs
//
// Loopback only: no real host is contacted, and no test writes more than a few
// kilobytes. The pinned 1.6 GB file is never fetched in tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use super::install::*;
use super::marker::*;
use super::paths::*;
use super::*;
use crate::pinned_asset::{part_path, sha256_of, PinnedFetcher};
use crate::providers::loopback_test_server::{Canned, LoopbackServer};
use crate::task_progress::TaskProgressPayload;

fn body() -> Vec<u8> {
    (0..5000u32).flat_map(|i| i.to_le_bytes()).collect()
}

fn expect_of(bytes: &[u8]) -> Expect {
    Expect {
        size: bytes.len() as u64,
        sha256: sha256_of(bytes),
    }
}

struct Probes {
    activations: Arc<AtomicUsize>,
    hashes: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<TaskProgressPayload>>>,
}

impl Probes {
    fn events(&self) -> Vec<TaskProgressPayload> {
        self.events.lock().unwrap().clone()
    }
    fn phases(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.phase).collect()
    }
}

/// An environment under `dir/models` with unlimited free space, no margin, an
/// activation that succeeds, and probes on every collaborator.
fn rig(dir: &Path, sources: &[String], data: &[u8]) -> (Env, Probes) {
    let activations = Arc::new(AtomicUsize::new(0));
    let hashes = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let (a, h, e) = (activations.clone(), hashes.clone(), events.clone());
    let env = Env {
        models_dir: dir.join("models"),
        sources: sources.to_vec(),
        expect: expect_of(data),
        disk_margin: 0,
        fetcher: PinnedFetcher::for_loopback_tests(&["127.0.0.1"]),
        available_space: Arc::new(|_| Some(u64::MAX)),
        hash: Arc::new(move |p| {
            h.fetch_add(1, Ordering::SeqCst);
            hash_file(p)
        }),
        activate: Arc::new(move || {
            a.fetch_add(1, Ordering::SeqCst);
            true
        }),
        sink: Some(Arc::new(move |p: &TaskProgressPayload| {
            e.lock().unwrap().push(p.clone());
        })),
    };
    (
        env,
        Probes {
            activations,
            hashes,
            events,
        },
    )
}

fn model_in(dir: &Path) -> PathBuf {
    dir.join("models").join(pin::FILE_NAME)
}

fn put_model(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = model_in(dir);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn set_mtime(path: &Path, to: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(to)
        .unwrap();
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

#[test]
fn the_models_paths_hang_off_the_root() {
    let root = Path::new("/data/root");
    assert_eq!(models_dir_in(root), PathBuf::from("/data/root/models"));
    assert_eq!(
        model_path_in(root),
        PathBuf::from("/data/root/models/privacy-filter-q8.gguf")
    );
    assert_eq!(
        marker_path_for(&model_path_in(root)),
        PathBuf::from("/data/root/models/privacy-filter-q8.gguf.verified")
    );
}

#[test]
fn a_debug_build_prefers_the_override_then_the_root_then_the_legacy_path() {
    let root = Path::new("/r");
    let home = Path::new("/h");
    let loc = resolve(Some(root), Some("/dev/model.gguf"), Some(home), true).unwrap();
    assert_eq!(loc.kind, LocationKind::DevOverride);
    assert_eq!(loc.path, PathBuf::from("/dev/model.gguf"));

    let loc = resolve(Some(root), None, Some(home), true).unwrap();
    assert_eq!(loc.kind, LocationKind::Installed);
    assert_eq!(loc.path, model_path_in(root));

    let loc = resolve(None, None, Some(home), true).unwrap();
    assert_eq!(loc.kind, LocationKind::DevLegacy);
    assert_eq!(
        loc.path,
        PathBuf::from("/h/.local/share/quietrabbit/models/privacy-filter-q8.gguf")
    );

    assert_eq!(resolve(None, None, None, true), None);
}

#[test]
fn an_empty_override_counts_as_unset() {
    let loc = resolve(Some(Path::new("/r")), Some(""), None, true).unwrap();
    assert_eq!(loc.kind, LocationKind::Installed);
}

#[test]
fn release_resolution_ignores_the_override_and_the_legacy_path() {
    let root = Path::new("/r");
    // With a root: the installed location, whatever the environment says.
    let loc = resolve(
        Some(root),
        Some("/attacker/model.gguf"),
        Some(Path::new("/h")),
        false,
    )
    .unwrap();
    assert_eq!(loc.kind, LocationKind::Installed);
    assert_eq!(loc.path, model_path_in(root));
    // Without a root: nothing, not the home path and not the override.
    assert_eq!(
        resolve(
            None,
            Some("/attacker/model.gguf"),
            Some(Path::new("/h")),
            false
        ),
        None
    );
}

#[test]
fn release_the_loader_gets_no_path_for_an_unverified_installed_file() {
    // The same pure function the process uses, with release inputs.
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    put_model(dir.path(), &data);
    let loc = resolve(Some(dir.path()), Some("/x"), None, false);
    assert_eq!(loadable(loc, &expect_of(&data)), None, "no marker, no load");
}

// ---------------------------------------------------------------------------
// marker
// ---------------------------------------------------------------------------

#[test]
fn a_recorded_model_checks_valid_and_a_changed_one_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);

    assert_eq!(
        check(&model, &marker_path, &expect),
        Check::Stale,
        "no marker yet"
    );
    record(&model, &marker_path, &expect).unwrap();
    assert_eq!(check(&model, &marker_path, &expect), Check::Valid);

    // A different modification time invalidates it.
    set_mtime(&model, SystemTime::now() + Duration::from_secs(3600));
    assert_eq!(check(&model, &marker_path, &expect), Check::Stale);

    // A different size is not the model at all.
    std::fs::write(&model, b"short").unwrap();
    assert_eq!(check(&model, &marker_path, &expect), Check::WrongSize);

    std::fs::remove_file(&model).unwrap();
    assert_eq!(check(&model, &marker_path, &expect), Check::Missing);
}

#[test]
fn a_marker_for_another_hash_size_or_version_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);
    record(&model, &marker_path, &expect).unwrap();
    let good = read_marker(&marker_path).unwrap();

    for bad in [
        Marker {
            sha256: "0".repeat(64),
            ..good.clone()
        },
        Marker {
            size: good.size + 1,
            ..good.clone()
        },
        Marker {
            version: 2,
            ..good.clone()
        },
    ] {
        write_marker(&marker_path, &bad).unwrap();
        assert_eq!(
            check(&model, &marker_path, &expect),
            Check::Stale,
            "{bad:?}"
        );
    }
}

#[test]
fn garbage_missing_and_oversized_markers_are_stale_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);

    assert_eq!(read_marker(&marker_path), None);
    for junk in [&b"not json"[..], b"{}", b"[1,2,3]", b"{\"version\":1}"] {
        std::fs::write(&marker_path, junk).unwrap();
        assert_eq!(read_marker(&marker_path), None);
        assert_eq!(check(&model, &marker_path, &expect), Check::Stale);
    }
    std::fs::write(&marker_path, vec![b' '; 10_000]).unwrap();
    assert_eq!(read_marker(&marker_path), None);
}

#[test]
fn writing_a_marker_leaves_no_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);
    record(&model, &marker_path, &expect_of(&data)).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path().join("models"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.iter().any(|n| n.ends_with(".tmp")),
        "left behind: {names:?}"
    );
}

#[test]
fn verify_trusts_a_valid_marker_without_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);
    record(&model, &marker_path, &expect).unwrap();
    let calls = AtomicUsize::new(0);
    let out = verify_existing(&model, &marker_path, &expect, &|p| {
        calls.fetch_add(1, Ordering::SeqCst);
        hash_file(p)
    })
    .unwrap();
    assert_eq!(out, Verified::Verified);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn verify_hashes_once_records_a_marker_and_then_trusts_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let marker_path = marker_path_for(&model);
    let calls = AtomicUsize::new(0);
    let hash = |p: &Path| {
        calls.fetch_add(1, Ordering::SeqCst);
        hash_file(p)
    };
    assert_eq!(
        verify_existing(&model, &marker_path, &expect, &hash).unwrap(),
        Verified::Verified
    );
    assert_eq!(
        verify_existing(&model, &marker_path, &expect, &hash).unwrap(),
        Verified::Verified
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second call used the marker"
    );
}

#[test]
fn verify_deletes_a_file_with_the_right_size_and_the_wrong_hash() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let mut wrong = data.clone();
    wrong[7] ^= 0xff;
    let model = put_model(dir.path(), &wrong);
    let marker_path = marker_path_for(&model);
    // A stale marker from a former file must go too.
    std::fs::write(&marker_path, b"{}").unwrap();
    let out = verify_existing(&model, &marker_path, &expect_of(&data), &hash_file).unwrap();
    assert_eq!(out, Verified::Rejected);
    assert!(!model.exists() && !marker_path.exists());
}

#[test]
fn verify_deletes_a_file_of_the_wrong_size_without_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let model = put_model(dir.path(), b"nope");
    let marker_path = marker_path_for(&model);
    let calls = AtomicUsize::new(0);
    let out = verify_existing(&model, &marker_path, &expect_of(&body()), &|p| {
        calls.fetch_add(1, Ordering::SeqCst);
        hash_file(p)
    })
    .unwrap();
    assert_eq!(out, Verified::Rejected);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!model.exists());
}

#[test]
fn verify_reports_a_missing_file_and_refuses_one_that_changes_while_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = model_in(dir.path());
    let marker_path = marker_path_for(&model);
    assert_eq!(
        verify_existing(&model, &marker_path, &expect, &hash_file).unwrap(),
        Verified::Missing
    );

    put_model(dir.path(), &data);
    let err = verify_existing(&model, &marker_path, &expect, &|p| {
        let digest = hash_file(p)?;
        set_mtime(p, SystemTime::now() + Duration::from_secs(7200));
        Ok(digest)
    })
    .unwrap_err();
    assert!(err.to_string().contains("changed"), "{err}");
    assert!(model.exists(), "a file that merely changed is not deleted");
    assert!(!marker_path.exists());
}

#[test]
fn a_symlinked_model_is_removed_as_a_link_not_through_it() {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.gguf");
        let mut wrong = body();
        wrong[0] ^= 1;
        std::fs::write(&target, &wrong).unwrap();
        let model = model_in(dir.path());
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &model).unwrap();
        let marker_path = marker_path_for(&model);
        let out = verify_existing(&model, &marker_path, &expect_of(&body()), &hash_file).unwrap();
        assert_eq!(out, Verified::Rejected);
        assert!(!model.exists());
        assert!(target.exists(), "the link's target is not QR's to delete");
    }
}

#[test]
fn the_loadable_path_follows_the_marker_for_the_installed_kind_only() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let expect = expect_of(&data);
    let model = put_model(dir.path(), &data);
    let installed = ModelLocation {
        path: model.clone(),
        kind: LocationKind::Installed,
    };
    assert_eq!(loadable(Some(installed.clone()), &expect), None);
    record(&model, &marker_path_for(&model), &expect).unwrap();
    assert_eq!(loadable(Some(installed), &expect), Some(model.clone()));

    for kind in [LocationKind::DevOverride, LocationKind::DevLegacy] {
        let dev = ModelLocation {
            path: PathBuf::from("/does/not/need/a/marker.gguf"),
            kind,
        };
        assert_eq!(loadable(Some(dev.clone()), &expect), Some(dev.path));
    }
    assert_eq!(loadable(None, &expect), None);
}

// ---------------------------------------------------------------------------
// the installer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_clean_install_downloads_records_the_marker_and_activates() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);

    let model = model_in(dir.path());
    assert_eq!(std::fs::read(&model).unwrap(), data);
    assert!(!part_path(&model).exists());
    assert_eq!(
        check(&model, &marker_path_for(&model), &expect_of(&data)),
        Check::Valid
    );
    assert_eq!(probes.activations.load(Ordering::SeqCst), 1);
    assert_eq!(
        probes.hashes.load(Ordering::SeqCst),
        0,
        "nothing existed to hash"
    );
    assert_eq!(installer.state(), InstallState::Installed);

    let events = probes.events();
    assert!(events.iter().all(|e| e.kind == "privacy_model"));
    assert!(events.windows(2).all(|w| w[0].task_id == w[1].task_id));
    let first = &events[0];
    assert_eq!(first.phase, "downloading");
    assert_eq!(
        (first.current, first.total),
        (Some(0), Some(data.len() as u64))
    );
    assert_eq!(events.last().unwrap().phase, "complete");
}

#[tokio::test]
async fn a_sideloaded_model_is_accepted_without_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    put_model(dir.path(), &data);
    let server = LoopbackServer::start(vec![Canned::bytes(500, Vec::new())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);

    assert_eq!(server.connections(), 0, "offline use: no network");
    assert_eq!(probes.hashes.load(Ordering::SeqCst), 1);
    assert_eq!(probes.activations.load(Ordering::SeqCst), 1);
    let model = model_in(dir.path());
    assert_eq!(
        check(&model, &marker_path_for(&model), &expect_of(&data)),
        Check::Valid
    );
    assert_eq!(
        probes.phases().first().map(String::as_str),
        Some("verifying")
    );
}

#[tokio::test]
async fn a_valid_marker_skips_the_hash_and_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let model = put_model(dir.path(), &data);
    record(&model, &marker_path_for(&model), &expect_of(&data)).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(500, Vec::new())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);

    assert_eq!(server.connections(), 0);
    assert_eq!(probes.hashes.load(Ordering::SeqCst), 0);
    assert_eq!(probes.activations.load(Ordering::SeqCst), 1);
    assert!(
        !probes.phases().contains(&"verifying".to_owned()),
        "a quiet re-check shows no verifying state"
    );
}

#[tokio::test]
async fn a_wrong_file_of_the_right_size_is_deleted_and_the_right_one_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let mut wrong = data.clone();
    wrong[100] ^= 0xff;
    put_model(dir.path(), &wrong);
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);

    assert_eq!(std::fs::read(model_in(dir.path())).unwrap(), data);
    assert_eq!(server.connections(), 1);
    assert_eq!(probes.hashes.load(Ordering::SeqCst), 1);
    let phases = probes.phases();
    assert_eq!(phases.first().map(String::as_str), Some("verifying"));
    assert!(phases.contains(&"downloading".to_owned()));
}

#[tokio::test]
async fn a_file_of_the_wrong_size_is_deleted_without_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    put_model(dir.path(), b"half a model");
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);
    assert_eq!(probes.hashes.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(model_in(dir.path())).unwrap(), data);
}

#[tokio::test]
async fn bytes_that_do_not_match_on_every_source_leave_nothing_and_do_not_activate() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let mut bad = data.clone();
    bad[5] ^= 0xff;
    let a = LoopbackServer::start(vec![Canned::bytes(200, bad.clone())]);
    let b = LoopbackServer::start(vec![Canned::bytes(200, bad)]);
    let (env, probes) = rig(dir.path(), &[a.base(), b.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(
        installer.run().await,
        Outcome::Failed(FailReason::VerificationFailed)
    );

    let model = model_in(dir.path());
    assert!(!model.exists() && !part_path(&model).exists());
    assert!(!marker_path_for(&model).exists());
    assert_eq!(probes.activations.load(Ordering::SeqCst), 0);
    assert_eq!(
        installer.state(),
        InstallState::Failed(FailReason::VerificationFailed)
    );
    let last = probes.events().pop().unwrap();
    assert_eq!(last.phase, "failed");
    assert_eq!(last.message.as_deref(), Some("verification_failed"));
}

#[tokio::test]
async fn the_first_source_is_tried_first_and_a_miss_falls_through_to_the_second() {
    let data = body();

    let dir = tempfile::tempdir().unwrap();
    let missing = LoopbackServer::start(vec![Canned::bytes(404, Vec::new())]);
    let good = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, _) = rig(dir.path(), &[missing.base(), good.base()], &data);
    assert_eq!(Installer::new(env).run().await, Outcome::Installed);
    assert_eq!((missing.connections(), good.connections()), (1, 1));

    let dir = tempfile::tempdir().unwrap();
    let first = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let second = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, _) = rig(dir.path(), &[first.base(), second.base()], &data);
    assert_eq!(Installer::new(env).run().await, Outcome::Installed);
    assert_eq!(
        (first.connections(), second.connections()),
        (1, 0),
        "the second source is not touched when the first delivers"
    );
}

#[tokio::test]
async fn a_partial_file_is_resumed_with_a_range_request_and_progress_starts_there() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let split = 7000usize;
    let model = model_in(dir.path());
    std::fs::create_dir_all(model.parent().unwrap()).unwrap();
    std::fs::write(part_path(&model), &data[..split]).unwrap();
    let server = LoopbackServer::start(vec![Canned::bytes(206, data[split..].to_vec())
        .with_header(
            "Content-Range",
            &format!("bytes {split}-{}/{}", data.len() - 1, data.len()),
        )]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Installed);

    assert_eq!(std::fs::read(&model).unwrap(), data);
    assert_eq!(
        server.captured()[0].header("range"),
        Some(format!("bytes={split}-").as_str())
    );
    let first = &probes.events()[0];
    assert_eq!(first.phase, "downloading");
    assert_eq!(
        first.current,
        Some(split as u64),
        "a resume reports its true start"
    );
}

#[tokio::test]
async fn a_cut_transfer_keeps_the_partial_file_and_the_next_run_resumes_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let cut = 4000usize;
    let first_server = LoopbackServer::start(vec![
        Canned::bytes(200, data[..cut].to_vec()).with_declared_length(data.len())
    ]);
    let (env, _) = rig(dir.path(), &[first_server.base()], &data);
    let installer = Installer::new(env);
    assert_eq!(
        installer.run().await,
        Outcome::Failed(FailReason::DownloadFailed)
    );
    let model = model_in(dir.path());
    let kept = std::fs::metadata(part_path(&model)).unwrap().len() as usize;
    assert!(kept > 0 && kept <= cut, "kept {kept} bytes");
    assert!(!model.exists());

    let second_server = LoopbackServer::start(vec![Canned::bytes(206, data[kept..].to_vec())
        .with_header(
            "Content-Range",
            &format!("bytes {kept}-{}/{}", data.len() - 1, data.len()),
        )]);
    let (env, _) = rig(dir.path(), &[second_server.base()], &data);
    let installer = Installer::new(env);
    assert_eq!(installer.run().await, Outcome::Installed);
    assert_eq!(std::fs::read(&model).unwrap(), data);
    assert_eq!(
        second_server.captured()[0].header("range"),
        Some(format!("bytes={kept}-").as_str())
    );
}

#[tokio::test]
async fn too_little_free_space_refuses_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (mut env, probes) = rig(dir.path(), &[server.base()], &data);
    env.available_space = Arc::new(|_| Some(100));
    let installer = Installer::new(env);

    assert_eq!(installer.run().await, Outcome::Failed(FailReason::LowDisk));

    assert_eq!(server.connections(), 0);
    assert!(!model_in(dir.path()).exists());
    assert_eq!(probes.activations.load(Ordering::SeqCst), 0);
    assert_eq!(
        probes.events().pop().unwrap().message.as_deref(),
        Some("low_disk")
    );
}

#[tokio::test]
async fn the_space_check_counts_what_a_partial_file_already_holds_plus_the_margin() {
    let data = body();
    let have = 7000usize;
    let margin = 100u64;
    let needed = (data.len() - have) as u64 + margin;

    for (free, expect_ok) in [(needed, true), (needed - 1, false)] {
        let dir = tempfile::tempdir().unwrap();
        let model = model_in(dir.path());
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::fs::write(part_path(&model), &data[..have]).unwrap();
        let server = LoopbackServer::start(vec![Canned::bytes(206, data[have..].to_vec())
            .with_header(
                "Content-Range",
                &format!("bytes {have}-{}/{}", data.len() - 1, data.len()),
            )]);
        let (mut env, _) = rig(dir.path(), &[server.base()], &data);
        env.disk_margin = margin;
        env.available_space = Arc::new(move |_| Some(free));
        let outcome = Installer::new(env).run().await;
        if expect_ok {
            assert_eq!(outcome, Outcome::Installed, "free = {free}");
        } else {
            assert_eq!(
                outcome,
                Outcome::Failed(FailReason::LowDisk),
                "free = {free}"
            );
            assert_eq!(server.connections(), 0);
            assert_eq!(
                std::fs::metadata(part_path(&model)).unwrap().len() as usize,
                have,
                "the partial file is left alone"
            );
        }
    }
}

#[tokio::test]
async fn unknown_free_space_does_not_block_the_download() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (mut env, _) = rig(dir.path(), &[server.base()], &data);
    env.available_space = Arc::new(|_| None);
    assert_eq!(Installer::new(env).run().await, Outcome::Installed);
}

#[tokio::test]
async fn a_filter_that_will_not_load_is_a_failure_not_an_install() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (mut env, _) = rig(dir.path(), &[server.base()], &data);
    env.activate = Arc::new(|| false);
    let installer = Installer::new(env);
    assert_eq!(
        installer.run().await,
        Outcome::Failed(FailReason::ModelUnavailable)
    );
    // The file is verified and stays: a later run only has to load it.
    let model = model_in(dir.path());
    assert_eq!(
        check(&model, &marker_path_for(&model), &expect_of(&data)),
        Check::Valid
    );
}

#[tokio::test]
async fn a_cancelled_run_stops_without_failing_and_keeps_what_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![Canned::bytes(200, data.clone())]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);
    installer.cancel();

    assert_eq!(installer.run().await, Outcome::Cancelled);

    assert_eq!(installer.state(), InstallState::Idle);
    assert_eq!(probes.activations.load(Ordering::SeqCst), 0);
    assert_eq!(probes.events().pop().unwrap().phase, "cancelled");
}

#[tokio::test]
async fn a_failed_run_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![
        Canned::bytes(404, Vec::new()),
        Canned::bytes(200, data.clone()),
    ]);
    let (env, probes) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    assert_eq!(
        installer.run().await,
        Outcome::Failed(FailReason::DownloadFailed)
    );
    assert_eq!(
        installer.state(),
        InstallState::Failed(FailReason::DownloadFailed)
    );
    assert_eq!(installer.run().await, Outcome::Installed);
    assert_eq!(probes.activations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn only_one_run_is_alive_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![
        Canned::bytes(200, data.clone()).with_delay(Duration::from_millis(600))
    ]);
    let (env, _) = rig(dir.path(), &[server.base()], &data);
    let installer = Arc::new(Installer::new(env));

    assert!(installer.start(), "the first call starts a run");
    assert!(installer.state().is_running());
    assert!(!installer.start(), "a second call is a no-op");
    assert_eq!(installer.run().await, Outcome::AlreadyRunning);

    // Let it finish, then it may be started again.
    for _ in 0..200 {
        if !installer.state().is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(installer.state(), InstallState::Installed);
    assert_eq!(server.connections(), 1, "one transfer, not two");
}

#[tokio::test]
async fn a_run_that_is_dropped_midway_does_not_hold_the_single_flight() {
    let dir = tempfile::tempdir().unwrap();
    let data = body();
    let server = LoopbackServer::start(vec![
        Canned::bytes(200, data.clone()).with_delay(Duration::from_secs(5))
    ]);
    let (env, _) = rig(dir.path(), &[server.base()], &data);
    let installer = Installer::new(env);

    let timed_out = tokio::time::timeout(Duration::from_millis(300), installer.run()).await;
    assert!(
        timed_out.is_err(),
        "the run was still waiting on the server"
    );

    assert_eq!(
        installer.state(),
        InstallState::Failed(FailReason::DownloadFailed)
    );
    assert!(!installer.state().is_running());
}

#[test]
fn fetch_errors_map_to_the_reason_the_user_can_act_on() {
    use crate::pinned_asset::FetchError;
    let plenty = Some(10u64 * 1024 * 1024 * 1024);
    let none_left = Some(0u64);
    assert_eq!(
        classify_fetch_error(&FetchError::HashMismatch, plenty),
        FailReason::VerificationFailed
    );
    assert_eq!(
        classify_fetch_error(
            &FetchError::SizeMismatch {
                expected: 1,
                got: 2
            },
            plenty
        ),
        FailReason::VerificationFailed
    );
    assert_eq!(
        classify_fetch_error(&FetchError::Io("write failed".into()), none_left),
        FailReason::LowDisk
    );
    assert_eq!(
        classify_fetch_error(&FetchError::Io("write failed".into()), plenty),
        FailReason::DownloadFailed
    );
    assert_eq!(
        classify_fetch_error(&FetchError::Io("write failed".into()), None),
        FailReason::DownloadFailed
    );
    for e in [
        FetchError::Network("down".into()),
        FetchError::Status(404),
        FetchError::RedirectRefused,
        FetchError::TooLarge(1),
        FetchError::NoSources,
    ] {
        assert_eq!(
            classify_fetch_error(&e, none_left),
            FailReason::DownloadFailed
        );
    }
}

#[test]
fn the_failure_codes_are_stable_strings() {
    assert_eq!(FailReason::DownloadFailed.code(), "download_failed");
    assert_eq!(FailReason::LowDisk.code(), "low_disk");
    assert_eq!(FailReason::VerificationFailed.code(), "verification_failed");
    assert_eq!(FailReason::ModelUnavailable.code(), "model_unavailable");
}

#[test]
fn the_production_environment_uses_the_pin_and_both_sources_in_order() {
    let root = tempfile::tempdir().unwrap();
    let env = Env::production(root.path(), Arc::new(|| true), None);
    assert_eq!(env.models_dir, root.path().join("models"));
    assert_eq!(
        env.sources,
        vec![pin::release_asset_url(), pin::upstream_url()]
    );
    assert_eq!(env.expect, pin_expect());
    assert_eq!(env.expect.size, pin::SIZE);
    assert_eq!(env.disk_margin, pin::DISK_MARGIN);
    assert!(env.sources[0].contains("github.com") && env.sources[1].contains("huggingface.co"));
}

// ---------------------------------------------------------------------------
// no account, no key
// ---------------------------------------------------------------------------

#[test]
fn the_setup_task_never_touches_the_session_key_or_the_writer_gate() {
    // The task is shared by every account and must keep running through a
    // lock, a logout or a second account signing in (decisions.id=864). It
    // cannot do that if it reads the key slot, so no production file in this
    // module names one. (This file is excluded: it holds the words.)
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/privacy_model");
    let forbidden = [
        "KeyRegistry",
        "GroupKeyRegistry",
        "UnlockedKey",
        "data_key",
        "epoch_guard",
        "writer_gate",
        "tauri::State",
    ];
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("tests.rs") {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&path).unwrap();
        for word in forbidden {
            assert!(!text.contains(word), "{path:?} mentions {word}");
        }
    }
    assert!(checked >= 4, "scanned {checked} files");
}
