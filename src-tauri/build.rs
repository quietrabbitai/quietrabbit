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
             Then retry the build."
        );

    // -- Privacy Filter (privacy-filter.cpp) --
    //
    // Build the library once before setting this variable:
    //   git clone https://github.com/localai-org/privacy-filter.cpp
    //   cd privacy-filter.cpp
    //   cmake --preset release-portable
    //   cmake --build --preset release-portable -j
    //
    // Then point PRIVACY_FILTER_LIB_DIR at the directory containing libpf.a:
    //   export PRIVACY_FILTER_LIB_DIR=/path/to/privacy-filter.cpp/build/release-portable/lib
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
        let lib_name = std::env::var("PRIVACY_FILTER_LIB_NAME")
            .unwrap_or_else(|_| "pf".to_owned());

        println!("cargo:rustc-cfg=privacy_filter_available");
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib=static={lib_name}");

        // C++ standard library — required by the C++ runtime inside libpf.
        // Use CARGO_CFG_TARGET_OS (target, not host) to emit the right lib name.
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        match target_os.as_str() {
            "linux"  => println!("cargo:rustc-link-lib=stdc++"),
            "macos"  => println!("cargo:rustc-link-lib=c++"),
            _        => {} // Windows: MSVC links C++ runtime automatically
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
