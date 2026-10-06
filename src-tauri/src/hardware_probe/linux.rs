// src-tauri/src/hardware_probe/linux.rs
//
// items.id=606: Linux GPU backend. sysfs reads + NVML (see nvml.rs); no CLI
// tools, no network. All parsing is in pure functions over file contents so
// it is fixture-tested; the filesystem readers are thin wrappers.
//
// AMD, verified on the dev machine (2026-10-06, one APU + one dGPU):
//   - /sys/class/drm/cardN/device/{vendor,device,mem_info_vram_total,
//     mem_info_gtt_total}: PCI ids and sizes. The APU reported a 512 MiB BIOS
//     carveout in mem_info_vram_total; the dGPU reported its real 8 GiB.
//   - /sys/class/kfd/kfd/topology/nodes/N/properties: gfx_target_version
//     (decimal MMmmss, e.g. 100302 = gfx1032), device_id (decimal PCI id),
//     vendor_id. Node 0 is the CPU (gfx_target_version 0). Absent when
//     amdkfd is not loaded -> architecture None -> usability Unknown.
//   - KFD memory-bank heap_type did NOT separate APU from dGPU (both reported
//     heap_type 1). What did: the KFD bank's size_in_bytes equals
//     mem_info_vram_total on the dGPU, but on the APU it is system-memory
//     sized (16.4 GB vs the 0.5 GB carveout) -- and mem_info_gtt_total is
//     also far above the carveout on the APU (about 30x) versus about 1.9x on
//     the dGPU. Rule: KFD bank size well above VRAM total (or, with no KFD,
//     GTT far above VRAM total) means the GPU shares system RAM.

use std::path::Path;

use super::nvml;
use super::support_table;
use super::{AdapterHint, GpuBackend, GpuInfo, GpuVendor, OllamaUsability};

pub struct LinuxBackend;

const PCI_VENDOR_AMD: u32 = 0x1002;
const PCI_VENDOR_NVIDIA: u32 = 0x10de;

const DRM_DIR: &str = "/sys/class/drm";
const KFD_NODES_DIR: &str = "/sys/class/kfd/kfd/topology/nodes";

/// KFD bank size must exceed this multiple of VRAM total to count as shared.
const KFD_SHARED_FACTOR: u64 = 2;
/// With no KFD data: GTT at least this multiple of VRAM total means shared.
const GTT_SHARED_FACTOR: u64 = 8;

// ---------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------

/// "0x1002\n" -> 0x1002.
pub fn parse_hex_id(s: &str) -> Option<u32> {
    let t = s.trim();
    u32::from_str_radix(t.strip_prefix("0x").unwrap_or(t), 16).ok()
}

pub fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KfdNode {
    pub vendor_id: u32,
    pub device_id: u32,
    pub gfx_target_version: u32,
    /// size_in_bytes of the node's first memory bank, if read.
    pub bank_size_bytes: Option<u64>,
}

/// Parses a KFD `properties` file ("key value" per line).
pub fn parse_kfd_properties(text: &str) -> KfdNode {
    let mut node = KfdNode::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(n) = v.parse::<u64>() else { continue };
        match k {
            "vendor_id" => node.vendor_id = n as u32,
            "device_id" => node.device_id = n as u32,
            "gfx_target_version" => node.gfx_target_version = n as u32,
            _ => {}
        }
    }
    node
}

/// Parses a KFD memory-bank `properties` file's size_in_bytes.
pub fn parse_kfd_bank_size(text: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        (it.next()? == "size_in_bytes").then(|| it.next()?.parse().ok())?
    })
}

/// gfx_target_version (decimal MMmmss) -> LLVM name; minor/step print as hex
/// digits like ROCm does: 100302 -> "gfx1032", 90010 -> "gfx90a", 0 -> None.
pub fn gfx_name(version: u32) -> Option<String> {
    if version == 0 {
        return None;
    }
    let (major, minor, step) = (version / 10_000, (version / 100) % 100, version % 100);
    Some(format!("gfx{major}{minor:x}{step:x}"))
}

/// Whether an AMD GPU shares system RAM (APU) rather than owning dedicated
/// VRAM. See header for the evidence behind the rule.
pub fn amd_is_integrated(vram: u64, gtt: Option<u64>, kfd_bank: Option<u64>) -> bool {
    if vram == 0 {
        return true;
    }
    if let Some(bank) = kfd_bank {
        return bank > vram.saturating_mul(KFD_SHARED_FACTOR);
    }
    gtt.is_some_and(|g| g >= vram.saturating_mul(GTT_SHARED_FACTOR))
}

// ---------------------------------------------------------------------
// Assembly (pure)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrmCard {
    pub vendor_id: u32,
    pub device_id: u32,
    pub vram_total: Option<u64>,
    pub gtt_total: Option<u64>,
}

