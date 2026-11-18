//! CF-19: the executor's `-C` allow-list must keep accepting what cargo really
//! passes. The command lines below are `cargo build -v --release` of
//! `tests/fixture` (cargo 1.100, paths replaced by `{T}`) plus the dev-profile
//! flags; each goes through the wrapper's classifier and then through the
//! executor's guard.

use crabfarm::guard::validate_args;
use crabfarm::paths::host_style;
use crabfarm::rustc_args::{Classified, ClassifyOptions, classify};

const LINES: &[&str] = &[
    // derive_mac (proc macro, release)
    "--crate-name derive_mac --edition=2021 derive_mac/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type proc-macro --emit=dep-info,link -C prefer-dynamic -C opt-level=3 -C embed-bitcode=no --check-cfg cfg(docsrs,test) --check-cfg cfg(feature,values()) -C metadata=5736afbbfb7a601e -C extra-filename=-5f517dc727c206e3 --out-dir {T}/release/deps -C strip=debuginfo -L dependency={T}/release/deps --extern proc_macro",
    // derive_mac (proc macro, dev)
    "--crate-name derive_mac --edition=2021 derive_mac/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type proc-macro --emit=dep-info,link -C prefer-dynamic -C embed-bitcode=no -C debuginfo=2 -C split-debuginfo=packed -C debug-assertions=on -C overflow-checks=on --check-cfg cfg(docsrs,test) -C metadata=729587204cd72433 -C extra-filename=-261d497f7092e3a6 --out-dir {T}/debug/deps -C strip=debuginfo -L dependency={T}/debug/deps --extern proc_macro",
    // embed, util (release libraries)
    "--crate-name embed --edition=2021 embed/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata,link -C opt-level=3 -C embed-bitcode=no --check-cfg cfg(docsrs,test) --check-cfg cfg(feature,values()) -C metadata=228e34425c657544 -C extra-filename=-a1073065d725138a --out-dir {T}/release/deps -C strip=debuginfo -L dependency={T}/release/deps",
    // core_lib with a proc macro and an rmeta extern
    "--crate-name core_lib --edition=2021 core_lib/src/lib.rs --error-format=json --json=diagnostic-rendered-ansi,artifacts,future-incompat --crate-type lib --emit=dep-info,metadata,link -C opt-level=3 -C embed-bitcode=no --check-cfg cfg(docsrs,test) --check-cfg cfg(feature,values()) -C metadata=1782d6c094ae27d3 -C extra-filename=-d336d6a5fb33f011 --out-dir {T}/release/deps -C strip=debuginfo -L dependency={T}/release/deps --extern derive_mac={T}/release/deps/derive_mac-261d497f7092e3a6.dll --extern util={T}/release/deps/libutil-93024968a252980e.rmeta",
    // a library built with options users put into RUSTFLAGS
    "--crate-name rf --edition=2021 rf/src/lib.rs --crate-type lib --emit=dep-info,metadata,link -C opt-level=2 -C codegen-units=4 -C panic=abort -C lto=thin -C embed-bitcode=yes -C target-feature=+crt-static -C relocation-model=pic -C force-frame-pointers=yes -C link-arg=-fuse-ld=lld -C metadata=1 -C extra-filename=-1 --out-dir {T}/release/deps",
];

fn render(line: &str, root: &str) -> Vec<String> {
    line.split(' ')
        .map(|a| {
            a.replace("{T}", root)
                .replace(".dll", if cfg!(windows) { ".dll" } else { ".so" })
        })
        .collect()
}

#[test]
fn what_cargo_passes_is_classified_remote_and_accepted_by_the_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp
        .path()
        .join("target")
        .to_string_lossy()
        .replace('\\', "/");
    let opts = ClassifyOptions {
        host_triple: "x86_64-pc-windows-msvc",
        cwd: tmp.path(),
        style: host_style(),
        strip_incremental: false,
    };
    for line in LINES {
        let args = render(line, &root);
        let Classified::Remote(cand) = classify(&args, &opts) else {
            panic!("not remote-eligible: {line}");
        };
        assert_eq!(
            validate_args(&cand.args, true),
            Ok(()),
            "the guard refuses what cargo passes: {line}"
        );
    }
}

#[test]
fn flags_outside_the_allow_list_keep_the_job_local() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = ClassifyOptions {
        host_triple: "x86_64-pc-windows-msvc",
        cwd: tmp.path(),
        style: host_style(),
        strip_incremental: false,
    };
    let base = "--crate-name x --edition=2021 x/src/lib.rs --crate-type lib --emit=dep-info,metadata,link --out-dir out -C metadata=1 -C extra-filename=-1";
    for extra in [
        "-C instrument-coverage",
        "-C llvm-args=-time-passes",
        "-C save-temps",
        "-C some-future-option=1",
    ] {
        let args: Vec<String> = format!("{base} {extra}")
            .split(' ')
            .map(String::from)
            .collect();
        assert!(
            matches!(classify(&args, &opts), Classified::Local { .. }),
            "{extra} must keep the job local"
        );
    }
    // A proc macro linked with a path argument stays local as well.
    let args: Vec<String> = format!("{base} --crate-type proc-macro -C link-arg=-L/opt/lib")
        .replace("--crate-type lib ", "")
        .split(' ')
        .map(String::from)
        .collect();
    assert!(matches!(classify(&args, &opts), Classified::Local { .. }));
}
