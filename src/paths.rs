//! Package A. Client paths and sandbox mapping (DESIGN section 6).
//!
//! Pure functions, no I/O. Normalization is lexical (no canonicalize), so it
//! works for paths that do not exist on the executing peer.

use crate::proto::{ClientPath, PathStyle};
use std::path::{Component, Path, PathBuf};

/// Path style of the machine this process runs on.
pub fn host_style() -> PathStyle {
    if cfg!(windows) {
        PathStyle::Windows
    } else {
        PathStyle::Posix
    }
}

/// Device names Windows resolves in every directory, with or without an
/// extension (`nul.txt` is still the NUL device).
const RESERVED_DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// The characters that complete `COM`/`LPT` to a reserved device name: the
/// digits 0 to 9 and the superscripts ¹, ² and ³ (Windows reserves `COM0`,
/// `LPT0` and `COM¹` to `LPT³` as well, CF-19).
fn is_device_digit(c: char) -> bool {
    matches!(c, '0'..='9' | '¹' | '²' | '³')
}

/// One path component as it may appear in a client path and in the sandbox.
///
/// The same string is a directory name on the submitter and, mapped, on every
/// peer, so the strictest OS rules apply to both styles. `:` and `\` would make
/// `PathBuf::push` replace the whole path on a Windows peer (`D:`, `D:foo`,
/// `D:\x`); trailing dots and spaces and device names are resolved differently
/// by Windows than their text says. A client path with such a component is not
/// remote-eligible; it never reaches the sandbox.
pub fn valid_component(c: &str) -> bool {
    if c.is_empty() || c == "." || c == ".." || c.ends_with('.') || c.ends_with(' ') {
        return false;
    }
    if c.chars().any(|ch| {
        ch.is_control() || matches!(ch, ':' | '\\' | '/' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return false;
    }
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    let up = stem.to_ascii_uppercase();
    if RESERVED_DEVICES.contains(&up.as_str()) {
        return false;
    }
    let numbered = |prefix: &str| {
        up.strip_prefix(prefix)
            .is_some_and(|d| d.chars().count() == 1 && d.chars().all(is_device_digit))
    };
    !(numbered("COM") || numbered("LPT"))
}

/// Resolve `.`/`..` lexically. `floor` components can never be popped (the
/// server and share of a UNC path); popping past the floor is an error.
fn normalize_components(raw: &str, sep: char, floor: usize) -> Option<Vec<&str>> {
    let mut out: Vec<&str> = Vec::new();
    for c in raw.split(sep) {
        match c {
            "" | "." => {}
            ".." => {
                if out.len() <= floor {
                    return None;
                }
                out.pop();
            }
            c if !valid_component(c) => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn parse_windows(input: &str) -> Option<String> {
    let s = input.replace('/', "\\");
    let (unc, rest): (bool, &str) = if let Some(r) = strip_prefix_ci(&s, r"\\?\UNC\") {
        (true, r)
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        (false, r)
    } else if let Some(r) = s.strip_prefix(r"\\") {
        (true, r)
    } else {
        (false, s.as_str())
    };
    if unc {
        let comps = normalize_components(rest, '\\', 2)?;
        if comps.len() < 2 {
            return None;
        }
        return Some(format!(r"\\{}", comps.join("\\")));
    }
    let b = rest.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        // `C:foo` (drive-relative), `C:`, device paths and relative paths.
        return None;
    }
    let comps = normalize_components(&rest[3..], '\\', 0)?;
    let drive = (b[0] as char).to_ascii_uppercase();
    if comps.is_empty() {
        Some(format!("{drive}:\\"))
    } else {
        Some(format!("{drive}:\\{}", comps.join("\\")))
    }
}

fn parse_posix(s: &str) -> Option<String> {
    if !s.starts_with('/') {
        return None;
    }
    let comps = normalize_components(s, '/', 0)?;
    Some(format!("/{}", comps.join("/")))
}

impl ClientPath {
    /// Parse and normalize an absolute path string in the given style.
    /// Windows: uppercase drive letter, strip `\\?\` (and `\\?\UNC\`), accept
    /// `/` as separator, collapse `.`/`..` lexically, no trailing separator
    /// (except a bare root like `C:\`). Returns `None` for relative paths.
    pub fn parse(style: PathStyle, s: &str) -> Option<ClientPath> {
        let path = match style {
            PathStyle::Windows => parse_windows(s)?,
            PathStyle::Posix => parse_posix(s)?,
        };
        Some(ClientPath { style, path })
    }

    /// Absolute path on this machine (style = this OS). Relative paths are
    /// joined to `cwd` first.
    pub fn from_local(p: &Path, cwd: &Path) -> Option<ClientPath> {
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        ClientPath::parse(host_style(), joined.to_str()?)
    }

    /// This path as a local `PathBuf` (only meaningful on the submitting machine).
    pub fn to_local(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// Root string and the components below it. Tolerates hand-built values
    /// (the fields are public) by returning an empty component list.
    fn split(&self) -> (String, Vec<&str>) {
        match self.style {
            PathStyle::Posix => (
                "/".to_string(),
                self.path.split('/').filter(|c| !c.is_empty()).collect(),
            ),
            PathStyle::Windows => {
                if let Some(rest) = self.path.strip_prefix(r"\\") {
                    let all: Vec<&str> = rest.split('\\').filter(|c| !c.is_empty()).collect();
                    if all.len() < 2 {
                        return (self.path.clone(), Vec::new());
                    }
                    (format!(r"\\{}\{}", all[0], all[1]), all[2..].to_vec())
                } else {
                    let drive = self.path.get(..2).unwrap_or("");
                    let rest = self.path.get(3..).unwrap_or("");
                    (
                        format!("{drive}\\"),
                        rest.split('\\').filter(|c| !c.is_empty()).collect(),
                    )
                }
            }
        }
    }

    /// True if `self` equals `prefix` or lies under it (component-wise;
    /// case-insensitive for Windows style).
    pub fn starts_with(&self, prefix: &ClientPath) -> bool {
        if self.style != prefix.style {
            return false;
        }
        let (r1, c1) = self.split();
        let (r2, c2) = prefix.split();
        if c2.len() > c1.len() {
            return false;
        }
        match self.style {
            PathStyle::Windows => {
                r1.to_lowercase() == r2.to_lowercase()
                    && c2
                        .iter()
                        .zip(&c1)
                        .all(|(a, b)| a.to_lowercase() == b.to_lowercase())
            }
            PathStyle::Posix => r1 == r2 && c2.iter().zip(&c1).all(|(a, b)| a == b),
        }
    }

    /// The root this path belongs to: `C:\` for Windows drive paths,
    /// `\\server\share` for UNC, `/` for Posix.
    pub fn root(&self) -> ClientPath {
        ClientPath {
            style: self.style,
            path: self.split().0,
        }
    }

    /// Directory names below the sandbox root, in order. `None` when the value
    /// is not a well-formed client path: fields are public, so a hand-built
    /// value from the network gets the same component checks as `parse`.
    fn sandbox_parts(&self) -> Option<Vec<String>> {
        let (root, comps) = self.split();
        let mut out: Vec<String> = match self.style {
            PathStyle::Posix => vec!["_".to_string()],
            PathStyle::Windows => match root.strip_prefix(r"\\") {
                Some(unc) => {
                    let names: Vec<&str> = unc.split('\\').collect();
                    if names.len() != 2 {
                        return None;
                    }
                    std::iter::once("UNC".to_string())
                        .chain(names.into_iter().map(str::to_string))
                        .collect()
                }
                None => {
                    let b = root.as_bytes();
                    if b.len() != 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\'
                    {
                        return None;
                    }
                    vec![(b[0] as char).to_string()]
                }
            },
        };
        out.extend(comps.into_iter().map(str::to_string));
        out.iter().all(|c| valid_component(c)).then_some(out)
    }
}

/// Maps client paths into a job sandbox root on the executing peer and back.
#[derive(Clone, Debug)]
pub struct SandboxMap {
    pub root: PathBuf,
}

impl SandboxMap {
    pub fn new(root: PathBuf) -> Self {
        SandboxMap { root }
    }

    /// `C:\a\b` -> `<root>/C/a/b`, `\\srv\share\a` -> `<root>/UNC/srv/share/a`,
    /// `/a/b` -> `<root>/_/a/b`.
    ///
    /// `None` for a path with an unsafe component (CF-7). Every component is a
    /// single plain name (no `:`, `\`, `/`, `..`), so `push` can neither replace
    /// the path (`D:`, `D:\x`) nor climb out of it; the result is checked to lie
    /// strictly below the root anyway.
    pub fn to_sandbox(&self, p: &ClientPath) -> Option<PathBuf> {
        let mut out = self.root.clone();
        for part in p.sandbox_parts()? {
            out.push(part);
        }
        let rel = out.strip_prefix(&self.root).ok()?;
        rel.components()
            .all(|c| matches!(c, Component::Normal(_)))
            .then_some(out)
    }

    /// Inverse of `to_sandbox`; `None` if `p` is not under `root`.
    pub fn from_sandbox(&self, p: &Path, style: PathStyle) -> Option<ClientPath> {
        let rel = p.strip_prefix(&self.root).ok()?;
        let mut parts: Vec<&str> = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_str()?),
                Component::CurDir => {}
                _ => return None,
            }
        }
        let (first, rest) = parts.split_first()?;
        let candidate = match style {
            PathStyle::Posix => {
                if *first != "_" {
                    return None;
                }
                format!("/{}", rest.join("/"))
            }
            PathStyle::Windows if *first == "UNC" => {
                if rest.len() < 2 {
                    return None;
                }
                format!(r"\\{}", rest.join("\\"))
            }
            PathStyle::Windows => {
                if first.len() != 1 || !first.as_bytes()[0].is_ascii_alphabetic() {
                    return None;
                }
                format!("{first}:\\{}", rest.join("\\"))
            }
        };
        ClientPath::parse(style, &candidate)
    }

    /// The sandbox path as rustc on this host prints it: host separators
    /// (on Windows `R\C\a\b`, never mixed). Use this for every sandbox path
    /// rendered into an argument or environment value, so output rewriting
    /// can find it again.
    ///
    /// Callers validate every path with `to_sandbox` first; for a path that
    /// fails it still returns a string inside the root, never one outside.
    pub fn render(&self, p: &ClientPath) -> String {
        let path = self
            .to_sandbox(p)
            .unwrap_or_else(|| self.root.join("_invalid_path_"));
        let s = path.to_string_lossy().into_owned();
        if cfg!(windows) {
            s.replace('/', "\\")
        } else {
            s
        }
    }

    /// `(sandbox root string, client root string)` for every distinct root of
    /// `paths`, for `--remap-path-prefix` and output rewriting. The sandbox
    /// side is rendered as rustc on this host will print it; the client side is
    /// the root as `ClientPath::root` gives it (`C:\`, `\\srv\share`, `/`).
    /// Longest sandbox string first.
    pub fn remaps<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a ClientPath>,
    ) -> Vec<(String, String)> {
        let mut roots: Vec<ClientPath> = Vec::new();
        for p in paths {
            let r = p.root();
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        let mut out: Vec<(String, String)> = roots
            .iter()
            .map(|r| (self.render(r), r.path.clone()))
            .collect();
        out.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(s: &str) -> Option<String> {
        ClientPath::parse(PathStyle::Windows, s).map(|p| p.path)
    }
    fn cp(style: PathStyle, s: &str) -> ClientPath {
        ClientPath::parse(style, s).expect("valid test path")
    }

    /// CF-19: `COM0`, `LPT0` and the superscript spellings are device names too.
    #[test]
    fn all_numbered_device_names_are_refused() {
        for c in [
            "COM0", "LPT0", "COM1", "LPT9", "com¹", "COM²", "COM³", "LPT¹", "lpt²", "LPT³",
            "COM0.txt", "COM¹.rs", "lpt0 ",
        ] {
            assert!(!valid_component(c), "{c:?}");
        }
        for c in ["COM", "COM10", "LPT00", "COM¹²", "COMx", "com0x", "xCOM1"] {
            assert!(valid_component(c), "{c:?}");
        }
    }

    #[test]
    fn windows_parse_normalizes() {
        assert_eq!(win(r"c:\Users\x\..\y\.\z"), Some(r"C:\Users\y\z".into()));
        assert_eq!(win("c:/Users/x/lib.rs"), Some(r"C:\Users\x\lib.rs".into()));
        assert_eq!(win(r"C:\a\b\\c\"), Some(r"C:\a\b\c".into()));
        assert_eq!(win(r"C:\"), Some(r"C:\".into()));
        assert_eq!(win(r"c:\a\.."), Some(r"C:\".into()));
        assert_eq!(win(r"\\?\c:\x\y"), Some(r"C:\x\y".into()));
        assert_eq!(win(r"\\?\UNC\srv\share\x"), Some(r"\\srv\share\x".into()));
        assert_eq!(win(r"\\srv\share"), Some(r"\\srv\share".into()));
        assert_eq!(win("//srv/share/a/../b"), Some(r"\\srv\share\b".into()));
    }

    #[test]
    fn windows_parse_rejects() {
        assert_eq!(win(r"C:\.."), None);
        assert_eq!(win(r"C:\a\..\.."), None);
        assert_eq!(win(r"src\lib.rs"), None);
        assert_eq!(win("C:foo"), None);
        assert_eq!(win("C:"), None);
        assert_eq!(win(r"\\srv"), None);
        assert_eq!(win(r"\\srv\share\.."), None);
        assert_eq!(win(r"\\.\COM1"), None);
        assert_eq!(win(""), None);
    }

    #[test]
    fn posix_parse() {
        let p = |s| ClientPath::parse(PathStyle::Posix, s).map(|p| p.path);
        assert_eq!(p("/a/./b/../c/"), Some("/a/c".into()));
        assert_eq!(p("/"), Some("/".into()));
        assert_eq!(p("/.."), None);
        assert_eq!(p("a/b"), None);
        // A backslash is a separator on a Windows peer, so it is refused on Posix too (CF-7).
        assert_eq!(p(r"/a\b"), None);
    }

    #[test]
    fn starts_with_is_componentwise_and_case_insensitive_on_windows() {
        let a = cp(PathStyle::Windows, r"C:\Users\Me\Proj\src");
        assert!(a.starts_with(&cp(PathStyle::Windows, r"c:\users\me")));
        assert!(a.starts_with(&a.clone()));
        assert!(a.starts_with(&cp(PathStyle::Windows, r"C:\")));
        assert!(!cp(PathStyle::Windows, r"C:\ab").starts_with(&cp(PathStyle::Windows, r"C:\a")));
        assert!(!cp(PathStyle::Windows, r"D:\a").starts_with(&cp(PathStyle::Windows, r"C:\a")));
        assert!(!cp(PathStyle::Windows, r"C:\a").starts_with(&cp(PathStyle::Windows, r"C:\a\b")));
        let u = cp(PathStyle::Windows, r"\\Srv\Share\x");
        assert!(u.starts_with(&cp(PathStyle::Windows, r"\\srv\share")));
        assert!(!u.starts_with(&cp(PathStyle::Windows, r"\\srv\other")));
    }

    #[test]
    fn starts_with_is_case_sensitive_on_posix() {
        let a = cp(PathStyle::Posix, "/Home/me/x");
        assert!(a.starts_with(&cp(PathStyle::Posix, "/Home/me")));
        assert!(!a.starts_with(&cp(PathStyle::Posix, "/home/me")));
        assert!(!cp(PathStyle::Posix, "/ab").starts_with(&cp(PathStyle::Posix, "/a")));
        assert!(a.starts_with(&cp(PathStyle::Posix, "/")));
        // Different styles never match.
        assert!(!a.starts_with(&cp(PathStyle::Windows, r"C:\")));
    }

    #[test]
    fn roots() {
        assert_eq!(cp(PathStyle::Windows, r"c:\a\b").root().path, r"C:\");
        assert_eq!(
            cp(PathStyle::Windows, r"\\srv\share\a").root().path,
            r"\\srv\share"
        );
        assert_eq!(cp(PathStyle::Posix, "/a/b").root().path, "/");
        assert_eq!(cp(PathStyle::Windows, r"C:\").root().path, r"C:\");
    }

    /// True when mapping `p` yields something that is not strictly below the root.
    fn escapes(m: &SandboxMap, p: &ClientPath) -> bool {
        let Some(out) = m.to_sandbox(p) else {
            return false;
        };
        match out.strip_prefix(&m.root) {
            Ok(rel) => !rel.components().all(|c| matches!(c, Component::Normal(_))),
            Err(_) => true,
        }
    }

    fn map() -> SandboxMap {
        SandboxMap::new(PathBuf::from("sbx").join("job1").join("root"))
    }

    #[test]
    fn sandbox_layout() {
        let m = map();
        let r = &m.root;
        assert_eq!(
            m.to_sandbox(&cp(PathStyle::Windows, r"C:\a\b")),
            Some(r.join("C").join("a").join("b"))
        );
        assert_eq!(
            m.to_sandbox(&cp(PathStyle::Windows, r"\\srv\share\a")),
            Some(r.join("UNC").join("srv").join("share").join("a"))
        );
        assert_eq!(
            m.to_sandbox(&cp(PathStyle::Posix, "/a/b")),
            Some(r.join("_").join("a").join("b"))
        );
        assert_eq!(
            m.to_sandbox(&cp(PathStyle::Windows, r"D:\")),
            Some(r.join("D"))
        );
        assert_eq!(m.to_sandbox(&cp(PathStyle::Posix, "/")), Some(r.join("_")));
    }

    #[test]
    fn sandbox_round_trips() {
        let m = map();
        for (style, s) in [
            (PathStyle::Windows, r"C:\Users\me\proj\src\lib.rs"),
            (PathStyle::Windows, r"D:\"),
            (PathStyle::Windows, r"\\srv\share\a\b.rs"),
            (PathStyle::Windows, r"\\srv\share"),
            (PathStyle::Windows, r"C:\with space\x"),
            (PathStyle::Posix, "/home/me/proj/src/lib.rs"),
            (PathStyle::Posix, "/"),
        ] {
            let p = cp(style, s);
            assert_eq!(
                m.from_sandbox(&m.to_sandbox(&p).expect("maps"), style),
                Some(p.clone()),
                "{s}"
            );
        }
    }

    #[test]
    fn from_sandbox_rejects_outside_and_malformed() {
        let m = map();
        assert_eq!(
            m.from_sandbox(Path::new("elsewhere/C/a"), PathStyle::Windows),
            None
        );
        assert_eq!(m.from_sandbox(&m.root, PathStyle::Windows), None);
        assert_eq!(
            m.from_sandbox(&m.root.join("CC").join("a"), PathStyle::Windows),
            None
        );
        assert_eq!(
            m.from_sandbox(&m.root.join("UNC").join("srv"), PathStyle::Windows),
            None
        );
        assert_eq!(
            m.from_sandbox(&m.root.join("C").join("a"), PathStyle::Posix),
            None
        );
        assert_eq!(
            m.from_sandbox(&m.root.join("_").join("a"), PathStyle::Windows),
            None
        );
        // Escaping the sandbox through `..` is refused.
        assert_eq!(
            m.from_sandbox(&m.root.join("C").join("..").join(".."), PathStyle::Windows),
            None
        );
    }

    #[test]
    fn remaps_distinct_roots_longest_first() {
        let m = map();
        let paths = [
            cp(PathStyle::Windows, r"C:\a"),
            cp(PathStyle::Windows, r"C:\b"),
            cp(PathStyle::Windows, r"\\srv\share\x"),
            cp(PathStyle::Windows, r"D:\z"),
        ];
        let r = m.remaps(&paths);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].1, r"\\srv\share");
        assert!(r[0].0.len() >= r[1].0.len() && r[1].0.len() >= r[2].0.len());
        let c = r.iter().find(|(_, c)| c == r"C:\").expect("C root");
        assert_eq!(c.0, m.render(&cp(PathStyle::Windows, r"C:\")));
        assert!(c.0.ends_with(if cfg!(windows) { r"root\C" } else { "root/C" }));
        let px = m.remaps(&[cp(PathStyle::Posix, "/a/b")]);
        assert_eq!(px[0].1, "/");
    }

    /// CF-7: every escape reproduced from the issue. On the old code each of
    /// these parses and maps to a path outside the sandbox root.
    #[test]
    fn components_that_can_replace_the_path_are_rejected() {
        for s in [
            r"C:\a\D:\evil",
            r"C:\a\D:foo",
            r"C:\a\D:\x\y",
            r"C:\a\b:stream",
            r"C:\a\b\nul",
            r"C:\a\com1.txt",
            r"C:\a\x.",
            r"C:\a\x ",
            r"C:\a\b*c",
            r"C:\a\b?c",
            "C:\\a\\b\"c",
            r"C:\a\b<c",
            r"C:\a\b|c",
            "C:\\a\\b\u{1}c",
        ] {
            assert_eq!(win(s), None, "{s:?}");
        }
        for s in [r"\\srv\D:\x", r"\\COM1\share", r"\\srv\share\aux.rs"] {
            assert_eq!(win(s), None, "{s:?}");
        }
        let p = |s: &str| ClientPath::parse(PathStyle::Posix, s);
        for s in [
            r"/D:\Windows\x",
            r"/a\..\..\..\x",
            "/a/D:foo",
            "/a/CON",
            "/a/lpt9.txt",
            "/a/x.",
            "/a/b*c",
            "/a/b\u{0}c",
        ] {
            assert_eq!(p(s), None, "{s:?}");
        }
        // Ordinary names stay valid, including dots inside and device-like prefixes.
        assert_eq!(
            win(r"C:\a\console\com10\nullable\x.y.rs"),
            Some(r"C:\a\console\com10\nullable\x.y.rs".into())
        );
        assert_eq!(
            p("/a/.cargo/registry/serde-1.0/src/lib.rs").map(|p| p.path),
            Some("/a/.cargo/registry/serde-1.0/src/lib.rs".into())
        );
    }

    /// CF-7: a hand-built `ClientPath` (the fields are public and arrive from
    /// the network) must never map outside the sandbox root either.
    #[test]
    fn hand_built_paths_never_leave_the_sandbox() {
        let m = SandboxMap::new(PathBuf::from("sbx").join("job1").join("root"));
        let evil = [
            (PathStyle::Windows, r"C:\a\D:\evil"),
            (PathStyle::Windows, r"C:\a\D:foo"),
            (PathStyle::Windows, r"C:\a\D:\x"),
            (PathStyle::Posix, r"/D:\Windows\x"),
            (PathStyle::Posix, r"/a\..\..\..\x"),
            (PathStyle::Posix, "/a/../../x"),
            (PathStyle::Windows, r"\\srv\share\D:\x"),
        ];
        for (style, s) in evil {
            let p = ClientPath {
                style,
                path: s.to_string(),
            };
            assert!(
                !escapes(&m, &p),
                "{s} maps outside the sandbox: {:?}",
                m.to_sandbox(&p)
            );
            assert_eq!(m.to_sandbox(&p), None, "{s} must be refused");
        }
    }

    #[cfg(windows)]
    #[test]
    fn render_uses_backslashes_only_on_windows() {
        let m = SandboxMap::new(PathBuf::from("E:/data/jobs/x/root"));
        assert_eq!(
            m.render(&cp(PathStyle::Windows, r"C:\a\b")),
            r"E:\data\jobs\x\root\C\a\b"
        );
    }

    #[test]
    fn from_local_joins_cwd() {
        let cwd = if cfg!(windows) {
            Path::new(r"C:\work")
        } else {
            Path::new("/work")
        };
        let p = ClientPath::from_local(Path::new("src/lib.rs"), cwd).expect("abs");
        assert_eq!(
            p.path,
            if cfg!(windows) {
                r"C:\work\src\lib.rs"
            } else {
                "/work/src/lib.rs"
            }
        );
        assert_eq!(p.style, host_style());
        assert_eq!(p.to_local(), PathBuf::from(&p.path));
        let up = ClientPath::from_local(Path::new("../x"), cwd).expect("abs");
        assert_eq!(up.path, if cfg!(windows) { r"C:\x" } else { "/x" });
    }
}