pub fn build_amd_gpu(card: &DrmCard, kfd: Option<&KfdNode>, name: Option<String>) -> GpuInfo {
    let architecture = kfd.and_then(|k| gfx_name(k.gfx_target_version));
    let vram = card.vram_total.unwrap_or(0);
    let integrated = amd_is_integrated(vram, card.gtt_total, kfd.and_then(|k| k.bank_size_bytes));
    GpuInfo {
        vendor: GpuVendor::Amd,
        name,
        usable_by_ollama: support_table::usability(GpuVendor::Amd, architecture.as_deref()),
        architecture,
        is_integrated: integrated,
        // Integrated GPUs share system RAM: the carveout is not their memory.
        vram_mb: if integrated || card.vram_total.is_none() {
            None
        } else {
            super::bytes_to_mb(vram)
        },
    }
}

fn build_amd_gpus(cards: &[DrmCard], kfd_nodes: &[KfdNode], hints: &[AdapterHint]) -> Vec<GpuInfo> {
    let mut used = vec![false; kfd_nodes.len()];
    cards
        .iter()
        .filter(|c| c.vendor_id == PCI_VENDOR_AMD)
        .map(|c| {
            let idx = kfd_nodes.iter().enumerate().position(|(i, k)| {
                !used[i]
                    && k.vendor_id == PCI_VENDOR_AMD
                    && k.device_id == c.device_id
                    && k.gfx_target_version != 0
            });
            if let Some(i) = idx {
                used[i] = true;
            }
            let name = hints
                .iter()
                .find(|h| h.vendor_id == c.vendor_id && h.device_id == c.device_id)
                .map(|h| h.name.clone());
            build_amd_gpu(c, idx.map(|i| &kfd_nodes[i]), name)
        })
        .collect()
}

fn nvidia_gpus(cards: &[DrmCard], nvml_devices: &[nvml::NvmlDevice]) -> Vec<GpuInfo> {
    if !nvml_devices.is_empty() {
        return nvml_devices.iter().map(nvml::to_gpu_info).collect();
    }
    // NVIDIA present on the PCI bus but NVML unavailable (no driver, or the
    // library failed): report it honestly as unknown rather than guessing.
    cards
        .iter()
        .filter(|c| c.vendor_id == PCI_VENDOR_NVIDIA)
        .map(|_| GpuInfo {
            vendor: GpuVendor::Nvidia,
            name: None,
            architecture: None,
            is_integrated: false,
            vram_mb: None,
            usable_by_ollama: OllamaUsability::Unknown,
        })
        .collect()
}

