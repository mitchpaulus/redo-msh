//! Live, linearized build logs.
//!
//! Every target's do-file streams its stderr into a per-target append-only log
//! file; the recursion trace is embedded in those same files as structured
//! event lines; and a single follower thread in the top-level process — the
//! only writer to the terminal — walks the trace depth-first, replaying
//! finished targets and live-tailing the one it is on. Parallel builds never
//! interleave because only the follower prints; long-running do-files stream
//! in real time because the follower tails the log the build is writing.
//!
//! The design is apenwarr redo's log linearizer (`redo-log --recursive
//! --follow`), rebuilt on two deliberate changes:
//!
//!   * Explicit terminator. A `done` event is appended to the target's own log
//!     before the build lock is released, so the follower classifies every
//!     termination positively. The lock is probed only to detect the negative
//!     space: EOF + lock free + no `done` can mean exactly one thing — the
//!     builder died.
//!   * Events travel by path, not by inherited fd. The parent's log path is
//!     passed in `REDO_LOG_PATH`; a child appends events by opening that path.
//!     This needs no fork, no fd inheritance, and no inode check against
//!     do-files that redirect their own stderr, and is identical on Windows.
//!
//! Invariants the two halves (writer in `build.rs`, follower here) lean on:
//!
//!   * I1 Single terminal writer: only the follower prints during a build.
//!   * I2 Single live log per target: guaranteed by the per-target build lock.
//!   * I3 Log-before-trace: a `do` event is appended only after the child's
//!     log file exists (created via temp + atomic rename, never truncated in
//!     place, so a reader always sees a complete instance).
//!   * I4 Done-before-unlock: if the follower observes the lock free, the
//!     `done` line — if the builder lived — is already readable.
//!   * I5 The follower is read-only: it never writes the database, and its
//!     only lock operation is a non-blocking probe. Events are advisory
//!     display data; log paths are derived by *hashing* the target name, so a
//!     do-file line that mimics an event can at worst print garbage — it can
//!     never make the follower touch a path outside the logs directory.
//!
//! Logs are deleted as they are consumed (the follower removes each target's
//! log after replaying it; the top-level removes the run trace at exit), and a
//! lock-probing GC at startup sweeps anything a crashed run left behind. Set
//! `REDO_KEEP_LOGS=1` to keep them all for post-mortem inspection.

use crate::root::Root;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Absolute path of the current target's log, exported to do-files; a child
/// `redo-ifchange` appends its trace events there.
pub const ENV_LOG_PATH: &str = "REDO_LOG_PATH";
/// Set to `1` to keep all log files instead of deleting them as consumed.
pub const ENV_KEEP_LOGS: &str = "REDO_KEEP_LOGS";

/// The follower's frame for the top-level run trace, which is not a target.
pub const RUN_TARGET: &str = "-";

/// Event lines are appended with a single write; keeping them small keeps
/// concurrent appends (do-file stderr vs. child events) atomic in practice on
/// every platform. Target paths are far below this in any buildable project.
pub const EVENT_LINE_BYTES_MAX: usize = 4096;
/// Follower per-line buffer bound; longer runs without a newline are flushed
/// in chunks so a binary-spewing do-file cannot grow memory without bound.
pub const LINE_BYTES_MAX: usize = 64 * 1024;
/// Build recursion bound, enforced by the builder (chain length) and asserted
/// by the follower (stack depth) — the same limit, checked on both sides.
pub const DEPTH_MAX: usize = 64;

const POLL_DELAY_INITIAL_MS: u64 = 10;
const POLL_DELAY_STEP_MS: u64 = 10;
const POLL_DELAY_MAX_MS: u64 = 200;
const EVENT_SENTINEL: &str = "@REDOM1:";

pub fn keep_logs() -> bool {
    std::env::var(ENV_KEEP_LOGS).as_deref() == Ok("1")
}

pub fn logs_dir(root: &Root) -> PathBuf {
    root.redo_dir().join("logs")
}

pub fn run_log_path(root: &Root, session: i64) -> PathBuf {
    logs_dir(root).join(format!("run.{session}.log"))
}

