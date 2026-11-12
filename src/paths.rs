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

/// Express the root-relative key `target` relative to the root-relative
/// directory key `from` (`""` = the root), with `/` separators — the inverse
/// of `normalize` for a cwd at `from`. Directory components are matched
/// case-insensitively, as the database does.
pub fn relative_key(from: &str, target: &str) -> String {
    let from: Vec<&str> = from.split('/').filter(|c| !c.is_empty()).collect();
    let to: Vec<&str> = target.split('/').filter(|c| !c.is_empty()).collect();
    // The target's last component is its name, never a shared directory.
    let dirs = to.len().saturating_sub(1);
    let common = from
        .iter()
        .zip(&to[..dirs])
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_key_inverts_normalize() {
        assert_eq!(relative_key("", "a/b.txt"), "a/b.txt");
        assert_eq!(relative_key("a", "a/b.txt"), "b.txt");
        assert_eq!(relative_key("A", "a/b.txt"), "b.txt");
        assert_eq!(relative_key("a/c", "a/b.txt"), "../b.txt");
        assert_eq!(relative_key("x/y", "top.txt"), "../../top.txt");
        // A directory named like the target is not a shared prefix.
        assert_eq!(relative_key("a", "a"), "../a");
    }

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
