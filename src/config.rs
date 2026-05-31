//! Per-project interpreter configuration: `redo.toml` at the project root.
//!
//! redo-msh's only built-in do-file interpreter is `msh`. To let the tool drop
//! onto existing projects whose do-files are written for another interpreter
//! (`sh -e`, `python3`, ...), a project may commit a `redo.toml` that maps
//! do-files to the command used to run them. We deliberately do **not** parse
//! shebangs; the mapping is always explicit and lives in version control.
//!
//! The file lives at the project root (not under `.redo/`, which is gitignored).
//!
//! ## Schema
//!
//! ```toml
//! # Shared across all platforms.
//! [interpreter]
//! default = ["msh"]                 # ultimate fallback
//!
//! [[interpreter.rule]]
//! match   = "*.py.do"               # glob on the do-file (see matching below)
//! command = ["python3"]
//!
//! # Per-platform overrides: linux / wsl / windows / macos.
//! [platform.linux]
//! default = ["sh", "-e"]
//!
//! [[platform.linux.rule]]
//! match   = "deploy/*.do"
//! command = ["bash", "-e"]
//!
//! [platform.windows]
//! default = ["msh"]
//! ```
//!
//! ## Resolution (first match wins)
//!
//! For a do-file at root-relative path `P`, on the current platform:
//!   1. a matching `rule` in the platform section,
//!   2. that platform section's `default`,
//!   3. a matching `rule` in the shared `[interpreter]` section,
//!   4. the shared `[interpreter].default`,
//!   5. the built-in `["msh"]`.
//!
//! WSL uses `[platform.wsl]` if present, otherwise falls back to
//! `[platform.linux]` (WSL is Linux), then to the shared/built-in layers.
//!
//! The resolved command is the argv prefix; redo-msh appends
//! `<dofile> $1 $2 $3` to it when spawning.
//!
//! ## Glob matching
//!
//! A `match` pattern with no `/` is tested against the do-file's **basename**
//! (so `*.do` matches every do-file anywhere); a pattern containing `/` is
//! tested against the full root-relative path. `*` matches any run of
//! non-`/` characters and `?` matches one; neither crosses a directory
//! separator.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// The built-in interpreter used when nothing in `redo.toml` matches.
pub const DEFAULT_INTERPRETER: &str = "msh";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    interpreter: Section,
    #[serde(default)]
    platform: Platforms,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Platforms {
    linux: Option<Section>,
    wsl: Option<Section>,
    windows: Option<Section>,
    macos: Option<Section>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    default: Option<Vec<String>>,
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    #[serde(rename = "match")]
    pattern: String,
    command: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Platform {
    Linux,
    Wsl,
    Windows,
    Macos,
    Other,
}

impl Section {
    /// The command for the first rule matching `dofile_rel`, if any.
    fn match_rule(&self, dofile_rel: &str, basename: &str) -> Option<Vec<String>> {
        for r in &self.rules {
            let text = if r.pattern.contains('/') { dofile_rel } else { basename };
            if glob_match(&r.pattern, text) {
                return Some(r.command.clone());
            }
        }
        None
    }
}

impl Config {
    /// Load `redo.toml` from the project root. A missing file yields the empty
    /// (all-defaults) configuration; a malformed file is an error.
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join("redo.toml");
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::Error::new(e).context(format!("reading {}", path.display()))),
        }
    }

    /// The platform section to consult first: the exact platform, or `linux`
    /// for WSL when no `wsl` section exists.
    fn primary_section(&self, plat: Platform) -> Option<&Section> {
        match plat {
            Platform::Wsl => self.platform.wsl.as_ref().or(self.platform.linux.as_ref()),
            Platform::Linux => self.platform.linux.as_ref(),
            Platform::Windows => self.platform.windows.as_ref(),
            Platform::Macos => self.platform.macos.as_ref(),
            Platform::Other => None,
        }
    }

    /// Resolve the interpreter argv prefix for a do-file (root-relative path).
    /// Always returns a non-empty command (worst case the built-in default).
    pub fn interpreter(&self, dofile_rel: &str) -> Vec<String> {
        let basename = dofile_rel.rsplit('/').next().unwrap_or(dofile_rel);
        if let Some(sec) = self.primary_section(current_platform()) {
            if let Some(cmd) = sec.match_rule(dofile_rel, basename) {
                return non_empty(cmd);
            }
            if let Some(d) = &sec.default {
                return non_empty(d.clone());
            }
        }
        if let Some(cmd) = self.interpreter.match_rule(dofile_rel, basename) {
            return non_empty(cmd);
        }
        if let Some(d) = &self.interpreter.default {
            return non_empty(d.clone());
        }
        vec![DEFAULT_INTERPRETER.to_string()]
    }
}

