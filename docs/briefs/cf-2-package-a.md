# CF-2 Package A: rustc args, paths, output rewriting, inputs

Read `docs/briefs/common.md` first; it is part of this brief. Pair Desk issue: `CF-2`. Branch: `cf-2-wrapper-logic`.

**Goal:** implement `src/paths.rs`, `src/rewrite.rs`, `src/rustc_args.rs`, `src/inputs.rs` per DESIGN sections 5, 6 and 8 (items 1, 2, 4) and the doc comments in those stubs.

**Order and early pushes (a sibling job depends on you):** CF-4 (executor) uses `paths::SandboxMap`, `rustc_args::render_args`, `rewrite::Rewriter` and `rewrite::artifact`. Implement and push them in this order: `paths.rs` → `rewrite.rs` → `render_args` → rest of `rustc_args.rs` → `inputs.rs`. Push after each.

**Details that matter**
- `ClientPath::parse`: both `\` and `/` accepted for Windows style; `\\?\C:\x` and `\\?\UNC\srv\share\x` normalized; drive letter uppercased; lexical `.`/`..` (`..` above root is an error → `None`); Windows comparisons in `starts_with` are case-insensitive, component-wise (`C:\ab` does not start with `C:\a`).
- `SandboxMap::remaps` returns the sandbox side as rustc on the executing host prints it (on Windows `R\C`, with the host's separators), longest first.
- `rewrite::Rewriter::stderr_line`: JSON lines are parsed with serde_json and all string values rewritten recursively (`rendered`, `spans[].file_name`, `children`, ...), then re-serialized compactly. For a Windows client, after a replaced prefix, `/` becomes `\` until the end of that path token (token ends at whitespace, quote, `)`, `,`, or `:` followed by a digit). Unit-test real rustc diagnostics in both JSON and rendered form, including a rendered `--> path:line:col` line and ANSI colour codes from `--json=diagnostic-rendered-ansi`.
- `rewrite::Rewriter::dep_info`: Makefile syntax, `\ ` escapes kept, `# env-dep:` lines kept.
- `rustc_args::classify` must accept both `--flag value` and `--flag=value`, `-Lx`/`-L x`, `-Cx=y`/`-C x=y`, repeated `--crate-type`, `--emit=a,b`, `--extern name` (no path), `--extern name=path`, `--extern priv:name=path` style modifiers, `-l static:+bundle=name`. Every DESIGN section 5 "always local" rule needs a test. Unknown flags: if a value looks like an absolute path (starts with a drive letter, `\\` or `/`), classify Local; otherwise Plain.
- Use **real cargo command lines** as test fixtures: create a tiny throwaway project in your scratch area (one lib with a registry-free path dependency and a proc-macro path dependency), run `cargo build -v` (timeout 120 s, `jobs = 2`), copy several `Running \`rustc ...\`` lines (lib, proc-macro, build script, bin, test) into test fixtures. Cargo on Windows may use `@argfile`; cover `expand_argfiles`.
- `produces`: library files only (rlib/rmeta per emit; proc-macro library named per host triple: `<name><extra>.dll` for `*-windows-*`, `lib<name><extra>.dylib` for `*-apple-*`, `lib<name><extra>.so` otherwise).
- `inputs::collect`: skip `target`, `.git` and any dot-directory under the package dir; follow no symlinks out of the package dir (record them as their target file content only if the target is a regular file inside the package dir, else skip); MAX_FILE / MAX_TOTAL caps.
- `inputs::HashCache`: mtime as i128 nanos since UNIX epoch; stream-hash with blake3 (`update_reader`).

**Acceptance (finite list)**
1. All four modules implemented, no `todo!()` left in them.
2. Unit tests for every bullet above, including path round trips `to_sandbox`/`from_sandbox` for drive, UNC and Posix paths, and at least 5 real cargo command lines classified correctly.
3. `cargo clippy --all-targets` clean for these modules; `cargo test paths rewrite rustc_args inputs` green.
4. Branch pushed; final report per common brief.
