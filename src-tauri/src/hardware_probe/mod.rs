// src-tauri/src/hardware_probe.rs
//
// items.id=435: hardware-capability-detection module. Produces a
// HardwareProfile matchable against providers.hardware_requirement
// (shared_013.sql, provider_store.rs -- nullable JSON, "min RAM/VRAM
// class, expected tokens/sec on a reference hardware class"), feeding
// PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md Part 4a's onboarding
// recommendation step. Part 4a's own matching/decision logic, the install
// -trigger mechanism (items.id=436), and onboarding UI (items.id=437) are
// all separately scoped and NOT built here -- this module only produces
// the detected facts.
//
// SCOPE NARROWING vs. hardware_requirement's literal "min RAM/VRAM class,
// expected tokens/sec" description: this module emits RAM class, CPU core
// count, and GPU presence/class only -- no tokens/sec estimate. Considered
// and rejected both a static reference-throughput table (no trustworthy
// data source once llmfit was rejected below; a fabricated number would be
// worse than none) and a runtime micro-benchmark (needs a model already
// installed, entangling this item with items.id=436; and
// providers/evaluation.rs's existing model_hardware_scores/hardware_factor
// mechanism already does observed-latency benchmarking of an
// already-running model -- the closer home for that half of the problem
// once a model is actually installed, not duplicated here).
//
// REJECTED: depending on the `llmfit` crate (crates.io, MIT, AlexsJones/
// llmfit) for this instead of building in-house. Investigated concretely,
// not a reflexive NIH call:
//   - llmfit-core's own Cargo.toml unconditionally depends on `ureq` (HTTP
//     client, no [features] section, nothing optional = true) and ships
//     providers.rs (multi-provider ecosystem discovery -- Ollama, llama.cpp,
//     MLX, LM Studio, Docker Model Runner) and share.rs/bench.rs (community
//     benchmark upload). That's compiled-in networking capability in
//     tension with this project's non-negotiable "no data leaves local
//     without explicit consent" / "no telemetry" tenets, and duplicates
//     this codebase's own Ollama detection (ollama_sidecar.rs, D6-353).
//   - Its GPU/accelerator detection shells out to nvidia-smi/rocm-smi/
//     lspci/vulkaninfo/system_profiler/PowerShell and parses text output --
//     fragile and tool-presence-dependent, a worse fit than wgpu's typed
//     adapter-enumeration API already in this dependency tree.
//   - Trust signal came back questionable: repo ~7 months old (created
//     2026-02-15) with 37,200 GitHub stars against only ~18-23k lifetime
//     crates.io downloads -- a ratio far outside the norm for genuinely-
//     adopted Rust tooling, worth independent scrutiny before trusting it
//     to feed what runs locally vs. gets routed to cloud.
//   - Scope mismatch: llmfit's own job (model fit/recommendation,
//     quantization-format awareness, TUI/web dashboard/REST API) duplicates
//     Part 4a's matching logic, which this item explicitly excludes.
// License (MIT) was fine and is not why it was rejected.
//
// GPU/VRAM (items.id=435 original reasoning, superseded by items.id=606 --
// see GpuInfo/GpuBackend below: VRAM is now measured per-OS from sysfs/NVML
// with the same no-network, no-CLI rule): wgpu's adapter-enumeration API reliably gives GPU presence,
// discrete-vs-integrated, and vendor/name -- it does not reliably expose
// precise VRAM bytes cross-platform through its safe API (limits() gives
// buffer/texture size ceilings, not raw VRAM size). Getting exact VRAM
// bytes would need vendor-specific bindings (NVML, Metal queries, ...) --
// the same footprint expansion just rejected above for llmfit's shelled-
// out CLI approach. GpuVramClass is deliberately coarse as a result.
// Not reusing cloud_chat_gpu_pane: that module hand-wires a single
// external-GLES adapter from GTK's pre-existing OpenGL context
// (wgpu_hal::gles::Adapter::new_external), is Linux-only, and never calls
// request_adapter/enumerate_adapters or reads AdapterInfo -- it's UI-
// compositor plumbing, not a capability-probe pattern. This module opens
// its own independent wgpu::Instance instead.
//
// CACHING: detected once, cached in instance_config (shared_023.sql),
// not re-probed every launch -- deliberately the opposite of Ollama's
// D6-353 live-every-startup check. Restated why: Ollama's check is a
// cheap 2s-timeout HTTP GET against state that can genuinely change
// between launches (is the daemon up right now); hardware enumeration is
// comparatively more work against a fact (this machine's RAM/CPU/GPU)
// that's static across launches on the same install. Re-detection is a
// deliberate, later-triggered action (e.g. a future onboarding
// "re-detect hardware" affordance, items.id=437), not automatic.

