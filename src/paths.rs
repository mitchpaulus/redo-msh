//! Path normalization.
//!
//! Dependency identity in the database is a path **relative to the project
//! root**, always using `/` as the separator, so a build tree is portable
//! across operating systems and the same file always hashes to the same key.
//!
//! Normalization is *logical* (lexical): we never call `canonicalize`, because
//! targets routinely do not exist yet. We resolve a possibly-relative input
//! against a base directory and then clean out `.` and `..` components without
//! touching the filesystem.

use std::path::{Component, Path, PathBuf};

/// Lexically clean a path: drop `.`, resolve `..` against prior components, and
/// collapse redundant separators. Does not touch the filesystem.
pub fn lexical_clean(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                match out.last() {
                    // Pop a normal component, but never pop past a root/prefix.
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::ParentDir) | None => out.push(comp),
                    // At a root or prefix, `..` is a no-op.
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                    Some(Component::CurDir) => unreachable!("CurDir never pushed"),
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Resolve `input` (relative to `base` if not absolute) and clean it.
pub fn resolve(base: &Path, input: &Path) -> PathBuf {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        base.join(input)
    };
    lexical_clean(&joined)
}

/// Convert an absolute, cleaned path into a root-relative DB key using `/`
/// separators. Returns `None` if `abs` is not within `root`.
pub fn to_root_relative(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut key = String::new();
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(&s.to_string_lossy());
        }
    }
    Some(key)
}

/// Normalize an `input` path (as typed on the command line, relative to `cwd`)
/// into a root-relative `/`-separated DB key.
pub fn normalize(root: &Path, cwd: &Path, input: &Path) -> Option<String> {
    let abs = resolve(cwd, input);
    to_root_relative(root, &abs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_dot_and_dotdot() {
        assert_eq!(lexical_clean(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(lexical_clean(Path::new("a/b/../../c")), PathBuf::from("c"));
        assert_eq!(lexical_clean(Path::new("../a")), PathBuf::from("../a"));
    }

    #[test]
    fn root_relative_keys_use_forward_slash() {
        let root = Path::new("/proj");
        let abs = Path::new("/proj/sub/app.txt");
        assert_eq!(to_root_relative(root, abs).as_deref(), Some("sub/app.txt"));
    }

    #[test]
    fn normalize_resolves_against_cwd() {
        let root = Path::new("/proj");
        let cwd = Path::new("/proj/sub");
        assert_eq!(
            normalize(root, cwd, Path::new("../top.txt")).as_deref(),
            Some("top.txt")
        );
        assert_eq!(
            normalize(root, cwd, Path::new("a.txt")).as_deref(),
            Some("sub/a.txt")
        );
    }

    #[test]
    fn outside_root_is_none() {
        let root = Path::new("/proj");
        let cwd = Path::new("/proj");
        assert_eq!(normalize(root, cwd, Path::new("../escape")), None);
    }
}