/// Liveness sentinel for a run: the top-level holds an exclusive kernel lock
/// on this file (never on the run log itself!) for its whole lifetime, and
/// the GC probes it. It must be a file nobody ever reads or writes: Windows
/// `LockFileEx` is a *mandatory* lock, so locking the run log itself would
/// block the follower's reads and every event append against it.
pub fn run_lock_path(root: &Root, session: i64) -> PathBuf {
    logs_dir(root).join(format!("run.{session}.lock"))
}

pub fn target_log_path(root: &Root, target_rel: &str) -> PathBuf {
    // Hash-derived name: shared with the lock file (same key), collision
    // resistant, and immune to path traversal by construction.
    logs_dir(root).join(format!("t.{}.log", crate::lock::target_key(target_rel)))
}

// ---- events -------------------------------------------------------------

/// One line of structured trace embedded in a log stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Child build started: appended to the *parent's* log, after the child's
    /// log file exists (I3). The follower descends on this.
    Do { session: i64, target: String },
    /// Build finished: appended to the target's *own* log, before the build
    /// lock is released (I4). The follower pops on this.
    Done { session: i64, exit: i32, target: String },
    /// The builder is blocked on another process's build lock for `target`.
    Waiting { session: i64, target: String },
}

impl Event {
    fn encode(&self) -> String {
        let line = match self {
            Event::Do { session, target } => {
                format!("{EVENT_SENTINEL}do:{session}@ {target}\n")
            }
            Event::Done { session, exit, target } => {
                format!("{EVENT_SENTINEL}done:{session}@ {exit} {target}\n")
            }
            Event::Waiting { session, target } => {
                format!("{EVENT_SENTINEL}waiting:{session}@ {target}\n")
            }
        };
        // Targets are root-relative paths; a path long enough to violate this
        // is unbuildable on every supported filesystem long before we get
        // here, so a violation is a programmer error, not an operating error.
        assert!(line.len() <= EVENT_LINE_BYTES_MAX);
        assert!(!line[..line.len() - 1].contains('\n'));
        line
    }

    #[cfg(test)]
    fn target(&self) -> &str {
        match self {
            Event::Do { target, .. } => target,
            Event::Done { target, .. } => target,
            Event::Waiting { target, .. } => target,
        }
    }
}

/// Append one event line to `sink` (a log file path), atomically via append
/// mode. Events are advisory display data (I5): a failed append degrades the
/// live display for one target but never the build, so errors are deliberately
/// swallowed here rather than propagated into build results.
pub fn event_append(sink: &Path, event: &Event) {
    let line = event.encode();
    if let Ok(mut f) = OpenOptions::new().append(true).open(sink) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Parse one log line (without its trailing newline) as an event. Any
/// deviation — wrong sentinel, unknown kind, non-decimal fields, extra
/// fields, oversized line — means "not an event": the line is user output and
/// must be printed verbatim, never partially applied.
pub fn event_parse(line: &[u8]) -> Option<Event> {
    if line.len() > EVENT_LINE_BYTES_MAX {
        return None;
    }
    let s = std::str::from_utf8(line).ok()?;
    let rest = s.strip_prefix(EVENT_SENTINEL)?;
    let (head, payload) = rest.split_once("@ ")?;
    let (kind, session) = head.split_once(':')?;
    let session: i64 = session.parse().ok()?;
    if session < 0 || payload.is_empty() {
        return None;
    }
    match kind {
        "do" => Some(Event::Do { session, target: payload.to_string() }),
        "waiting" => Some(Event::Waiting { session, target: payload.to_string() }),
        "done" => {
            let (exit, target) = payload.split_once(' ')?;
            let exit: i32 = exit.parse().ok()?;
            if target.is_empty() {
                return None;
            }
            Some(Event::Done { session, exit, target: target.to_string() })
        }
        _ => None,
    }
}

// ---- writer side ----------------------------------------------------------

/// Create (or replace) a log file via temp + atomic rename in the same
/// directory (I3/I5): a follower holding the previous instance open keeps
/// reading a complete, immutable stream; a follower opening the path sees
/// either the old instance or the new one, never a truncated file.
pub fn create_log(path: &Path) -> Result<()> {
    let dir = path.parent().expect("log path has a parent directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).expect("log file name is utf-8"),
        std::process::id()
    ));
    File::create(&tmp).with_context(|| format!("creating log temp {}", tmp.display()))?;
    let renamed = crate::build::atomic_rename(&tmp, path);
    if renamed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    renamed?;
    assert!(path.exists()); // I3, writer half: exists before the `do` event.
    Ok(())
}

