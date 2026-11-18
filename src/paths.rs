//! Package A. Client paths and sandbox mapping (DESIGN section 6).
//!
//! Pure functions, no I/O. Normalization is lexical (no canonicalize), so it
//! works for paths that do not exist on the executing peer.

use crate::proto::{ClientPath, PathStyle};
use std::path::{Component, Path, PathBuf};

/// Path style of the machine this process runs on.
pub fn host_style() -> PathStyle {
    if cfg!(windows) { PathStyle::Windows } else { PathStyle::Posix }
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
            c => out.push(c),
        }
    }
    Some(out)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
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
        let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
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
            PathStyle::Posix => ("/".to_string(), self.path.split('/').filter(|c| !c.is_empty()).collect()),
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
                    (format!("{drive}\\"), rest.split('\\').filter(|c| !c.is_empty()).collect())
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
                    && c2.iter().zip(&c1).all(|(a, b)| a.to_lowercase() == b.to_lowercase())
            }
            PathStyle::Posix => r1 == r2 && c2.iter().zip(&c1).all(|(a, b)| a == b),
        }
    }

    /// The root this path belongs to: `C:\` for Windows drive paths,
    /// `\\server\share` for UNC, `/` for Posix.
    pub fn root(&self) -> ClientPath {
        ClientPath { style: self.style, path: self.split().0 }
    }

    /// Directory names below the sandbox root, in order.
    fn sandbox_parts(&self) -> Vec<String> {
        let (root, comps) = self.split();
        let mut out: Vec<String> = match self.style {
            PathStyle::Posix => vec!["_".to_string()],
            PathStyle::Windows => match root.strip_prefix(r"\\") {
                Some(unc) => std::iter::once("UNC".to_string()).chain(unc.split('\\').map(str::to_string)).collect(),
                None => vec![root.chars().take(1).collect()],
            },
        };
        out.extend(comps.into_iter().map(str::to_string));
        out
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
    pub fn to_sandbox(&self, p: &ClientPath) -> PathBuf {
        let mut out = self.root.clone();
        for part in p.sandbox_parts() {
            out.push(part);
        }
        out
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
    pub fn render(&self, p: &ClientPath) -> String {
        let s = self.to_sandbox(p).to_string_lossy().into_owned();
        if cfg!(windows) { s.replace('/', "\\") } else { s }
    }

    /// `(sandbox root string, client root string)` for every distinct root of
    /// `paths`, for `--remap-path-prefix` and output rewriting. The sandbox
    /// side is rendered as rustc on this host will print it; the client side is
    /// the root as `ClientPath::root` gives it (`C:\`, `\\srv\share`, `/`).
    /// Longest sandbox string first.
    pub fn remaps<'a>(&self, paths: impl IntoIterator<Item = &'a ClientPath>) -> Vec<(String, String)> {
        let mut roots: Vec<ClientPath> = Vec::new();
        for p in paths {
            let r = p.root();
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        let mut out: Vec<(String, String)> = roots.iter().map(|r| (self.render(r), r.path.clone())).collect();
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
        // A backslash is an ordinary character on Posix.
        assert_eq!(p(r"/a\b"), Some(r"/a\b".into()));
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
        assert_eq!(cp(PathStyle::Windows, r"\\srv\share\a").root().path, r"\\srv\share");
        assert_eq!(cp(PathStyle::Posix, "/a/b").root().path, "/");
        assert_eq!(cp(PathStyle::Windows, r"C:\").root().path, r"C:\");
    }

    fn map() -> SandboxMap {
        SandboxMap::new(PathBuf::from("sbx").join("job1").join("root"))
    }

    #[test]
    fn sandbox_layout() {
        let m = map();
        let r = &m.root;
        assert_eq!(m.to_sandbox(&cp(PathStyle::Windows, r"C:\a\b")), r.join("C").join("a").join("b"));
        assert_eq!(
            m.to_sandbox(&cp(PathStyle::Windows, r"\\srv\share\a")),
            r.join("UNC").join("srv").join("share").join("a")
        );
        assert_eq!(m.to_sandbox(&cp(PathStyle::Posix, "/a/b")), r.join("_").join("a").join("b"));
        assert_eq!(m.to_sandbox(&cp(PathStyle::Windows, r"D:\")), r.join("D"));
        assert_eq!(m.to_sandbox(&cp(PathStyle::Posix, "/")), r.join("_"));
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
            assert_eq!(m.from_sandbox(&m.to_sandbox(&p), style), Some(p.clone()), "{s}");
        }
    }

    #[test]
    fn from_sandbox_rejects_outside_and_malformed() {
        let m = map();
        assert_eq!(m.from_sandbox(Path::new("elsewhere/C/a"), PathStyle::Windows), None);
        assert_eq!(m.from_sandbox(&m.root, PathStyle::Windows), None);
        assert_eq!(m.from_sandbox(&m.root.join("CC").join("a"), PathStyle::Windows), None);
        assert_eq!(m.from_sandbox(&m.root.join("UNC").join("srv"), PathStyle::Windows), None);
        assert_eq!(m.from_sandbox(&m.root.join("C").join("a"), PathStyle::Posix), None);
        assert_eq!(m.from_sandbox(&m.root.join("_").join("a"), PathStyle::Windows), None);
        // Escaping the sandbox through `..` is refused.
        assert_eq!(m.from_sandbox(&m.root.join("C").join("..").join(".."), PathStyle::Windows), None);
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

    #[cfg(windows)]
    #[test]
    fn render_uses_backslashes_only_on_windows() {
        let m = SandboxMap::new(PathBuf::from("E:/data/jobs/x/root"));
        assert_eq!(m.render(&cp(PathStyle::Windows, r"C:\a\b")), r"E:\data\jobs\x\root\C\a\b");
    }

    #[test]
    fn from_local_joins_cwd() {
        let cwd = if cfg!(windows) { Path::new(r"C:\work") } else { Path::new("/work") };
        let p = ClientPath::from_local(Path::new("src/lib.rs"), cwd).expect("abs");
        assert_eq!(p.path, if cfg!(windows) { r"C:\work\src\lib.rs" } else { "/work/src/lib.rs" });
        assert_eq!(p.style, host_style());
        assert_eq!(p.to_local(), PathBuf::from(&p.path));
        let up = ClientPath::from_local(Path::new("../x"), cwd).expect("abs");
        assert_eq!(up.path, if cfg!(windows) { r"C:\x" } else { "/x" });
    }
}
