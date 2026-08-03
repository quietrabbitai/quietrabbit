fn main() {
    // -- SQLCipher (existing) --
    // Verify libsqlcipher is installed and discoverable before the build proceeds.
    // Minimum version 3.0 required for hex-key PRAGMA syntax used by all QR openers.
    pkg_config::Config::new()
        .atleast_version("3.0")
        .probe("sqlcipher")
        .expect(
            "libsqlcipher not found. Install the development package:\n  \
             Arch/Garuda: sudo pacman -S sqlcipher\n  \
             Debian/Ubuntu: sudo apt install libsqlcipher-dev\n  \
             Then retry the build.",
        );

    // -- Privacy Filter (privacy-filter.cpp) --
    //
    // Build the library once before setting this variable:
    //   git clone https://github.com/localai-org/privacy-filter.cpp
    //   cd privacy-filter.cpp
    //   cmake --preset release-portable
    //   cmake --build --preset release-portable -j
    //
    // Then point PRIVACY_FILTER_LIB_DIR at the build root:
    //   export PRIVACY_FILTER_LIB_DIR=/path/to/privacy-filter.cpp/build/release-portable
    //
    // Layout expected under PRIVACY_FILTER_LIB_DIR:
    //   libpf.a                  — static archive (linked directly)
    //   ggml/src/libggml-base.so — ggml base (ggml_init, ggml_new_tensor, etc.)
    //   ggml/src/libggml.so      — ggml backend registry (ggml_backend_load_all, etc.)
    //
    // If unset: Privacy Filter FFI is compiled out (cfg flag absent) and gate3
    // uses the pre-filter sensitivity-based block. Acceptable for dev/test builds.
    // Production builds MUST set PRIVACY_FILTER_LIB_DIR.
    //
    // Override library name (default "pf") via PRIVACY_FILTER_LIB_NAME if your
    // cmake build produces a differently-named output (e.g. "privacy_filter").
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PRIVACY_FILTER_LIB_DIR");
    println!("cargo:rerun-if-env-changed=PRIVACY_FILTER_LIB_NAME");
    println!("cargo:rustc-check-cfg=cfg(privacy_filter_available)");

    if let Ok(lib_dir) = std::env::var("PRIVACY_FILTER_LIB_DIR") {
        let lib_name = std::env::var("PRIVACY_FILTER_LIB_NAME").unwrap_or_else(|_| "pf".to_owned());

        println!("cargo:rustc-cfg=privacy_filter_available");
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib=static={lib_name}");
        // ggml libs live in ggml/src/ under the build root.
        // ggml-base: ggml_init, ggml_new_tensor, ggml_get_name, etc.
        // ggml:      ggml_backend_load_all, ggml_backend_dev_count, etc.
        println!("cargo:rustc-link-search=native={lib_dir}/ggml/src");
        println!("cargo:rustc-link-lib=dylib=ggml-base");
        println!("cargo:rustc-link-lib=dylib=ggml");

        // C++ standard library — required by the C++ runtime inside libpf.
        // Use CARGO_CFG_TARGET_OS (target, not host) to emit the right lib name.
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        match target_os.as_str() {
            "linux" => println!("cargo:rustc-link-lib=stdc++"),
            "macos" => println!("cargo:rustc-link-lib=c++"),
            _ => {} // Windows: MSVC links C++ runtime automatically
        }

        // ggml's backend loader (ggml_backend_load_all, invoked internally by
        // pf_load) dynamically loads CPU-ISA-dispatched backend plugins —
        // bin/libggml-cpu-{zen4,alderlake,sandybridge,...}.so, ~14 variants,
        // scored and picked at runtime — from a directory it's told about via
        // pf_set_backend_dir() (privacy_filter.rs calls this with a path
        // resolved from Tauri's resource dir at startup; see main.rs setup()).
        // Stage those variant files, plus the ggml-base/ggml runtime libs,
        // into resources/ggml-backends/ so tauri.conf.json's bundle.resources
        // entry ships them with the packaged app.
        let backend_bin_dir = format!("{lib_dir}/bin");
        let dest_dir = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("resources/ggml-backends");
        std::fs::create_dir_all(&dest_dir).expect("failed to create resources/ggml-backends");

        let mut staged_any = false;
        if let Ok(entries) = std::fs::read_dir(&backend_bin_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_backend_so = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("libggml-cpu-") && n.ends_with(".so"));
                if is_backend_so {
                    let dest = dest_dir.join(path.file_name().unwrap());
                    std::fs::copy(&path, &dest)
                        .unwrap_or_else(|e| panic!("failed to copy {path:?} to {dest:?}: {e}"));
                    staged_any = true;
                }
            }
        }
        if !staged_any {
            println!(
                "cargo:warning=no libggml-cpu-*.so variants found under \
                 {backend_bin_dir} — CPU backend dispatch will fail at runtime. \
                 Did you build the release-portable preset (GGML_CPU_ALL_VARIANTS=ON)?"
            );
        }

        // Compile-time fallback for contexts with no Tauri AppHandle/resource
        // dir (the calibration harness, `cargo test`, `cargo run --example`).
        // privacy_filter.rs uses this only when set_backend_dir() was never
        // called at runtime.
        println!("cargo:rustc-env=PRIVACY_FILTER_BACKEND_DIR_DEV={backend_bin_dir}");

        // -- ggml-base/ggml runtime dylib discovery (separate problem from the
        // CPU-ISA dlopen() dispatch above) --
        //
        // The `dylib=ggml-base` / `dylib=ggml` links above (lines ~52-53) make
        // libggml-base.so.0 / libggml.so.0 *link-time* dependencies (DT_NEEDED
        // entries) of the quietrabbit binary itself. Unlike the CPU variants —
        // which ggml dlopen()s at its own discretion from a directory we pass
        // it explicitly at runtime via pf_set_backend_dir() — these two are
        // resolved by the dynamic linker (ld.so) before main() ever runs, and
        // ld.so has no way to ask the process where to look: it only checks
        // DT_RPATH/DT_RUNPATH baked into the binary, LD_LIBRARY_PATH, and the
        // system default paths (/etc/ld.so.cache, /usr/lib, ...). None of
        // those resolve for a packaged app with no launcher-set env var —
        // the binary fails to start at all, not just Privacy Filter.
        //
        // Fix: bake an RPATH at link time (ld.so does the $ORIGIN
        // substitution at load time, so no shell/build-time expansion here).
        // Stage the two libs into the same resources/ggml-backends/ dir as
        // the CPU variants (already wired into tauri.conf.json's
        // bundle.resources and app.path().resource_dir()/set_backend_dir()),
        // and point RPATH at it two ways:
        //
        //  1. $ORIGIN/../lib/<productName>/ggml-backends — for the installed/
        //     packaged binary. Verified against tauri-bundler (v2.9.0) source:
        //     debian.rs's generate_data() (reused as-is by appimage.rs) and
        //     rpm.rs independently both install the binary at usr/bin/<name>
        //     and bundle.resources at usr/lib/<productName>/ — i.e. the same
        //     fixed ../lib/<productName> offset from the executable's own
        //     directory on all three Linux bundle targets. This is also
        //     exactly the relative path tauri's own resource_dir() computes
        //     at runtime (crates/tauri-utils/src/platform.rs), which is what
        //     main.rs's setup() hook already relies on for set_backend_dir().
        //     productName is read from tauri.conf.json below (falling back to
        //     CARGO_PKG_NAME) rather than hardcoded, matching how
        //     tauri-codegen resolves PackageInfo.name.
        //  2. The absolute PRIVACY_FILTER_LIB_DIR/ggml/src path — for
        //     `cargo test` / `cargo build` and running the raw target/debug
        //     binary directly, none of which go through tauri-cli's
        //     resource-staging step that makes path (1) resolve in dev.
        //     Same "bake the dev machine's absolute build path" precedent as
        //     PRIVACY_FILTER_BACKEND_DIR_DEV above, just at the ELF rpath
        //     layer instead of the ggml_backend_load_all_from_path layer.
        let ggml_src_dir = format!("{lib_dir}/ggml/src");
        for soname in ["libggml-base.so.0", "libggml.so.0"] {
            let src = std::path::Path::new(&ggml_src_dir).join(soname);
            let dest = dest_dir.join(soname);
            std::fs::copy(&src, &dest)
                .unwrap_or_else(|e| panic!("failed to copy {src:?} to {dest:?}: {e}"));
        }

        if target_os == "linux" {
            let product_name = {
                let conf_path =
                    std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
                        .join("tauri.conf.json");
                let conf_text = std::fs::read_to_string(&conf_path)
                    .unwrap_or_else(|e| panic!("failed to read {conf_path:?}: {e}"));
                let conf: serde_json::Value = serde_json::from_str(&conf_text)
                    .unwrap_or_else(|e| panic!("failed to parse {conf_path:?}: {e}"));
                conf.get("productName")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| std::env::var("CARGO_PKG_NAME").unwrap())
            };

            // Plain rustc-link-arg (not the -bins-only variant): the
            // regression test at tests/privacy_filter_backend_dir.rs links
            // its own separate test binary under target/debug/deps/, which
            // rustc-link-arg-bins does NOT cover (it's scoped to [[bin]]/
            // main.rs targets only) — per the Cargo book, plain
            // rustc-link-arg applies to bins, cdylib, examples, tests, and
            // benches alike, which is what both the packaged app binary and
            // `cargo test` need here.
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/{product_name}/ggml-backends:{ggml_src_dir}"
            );
        }
    } else {
        println!(
            "cargo:warning=PRIVACY_FILTER_LIB_DIR not set — \
             Privacy Filter FFI disabled. Gate3 will use pre-filter \
             sensitivity block. Set this variable for production builds."
        );
    }

    tauri_build::build()
}