/// Guarantees the `done` terminator (I4) on every exit path of a build.
///
/// Created after the target's log exists and its `do` event is appended;
/// consumed by `commit_success()` on the one fully successful path. On every
/// other path (do-file failure, engine error, panic) `Drop` writes `done`
/// with the recorded do-file exit code, or 1 if the do-file itself succeeded
/// but the build did not commit. Both writes happen while the caller still
/// holds the build lock, which is what makes I4 hold.
pub struct DoneGuard {
    log_path: PathBuf,
    session: i64,
    target: String,
    exit: i32,
    committed: bool,
}

impl DoneGuard {
    pub fn new(log_path: PathBuf, session: i64, target: String) -> DoneGuard {
        // Pessimistic default: if we never learn an exit code, the build
        // errored before (or while) running the do-file.
        DoneGuard { log_path, session, target, exit: 1, committed: false }
    }

    pub fn record_exit(&mut self, exit: i32) {
        self.exit = exit;
    }

    pub fn commit_success(mut self) {
        assert_eq!(self.exit, 0);
        assert!(!self.committed);
        self.write_done(0);
        self.committed = true;
    }

    fn write_done(&self, exit: i32) {
        event_append(
            &self.log_path,
            &Event::Done { session: self.session, exit, target: self.target.clone() },
        );
    }
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Engine failure after a clean do-file still ends "failed".
            let exit = if self.exit == 0 { 1 } else { self.exit };
            self.write_done(exit);
        }
    }
}

// ---- follower ---------------------------------------------------------------

/// Incremental line reader over a growing file. `next_line` returns complete
/// lines (without the newline; CRLF folded) and `None` when no complete line
/// is available *yet* — EOF on a live log is a pause, not an end.
struct LineReader {
    file: File,
    buf: Vec<u8>,
}

impl LineReader {
    fn open(path: &Path) -> std::io::Result<LineReader> {
        Ok(LineReader { file: File::open(path)?, buf: Vec::new() })
    }