/// Guard against an empty `command = []` in the config, which would otherwise
/// produce an un-spawnable interpreter; fall back to the built-in default.
fn non_empty(cmd: Vec<String>) -> Vec<String> {
    if cmd.is_empty() {
        vec![DEFAULT_INTERPRETER.to_string()]
    } else {
        cmd
    }
}

fn current_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "linux") {
        if is_wsl() {
            Platform::Wsl
        } else {
            Platform::Linux
        }
    } else {
        Platform::Other
    }
}

/// Detect WSL: the interop env vars Microsoft sets, or a "microsoft"/"WSL"
/// marker in the kernel release string.
#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("microsoft") || s.contains("wsl")
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn is_wsl() -> bool {
    false
}

/// Glob match: `*` matches any run of non-`/`, `?` matches one non-`/`, every
/// other byte is literal. Anchored (must match the whole text).
fn glob_match(pattern: &str, text: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                if m(&p[1..], t) {
                    return true;
                }
                matches!(t.first(), Some(&c) if c != b'/') && m(p, &t[1..])
            }
            Some(b'?') => matches!(t.first(), Some(&c) if c != b'/') && m(&p[1..], &t[1..]),
            Some(&pc) => matches!(t.first(), Some(&tc) if tc == pc) && m(&p[1..], &t[1..]),
        }
    }
    m(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.do", "app.do"));
        assert!(glob_match("*.do", "a.b.c.do"));
        assert!(!glob_match("*.do", "app.dof"));
        assert!(glob_match("default.do", "default.do"));
        assert!(glob_match("*.py.do", "build.py.do"));
        assert!(!glob_match("*.py.do", "build.do"));
        // `*` does not cross a directory separator.
        assert!(!glob_match("*.do", "sub/app.do"));
        assert!(glob_match("deploy/*.do", "deploy/ship.do"));
        assert!(!glob_match("deploy/*.do", "deploy/nested/ship.do"));
        assert!(glob_match("?.do", "a.do"));
        assert!(!glob_match("?.do", "ab.do"));
    }

    fn cfg(s: &str) -> Config {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn empty_config_uses_builtin() {
        assert_eq!(Config::default().interpreter("a.do"), vec!["msh"]);
    }

    #[test]
    fn shared_default_and_rule() {
        let c = cfg(r#"
            [interpreter]
            default = ["msh"]
            [[interpreter.rule]]
            match = "*.py.do"
            command = ["python3"]
        "#);
        assert_eq!(c.interpreter("x.py.do"), vec!["python3"]);
        assert_eq!(c.interpreter("x.do"), vec!["msh"]);
    }

    #[test]
    fn basename_rule_matches_in_subdir() {
        let c = cfg(r#"
            [[interpreter.rule]]
            match = "*.do"
            command = ["sh", "-e"]
        "#);
        assert_eq!(c.interpreter("deep/sub/app.do"), vec!["sh", "-e"]);
    }

    #[test]
    fn empty_command_falls_back() {
        let c = cfg(r#"
            [interpreter]
            default = []
        "#);
        assert_eq!(c.interpreter("a.do"), vec!["msh"]);
    }
}