// ---------------------------------------------------------------------
// Filesystem readers
// ---------------------------------------------------------------------

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_drm_cards(drm_dir: &Path) -> Vec<DrmCard> {
    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // "card0", not connectors like "card0-DP-1".
        .filter(|n| {
            n.strip_prefix("card")
                .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| {
            let dev = drm_dir.join(&n).join("device");
            Some(DrmCard {
                vendor_id: parse_hex_id(&read(&dev.join("vendor"))?)?,
                device_id: parse_hex_id(&read(&dev.join("device"))?)?,
                vram_total: read(&dev.join("mem_info_vram_total")).and_then(|s| parse_u64(&s)),
                gtt_total: read(&dev.join("mem_info_gtt_total")).and_then(|s| parse_u64(&s)),
            })
        })
        .collect()
}

fn read_kfd_nodes(nodes_dir: &Path) -> Vec<KfdNode> {
    let Ok(entries) = std::fs::read_dir(nodes_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    dirs.iter()
        .filter_map(|d| {
            let mut node = parse_kfd_properties(&read(&d.join("properties"))?);
            node.bank_size_bytes =
                read(&d.join("mem_banks/0/properties")).and_then(|s| parse_kfd_bank_size(&s));
            Some(node)
        })
        .collect()
}

impl GpuBackend for LinuxBackend {
    fn probe(&self, hints: &[AdapterHint]) -> Vec<GpuInfo> {
        let cards = read_drm_cards(Path::new(DRM_DIR));
        let kfd = read_kfd_nodes(Path::new(KFD_NODES_DIR));
        let mut out = build_amd_gpus(&cards, &kfd, hints);
        let has_nvidia = cards.iter().any(|c| c.vendor_id == PCI_VENDOR_NVIDIA);
        if has_nvidia {
            out.extend(nvidia_gpus(&cards, &nvml::query(nvml::NVML_LIB_LINUX)));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures captured from the dev machine on 2026-10-06.
    const KFD_CPU: &str = "cpu_cores_count 16\nvendor_id 0\ndevice_id 0\ngfx_target_version 0\n";
    const KFD_DGPU: &str =
        "simd_count 56\nvendor_id 4098\ndevice_id 29695\ngfx_target_version 100302\n";
    const KFD_APU: &str =
        "simd_count 4\nvendor_id 4098\ndevice_id 5710\ngfx_target_version 100306\n";

    fn nodes() -> Vec<KfdNode> {
        let mut dgpu = parse_kfd_properties(KFD_DGPU);
        dgpu.bank_size_bytes = parse_kfd_bank_size("heap_type 1\nsize_in_bytes 8573157376\n");
        let mut apu = parse_kfd_properties(KFD_APU);
        apu.bank_size_bytes = parse_kfd_bank_size("heap_type 1\nsize_in_bytes 16377958400\n");
        vec![parse_kfd_properties(KFD_CPU), dgpu, apu]
    }

    fn cards() -> Vec<DrmCard> {
        vec![
            DrmCard {
                vendor_id: 0x1002,
                device_id: 0x164e,
                vram_total: Some(536_870_912),
                gtt_total: Some(16_377_958_400),
            },
            DrmCard {
                vendor_id: 0x1002,
                device_id: 0x73ff,
                vram_total: Some(8_573_157_376),
                gtt_total: Some(16_377_958_400),
            },
        ]
    }

    #[test]
    fn scalar_parsers() {
        assert_eq!(parse_hex_id("0x1002\n"), Some(0x1002));
        assert_eq!(parse_hex_id("nope"), None);
        assert_eq!(parse_u64("8573157376\n"), Some(8_573_157_376));
        assert_eq!(parse_u64(""), None);
    }

    #[test]
    fn gfx_names() {
        assert_eq!(gfx_name(100302).as_deref(), Some("gfx1032"));
        assert_eq!(gfx_name(100306).as_deref(), Some("gfx1036"));
        assert_eq!(gfx_name(90010).as_deref(), Some("gfx90a"));
        assert_eq!(gfx_name(90008).as_deref(), Some("gfx908"));
        assert_eq!(gfx_name(90000).as_deref(), Some("gfx900"));
        assert_eq!(gfx_name(120001).as_deref(), Some("gfx1201"));
        assert_eq!(gfx_name(0), None);
    }

    #[test]
    fn dev_machine_fixture_resolves_gfx1032_dgpu_and_apu() {
        let gpus = build_amd_gpus(&cards(), &nodes(), &[]);
        assert_eq!(gpus.len(), 2);

        let apu = &gpus[0];
        assert!(apu.is_integrated);
        assert_eq!(apu.vram_mb, None);
        assert_eq!(apu.architecture.as_deref(), Some("gfx1036"));
        assert_eq!(apu.usable_by_ollama, OllamaUsability::Unknown);

        let dgpu = &gpus[1];
        assert!(!dgpu.is_integrated);
        assert_eq!(dgpu.vram_mb, Some(8_573));
        assert_eq!(dgpu.architecture.as_deref(), Some("gfx1032"));
        assert_eq!(dgpu.usable_by_ollama, OllamaUsability::Unusable);
    }

    #[test]
    fn integrated_rule_without_kfd_uses_gtt_ratio() {
        // APU numbers: GTT ~30x the carveout.
        assert!(amd_is_integrated(536_870_912, Some(16_377_958_400), None));
        // dGPU numbers: GTT ~1.9x VRAM.
        assert!(!amd_is_integrated(
            8_573_157_376,
            Some(16_377_958_400),
            None
        ));
        assert!(amd_is_integrated(0, None, None));
        // Nothing to go on: assume dedicated, never invent sharing.
        assert!(!amd_is_integrated(8_573_157_376, None, None));
    }

    #[test]
    fn no_kfd_means_unknown_architecture() {
        let gpus = build_amd_gpus(&cards()[1..], &[], &[]);
        assert_eq!(gpus[0].architecture, None);
        assert_eq!(gpus[0].usable_by_ollama, OllamaUsability::Unknown);
        assert_eq!(gpus[0].vram_mb, Some(8_573));
    }

    #[test]
    fn name_comes_from_matching_adapter_hint() {
        let hints = [AdapterHint {
            vendor_id: 0x1002,
            device_id: 0x73ff,
            name: "AMD Radeon RX 6600 (RADV NAVI23)".into(),
            is_integrated: false,
        }];
        let gpus = build_amd_gpus(&cards(), &nodes(), &hints);
        assert_eq!(gpus[0].name, None);
        assert_eq!(
            gpus[1].name.as_deref(),
            Some("AMD Radeon RX 6600 (RADV NAVI23)")
        );
    }

    #[test]
    fn nvidia_without_nvml_is_unknown_not_error() {
        let c = [DrmCard {
            vendor_id: 0x10de,
            device_id: 0x2206,
            vram_total: None,
            gtt_total: None,
        }];
        let gpus = nvidia_gpus(&c, &[]);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].usable_by_ollama, OllamaUsability::Unknown);
        assert_eq!(gpus[0].vram_mb, None);
    }

    /// Reads this machine's real sysfs. Run manually: on the dev AMD box it
    /// expects the 8 GiB gfx1032 dGPU to be Unusable.
    #[test]
    #[ignore]
    fn live_probe_prints_this_machine() {
        let gpus = LinuxBackend.probe(&[]);
        eprintln!("{gpus:#?}");
        assert!(!gpus.is_empty());
    }
}