    fn next_line(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Some(line);
            }
            if self.buf.len() >= LINE_BYTES_MAX {
                // Bound memory: flush an endless unterminated run as a chunk.
                return Some(std::mem::take(&mut self.buf));
            }
            let mut chunk = [0u8; 8192];
            match self.file.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
    }

    /// Whatever is buffered without a terminating newline (crash flush).
    fn take_partial(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// One open log in the follower's DFS walk.
struct Frame {
    /// Root-relative target, or `RUN_TARGET` for the run trace.
    target: String,
    /// Indentation depth of this frame's own header/footer lines.
    indent: usize,
    path: PathBuf,
    reader: LineReader,
    /// A child block was printed since this frame's last own line, so its
    /// next user line is prefixed with a `(resumed)` marker.
    interrupted: bool,
}

/// Handle to the follower thread; join it after appending the run trace's
/// `done` sentinel, before printing anything else to stderr (I1).
pub struct Follower {
    handle: std::thread::JoinHandle<()>,
}

pub fn follow_start(root: Root, session: i64) -> Follower {
    Follower { handle: std::thread::spawn(move || follow_run(&root, session)) }
}

impl Follower {
    pub fn join(self) {
        if self.handle.join().is_err() {
            eprintln!("redo-msh: log follower panicked; build output may be incomplete");
        }
    }
}

/// The DFS walk: iterative, bounded by `DEPTH_MAX`, terminated by the run
/// trace's `done` sentinel (or by classifying every open frame).
fn follow_run(root: &Root, session: i64) {
    let run_path = run_log_path(root, session);
    let reader = match LineReader::open(&run_path) {
        Ok(r) => r,
        // The run log is created before this thread starts; failing to open
        // it means we can display nothing — the build itself is unaffected.
        Err(_) => return,
    };
    let mut stack: Vec<Frame> = Vec::with_capacity(DEPTH_MAX + 1);
    stack.push(Frame {
        target: RUN_TARGET.to_string(),
        indent: 0,
        path: run_path,
        reader,
        interrupted: false,
    });
    let mut visited: HashSet<String> = HashSet::new();
    let mut delay_ms = POLL_DELAY_INITIAL_MS;

    while !stack.is_empty() {
        assert!(stack.len() <= DEPTH_MAX + 1);
        match stack.last_mut().expect("stack is non-empty").reader.next_line() {
            Some(line) => {
                delay_ms = POLL_DELAY_INITIAL_MS;
                match event_parse(&line) {
                    Some(ev) => handle_event(root, session, &mut stack, &mut visited, ev, &line),
                    None => print_user_line(stack.last_mut().expect("stack is non-empty"), &line),
                }
            }
            None => {
                // No complete line available. A target frame whose build lock
                // is free and which never wrote `done` has exactly one
                // explanation: the builder died (I4).
                let top = stack.last_mut().expect("stack is non-empty");
                let builder_died =
                    top.target != RUN_TARGET && crate::lock::probe_unlocked(root, &top.target);
                if builder_died {
                    let partial = top.reader.take_partial();
                    if !partial.is_empty() {
                        print_user_line(top, &partial);
                    }
                    print_note(top.indent, &format!("redo  {}  (crashed)", top.target));
                    // Leave the file for the startup GC: without a `done` we
                    // do not know the log belongs to our session.
                    stack.pop();
                    mark_interrupted(&mut stack);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    delay_ms = (delay_ms + POLL_DELAY_STEP_MS).min(POLL_DELAY_MAX_MS);
                }
            }
        }
    }
    assert!(stack.is_empty());
}

/// All follower control flow lives here (events move the DFS; everything else
/// is printing). `raw` is the original line, reprinted verbatim when an event
/// is well-formed but does not fit the current frame (spoof-safe: display
/// only, never control flow).
fn handle_event(
    root: &Root,
    session: i64,
    stack: &mut Vec<Frame>,
    visited: &mut HashSet<String>,
    ev: Event,
    raw: &[u8],
) {
    let top = stack.last_mut().expect("stack is non-empty");
    let child_indent = if top.target == RUN_TARGET { 0 } else { top.indent + 1 };
    match ev {
        Event::Do { target, .. } => {
            print_note(child_indent, &format!("redo  {target}"));
            top.interrupted = true;
            if !visited.insert(target.to_ascii_lowercase()) {
                return; // Already traced (shared dep); header alone suffices.
            }
            if stack.len() > DEPTH_MAX {
                print_note(child_indent, &format!("redo  {target}  (too deep to trace)"));
                return;
            }
            let path = target_log_path(root, &target);
            let reader = match open_with_retry(&path) {
                Some(r) => r,
                None => {
                    // I3 violated from our side of the fence: the builder
                    // must have died between `do` and now, or its log was
                    // swept by a concurrent run's GC. Display-only loss.
                    print_note(child_indent, &format!("redo  {target}  (log missing)"));
                    return;
                }
            };
            stack.push(Frame { target, indent: child_indent, path, reader, interrupted: false });
        }
        Event::Done { session: done_session, exit, target } => {
            if target != top.target {
                // A `done` for some other target can only be user output that
                // parsed as an event; show it rather than acting on it.
                print_user_line(top, raw);
                return;
            }
            // The run trace's own sentinel gets no footer: a failed run
            // already reports its error after the follower drains.
            if exit != 0 && top.target != RUN_TARGET {
                print_note(top.indent, &format!("redo  {}  (failed, exit {exit})", top.target));
            }
            let finished = stack.pop().expect("stack is non-empty");
            drop(finished.reader); // Close before deleting (Windows).
            let ours = done_session == session && finished.target != RUN_TARGET;
            if ours && !keep_logs() {
                let _ = fs::remove_file(&finished.path);
            }
            if done_session != session && finished.target != RUN_TARGET {
                print_note(finished.indent, &format!(
                    "redo  {}  (built by another redo run)",
                    finished.target
                ));
            }
            mark_interrupted(stack);
        }
        Event::Waiting { target, .. } => {
            print_note(child_indent, &format!("redo  {target}  (waiting on lock)"));
            top.interrupted = true;
        }
    }
}

fn mark_interrupted(stack: &mut [Frame]) {
    if let Some(parent) = stack.last_mut() {
        parent.interrupted = true;
    }
}

/// One retry after a short delay covers the benign race where the follower
/// reads the `do` event in the instant between temp-create and rename.
fn open_with_retry(path: &Path) -> Option<LineReader> {
    for attempt in 0..2 {
        if let Ok(r) = LineReader::open(path) {
            return Some(r);
        }
        if attempt == 0 {
            std::thread::sleep(std::time::Duration::from_millis(POLL_DELAY_INITIAL_MS));
        }
    }
    None
}

fn print_user_line(frame: &mut Frame, line: &[u8]) {
    if frame.interrupted {
        frame.interrupted = false;
        if frame.target != RUN_TARGET {
            print_note(frame.indent, &format!("redo  {}  (resumed)", frame.target));
        }
    }
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(line);
    let _ = err.write_all(b"\n");
}

fn print_note(indent: usize, text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{}{}", "  ".repeat(indent), text);
}

// ---- startup GC ---------------------------------------------------------

/// Sweep log files left behind by crashed or killed runs. Liveness is decided
/// by lock probes, never by age: a run log is deletable iff no live top-level
/// holds its flock; a target log is deletable iff no builder holds the
/// target's build lock (the log name *is* the lock key). Deleting a finished
/// target's log out from under a concurrent run's follower costs that run a
/// "(log missing)" note — display-only, accepted, and rare.
pub fn gc_logs(root: &Root, current_session: i64) {
    if keep_logs() {
        return;
    }
    let own_prefix = format!("run.{current_session}.");
    let dir = logs_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return, // No logs directory yet: nothing to sweep.
    };
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with(&own_prefix) {
            continue; // Our own run log and lock sentinel.
        }
        let stale = if name.ends_with(".tmp") {
            true // Crashed between temp-create and rename.
        } else if let Some(session) = name
            .strip_prefix("run.")
            .and_then(|n| n.strip_suffix(".log").or_else(|| n.strip_suffix(".lock")))
        {
            // A run's log and lock sentinel are stale together, iff no live
            // top-level holds the sentinel's kernel lock.
            lock_file_is_free(&dir.join(format!("run.{session}.lock")))
        } else if let Some(key) =
            name.strip_prefix("t.").and_then(|n| n.strip_suffix(".log"))
        {
            crate::lock::probe_key_unlocked(root, key)
        } else {
            false // Not ours to judge.
        };
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Whether nobody holds a kernel lock on `path` (a lock-only sentinel file; a
/// missing file has no holder). Kernel locks are released on process death,
/// so this is a true liveness probe, never an age heuristic.
fn lock_file_is_free(path: &Path) -> bool {
    let f = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false, // Can't tell: claim held, the safe answer.
    };
    if f.try_lock_exclusive().is_ok() {
        let _ = FileExt::unlock(&f);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ev: Event) {
        let line = ev.encode();
        let stripped = line.strip_suffix('\n').unwrap().as_bytes();
        assert_eq!(event_parse(stripped), Some(ev));
    }

    #[test]
    fn event_roundtrip_every_kind() {
        roundtrip(Event::Do { session: 7, target: "sub dir/out.txt".into() });
        roundtrip(Event::Done { session: 7, exit: 0, target: "a.txt".into() });
        roundtrip(Event::Done { session: 7, exit: 127, target: "a b.txt".into() });
        roundtrip(Event::Waiting { session: 1, target: "x".into() });
        roundtrip(Event::Done { session: 3, exit: 1, target: RUN_TARGET.into() });
    }

    #[test]
    fn event_parse_rejects_malformed() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain user output",
            b"@REDOM1:do:5@ ",                     // empty payload
            b"@REDOM1:do:5@x",                     // missing "@ " separator space
            b"@REDOM1:do:@ t",                     // non-decimal session
            b"@REDOM1:do:-1@ t",                   // negative session
            b"@REDOM1:frob:5@ t",                  // unknown kind
            b"@REDOM1:done:5@ t",                  // done without exit code
            b"@REDOM1:done:5@ x t",                // non-decimal exit
            b"@REDOM1:done:5@ 0 ",                 // done with empty target
            b"@REDOM2:do:5@ t",                    // wrong version
            b" @REDOM1:do:5@ t",                   // sentinel not at start
        ];
        for c in cases {
            assert_eq!(event_parse(c), None, "should reject {:?}", String::from_utf8_lossy(c));
        }
        // Oversized lines are user output even if they start with the sentinel.
        let mut big = b"@REDOM1:do:5@ ".to_vec();
        big.extend(std::iter::repeat(b'a').take(EVENT_LINE_BYTES_MAX));
        assert_eq!(event_parse(&big), None);
    }

    #[test]
    fn event_parse_traversal_payload_is_display_only() {
        // A hostile payload parses as an event, but the path the follower
        // would open is hash-derived inside logs/ — traversal is impossible.
        let ev = event_parse(b"@REDOM1:do:5@ ../../etc/passwd").unwrap();
        let root = Root { dir: PathBuf::from("/proj") };
        let p = target_log_path(&root, ev.target());
        assert!(p.starts_with(logs_dir(&root)));
        let name = p.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("t.") && name.ends_with(".log"));
        assert_eq!(name.len(), "t.".len() + 32 + ".log".len());
    }

    /// The run liveness lock must never block the run log's readers or
    /// writers. Trivially true on Unix (flock is advisory); on Windows,
    /// kernel locks are MANDATORY, so this test fails if the lock is ever
    /// moved back onto the run log itself (the bug: an empty run trace, no
    /// live output at all, and a follower that never terminates).
    #[test]
    fn held_run_lock_does_not_block_run_log_io() {
        use std::io::Read;
        let dir = std::env::temp_dir().join(format!("redom-runlock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = Root { dir: dir.clone() };
        let session = 42;
        let run_log = run_log_path(&root, session);
        let run_lock = run_lock_path(&root, session);
        fs::create_dir_all(logs_dir(&root)).unwrap();
        assert_ne!(run_log, run_lock);

        let lock_file = File::create(&run_lock).unwrap();
        lock_file.lock_exclusive().unwrap();
        File::create(&run_log).unwrap();

        // With the lock held: appends land and reads see them.
        let ev = Event::Do { session, target: "a.txt".into() };
        event_append(&run_log, &ev);
        let mut contents = String::new();
        File::open(&run_log).unwrap().read_to_string(&mut contents).unwrap();
        assert_eq!(event_parse(contents.trim_end().as_bytes()), Some(ev));

        // And the GC still proves the run live via the sentinel.
        assert!(!super::lock_file_is_free(&run_lock));
        FileExt::unlock(&lock_file).unwrap();
        assert!(super::lock_file_is_free(&run_lock));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_and_lock_share_one_key() {
        // The follower probes the lock for the log it is tailing; the two
        // artifacts must never disagree about which target they name.
        let root = Root { dir: PathBuf::from("/proj") };
        let log = target_log_path(&root, "Sub/Out.txt");
        let lock = crate::lock::lock_path(&root, "sub/out.txt");
        let log_key = log.file_name().unwrap().to_str().unwrap();
        let lock_key = lock.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            log_key.trim_start_matches("t.").trim_end_matches(".log"),
            lock_key.trim_end_matches(".lock")
        );
    }
}
