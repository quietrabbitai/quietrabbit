fn main() {
    // Verify that the SQLCipher development package is installed and
    // discoverable via pkg-config before the rest of the build proceeds.
    // Fails fast with a clear message rather than a cryptic linker error.
    // Minimum version 3.0: required for hex-key PRAGMA syntax
    // ("x'...'") used by all QR connection openers.
    pkg_config::Config::new()
        .atleast_version("3.0")
        .probe("sqlcipher")
        .expect(
            "libsqlcipher not found. Install the development package:\n  \
             Arch/Garuda: sudo pacman -S sqlcipher\n  \
             Debian/Ubuntu: sudo apt install libsqlcipher-dev\n  \
             Then retry the build."
        );

    tauri_build::build()
}
