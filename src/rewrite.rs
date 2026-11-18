//! Package A. Rewrite rustc output from sandbox paths to client paths
//! (DESIGN section 6).

use serde_json::Value;

/// Ordered `(sandbox prefix, client prefix)` pairs, longest sandbox prefix
/// first. Built from `SandboxMap::remaps`.
#[derive(Clone, Debug, Default)]
pub struct Rewriter {
    pub pairs: Vec<(String, String)>,
    /// Client is Windows: convert `/` to `\` inside a rewritten path token.
    pub client_windows: bool,
}

fn is_sep(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

/// Does `s[at..]` start with `prefix`, treating `/` and `\` as equal? rustc
/// joins path parts with the host separator, but a path that came from a
/// `--remap-path-prefix` or a macro may mix them.
fn matches_at(s: &str, at: usize, prefix: &str) -> Option<usize> {
    let end = at.checked_add(prefix.len())?;
    let hay = s.as_bytes().get(at..end)?;
    let ok = hay
        .iter()
        .zip(prefix.as_bytes())
        .all(|(a, b)| a == b || (is_sep(*a) && is_sep(*b)));
    ok.then_some(end)
}

/// Dep-info files escape a space in a path as `\ `.
fn dep_escape(s: &str) -> String {
    s.replace(' ', "\\ ")
}

impl Rewriter {
    /// Build from `SandboxMap::remaps` output (any order).
    pub fn new(pairs: Vec<(String, String)>, client_windows: bool) -> Self {
        let mut pairs = pairs;
        pairs.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
        Rewriter {
            pairs,
            client_windows,
        }
    }

    /// Rewrite one stderr line. JSON lines (start with `{`) are parsed with
    /// serde_json and every string value is rewritten (including the
    /// `rendered` field), then re-serialized compactly; other lines are
    /// rewritten as text. Unparseable JSON falls back to text rewriting.
    /// A line that contains no sandbox path is returned byte for byte.
    pub fn stderr_line(&self, line: &str) -> String {
        if line.trim_start().starts_with('{')
            && let Ok(mut v) = serde_json::from_str::<Value>(line)
        {
            if self.value(&mut v) {
                // Re-serializing cannot fail for a Value; keep the original on the impossible path.
                return serde_json::to_string(&v).unwrap_or_else(|_| line.to_string());
            }
            return line.to_string();
        }
        self.text(line)
    }

    /// Rewrite all strings of a JSON value in place; true if anything changed.
    fn value(&self, v: &mut Value) -> bool {
        match v {
            Value::String(s) => match self.rewrite(s, false) {
                Some(n) => {
                    *s = n;
                    true
                }
                None => false,
            },
            Value::Array(a) => a.iter_mut().fold(false, |ch, x| self.value(x) | ch),
            Value::Object(o) => o.iter_mut().fold(false, |ch, (_, x)| self.value(x) | ch),
            _ => false,
        }
    }

    /// Rewrite a Makefile-style dep-info file (`.d`), preserving `\ ` escapes
    /// and `# env-dep:` lines. Prefixes inside `# env-dep:` values are
    /// rewritten too: cargo compares those values with its own environment.
    pub fn dep_info(&self, contents: &str) -> String {
        let mut out = String::with_capacity(contents.len());
        for line in contents.split_inclusive('\n') {
            let dep_syntax = !line.starts_with("# env-dep:");
            match self.rewrite(line, dep_syntax) {
                Some(n) => out.push_str(&n),
                None => out.push_str(line),
            }
        }
        out
    }

    /// Rewrite every occurrence of a sandbox prefix in a string.
    pub fn text(&self, s: &str) -> String {
        self.rewrite(s, false).unwrap_or_else(|| s.to_string())
    }

    /// Core scan. `dep`: Makefile escaping (`\ ` is part of a path). Returns
    /// `None` when nothing matched.
    fn rewrite(&self, s: &str, dep: bool) -> Option<String> {
        if self.pairs.is_empty() {
            return None;
        }
        let mut pairs: Vec<(String, String)> = if dep {
            self.pairs
                .iter()
                .map(|(a, b)| (dep_escape(a), dep_escape(b)))
                .collect()
        } else {
            self.pairs.clone()
        };
        pairs.sort_by_key(|(a, _)| std::cmp::Reverse(a.len()));

        let mut out = String::with_capacity(s.len());
        let mut changed = false;
        // Active separator conversion for the rest of the current path token.
        let mut conv: Option<(char, char)> = None;
        let mut prev: Option<char> = None;
        let mut i = 0;
        while i < s.len() {
            let Some(c) = s[i..].chars().next() else {
                break;
            };
            let rest = &s[i + c.len_utf8()..];

            // ANSI colour sequence: copy through, it separates tokens.
            if c == '\x1b' && rest.starts_with('[') {
                let mut end = i + 2;
                for (off, ch) in s[i + 2..].char_indices() {
                    end = i + 2 + off + ch.len_utf8();
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
                out.push_str(&s[i..end]);
                i = end;
                prev = Some(' ');
                conv = None;
                continue;
            }

            // A match may be preceded by the verbatim marker `\\?\`.
            let start = if c == '\\' && s[i..].starts_with(r"\\?\") {
                i + 4
            } else {
                i
            };
            if prev.is_none_or(|p| !p.is_alphanumeric()) {
                let hit = pairs.iter().find_map(|(sb, cl)| {
                    let end = matches_at(s, start, sb)?;
                    // The prefix must end a path component, not a longer name.
                    let boundary = s[end..]
                        .chars()
                        .next()
                        .is_none_or(|n| !(n.is_alphanumeric() || n == '_' || n == '-' || n == '.'));
                    boundary.then_some((end, sb, cl))
                });
                if let Some((end, sb, cl)) = hit {
                    let followed_by_sep = s.as_bytes().get(end).is_some_and(|b| is_sep(*b));
                    if followed_by_sep {
                        // `C:\` + `\a` would double the separator.
                        out.push_str(cl.trim_end_matches(['/', '\\']));
                    } else {
                        out.push_str(cl);
                    }
                    conv = if self.client_windows {
                        Some(('/', '\\'))
                    } else if sb.contains('\\') {
                        Some(('\\', '/'))
                    } else {
                        None
                    };
                    changed = true;
                    prev = s[..end].chars().last();
                    i = end;
                    continue;
                }
            }

            if let Some((from, to)) = conv {
                let escaped_space = dep && c == ' ' && prev == Some('\\');
                let ends = !escaped_space
                    && (c.is_whitespace()
                        || matches!(c, '"' | '\'' | ')' | ',')
                        || (c == ':' && rest.chars().next().is_some_and(|d| d.is_ascii_digit())));
                if ends {
                    conv = None;
                    out.push(c);
                } else if c == from && !(dep && from == '\\' && rest.starts_with(' ')) {
                    out.push(to);
                    changed = true;
                } else {
                    out.push(c);
                }
            } else {
                out.push(c);
            }
            prev = Some(c);
            i += c.len_utf8();
        }
        changed.then_some(out)
    }
}

/// If `line` is a rustc artifact notification
/// (`{"artifact":"<path>","emit":"<kind>"}`), return `(path, emit)`.
pub fn artifact(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    if !t.starts_with('{') || !t.contains("\"artifact\"") {
        return None;
    }
    let v: Value = serde_json::from_str(t).ok()?;
    let o = v.as_object()?;
    Some((
        o.get("artifact")?.as_str()?.to_string(),
        o.get("emit")?.as_str()?.to_string(),
    ))
}

/// Names from `# env-dep:NAME` / `# env-dep:NAME=VALUE` lines of a dep-info file.
pub fn env_deps(dep_info: &str) -> Vec<String> {
    dep_info
        .lines()
        .filter_map(|l| l.strip_prefix("# env-dep:"))
        .map(|rest| {
            rest.trim_end_matches('\r')
                .split('=')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|n| !n.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SB: &str = r"E:\jobs\j1\root\C";

    fn win() -> Rewriter {
        Rewriter::new(vec![(SB.to_string(), r"C:\".to_string())], true)
    }

    #[test]
    fn text_replaces_prefix_and_converts_slashes_in_token() {
        let r = win();
        assert_eq!(
            r.text(r"  --> E:\jobs\j1\root\C\proj/src/lib.rs:2:9"),
            r"  --> C:\proj\src\lib.rs:2:9"
        );
        // Token ends at `:digit`, whitespace, quotes, `)` and `,`; later slashes stay.
        assert_eq!(
            r.text(r"E:\jobs\j1\root\C\a/b.rs:3 and 1/2"),
            r"C:\a\b.rs:3 and 1/2"
        );
        assert_eq!(
            r.text(r#"see "E:\jobs\j1\root\C\a/b" or a/b"#),
            r#"see "C:\a\b" or a/b"#
        );
        assert_eq!(r.text(r"(E:\jobs\j1\root\C\a/b), x/y"), r"(C:\a\b), x/y");
        assert_eq!(r.text(r"E:\jobs\j1\root\C\a/b, x/y"), r"C:\a\b, x/y");
    }

    #[test]
    fn text_respects_boundaries() {
        let r = win();
        // Not a path component boundary.
        assert_eq!(r.text(r"E:\jobs\j1\root\Cx\a"), r"E:\jobs\j1\root\Cx\a");
        assert_eq!(r.text(r"xE:\jobs\j1\root\C\a"), r"xE:\jobs\j1\root\C\a");
        // The bare root alone keeps its separator.
        assert_eq!(r.text(r"in E:\jobs\j1\root\C"), r"in C:\");
        assert_eq!(r.text("nothing here"), "nothing here");
    }

    #[test]
    fn text_handles_verbatim_prefix_and_mixed_separators() {
        let r = win();
        assert_eq!(r.text(r"\\?\E:\jobs\j1\root\C\x\y.rs"), r"C:\x\y.rs");
        assert_eq!(r.text(r"E:/jobs/j1/root/C/x/y.rs"), r"C:\x\y.rs");
    }

    #[test]
    fn longest_prefix_wins() {
        let r = Rewriter::new(
            vec![
                (r"E:\j\root\C".into(), r"C:\".into()),
                (r"E:\j\root\UNC\srv\share".into(), r"\\srv\share".into()),
            ],
            true,
        );
        assert_eq!(
            r.text(r"E:\j\root\UNC\srv\share\a/b.rs"),
            r"\\srv\share\a\b.rs"
        );
        assert_eq!(r.text(r"E:\j\root\C\a/b.rs"), r"C:\a\b.rs");
    }

    #[test]
    fn posix_client_from_posix_peer() {
        let r = Rewriter::new(vec![("/data/jobs/j1/root/_".into(), "/".into())], false);
        assert_eq!(
            r.text("--> /data/jobs/j1/root/_/home/me/src/lib.rs:1:1"),
            "--> /home/me/src/lib.rs:1:1"
        );
    }

    #[test]
    fn posix_client_from_windows_peer_converts_backslashes() {
        let r = Rewriter::new(vec![(r"E:\jobs\j1\root\_".into(), "/".into())], false);
        assert_eq!(
            r.text(r"--> E:\jobs\j1\root\_\home\me\lib.rs:1:1"),
            "--> /home/me/lib.rs:1:1"
        );
    }

    // Shape of a real `--error-format=json --json=diagnostic-rendered-ansi` line.
    fn json_diag() -> String {
        let rendered = "\u{1b}[0m\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m\u{1b}[0m\u{1b}[1m: unused variable: `x`\u{1b}[0m\n\
\u{20}\u{20}\u{1b}[0m\u{1b}[0m\u{1b}[1m\u{1b}[38;5;12m--> \u{1b}[0m\u{1b}[0mE:\\jobs\\j1\\root\\C\\proj/src/lib.rs:2:9\u{1b}[0m\n";
        serde_json::json!({
            "$message_type": "diagnostic",
            "message": "unused variable: `x`",
            "code": {"code": "unused_variables", "explanation": null},
            "level": "warning",
            "spans": [{
                "file_name": r"E:\jobs\j1\root\C\proj\src\lib.rs",
                "byte_start": 10, "byte_end": 11, "line_start": 2, "line_end": 2,
                "column_start": 9, "column_end": 10, "is_primary": true,
                "text": [{"text": "    let x = 1;", "highlight_start": 9, "highlight_end": 10}],
                "label": null, "suggested_replacement": null,
                "expansion": null
            }],
            "children": [{
                "message": "defined in E:\\jobs\\j1\\root\\C\\proj\\src\\lib.rs",
                "code": null, "level": "note",
                "spans": [{"file_name": r"E:\jobs\j1\root\C\dep\src\lib.rs"}],
                "children": [], "rendered": null
            }],
            "rendered": rendered
        })
        .to_string()
    }

    #[test]
    fn json_diagnostic_is_rewritten_recursively() {
        let out = win().stderr_line(&json_diag());
        assert!(!out.contains("jobs"), "sandbox path left in: {out}");
        let v: Value = serde_json::from_str(&out).expect("still json");
        assert_eq!(v["spans"][0]["file_name"], r"C:\proj\src\lib.rs");
        assert_eq!(
            v["children"][0]["spans"][0]["file_name"],
            r"C:\dep\src\lib.rs"
        );
        assert_eq!(
            v["children"][0]["message"],
            r"defined in C:\proj\src\lib.rs"
        );
        let rendered = v["rendered"].as_str().expect("rendered");
        // ANSI escapes survive, the path after one is rewritten, slashes converted.
        assert!(
            rendered.contains("\u{1b}[0mC:\\proj\\src\\lib.rs:2:9\u{1b}[0m"),
            "{rendered:?}"
        );
        // Numbers and nulls untouched.
        assert_eq!(v["spans"][0]["byte_start"], 10);
        assert!(v["code"]["explanation"].is_null());
        assert!(!out.contains('\n'), "compact single line");
    }

    #[test]
    fn json_line_without_sandbox_paths_is_returned_unchanged() {
        let line = r#"{"b": 1,   "a":"x"}"#;
        assert_eq!(win().stderr_line(line), line);
    }

    #[test]
    fn invalid_json_falls_back_to_text() {
        let line = r"{ not json E:\jobs\j1\root\C\a/b.rs";
        assert_eq!(win().stderr_line(line), r"{ not json C:\a\b.rs");
    }

    #[test]
    fn rendered_text_lines() {
        let r = win();
        assert_eq!(
            r.stderr_line(r"error: couldn't read `E:\jobs\j1\root\C\p\x.rs`: nope"),
            r"error: couldn't read `C:\p\x.rs`: nope"
        );
        assert_eq!(r.stderr_line(" 2 |     let x = 1;"), " 2 |     let x = 1;");
    }

    #[test]
    fn artifact_notifications() {
        assert_eq!(
            artifact(r#"{"artifact":"C:\\t\\libfoo-1.rmeta","emit":"metadata"}"#),
            Some((r"C:\t\libfoo-1.rmeta".to_string(), "metadata".to_string()))
        );
        assert_eq!(
            artifact(r#"{"emit":"link","artifact":"/t/libfoo.rlib"}"#),
            Some(("/t/libfoo.rlib".into(), "link".into()))
        );
        assert_eq!(artifact(&json_diag()), None);
        assert_eq!(artifact("warning: x"), None);
        assert_eq!(artifact(r#"{"artifact":"x"}"#), None);
        assert_eq!(artifact(r#"{"artifact":1,"emit":"link"}"#), None);
    }

    #[test]
    fn dep_info_rewrites_paths_keeps_escapes_and_env_lines() {
        let r = win();
        let d = "E:\\jobs\\j1\\root\\C\\t\\libfoo-1.rmeta: E:\\jobs\\j1\\root\\C\\p\\src/lib.rs E:\\jobs\\j1\\root\\C\\my\\ dir/x.rs\r\n\
E:\\jobs\\j1\\root\\C\\p\\src/lib.rs:\n\
\n\
# env-dep:CARGO_PKG_NAME=foo\n\
# env-dep:OUT_DIR=E:\\jobs\\j1\\root\\C\\o\n\
# env-dep:HOME\n";
        let out = r.dep_info(d);
        let want = "C:\\t\\libfoo-1.rmeta: C:\\p\\src\\lib.rs C:\\my\\ dir\\x.rs\r\n\
C:\\p\\src\\lib.rs:\n\
\n\
# env-dep:CARGO_PKG_NAME=foo\n\
# env-dep:OUT_DIR=C:\\o\n\
# env-dep:HOME\n";
        assert_eq!(out, want);
    }

    #[test]
    fn dep_info_matches_prefix_with_escaped_space() {
        let r = Rewriter::new(vec![(r"E:\my data\root\C".into(), r"C:\".into())], true);
        let out = r.dep_info("E:\\my\\ data\\root\\C\\a\\ b/c.rs: x\n");
        assert_eq!(out, "C:\\a\\ b\\c.rs: x\n");
    }

    #[test]
    fn dep_info_posix_client_keeps_escaped_space() {
        let r = Rewriter::new(vec![(r"E:\j\root\_".into(), "/".into())], false);
        let out = r.dep_info("E:\\j\\root\\_\\home\\me\\my\\ dir\\x.rs: y\n");
        assert_eq!(out, "/home/me/my\\ dir/x.rs: y\n");
    }

    #[test]
    fn env_dep_names() {
        let d = "a: b\r\n# env-dep:FOO=bar=baz\r\n# env-dep:BAR\n# env-dep:\n# comment\n";
        assert_eq!(env_deps(d), vec!["FOO".to_string(), "BAR".to_string()]);
        assert!(env_deps("a: b\n").is_empty());
    }
}
