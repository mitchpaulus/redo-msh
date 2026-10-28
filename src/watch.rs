//! `redo --watch`: rebuild whenever a recorded dependency changes.
//!
//! An outer loop around ordinary top-level builds. Each build runs as a fresh
//! child process (this same executable, minus `--watch`), so per-run process
//! state — the jobserver, the run trace, cached flags, an `all`/`quit` prompt
//! answer — never carries from one build into the next. After every build the
//! watch set is read back from the database: redo discovers dependencies
//! while do-files run, so the set is only known, and may change, afterwards.
//!
//! Change detection is stat polling, the one mechanism that behaves the same
//! on every platform and filesystem, including WSL drvfs, where edits made by
//! Windows programs never raise inotify events. A changed stat is only a
//! hint. What decides is the database: an item is *dirty* when its content
//! hash differs from the recorded edge, a recorded output is missing, or an
//! ifcreate path now exists — so a touch or an identical rewrite does nothing.
//!
//! Retry guard: a build that fails, or refuses to overwrite a hand-edited
//! output (watch mode never prompts; it sets `REDO_NO`), leaves its dirty
//! items dirty. Only a dirty item that *changed* since the last build started
//! triggers a rebuild, so such a state waits for the next edit instead of
//! rebuilding in a loop.

use crate::db::DepKind;
use crate::root::Root;
use crate::stamp;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Default stat-polling period; `REDO_WATCH_INTERVAL_MS` overrides it.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(500);

/// After a change is seen, wait until stats hold still for this long (an
/// editor's save or a `git checkout` touches many files in a burst)...
const SETTLE: Duration = Duration::from_millis(100);
/// ...but never longer than this.
const SETTLE_MAX: Duration = Duration::from_secs(2);

/// An item first seen after a build counts as changed during that build if
/// its mtime is at most this much older than the build's start (filesystem
/// timestamps can lag the clock slightly).
const MTIME_SLACK_NANOS: i64 = 1_000_000_000;

/// What a stat says about a path. Compared between polls to notice changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Obs {
    exists: bool,
    mtime: i64,
    size: i64,
}

fn observe(path: &Path) -> Obs {
    match fs::metadata(path) {
        Ok(m) => Obs {
            exists: true,
            mtime: stamp::mtime_nanos(&m),
            size: m.len() as i64,
        },
        Err(_) => Obs {
            exists: false,
            mtime: 0,
            size: 0,
        },
    }
}

/// How an item becomes dirty.
#[derive(Debug, Clone)]
enum Check {
    /// A source (or do-file) edge: dirty when the content differs from the
    /// hash recorded at build time. `mtime`/`size` are the edge's fast path.
    Content {
        csum: Option<String>,
        mtime: Option<i64>,
        size: Option<i64>,
    },
    /// A built output redo recorded: dirty when it is missing.
    Output,
    /// An ifcreate path (or do-file candidate): dirty when it exists.
    Absent,
}

#[derive(Debug, Clone)]
struct Item {
    /// Root-relative path, `/`-separated (the database key).
    path: String,
    check: Check,
}

/// Everything one watch cycle polls, derived from the database.
struct WatchSet {
    items: Vec<Item>,
    /// Targets visited while collecting `items`.
    nodes: Vec<String>,
}

/// Observations keyed by case-folded path (the database's NOCASE identity).
type Snapshot = HashMap<String, Obs>;

