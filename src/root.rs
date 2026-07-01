//! Project root discovery and initialization.
//!
//! The project root is the nearest ancestor directory (starting at the cwd)
//! that either already has `.redom/redo-msh.db`, or is a `.git` root. An
//! explicit `.redom/redo-msh.db` always wins when it is at least as close as a
//! `.git` directory. `redo-msh root [dir]` initializes a root explicitly.

use crate::{db, fsguard};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const REDO_DIR: &str = ".redom";
pub const DB_NAME: &str = "redo-msh.db";

#[derive(Debug, Clone)]
pub struct Root {
    pub dir: PathBuf,
}

impl Root {
    pub fn redo_dir(&self) -> PathBuf {
        self.dir.join(REDO_DIR)
    }

    pub fn db_path(&self) -> PathBuf {
        self.redo_dir().join(DB_NAME)
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.redo_dir().join("locks")
    }

    /// Discover the root by walking up from `start`.
    pub fn discover(start: &Path) -> Result<Root> {
        let start = crate::paths::lexical_clean(&abs(start)?);
        for dir in start.ancestors() {
            if dir.join(REDO_DIR).join(DB_NAME).is_file() {
                return Ok(Root {
                    dir: dir.to_path_buf(),
                });
            }
            if dir.join(".git").exists() {
                return Ok(Root {
                    dir: dir.to_path_buf(),
                });
            }
        }
        bail!(
            "no project root found at or above {}. Run `redo-msh root` to create one, \
             or work inside a git repository.",
            start.display()
        )
    }

    /// Initialize a project root at `dir`: create `.redom/`, enforce the
    /// local-filesystem requirement, and create the database + schema.
    pub fn init(dir: &Path) -> Result<Root> {
        let dir = crate::paths::lexical_clean(&abs(dir)?);
        let redo_dir = dir.join(REDO_DIR);
        std::fs::create_dir_all(&redo_dir)
            .with_context(|| format!("creating {}", redo_dir.display()))?;
        fsguard::enforce_local(&redo_dir)?;
        std::fs::create_dir_all(dir.join(REDO_DIR).join("locks"))?;

        let root = Root { dir };
        let conn = db::open(&root.db_path())?;
        drop(conn);
        Ok(root)
    }

    /// Open the database for an already-discovered root, after enforcing the
    /// local-filesystem requirement.
    pub fn open_db(&self) -> Result<rusqlite::Connection> {
        fsguard::enforce_local(&self.redo_dir())?;
        let path = self.db_path();
        if path.is_file() {
            db::open(&path)
        } else {
            // A git root without an initialized db yet: create it lazily.
            std::fs::create_dir_all(self.redo_dir())?;
            std::fs::create_dir_all(self.locks_dir())?;
            db::open(&path)
        }
    }
}

/// Absolutize a path against the current working directory without touching
/// the filesystem (the path may not exist).
fn abs(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        let cwd = std::env::current_dir().context("getting current directory")?;
        Ok(cwd.join(p))
    }
}
