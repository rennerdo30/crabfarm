include!(concat!(env!("OUT_DIR"), "/generated.rs"));

/// Set by the build script with `cargo:rustc-env` (CF-21).
pub const BUILT: &str = env!("CF_BUILT");
