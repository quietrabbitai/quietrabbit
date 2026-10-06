// src-tauri/src/hardware_probe/support_table.rs
//
// items.id=606: curated "can the pinned Ollama use this GPU" table, keyed by
// GPU ARCHITECTURE (gfx target / CUDA compute capability), never product name.
//
// REVISIT WHEN items.id=583 PINS A REAL OLLAMA BINARY. This table was written
// against the Ollama version seen in the dev log (0.34.4) and Ollama's GPU
// docs page (https://docs.ollama.com/gpu, read 2026-10-06). Ollama's support
// list moves between releases; re-check the page for the pinned version and
// update OLLAMA_SUPPORT_TABLE_VERSION with it.
//
// Verdict policy (decisions.id=852, Jason's conditions on items.id=606):
//   - Usable   only with direct evidence of support.
//   - Unusable only with direct evidence of non-support.
//   - Unknown  for everything else. An engine treats Unusable AND Unknown as
//     CPU; Unknown is never an error.
// Direct "Unusable" evidence today:
//   - NVIDIA compute capability below 5.0 (the docs' stated floor).
//   - AMD gfx1032: observed on Jason's machine, Ollama dropped it with
//     "no rocblas support for gfx target" and ran CPU-only.
// Other AMD targets absent from the docs list are Unknown, not Unusable:
// Vulkan can cover more AMD hardware depending on version, and absence from a
// docs list is not proof of failure.

use super::{GpuVendor, OllamaUsability};

/// Ollama version this table was written against. See header: revisit when
/// items.id=583 pins a real binary.
pub const OLLAMA_SUPPORT_TABLE_VERSION: &str = "0.34.4";

/// AMD LLVM gfx targets the Ollama GPU docs list for ROCm on Linux
/// (https://docs.ollama.com/gpu, 2026-10-06). Only targets confirmed on that
/// page are listed; gfx900/gfx906 etc. are deliberately absent (not listed
/// there, so Unknown here).
const AMD_ROCM_LISTED: &[&str] = &[
    "gfx908", "gfx90a", "gfx942", "gfx950", "gfx1030", "gfx1100", "gfx1101", "gfx1102", "gfx1150",
    "gfx1151", "gfx1200", "gfx1201",
];

/// AMD targets with direct evidence of being dropped by Ollama.
const AMD_KNOWN_UNSUPPORTED: &[&str] = &["gfx1032"];

/// Minimum NVIDIA compute capability (major, minor) per the Ollama docs.
const NVIDIA_MIN_CC: (u32, u32) = (5, 0);

/// Parses an architecture string like "sm_86" into (major, minor). The minor
/// part is a single digit by CUDA convention.
pub fn parse_sm(arch: &str) -> Option<(u32, u32)> {
    let digits = arch.strip_prefix("sm_")?;
    if digits.len() < 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (maj, min) = digits.split_at(digits.len() - 1);
    Some((maj.parse().ok()?, min.parse().ok()?))
}

pub fn usability(vendor: GpuVendor, architecture: Option<&str>) -> OllamaUsability {
    let Some(arch) = architecture else {
        return OllamaUsability::Unknown;
    };
    match vendor {
        GpuVendor::Nvidia => match parse_sm(arch) {
            Some(cc) if cc >= NVIDIA_MIN_CC => OllamaUsability::Usable,
            Some(_) => OllamaUsability::Unusable,
            None => OllamaUsability::Unknown,
        },
        GpuVendor::Amd => {
            if AMD_ROCM_LISTED.contains(&arch) {
                OllamaUsability::Usable
            } else if AMD_KNOWN_UNSUPPORTED.contains(&arch) {
                OllamaUsability::Unusable
            } else {
                OllamaUsability::Unknown
            }
        }
        // Intel (Vulkan), Apple (Metal, items.id=609) and others: no
        // verified evidence encoded here yet.
        _ => OllamaUsability::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_compute_capability_floor() {
        assert_eq!(
            usability(GpuVendor::Nvidia, Some("sm_86")),
            OllamaUsability::Usable
        );
        assert_eq!(
            usability(GpuVendor::Nvidia, Some("sm_50")),
            OllamaUsability::Usable
        );
        assert_eq!(
            usability(GpuVendor::Nvidia, Some("sm_37")),
            OllamaUsability::Unusable
        );
        assert_eq!(
            usability(GpuVendor::Nvidia, Some("sm_100")),
            OllamaUsability::Usable
        );
        assert_eq!(
            usability(GpuVendor::Nvidia, Some("garbage")),
            OllamaUsability::Unknown
        );
    }

    #[test]
    fn amd_listed_known_bad_and_unlisted() {
        assert_eq!(
            usability(GpuVendor::Amd, Some("gfx1030")),
            OllamaUsability::Usable
        );
        assert_eq!(
            usability(GpuVendor::Amd, Some("gfx1032")),
            OllamaUsability::Unusable
        );
        // Unlisted, no direct evidence: Unknown, not Unusable.
        assert_eq!(
            usability(GpuVendor::Amd, Some("gfx1036")),
            OllamaUsability::Unknown
        );
        assert_eq!(usability(GpuVendor::Amd, None), OllamaUsability::Unknown);
    }

    #[test]
    fn other_vendors_are_unknown() {
        assert_eq!(
            usability(GpuVendor::Intel, Some("xe")),
            OllamaUsability::Unknown
        );
        assert_eq!(usability(GpuVendor::Other, None), OllamaUsability::Unknown);
    }

    #[test]
    fn parse_sm_values() {
        assert_eq!(parse_sm("sm_86"), Some((8, 6)));
        assert_eq!(parse_sm("sm_100"), Some((10, 0)));
        assert_eq!(parse_sm("sm_5"), None);
        assert_eq!(parse_sm("gfx1030"), None);
    }
}
