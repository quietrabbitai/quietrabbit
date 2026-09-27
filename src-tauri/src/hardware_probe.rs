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
// GPU/VRAM: wgpu's adapter-enumeration API reliably gives GPU presence,
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

/// Coarse GPU/VRAM class. Deliberately does not carry a byte count -- see
/// this module's header comment on why precise cross-platform VRAM size
/// isn't attempted here. `DiscreteUnknownSize` means "a discrete GPU was
/// found, but this module cannot size its VRAM with current in-tree
/// tooling" -- not an error, a known and documented gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum GpuVramClass {
    Integrated,
    DiscreteUnknownSize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct HardwareProfile {
    pub ram_mb: u32,
    pub ram_class: RamClass,
    // u32, not usize: same specta-typescript BigInt-export restriction as
    // ram_mb above. A core count safely fits u32.
    pub cpu_cores: u32,
    pub gpu_present: bool,
    pub gpu_is_discrete: bool,
    /// The GPU's driver-reported name (e.g. "NVIDIA GeForce RTX 3080"),
    /// from wgpu's AdapterInfo.name -- not a decoded PCI vendor ID. None
    /// when gpu_present is false.
    pub gpu_name: Option<String>,
    pub gpu_vram_class: Option<GpuVramClass>,
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

/// GPU probe via wgpu's normal multi-backend adapter-enumeration path --
/// an independent wgpu::Instance from cloud_chat_gpu_pane's compositor-
/// specific one (see module header). Enumerates every backend available on
/// this platform, picks the best real GPU found (discrete preferred over
/// integrated), and ignores CPU/software-rasterizer and virtual adapters
/// (llvmpipe, WARP, ...) -- those aren't real inference-capable hardware.
async fn detect_gpu() -> (bool, bool, Option<String>, Option<GpuVramClass>) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapters = instance.enumerate_adapters(wgpu::Backends::all()).await;

    let mut best: Option<wgpu::AdapterInfo> = None;
    for adapter in &adapters {
        let info = adapter.get_info();
        let is_candidate = matches!(
            info.device_type,
            wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
        );
        if !is_candidate {
            continue;
        }
        let is_better = match &best {
            None => true,
            Some(current) => {
                info.device_type == wgpu::DeviceType::DiscreteGpu
                    && current.device_type != wgpu::DeviceType::DiscreteGpu
            }
        };
        if is_better {
            best = Some(info);
        }
    }

    match best {
        Some(info) => {
            let is_discrete = info.device_type == wgpu::DeviceType::DiscreteGpu;
            let vram_class = if is_discrete {
                GpuVramClass::DiscreteUnknownSize
            } else {
                GpuVramClass::Integrated
            };
            (true, is_discrete, Some(info.name), Some(vram_class))
        }
        None => (false, false, None, None),
    }
}

/// Runs a fresh probe unconditionally -- callers wanting the cache-once
/// behavior should use `get_or_detect` instead.
pub async fn detect() -> HardwareProfile {
    let (ram_mb, cpu_cores) = detect_ram_and_cpu();
    let (gpu_present, gpu_is_discrete, gpu_name, gpu_vram_class) = detect_gpu().await;

    HardwareProfile {
        ram_mb,
        ram_class: RamClass::from_mb(ram_mb),
        cpu_cores,
        gpu_present,
        gpu_is_discrete,
        gpu_name,
        gpu_vram_class,
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
                Ok(profile) => return profile,
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

        let fixture = HardwareProfile {
            ram_mb: 17_179,
            ram_class: RamClass::High,
            cpu_cores: 8,
            gpu_present: true,
            gpu_is_discrete: true,
            gpu_name: Some("Test GPU".to_string()),
            gpu_vram_class: Some(GpuVramClass::DiscreteUnknownSize),
        };
        store_profile(&pool, &fixture).await;

        let reused = get_or_detect(&pool).await;
        assert_eq!(reused.ram_mb, fixture.ram_mb);
        assert_eq!(reused.cpu_cores, fixture.cpu_cores);
        assert_eq!(reused.gpu_name, fixture.gpu_name);

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
}
