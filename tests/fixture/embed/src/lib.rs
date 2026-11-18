/// CF-10: the manifest dir is baked into this library. A remote build would
/// bake in the peer's sandbox path; crabfarm must notice and build it locally,
/// so the app prints the CLIENT's directory.
pub const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");