use serde::{Deserialize, Serialize};
use specta::Type;
use sysinfo::System;

#[cfg(target_os = "linux")]
mod linux;
mod nvml;
mod support_table;

const CACHE_KEY_PROFILE: &str = "hardware_profile_json";
const CACHE_KEY_DETECTED_AT: &str = "hardware_profile_detected_at";

// RAM class boundaries, in megabytes (not bytes: specta-typescript forbids
// exporting BigInt-style types -- usize/isize/i64/u64/i128/u128 -- across
// the IPC boundary, same reasoning as ReenterStepResponse.resume_from_index
// in commands/execution.rs; u32 megabytes has no realistic overflow risk
// and drops byte-level precision this "class" bucketing never needed
// anyway). A real judgment call, restated in full: nothing today validates
// hardware_requirement's internal JSON shape (it's freeform, unvalidated
// beyond "is it JSON"), so this module is free to define its own
// vocabulary -- but Part 4a's matching code (out of scope here) and
// whoever later hand-curates hardware_requirement values on provider rows
// will need to use the same class names for matching to work at all.
// Thresholds chosen as round, easily-communicated cutoffs (8/16/32 GB)
// rather than derived from any specific model's real requirements --
// adjust here if Part 4a's matching logic needs finer buckets once it
// exists.
const RAM_LOW_MAX_MB: u32 = 8_000;
const RAM_MEDIUM_MAX_MB: u32 = 16_000;
const RAM_HIGH_MAX_MB: u32 = 32_000;

/// Coarse RAM class -- see RAM_*_MAX_MB above for the exact boundaries this
/// is bucketed from `sysinfo::System::total_memory()`'s byte count,
/// converted to whole megabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum RamClass {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl RamClass {
    fn from_mb(total_mb: u32) -> Self {
        if total_mb <= RAM_LOW_MAX_MB {
            RamClass::Low
        } else if total_mb <= RAM_MEDIUM_MAX_MB {
            RamClass::Medium
        } else if total_mb <= RAM_HIGH_MAX_MB {
            RamClass::High
        } else {
            RamClass::VeryHigh
        }
    }
}

/// Bumped whenever the HardwareProfile shape or detection semantics change.
/// A cached profile with a different (or missing) version is re-detected.
/// v1 = pre-items.id=606 shape (gpu_present/gpu_vram_class); v2 = gpus list.
pub const PROFILE_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other,
}

impl GpuVendor {
    fn from_pci(id: u32) -> Self {
        match id {
            0x10de => GpuVendor::Nvidia,
            0x1002 => GpuVendor::Amd,
            0x8086 => GpuVendor::Intel,
            0x106b => GpuVendor::Apple,
            _ => GpuVendor::Other,
        }
    }
}

/// Whether the pinned Ollama can use this GPU. Unusable and Unknown both
/// mean "treat as CPU" to the engine; Unknown is never an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum OllamaUsability {
    Usable,
    Unusable,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct GpuInfo {
    pub vendor: GpuVendor,
    /// Driver/product name when a source provided one.
    pub name: Option<String>,
    /// "gfx1032" (AMD), "sm_86" (NVIDIA), ... None when not determinable.
    pub architecture: Option<String>,
    /// Shares system RAM (APU / unified memory): vram_mb is None for these.
    pub is_integrated: bool,
    /// Dedicated VRAM in decimal megabytes (same unit as ram_mb; specta
    /// forbids u64 across IPC). None when unknown or shared with system RAM.
    pub vram_mb: Option<u32>,
    pub usable_by_ollama: OllamaUsability,
}

/// A wgpu adapter reduced to what backends need: PCI ids for naming and the
/// cross-OS fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterHint {
    pub vendor_id: u32,
    pub device_id: u32,
    pub name: String,
    pub is_integrated: bool,
}

/// Per-OS GPU backend. Linux is implemented (items.id=606); Windows
/// (items.id=608) and macOS (items.id=609) implement this and replace the
/// no-op backend below. Backends return only what they can verify; vendors
/// they miss fall back to a wgpu-derived Unknown entry.
pub trait GpuBackend {
    fn probe(&self, hints: &[AdapterHint]) -> Vec<GpuInfo>;
}