/// `redo --watch [targets...]`: build, then rebuild whenever a dependency
/// changes, until interrupted.
pub fn watch(targets: &[String], jobs: usize) -> Result<()> {
    // Killing the watcher (Ctrl-C, taskkill) must take an in-flight build
    // with it; Unix gets this from the terminal's process group.
    crate::winjob::setup();
    let cwd = std::env::current_dir()?;
    let root = Root::discover(&cwd)?;
    let default_target = ["all".to_string()];
    let names = if targets.is_empty() { &default_target[..] } else { targets };
    let requested = names
        .iter()
        .map(|t| {
            crate::paths::normalize(&root.dir, &cwd, Path::new(t))
                .ok_or_else(|| anyhow::anyhow!("{t}: target is outside the project root"))
        })
        .collect::<Result<Vec<_>>>()?;
    let exe = std::env::current_exe().context("locating the redo-msh executable")?;
    let interval = poll_interval();

    let mut w = Watcher {
        root,
        hashes: HashMap::new(),
    };
    let mut roots = requested.clone();
    let mut set = w.load(&roots, &requested)?;
    loop {
        // What the build is about to see, for the retry guard.
        let before = w.snapshot(&set);
        let started = stamp::now_nanos();
        let ok = run_build(&exe, jobs, targets)?;

        // A failed build loses edges: a parent records its ifchange edges
        // only after its deps succeed, and cleared its old ones when it
        // started. Keep watching what we watched before, plus every target
        // left uncommitted, so the edit that fixes the failure is seen.
        roots = requested.clone();
        if !ok {
            roots.extend(set.nodes.iter().cloned());
            roots.extend(w.uncommitted()?);
        }
        set = w.load(&roots, &requested)?;
        // Taken before the check below, so an edit landing between the two
        // is caught by one or the other.
        let since = stamp::now_nanos();
        let baseline = w.snapshot(&set);

        let mut changed = w.changed(&set, &before, started);
        if changed.is_empty() {
            if ok {
                eprintln!("redo-msh: watch: build succeeded");
            } else {
                eprintln!("redo-msh: watch: build failed; will retry when a dependency changes");
            }
            changed = w.wait(&mut set, baseline, since, &roots, &requested, interval)?;
            eprintln!("redo-msh: watch: changed: {}; rebuilding", summarize(&changed));
        } else {
            eprintln!(
                "redo-msh: watch: changed during the build: {}; rebuilding",
                summarize(&changed)
            );
        }
    }
}

struct Watcher {
    root: Root,
    /// Content hashes by path, valid while the stat is unchanged, so a
    /// touched-but-identical file is hashed once rather than every poll.
    hashes: HashMap<String, (Obs, Option<String>)>,
}

impl Watcher {
    fn abs(&self, rel: &str) -> PathBuf {
        self.root.dir.join(rel)
    }

    /// Collect the watch set: every recorded dependency reachable from
    /// `roots`, plus the do-file candidates of any requested target redo has
    /// never recorded (a build with no do-file fails before recording
    /// anything).
    fn load(&self, roots: &[String], requested: &[String]) -> Result<WatchSet> {
        let conn = self.root.open_db()?;
        let mut items = Vec::new();
        let mut item_keys = HashSet::new();
        let mut add = |item: Item| {
            let tag = match &item.check {
                Check::Content { csum, .. } => format!("c:{}", csum.as_deref().unwrap_or("")),
                Check::Output => "o".to_string(),
                Check::Absent => "a".to_string(),
            };
            if item_keys.insert((item.path.to_ascii_lowercase(), tag)) {
                items.push(item);
            }
        };

        // A requested target redo has no record of is waiting for a do-file
        // to appear. One that just did is reported as such (dirty), not
        // silently dropped from the set before the change can be compared.
        for t in requested {
            if is_target(&conn, t)? {
                continue;
            }
            let (found, candidates) = crate::dofile::find(&self.root, t);
            for path in candidates.into_iter().chain(found.map(|d| d.dofile_rel)) {
                add(Item { path, check: Check::Absent });
            }
        }

        let mut nodes = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: VecDeque<String> = roots.iter().cloned().collect();
        while let Some(target) = queue.pop_front() {
            if !seen.insert(target.to_ascii_lowercase()) {
                continue;
            }
            if output_recorded(&conn, &target)? {
                add(Item {
                    path: target.clone(),
                    check: Check::Output,
                });
            }
            for (kind, dep, csum, mtime, size) in edges(&conn, &target)? {
                let Some(dep) = dep else { continue };
                match kind {
                    // Neither is a file to watch: `always` targets rebuild
                    // when something else triggers a build, and uncommitted
                    // ones are retried by an edit to what they depend on.
                    DepKind::Always | DepKind::Uncommitted => {}
                    DepKind::IfCreate => add(Item {
                        path: dep,
                        check: Check::Absent,
                    }),
                    DepKind::IfChange | DepKind::DoFile => {
                        if is_target(&conn, &dep)? {
                            queue.push_back(dep);
                        } else {
                            add(Item {
                                path: dep,
                                check: Check::Content { csum, mtime, size },
                            });
                        }
                    }
                }
            }
            nodes.push(target);
        }
        Ok(WatchSet { items, nodes })
    }

