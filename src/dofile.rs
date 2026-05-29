//! do-file resolution.
//!
//! Mirrors apenwarr redo's `possible_do_files`, with one deliberate deviation:
//! the ascent stops at the **project root** rather than the filesystem root,
//! so a redo project embedded inside another never escapes its own root.
//!
//! Search order for target `sub/app.txt` under root `R`:
//!   1. `R/sub/app.txt.do`                       (exact, in target's dir)
//!   2. `R/sub/default.txt.do`, `R/sub/default.do`   (defaults, target's dir)
//!   3. `R/default.txt.do`, `R/default.do`           (defaults, each ancestor
//!      up to the root), with the matched basename carrying the subdir prefix.
//!
//! Every candidate that does *not* exist becomes an `ifcreate` dependency of
//! the target (creating a more-specific .do later invalidates the target).

use crate::root::Root;
use std::path::PathBuf;

/// A resolved do-file and the arguments to invoke it with.
#[derive(Debug, Clone)]
pub struct DoFile {
    /// Absolute path to the chosen `.do` file.
    pub dofile_abs: PathBuf,
    /// Root-relative path of the chosen `.do` file (DB key).
    pub dofile_rel: String,
    /// Directory the do-file runs in (absolute) = the dir containing the .do.
    pub dodir_abs: PathBuf,
    /// `$1`: target path relative to `dodir` (may include a subdir prefix).
    pub arg_target: String,
    /// `$2`: `arg_target` with the matched extension removed.
    pub arg_base: String,
}

/// A single candidate considered during resolution.
struct Candidate {
    dofile_rel: String,
    dodir_rel: String, // root-relative dir of the do-file ("" = root)
    arg_target: String,
    arg_base: String,
}

/// Default do-file forms for a filename, longest-extension first.
/// `app.txt` -> [(default.txt.do, app, .txt), (default.do, app.txt, )].
fn default_do_files(filename: &str) -> Vec<(String, String, String)> {
    let parts: Vec<&str> = filename.split('.').collect();
    let mut out = Vec::new();
    for i in 1..=parts.len() {
        let basename = parts[..i].join(".");
        let ext_parts = &parts[i..];
        let dofile = if ext_parts.is_empty() {
            "default.do".to_string()
        } else {
            format!("default.{}.do", ext_parts.join("."))
        };
        out.push((dofile, basename, ext_parts.join(".")));
    }
    out
}

fn join_rel(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    }
}

/// Ordered list of candidate do-files for a root-relative target.
fn candidates(target_rel: &str) -> Vec<Candidate> {
    let parts: Vec<&str> = target_rel.split('/').filter(|s| !s.is_empty()).collect();
    let (dir_parts, name) = match parts.split_last() {
        Some((name, dir)) => (dir.to_vec(), *name),
        None => return Vec::new(),
    };
    let target_dir_rel = dir_parts.join("/");

    let mut out = Vec::new();
    // 1. Exact `name.do` in the target's own directory.
    out.push(Candidate {
        dofile_rel: join_rel(&target_dir_rel, &format!("{name}.do")),
        dodir_rel: target_dir_rel.clone(),
        arg_target: name.to_string(),
        arg_base: name.to_string(),
    });

    // 2+. default.*.do in the target's dir, then each ancestor up to the root.
    for k in (0..=dir_parts.len()).rev() {
        let dodir_rel = dir_parts[..k].join("/");
        let subdir = dir_parts[k..].join("/");
        for (dofile, basename, _ext) in default_do_files(name) {
            out.push(Candidate {
                dofile_rel: join_rel(&dodir_rel, &dofile),
                dodir_rel: dodir_rel.clone(),
                arg_target: join_rel(&subdir, name),
                arg_base: join_rel(&subdir, &basename),
            });
        }
    }
    out
}

/// Find the do-file for `target_rel`, returning the match plus the absent
/// candidates that preceded it. Returns `Ok(None)` if no do-file exists (the
/// target is a source file), with all candidates collected as `absent`.
pub fn find(root: &Root, target_rel: &str) -> (Option<DoFile>, Vec<String>) {
    let mut absent = Vec::new();
    for c in candidates(target_rel) {
        let dofile_abs = root.dir.join(&c.dofile_rel);
        if dofile_abs.is_file() {
            return (
                Some(DoFile {
                    dofile_abs,
                    dofile_rel: c.dofile_rel,
                    dodir_abs: root.dir.join(&c.dodir_rel),
                    arg_target: c.arg_target,
                    arg_base: c.arg_base,
                }),
                absent,
            );
        }
        absent.push(c.dofile_rel);
    }
    (None, absent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rels(target: &str) -> Vec<String> {
        candidates(target).into_iter().map(|c| c.dofile_rel).collect()
    }

    #[test]
    fn flat_target() {
        assert_eq!(
            rels("app.txt"),
            vec!["app.txt.do", "default.txt.do", "default.do"]
        );
    }

    #[test]
    fn nested_target_ascends_to_root() {
        assert_eq!(
            rels("sub/app.txt"),
            vec![
                "sub/app.txt.do",
                "sub/default.txt.do",
                "sub/default.do",
                "default.txt.do",
                "default.do",
            ]
        );
    }

    #[test]
    fn multi_extension() {
        assert_eq!(
            rels("a.b.c"),
            vec!["a.b.c.do", "default.b.c.do", "default.c.do", "default.do"]
        );
    }

    #[test]
    fn arg_base_strips_matched_ext() {
        let c = candidates("sub/app.txt");
        // sub/default.txt.do matches basename "app" -> arg_base "app", arg_target "app.txt"
        let txt = c.iter().find(|c| c.dofile_rel == "sub/default.txt.do").unwrap();
        assert_eq!(txt.arg_target, "app.txt");
        assert_eq!(txt.arg_base, "app");
        // root default.do matches with subdir prefix
        let root_def = c.iter().find(|c| c.dofile_rel == "default.do").unwrap();
        assert_eq!(root_def.arg_target, "sub/app.txt");
        assert_eq!(root_def.arg_base, "sub/app.txt");
    }
}