/// Stand-in until 608/609 land: contributes nothing, so the wgpu fallback
/// supplies presence-only entries with usable_by_ollama = Unknown.
#[cfg(not(target_os = "linux"))]
struct NoopBackend;
#[cfg(not(target_os = "linux"))]
impl GpuBackend for NoopBackend {
    fn probe(&self, _hints: &[AdapterHint]) -> Vec<GpuInfo> {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn platform_backend() -> impl GpuBackend {
    linux::LinuxBackend
}
#[cfg(not(target_os = "linux"))]
fn platform_backend() -> impl GpuBackend {
    NoopBackend
}

/// Decimal megabytes, matching ram_mb's unit. None if it overflows u32.
pub(crate) fn bytes_to_mb(bytes: u64) -> Option<u32> {
    u32::try_from(bytes / 1_000_000).ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct HardwareProfile {
    /// See PROFILE_VERSION. Required (no serde default) so an old-shape
    /// cached profile fails to parse and is re-detected.
    pub profile_version: u32,
    pub ram_mb: u32,
    pub ram_class: RamClass,
    // u32, not usize: same specta-typescript BigInt-export restriction as
    // ram_mb above. A core count safely fits u32.
    pub cpu_cores: u32,
    /// Every real GPU found (software rasterizers excluded).
    pub gpus: Vec<GpuInfo>,
    /// The GPU a recommendation should reason about: Usable first, then
    /// dedicated over integrated, then most VRAM. None when no GPU.
    pub primary_gpu: Option<GpuInfo>,
}

/// Picks the primary GPU. See HardwareProfile::primary_gpu.
pub fn select_primary(gpus: &[GpuInfo]) -> Option<GpuInfo> {
    gpus.iter()
        .max_by_key(|g| {
            (
                g.usable_by_ollama == OllamaUsability::Usable,
                !g.is_integrated,
                g.vram_mb.unwrap_or(0),
            )
        })
        .cloned()
}

/// Pure, synchronous RAM/CPU probe -- `sysinfo::System::new_all()` refreshes
/// everything this needs in one call, no separate refresh_* calls required.
fn detect_ram_and_cpu() -> (u32, u32) {
    let sys = System::new_all();
    let ram_bytes = sys.total_memory(); // bytes since sysinfo 0.26.0, not KB
    let ram_mb = (ram_bytes / 1_000_000) as u32;
    let cpu_cores = System::physical_core_count().unwrap_or_else(|| sys.cpus().len()) as u32;
    (ram_mb, cpu_cores)
}

/// wgpu adapter enumeration, real GPUs only (CPU/software-rasterizer and
/// virtual adapters like llvmpipe/WARP are not inference hardware).
async fn enumerate_adapters() -> Vec<AdapterHint> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    instance
        .enumerate_adapters(wgpu::Backends::all())
        .await
        .iter()
        .map(|a| a.get_info())
        .filter(|i| {
            matches!(
                i.device_type,
                wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
            )
        })
        .map(|i| AdapterHint {
            vendor_id: i.vendor,
            device_id: i.device,
            name: i.name,
            is_integrated: i.device_type == wgpu::DeviceType::IntegratedGpu,
        })
        .collect()
}

/// Presence-only entries for adapter vendors the backend produced nothing for.
fn fallback_gpus(hints: &[AdapterHint], found: &[GpuInfo]) -> Vec<GpuInfo> {
    let mut extra: Vec<GpuInfo> = Vec::new();
    for h in hints {
        let vendor = GpuVendor::from_pci(h.vendor_id);
        let covered = found.iter().chain(extra.iter()).any(|g| g.vendor == vendor);
        if covered {
            continue;
        }
        extra.push(GpuInfo {
            vendor,
            name: Some(h.name.clone()),
            architecture: None,
            is_integrated: h.is_integrated,
            vram_mb: None,
            usable_by_ollama: OllamaUsability::Unknown,
        });
    }
    extra
}

async fn detect_gpus() -> Vec<GpuInfo> {
    let hints = enumerate_adapters().await;
    let mut gpus = platform_backend().probe(&hints);
    let extra = fallback_gpus(&hints, &gpus);
    gpus.extend(extra);
    gpus
}

/// Runs a fresh probe unconditionally -- callers wanting the cache-once
/// behavior should use `get_or_detect` instead.
pub async fn detect() -> HardwareProfile {
    let (ram_mb, cpu_cores) = detect_ram_and_cpu();
    let gpus = detect_gpus().await;
    log::info!(
        "hardware_probe: detected {} GPU(s); support table written against Ollama {}",
        gpus.len(),
        support_table::OLLAMA_SUPPORT_TABLE_VERSION
    );

    HardwareProfile {
        profile_version: PROFILE_VERSION,
        ram_mb,
        ram_class: RamClass::from_mb(ram_mb),
        cpu_cores,
        primary_gpu: select_primary(&gpus),
        gpus,
    }
}

/// Reads the cached profile from instance_config (shared.db), or detects
/// and caches a fresh one on a cache miss. Matches nightly_batch.rs's own
/// inline sqlx::query() idiom against instance_config -- no dedicated store
/// module, following that table's established "config as data" convention.
pub async fn get_or_detect(pool: &sqlx::SqlitePool) -> HardwareProfile {
    let cached: Option<(String,)> =
        sqlx::query_as("SELECT value FROM instance_config WHERE key = ?")
            .bind(CACHE_KEY_PROFILE)
            .fetch_optional(pool)
            .await
            .unwrap_or(None);

    if let Some((raw,)) = cached {
        if !raw.is_empty() {
            match serde_json::from_str::<HardwareProfile>(&raw) {
                Ok(profile) if profile.profile_version == PROFILE_VERSION => return profile,
                Ok(profile) => log::info!(
                    "hardware_probe: cached profile is version {}, current is {}; re-detecting",
                    profile.profile_version,
                    PROFILE_VERSION
                ),
                Err(e) => log::warn!(
                    "hardware_probe: cached hardware_profile_json failed to parse, \
                     re-detecting: {e}"
                ),
            }
        }
    }

    let profile = detect().await;
    store_profile(pool, &profile).await;
    profile
}

async fn store_profile(pool: &sqlx::SqlitePool, profile: &HardwareProfile) {
    let Ok(serialized) = serde_json::to_string(profile) else {
        log::error!("hardware_probe: failed to serialize HardwareProfile, not caching");
        return;
    };

    if let Err(e) = sqlx::query("UPDATE instance_config SET value = ? WHERE key = ?")
        .bind(&serialized)
        .bind(CACHE_KEY_PROFILE)
        .execute(pool)
        .await
    {
        log::error!("hardware_probe: failed to persist hardware_profile_json: {e}");
        return;
    }

    if let Err(e) = sqlx::query("UPDATE instance_config SET value = ? WHERE key = ?")
        .bind(crate::providers::utils::now())
        .bind(CACHE_KEY_DETECTED_AT)
        .execute(pool)
        .await
    {
        log::error!("hardware_probe: failed to persist hardware_profile_detected_at: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    // ------------------------------------------------------------------
    // Pure classification tests -- no DB, no real hardware.
    // ------------------------------------------------------------------

    #[test]
    fn ram_class_boundaries() {
        assert_eq!(RamClass::from_mb(1), RamClass::Low);
        assert_eq!(RamClass::from_mb(RAM_LOW_MAX_MB), RamClass::Low);
        assert_eq!(RamClass::from_mb(RAM_LOW_MAX_MB + 1), RamClass::Medium);
        assert_eq!(RamClass::from_mb(RAM_MEDIUM_MAX_MB), RamClass::Medium);
        assert_eq!(RamClass::from_mb(RAM_MEDIUM_MAX_MB + 1), RamClass::High);
        assert_eq!(RamClass::from_mb(RAM_HIGH_MAX_MB), RamClass::High);
        assert_eq!(RamClass::from_mb(RAM_HIGH_MAX_MB + 1), RamClass::VeryHigh);
    }

    fn fixture_gpu(vendor: GpuVendor, integrated: bool, vram: Option<u32>) -> GpuInfo {
        GpuInfo {
            vendor,
            name: Some("Test GPU".to_string()),
            architecture: Some("gfx1030".to_string()),
            is_integrated: integrated,
            vram_mb: vram,
            usable_by_ollama: OllamaUsability::Usable,
        }
    }

    fn fixture_profile() -> HardwareProfile {
        let gpus = vec![fixture_gpu(GpuVendor::Amd, false, Some(8_573))];
        HardwareProfile {
            profile_version: PROFILE_VERSION,
            ram_mb: 17_179,
            ram_class: RamClass::High,
            cpu_cores: 8,
            primary_gpu: select_primary(&gpus),
            gpus,
        }
    }

    #[test]
    fn primary_gpu_prefers_usable_then_dedicated_then_vram() {
        let mut unusable_big = fixture_gpu(GpuVendor::Amd, false, Some(16_000));
        unusable_big.usable_by_ollama = OllamaUsability::Unusable;
        let usable_small = fixture_gpu(GpuVendor::Nvidia, false, Some(4_000));
        let picked = select_primary(&[unusable_big.clone(), usable_small.clone()]).unwrap();
        assert_eq!(picked, usable_small);
        assert_eq!(select_primary(&[]), None);
        let integrated = fixture_gpu(GpuVendor::Amd, true, None);
        let dedicated = fixture_gpu(GpuVendor::Amd, false, Some(8_000));
        assert_eq!(
            select_primary(&[integrated, dedicated.clone()]).unwrap(),
            dedicated
        );
    }

    #[test]
    fn fallback_adds_only_uncovered_vendors() {
        let hints = vec![
            AdapterHint {
                vendor_id: 0x1002,
                device_id: 1,
                name: "amd".into(),
                is_integrated: false,
            },
            AdapterHint {
                vendor_id: 0x8086,
                device_id: 2,
                name: "intel".into(),
                is_integrated: true,
            },
        ];
        let found = vec![fixture_gpu(GpuVendor::Amd, false, Some(8_000))];
        let extra = fallback_gpus(&hints, &found);
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].vendor, GpuVendor::Intel);
        assert_eq!(extra[0].usable_by_ollama, OllamaUsability::Unknown);
        assert!(extra[0].is_integrated);
    }

    #[test]
    fn bytes_to_mb_is_decimal_and_bounded() {
        assert_eq!(bytes_to_mb(8_573_157_376), Some(8_573));
        assert_eq!(bytes_to_mb(u64::MAX), None);
    }

    // ------------------------------------------------------------------
    // Integration test -- real shared.db cache round-trip. Follows
    // provider_store.rs's own #[cfg(test)] + connect_options_unencrypted(
    // &db_path_shared()) pattern. GPU enumeration itself isn't
    // meaningfully unit-testable in CI (depends on a real adapter being
    // present) -- this test injects a fixed profile directly rather than
    // calling detect(), so it exercises the cache round-trip only.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn get_or_detect_caches_and_reuses_the_stored_profile() {
        let _lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        let fixture = fixture_profile();
        store_profile(&pool, &fixture).await;

        let reused = get_or_detect(&pool).await;
        assert_eq!(reused.ram_mb, fixture.ram_mb);
        assert_eq!(reused.cpu_cores, fixture.cpu_cores);
        assert_eq!(reused.primary_gpu, fixture.primary_gpu);

        let detected_at: (String,) = sqlx::query_as(
            "SELECT value FROM instance_config WHERE key = 'hardware_profile_detected_at'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !detected_at.0.is_empty(),
            "store_profile must persist hardware_profile_detected_at"
        );

        match &saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    #[tokio::test]
    async fn get_or_detect_falls_back_to_a_fresh_detect_on_empty_cache() {
        let _lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        // Freshly migrated: hardware_profile_json seeds to '' (never
        // detected). This must fall through to a real detect() rather than
        // panicking on empty-string JSON parse -- runs against this
        // machine's real hardware, so only RAM/CPU (always present) are
        // asserted, not any specific GPU outcome.
        let profile = get_or_detect(&pool).await;
        assert!(profile.ram_mb > 0);
        assert!(profile.cpu_cores > 0);

        match &saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    async fn pool_for_cache_test() -> sqlx::SqlitePool {
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool must connect")
    }

    async fn put_raw_cache(pool: &sqlx::SqlitePool, raw: &str) {
        sqlx::query("UPDATE instance_config SET value = ? WHERE key = ?")
            .bind(raw)
            .bind(CACHE_KEY_PROFILE)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn old_shape_cached_profile_is_redetected() {
        let _lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        let pool = pool_for_cache_test().await;

        // The exact v1 (pre-items.id=606) serialized shape.
        put_raw_cache(
            &pool,
            r#"{"ram_mb":17179,"ram_class":"high","cpu_cores":8,"gpu_present":true,
                "gpu_is_discrete":true,"gpu_name":"Old GPU",
                "gpu_vram_class":"discrete_unknown_size"}"#,
        )
        .await;

        let profile = get_or_detect(&pool).await;
        assert_eq!(profile.profile_version, PROFILE_VERSION);
        assert_ne!(profile.ram_mb, 17_179, "must be a fresh detection");

        let stored: (String,) = sqlx::query_as("SELECT value FROM instance_config WHERE key = ?")
            .bind(CACHE_KEY_PROFILE)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            stored.0.contains("profile_version"),
            "cache must be rewritten"
        );

        match &saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    #[tokio::test]
    async fn wrong_version_cached_profile_is_redetected() {
        let _lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        let pool = pool_for_cache_test().await;

        let mut stale = fixture_profile();
        stale.profile_version = PROFILE_VERSION + 1;
        put_raw_cache(&pool, &serde_json::to_string(&stale).unwrap()).await;

        let profile = get_or_detect(&pool).await;
        assert_eq!(profile.profile_version, PROFILE_VERSION);
        assert_ne!(profile.ram_mb, stale.ram_mb);

        match &saved_root {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }
}
