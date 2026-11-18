//! `crabfarm path add|remove`: the user PATH on Windows (HKCU `Environment`),
//! a printed shell line elsewhere. The edit logic works on a backend trait so
//! tests use an in-memory fake; only [`RegistryEnv`] touches the registry.

use std::path::Path;

/// The stored user PATH and its registry type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathValue {
    pub value: String,
    /// REG_EXPAND_SZ (the usual type) rather than REG_SZ.
    pub expand: bool,
}

pub trait EnvBackend {
    fn read(&self) -> anyhow::Result<Option<PathValue>>;
    fn write(&mut self, v: &PathValue) -> anyhow::Result<()>;
    /// Tell running programs the environment changed.
    fn broadcast(&mut self);
    /// `%VAR%` references replaced by their values, for COMPARING entries
    /// only: what is written is never expanded. The default expands nothing.
    fn expand(&self, entry: &str) -> String {
        entry.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Changed,
    Unchanged,
}

/// Windows paths compare case-insensitively and ignore a trailing separator.
fn norm(entry: &str) -> String {
    entry
        .trim()
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

/// A value with an embedded NUL cannot be represented once read as text;
/// writing it back would cut it, so such a PATH is refused, never rewritten.
fn refuse_nul(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.contains('\0'),
        "the user PATH contains an embedded NUL character; not touching it (a rewrite would cut it)"
    );
    Ok(())
}

/// Append `dir` unless it is already an entry (compared with `%VAR%`
/// references expanded, so `%LOCALAPPDATA%\Programs\crabfarm` counts). Everything
/// else in the value stays byte for byte as it was, and the registry type is
/// kept (a new value is REG_EXPAND_SZ, like the one Windows creates).
pub fn add(backend: &mut dyn EnvBackend, dir: &str) -> anyhow::Result<Change> {
    let current = backend.read()?;
    let (value, expand) = match &current {
        Some(v) => (v.value.as_str(), v.expand),
        None => ("", true),
    };
    refuse_nul(value)?;
    refuse_nul(dir)?;
    let want = norm(&backend.expand(dir));
    if value
        .split(';')
        .any(|e| norm(e) == norm(dir) || norm(&backend.expand(e)) == want)
    {
        return Ok(Change::Unchanged);
    }
    let new = if value.trim().is_empty() {
        dir.to_string()
    } else if value.ends_with(';') {
        // Keep the closing separator too: `remove` then restores the stored
        // text exactly, which a value that ended with `;` (the Windows default
        // for many user PATHs) needs (CF-29).
        format!("{value}{dir};")
    } else {
        format!("{value};{dir}")
    };
    backend.write(&PathValue { value: new, expand })?;
    backend.broadcast();
    Ok(Change::Changed)
}

/// Remove the entries equal to `dir` and nothing else.
pub fn remove(backend: &mut dyn EnvBackend, dir: &str) -> anyhow::Result<Change> {
    let Some(current) = backend.read()? else {
        return Ok(Change::Unchanged);
    };
    refuse_nul(&current.value)?;
    let kept: Vec<&str> = current
        .value
        .split(';')
        .filter(|e| norm(e) != norm(dir))
        .collect();
    if kept.len() == current.value.split(';').count() {
        return Ok(Change::Unchanged);
    }
    backend.write(&PathValue {
        value: kept.join(";"),
        expand: current.expand,
    })?;
    backend.broadcast();
    Ok(Change::Changed)
}

/// The line a user adds to a shell profile on macOS and Linux.
pub fn shell_line(dir: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", dir.display())
}

#[cfg(windows)]
pub struct RegistryEnv;

#[cfg(windows)]
impl EnvBackend for RegistryEnv {
    fn read(&self) -> anyhow::Result<Option<PathValue>> {
        Ok(
            crate::winreg::read_str("Environment", "Path")?.map(|v| PathValue {
                value: v.value,
                expand: v.expand,
            }),
        )
    }
    fn write(&mut self, v: &PathValue) -> anyhow::Result<()> {
        crate::winreg::write_str(
            "Environment",
            "Path",
            &crate::winreg::StrValue {
                value: v.value.clone(),
                expand: v.expand,
            },
        )
    }
    fn broadcast(&mut self) {
        crate::winreg::broadcast_environment_change();
    }
    fn expand(&self, entry: &str) -> String {
        crate::winreg::expand_env(entry)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[derive(Default)]
    pub struct FakeEnv {
        pub value: Option<PathValue>,
        pub writes: u32,
        pub broadcasts: u32,
        /// Variables `expand` knows (name without percent signs, value).
        pub vars: Vec<(String, String)>,
    }

    impl FakeEnv {
        pub fn with(value: &str, expand: bool) -> FakeEnv {
            FakeEnv {
                value: Some(PathValue {
                    value: value.into(),
                    expand,
                }),
                ..Default::default()
            }
        }
    }

    impl EnvBackend for FakeEnv {
        fn read(&self) -> anyhow::Result<Option<PathValue>> {
            Ok(self.value.clone())
        }
        fn write(&mut self, v: &PathValue) -> anyhow::Result<()> {
            self.value = Some(v.clone());
            self.writes += 1;
            Ok(())
        }
        fn broadcast(&mut self) {
            self.broadcasts += 1;
        }
        fn expand(&self, entry: &str) -> String {
            let mut s = entry.to_string();
            for (k, v) in &self.vars {
                s = s.replace(&format!("%{k}%"), v);
            }
            s
        }
    }

    const DIR: &str = r"C:\Users\a\AppData\Local\Programs\crabfarm";

    #[test]
    fn add_appends_once_and_is_idempotent() {
        let mut e = FakeEnv::with(r"C:\Windows;%USERPROFILE%\.cargo\bin", true);
        assert_eq!(add(&mut e, DIR).unwrap(), Change::Changed);
        assert_eq!(
            e.value.as_ref().unwrap().value,
            format!(r"C:\Windows;%USERPROFILE%\.cargo\bin;{DIR}")
        );
        assert_eq!(add(&mut e, DIR).unwrap(), Change::Unchanged);
        // A trailing backslash and another case are the same entry.
        assert_eq!(
            add(&mut e, &format!("{}\\", DIR.to_uppercase())).unwrap(),
            Change::Unchanged
        );
        assert_eq!((e.writes, e.broadcasts), (1, 1));
    }

    /// CF-27: an entry written with `%VAR%` is the same directory once
    /// expanded; it is not added a second time, and nothing is expanded in
    /// what is written.
    #[test]
    fn an_entry_that_expands_to_the_directory_counts_as_present() {
        let mut e = FakeEnv::with(r"C:\Windows;%LOCALAPPDATA%\Programs\crabfarm", true);
        e.vars = vec![("LOCALAPPDATA".into(), r"C:\Users\a\AppData\Local".into())];
        assert_eq!(add(&mut e, DIR).unwrap(), Change::Unchanged);
        assert_eq!(e.writes, 0);
        // A different directory is still appended, the other entry stays unexpanded.
        let mut e = FakeEnv::with(r"%LOCALAPPDATA%\Other", true);
        e.vars = vec![("LOCALAPPDATA".into(), r"C:\Users\a\AppData\Local".into())];
        assert_eq!(add(&mut e, DIR).unwrap(), Change::Changed);
        assert_eq!(
            e.value.unwrap().value,
            format!(r"%LOCALAPPDATA%\Other;{DIR}")
        );
    }

    #[test]
    fn a_path_with_an_embedded_nul_is_refused_and_not_written() {
        let mut e = FakeEnv::with("C:\\a;C:\\b\0C:\\c", true);
        assert!(add(&mut e, DIR).is_err());
        assert!(remove(&mut e, DIR).is_err());
        assert_eq!(e.writes, 0);
        let mut e = FakeEnv::default();
        assert!(add(&mut e, "C:\\x\0y").is_err());
    }

    #[test]
    fn add_keeps_the_registry_type_and_unexpanded_entries() {
        let mut e = FakeEnv::with(r"%SystemRoot%;C:\x;", true);
        add(&mut e, DIR).unwrap();
        let v = e.value.unwrap();
        assert!(v.expand, "REG_EXPAND_SZ must stay REG_EXPAND_SZ");
        assert_eq!(v.value, format!(r"%SystemRoot%;C:\x;{DIR};"));
        // And remove gives back exactly what was stored (CF-29).
        for stored in [r"%SystemRoot%;C:\x;", r"%SystemRoot%;C:\x"] {
            let mut e = FakeEnv::with(stored, true);
            add(&mut e, DIR).unwrap();
            remove(&mut e, DIR).unwrap();
            assert_eq!(e.value.unwrap().value, stored);
        }

        let mut e = FakeEnv::with(r"C:\x", false);
        add(&mut e, DIR).unwrap();
        assert!(!e.value.unwrap().expand, "REG_SZ stays REG_SZ");
    }

    #[test]
    fn add_creates_a_missing_value_as_expand_sz() {
        let mut e = FakeEnv::default();
        add(&mut e, DIR).unwrap();
        assert_eq!(
            e.value,
            Some(PathValue {
                value: DIR.into(),
                expand: true
            })
        );
    }

    #[test]
    fn remove_takes_only_the_exact_entry() {
        let other = format!("{DIR}-other");
        let nested = format!(r"{DIR}\bin");
        let v = format!(r"C:\a;{DIR};{other};%X%\crabfarm;{nested}");
        let mut e = FakeEnv::with(&v, true);
        assert_eq!(remove(&mut e, DIR).unwrap(), Change::Changed);
        assert_eq!(
            e.value.as_ref().unwrap().value,
            format!(r"C:\a;{other};%X%\crabfarm;{nested}")
        );
        assert!(e.value.as_ref().unwrap().expand);
        assert_eq!(remove(&mut e, DIR).unwrap(), Change::Unchanged);
        assert_eq!((e.writes, e.broadcasts), (1, 1));
    }

    #[test]
    fn add_then_remove_restores_the_original_value() {
        let original = r"C:\Windows;%USERPROFILE%\.cargo\bin";
        let mut e = FakeEnv::with(original, true);
        add(&mut e, DIR).unwrap();
        remove(&mut e, DIR).unwrap();
        assert_eq!(e.value.unwrap().value, original);
    }

    #[test]
    fn remove_without_a_path_value_does_nothing() {
        let mut e = FakeEnv::default();
        assert_eq!(remove(&mut e, DIR).unwrap(), Change::Unchanged);
        assert_eq!(e.writes, 0);
    }
}
