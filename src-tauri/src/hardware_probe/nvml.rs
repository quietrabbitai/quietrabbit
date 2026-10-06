// src-tauri/src/hardware_probe/nvml.rs
//
// items.id=606: NVIDIA memory + compute capability through NVML, loaded at
// runtime with `libloading` (already a dependency, used for libepoxy). Chosen
// over the `nvml-wrapper` crate: that adds a bindgen-generated -sys crate to
// `cargo deny` scope and dlopens the same library anyway. NVML talks to the
// local kernel driver only -- no network, no CLI tools. If the library is
// absent or any call fails, the result is simply "no NVML GPUs" and the
// caller degrades to Unknown; never an error.
//
// Kept platform-neutral on purpose: items.id=608 (Windows) can reuse
// `query()` with "nvml.dll". NOT verified against real NVIDIA hardware in
// items.id=606 (no NVIDIA machine available) -- the pure conversion logic is
// fixture-tested, the FFI itself is not.

use std::ffi::{c_char, c_int, c_uint, c_void};

use super::support_table;
use super::{GpuInfo, GpuVendor};

pub const NVML_LIB_LINUX: &str = "libnvidia-ml.so.1";

/// NVML-shaped raw values for one device, before conversion to GpuInfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmlDevice {
    pub name: String,
    pub total_bytes: u64,
    pub cc_major: u32,
    pub cc_minor: u32,
}

/// Pure conversion, fixture-tested. NVML devices are discrete GPUs (Tegra
/// SoCs are not exposed this way), so is_integrated is false.
pub fn to_gpu_info(dev: &NvmlDevice) -> GpuInfo {
    let architecture = format!("sm_{}{}", dev.cc_major, dev.cc_minor);
    GpuInfo {
        vendor: GpuVendor::Nvidia,
        name: Some(dev.name.clone()),
        usable_by_ollama: support_table::usability(GpuVendor::Nvidia, Some(&architecture)),
        architecture: Some(architecture),
        is_integrated: false,
        vram_mb: super::bytes_to_mb(dev.total_bytes),
    }
}

#[repr(C)]
#[derive(Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

type NvmlInit = unsafe extern "C" fn() -> c_int;
type NvmlShutdown = unsafe extern "C" fn() -> c_int;
type NvmlCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
type NvmlHandleByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
type NvmlName = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int;
type NvmlMemInfo = unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> c_int;
type NvmlCc = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;

/// Loads NVML and reads every device. Empty on any failure.
pub fn query(lib_name: &str) -> Vec<NvmlDevice> {
    // SAFETY: loading a system library by soname and calling documented NVML
    // C entry points with correctly-typed out-pointers. Every return code is
    // checked; the library handle outlives all function pointers (they are
    // only used inside this function, before `lib` drops).
    unsafe {
        let Ok(lib) = libloading::Library::new(lib_name) else {
            return Vec::new();
        };
        let (Ok(init), Ok(shutdown), Ok(count), Ok(by_index), Ok(name), Ok(mem), Ok(cc)) = (
            lib.get::<NvmlInit>(b"nvmlInit_v2\0"),
            lib.get::<NvmlShutdown>(b"nvmlShutdown\0"),
            lib.get::<NvmlCount>(b"nvmlDeviceGetCount_v2\0"),
            lib.get::<NvmlHandleByIndex>(b"nvmlDeviceGetHandleByIndex_v2\0"),
            lib.get::<NvmlName>(b"nvmlDeviceGetName\0"),
            lib.get::<NvmlMemInfo>(b"nvmlDeviceGetMemoryInfo\0"),
            lib.get::<NvmlCc>(b"nvmlDeviceGetCudaComputeCapability\0"),
        ) else {
            return Vec::new();
        };

        if init() != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut n: c_uint = 0;
        if count(&mut n) == 0 {
            for i in 0..n {
                let mut handle: *mut c_void = std::ptr::null_mut();
                if by_index(i, &mut handle) != 0 {
                    continue;
                }
                let mut buf = [0 as c_char; 96];
                let mut info = NvmlMemory::default();
                let (mut maj, mut min): (c_int, c_int) = (0, 0);
                if mem(handle, &mut info) != 0 || cc(handle, &mut maj, &mut min) != 0 {
                    continue;
                }
                let gpu_name = if name(handle, buf.as_mut_ptr(), buf.len() as c_uint) == 0 {
                    std::ffi::CStr::from_ptr(buf.as_ptr())
                        .to_string_lossy()
                        .into_owned()
                } else {
                    String::new()
                };
                out.push(NvmlDevice {
                    name: gpu_name,
                    total_bytes: info.total,
                    cc_major: maj.max(0) as u32,
                    cc_minor: min.max(0) as u32,
                });
            }
        }
        shutdown();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware_probe::OllamaUsability;

    #[test]
    fn rtx_3080_fixture() {
        let g = to_gpu_info(&NvmlDevice {
            name: "NVIDIA GeForce RTX 3080".into(),
            total_bytes: 10_737_418_240,
            cc_major: 8,
            cc_minor: 6,
        });
        assert_eq!(g.architecture.as_deref(), Some("sm_86"));
        assert_eq!(g.vram_mb, Some(10_737));
        assert_eq!(g.usable_by_ollama, OllamaUsability::Usable);
        assert!(!g.is_integrated);
    }

    #[test]
    fn old_kepler_is_unusable() {
        let g = to_gpu_info(&NvmlDevice {
            name: "Tesla K80".into(),
            total_bytes: 12_884_901_888,
            cc_major: 3,
            cc_minor: 7,
        });
        assert_eq!(g.architecture.as_deref(), Some("sm_37"));
        assert_eq!(g.usable_by_ollama, OllamaUsability::Unusable);
    }

    #[test]
    fn missing_library_yields_no_devices() {
        assert!(query("libdefinitely-not-nvml.so.99").is_empty());
    }
}