    /// Targets whose last build failed or crashed before committing.
    fn uncommitted(&self) -> Result<Vec<String>> {
        let conn = self.root.open_db()?;
        let mut stmt = conn.prepare("SELECT DISTINCT target FROM deps WHERE kind = ?1")?;
        let rows = stmt
            .query_map([DepKind::Uncommitted as i64], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn snapshot(&self, set: &WatchSet) -> Snapshot {
        set.items
            .iter()
            .map(|it| (it.path.to_ascii_lowercase(), observe(&self.abs(&it.path))))
            .collect()
    }

    /// Whether `item` disagrees with what the database recorded.
    fn is_dirty(&mut self, item: &Item, obs: Obs) -> bool {
        match &item.check {
            Check::Absent => obs.exists,
            Check::Output => !obs.exists,
            Check::Content { csum, mtime, size } => {
                if !obs.exists {
                    return csum.is_some();
                }
                if *mtime == Some(obs.mtime) && *size == Some(obs.size) {
                    return false;
                }
                self.current_csum(&item.path, obs).as_deref() != csum.as_deref()
            }
        }
    }

    fn current_csum(&mut self, rel: &str, obs: Obs) -> Option<String> {
        let key = rel.to_ascii_lowercase();
        if let Some((seen, csum)) = self.hashes.get(&key) {
            if *seen == obs {
                return csum.clone();
            }
        }
        // An unreadable file hashes as None: treated as changed, which at
        // worst costs one build that reports the real error.
        let csum = stamp::stamp_file(&self.abs(rel))
            .ok()
            .flatten()
            .map(|s| s.csum);
        self.hashes.insert(key, (obs, csum.clone()));
        csum
    }

    /// Dirty items that changed since `baseline` was taken at `since`. An
    /// item missing from the baseline counts as changed only if its file was
    /// written after `since` (newly discovered dependencies were stamped
    /// during the build, so they are dirty only if edited after that).
    fn changed(&mut self, set: &WatchSet, baseline: &Snapshot, since: i64) -> Vec<String> {
        let mut out = Vec::new();
        for item in &set.items {
            let obs = observe(&self.abs(&item.path));
            let moved = match baseline.get(&item.path.to_ascii_lowercase()) {
                Some(b) => *b != obs,
                None => obs.exists && obs.mtime >= since - MTIME_SLACK_NANOS,
            };
            if moved && self.is_dirty(item, obs) && !out.contains(&item.path) {
                out.push(item.path.clone());
            }
        }
        out
    }

    /// Poll until a dependency changes from `baseline` (taken at `since`)
    /// and is dirty; return the changed paths. `set` is refreshed from the
    /// database at each wake-up, since another redo run may have brought
    /// things up to date in the meantime.
    fn wait(
        &mut self,
        set: &mut WatchSet,
        mut baseline: Snapshot,
        mut since: i64,
        roots: &[String],
        requested: &[String],
        interval: Duration,
    ) -> Result<Vec<String>> {
        eprintln!(
            "redo-msh: watch: watching {} path{} for changes (Ctrl-C to stop)",
            set.items.len(),
            if set.items.len() == 1 { "" } else { "s" }
        );
        loop {
            loop {
                std::thread::sleep(interval);
                if self.snapshot(set) != baseline {
                    break;
                }
            }
            self.settle(set);
            *set = self.load(roots, requested)?;
            // The next baseline, taken before the check for the same reason
            // as in `watch`.
            let next_since = stamp::now_nanos();
            let next = self.snapshot(set);
            let changed = self.changed(set, &baseline, since);
            if !changed.is_empty() {
                return Ok(changed);
            }
            // Touched but identical, or changed back: keep waiting.
            since = next_since;
            baseline = next;
        }
    }

    /// Wait for a burst of writes to finish.
    fn settle(&self, set: &WatchSet) {
        let deadline = Instant::now() + SETTLE_MAX;
        let mut last = self.snapshot(set);
        while Instant::now() < deadline {
            std::thread::sleep(SETTLE);
            let now = self.snapshot(set);
            if now == last {
                return;
            }
            last = now;
        }
    }
}

/// Run one ordinary top-level build of `targets` as a child process.
/// Returns whether it succeeded; its own output already explained a failure.
fn run_build(exe: &Path, jobs: usize, targets: &[String]) -> Result<bool> {
    let mut cmd = Command::new(exe);
    cmd.arg(format!("-j{jobs}")).args(targets);
    // Nobody is watching the terminal when a save triggers a build, so a
    // hand-edited output is refused and reported rather than prompted for.
    // `--yes` (already in our environment as REDO_YES) still overwrites.
    if std::env::var("REDO_YES").as_deref() != Ok("1") {
        cmd.env("REDO_NO", "1");
    }
    let status = cmd
        .status()
        .with_context(|| format!("running {}", exe.display()))?;
    Ok(status.success())
}

fn poll_interval() -> Duration {
    match std::env::var("REDO_WATCH_INTERVAL_MS") {
        Ok(v) => match v.parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => {
                eprintln!(
                    "redo-msh: warning: ignoring REDO_WATCH_INTERVAL_MS={v:?} \
                     (expected a positive number of milliseconds)"
                );
                DEFAULT_INTERVAL
            }
        },
        Err(_) => DEFAULT_INTERVAL,
    }
}

fn summarize(paths: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut s = paths.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > SHOWN {
        s.push_str(&format!(" (+{} more)", paths.len() - SHOWN));
    }
    s
}

type Edge = (DepKind, Option<String>, Option<String>, Option<i64>, Option<i64>);

fn edges(conn: &Connection, target: &str) -> Result<Vec<Edge>> {
    let mut stmt =
        conn.prepare("SELECT kind, dep, csum, mtime, size FROM deps WHERE target = ?1")?;
    let rows = stmt.query_map(params![target], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, dep, csum, mtime, size) = row?;
        if let Some(kind) = DepKind::from_i64(kind) {
            out.push((kind, dep, csum, mtime, size));
        }
    }
    Ok(out)
}

/// Whether redo builds `path`: it has a recorded do-file, or a build of it
/// was at least started (a first build that failed has edges but no do-file
/// recorded in `files` yet).
fn is_target(conn: &Connection, path: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1 AND dofile IS NOT NULL)
             OR EXISTS(SELECT 1 FROM deps WHERE target = ?1)",
        params![path],
        |r| r.get(0),
    )?)
}

/// Whether `target` is a generated target with a recorded output file
/// (phony targets record none).
fn output_recorded(conn: &Connection, target: &str) -> Result<bool> {
    let csum: Option<Option<String>> = conn
        .query_row(
            "SELECT csum FROM files WHERE path = ?1 AND dofile IS NOT NULL",
            params![target],
            |r| r.get(0),
        )
        .optional()?;
    Ok(matches!(csum, Some(Some(_))))
}
