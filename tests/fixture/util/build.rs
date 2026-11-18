use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    // The unused function is deliberate: its warning points into OUT_DIR, an
    // absolute path outside the package, which the end-to-end test expects to
    // be shown with the client's path.
    let generated = "pub const GENERATED: &str = \"from-build-script\";\nfn generated_but_unused() {}\n";
    std::fs::write(out.join("generated.rs"), generated).expect("write generated.rs");
    println!("cargo:rerun-if-changed=build.rs");
    // CF-21: a value the crate reads with `env!`; it only exists because of this line.
    println!("cargo:rustc-env=CF_BUILT=yes");
}
