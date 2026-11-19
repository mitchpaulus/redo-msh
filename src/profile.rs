//! `--profile`: a whole-run timing profile, written to one text file.
//!
//! A redo build is a tree of processes (the top-level `redo`, every
//! `redo-ifchange` a do-file runs, and so on down), so the profile is
//! gathered the same way the live log is: by *path*. The top-level process
//! publishes `REDO_PROFILE` (the file) and `REDO_PROFILE_T0` (the run's
//! time origin) in the environment; every redo process in the tree appends
//! its timing records to that file, each batch as a single append. When the
//! run ends the top-level process reads the records back, aggregates them,
//! and rewrites the file as a report followed by the raw records.
//!
//! Two kinds of record:
//!
//! * a **span** (`S`) is one timed occurrence — a do-file run, a lock wait,
//!   a write transaction — with its start (µs since the run began), its
//!   duration, and a name (usually the target);
//! * a **tally** (`T`) is the per-process total of something too frequent to
//!   list individually: each SQL statement, stamp-cache lookups, do-file
//!   searches.
//!
//! When profiling is off every entry point is one relaxed atomic load.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The profile file (absolute path). Set by `--profile`, or by hand.
pub const ENV: &str = "REDO_PROFILE";
/// The run's time origin, unix microseconds. Set by the process that owns
/// the profile (the outermost `redo`); its presence is what tells every
/// other process that it is a participant, not the owner.
pub const ENV_T0: &str = "REDO_PROFILE_T0";
/// File used when `--profile` is given without a path.
pub const DEFAULT_FILE: &str = "redo-profile.txt";

/// Buffered records are appended to the file once they exceed this.
const FLUSH_BYTES: usize = 32 * 1024;

struct State {
    path: PathBuf,
    t0_us: i64,
    pid: u32,
    buf: Mutex<Vec<u8>>,
    /// (category, name) -> (count, total µs, max µs)
    tallies: Mutex<HashMap<(&'static str, String), (u64, u64, u64)>>,
}

static ON: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Option<State>> = OnceLock::new();
static PROC_START: OnceLock<(Instant, i64)> = OnceLock::new();
static OWNER: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);

fn unix_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Note when this process started. Call first thing in `main`. A process
/// that finds itself inside a profiled run (both variables set) joins it.
pub fn process_start() {
    PROC_START.get_or_init(|| (Instant::now(), unix_us()));
    if std::env::var_os(ENV_T0).is_some() {
        state();
    }
}

/// `--profile[=PATH]` was given: publish the profile path (made absolute:
/// do-files run in other directories) to this process and its children.
pub fn request(path: Option<&str>) {
    let p = PathBuf::from(path.filter(|p| !p.is_empty()).unwrap_or(DEFAULT_FILE));
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
    };
    std::env::set_var(ENV, abs);
}

fn state() -> Option<&'static State> {
    STATE
        .get_or_init(|| {
            let path = PathBuf::from(std::env::var_os(ENV)?);
            let t0_us = std::env::var(ENV_T0).ok()?.parse().ok()?;
            ON.store(true, Ordering::Release);
            Some(State {
                path,
                t0_us,
                pid: std::process::id(),
                buf: Mutex::new(Vec::new()),
                tallies: Mutex::new(HashMap::new()),
            })
        })
        .as_ref()
}

/// Whether this process is recording.
#[inline]
pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Start a profiled run if one was requested and none is in progress: this
/// process becomes the owner, truncates the file, and fixes the time origin.
/// A `redo` nested inside a profiled run (the origin is already published)
/// simply participates.
pub fn begin_run() {
    if std::env::var_os(ENV).is_none() || std::env::var_os(ENV_T0).is_some() {
        return;
    }
    let (_, start_us) = *PROC_START.get_or_init(|| (Instant::now(), unix_us()));
    let path = PathBuf::from(std::env::var_os(ENV).expect("checked above"));
    if let Err(e) = std::fs::write(&path, b"") {
        eprintln!("redo-msh: --profile: cannot write {}: {e}; not profiling", path.display());
        std::env::remove_var(ENV);
        return;
    }
    std::env::set_var(ENV_T0, start_us.to_string());
    OWNER.store(true, Ordering::Release);
    FINISHED.store(false, Ordering::Release);
    if state().is_none() {
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    info("command", &args.join(" "));
    info("version", env!("CARGO_PKG_VERSION"));
    info("platform", std::env::consts::OS);
    if let Ok(cwd) = std::env::current_dir() {
        info("cwd", &cwd.display().to_string());
    }
    if let Ok(n) = std::thread::available_parallelism() {
        info("cpus", &n.to_string());
    }
}

fn clean(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains(['\t', '\n', '\r']) {
        s.replace(['\t', '\n', '\r'], " ").into()
    } else {
        s.into()
    }
}

fn push(st: &State, line: &str) {
    let mut buf = st.buf.lock().unwrap();
    buf.extend_from_slice(line.as_bytes());
    if buf.len() >= FLUSH_BYTES {
        flush_locked(st, &mut buf);
    }
}

fn flush_locked(st: &State, buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    // One append per batch, so concurrent processes' records never
    // interleave mid-line.
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&st.path) {
        let _ = f.write_all(buf);
    }
    buf.clear();
}

/// Record a fact about the run (shown in the report's header).
pub fn info(key: &str, value: &str) {
    if let Some(st) = state() {
        push(st, &format!("I\t{}\t{}\n", clean(key), clean(value)));
    }
}

/// A timed occurrence; recorded when dropped.
pub struct Span(Option<SpanInner>);

struct SpanInner {
    start: Instant,
    start_us: i64,
    cat: &'static str,
    name: String,
    detail: String,
}

/// Time one occurrence of `cat`, named `name` (usually the target).
#[inline]
pub fn span(cat: &'static str, name: &str) -> Span {
    if !enabled() {
        return Span(None);
    }
    Span(Some(SpanInner {
        start: Instant::now(),
        start_us: unix_us(),
        cat,
        name: name.to_string(),
        detail: String::new(),
    }))
}

impl Span {
    /// Attach a free-form note (lazy: costs nothing when not profiling).
    pub fn detail(&mut self, f: impl FnOnce() -> String) {
        if let Some(s) = &mut self.0 {
            s.detail = f();
        }
    }

    /// Microseconds since this span started (0 when not profiling).
    pub fn elapsed_us(&self) -> u64 {
        self.0.as_ref().map(|s| s.start.elapsed().as_micros() as u64).unwrap_or(0)
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let (Some(s), Some(st)) = (self.0.take(), state()) else { return };
        let dur = s.start.elapsed().as_micros();
        push(
            st,
            &format!(
                "S\t{}\t{}\t{}\t{}\t{}\t{}\n",
                s.start_us - st.t0_us,
                dur,
                st.pid,
                s.cat,
                clean(&s.name),
                clean(&s.detail)
            ),
        );
    }
}

/// Add one occurrence to a per-process tally.
pub fn tally_add(cat: &'static str, name: &str, dur: Duration) {
    let Some(st) = state() else { return };
    let us = dur.as_micros() as u64;
    let mut t = st.tallies.lock().unwrap();
    let e = t.entry((cat, name.to_string())).or_insert((0, 0, 0));
    e.0 += 1;
    e.1 += us;
    e.2 = e.2.max(us);
}

/// A timed occurrence that is only tallied; recorded when dropped.
pub struct Tally(Option<(Instant, &'static str, &'static str)>);

/// Time one occurrence of a frequent operation into the tally `cat`/`name`.
#[inline]
pub fn tally(cat: &'static str, name: &'static str) -> Tally {
    if !enabled() {
        return Tally(None);
    }
    Tally(Some((Instant::now(), cat, name)))
}

impl Tally {
    /// Re-file this occurrence under another name (e.g. `cached`/`hashed`).
    pub fn rename(&mut self, name: &'static str) {
        if let Some(t) = &mut self.0 {
            t.2 = name;
        }
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        if let Some((start, cat, name)) = self.0.take() {
            tally_add(cat, name, start.elapsed());
        }
    }
}

/// SQLite's per-statement profile callback (see `db::open`): tallies every
/// statement by its text, with comments dropped and whitespace collapsed.
pub fn sql_hook(sql: &str, dur: Duration) {
    let text = sql
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    tally_add("sql", &text, dur);
}

/// Record this process's lifetime and tallies and flush. Call just before
/// the process exits (and before the owner writes the report).
pub fn finish_process(what: &str) {
    let Some(st) = state() else { return };
    if FINISHED.swap(true, Ordering::AcqRel) {
        return;
    }
    let (start, start_us) = *PROC_START.get_or_init(|| (Instant::now(), unix_us()));
    let target = std::env::var("REDO_TARGET").unwrap_or_default();
    let mut out = format!(
        "S\t{}\t{}\t{}\tproc\t{}\t{}\n",
        start_us - st.t0_us,
        start.elapsed().as_micros(),
        st.pid,
        clean(if target.is_empty() { "-" } else { &target }),
        clean(what)
    );
    for ((cat, name), (count, total, max)) in st.tallies.lock().unwrap().drain() {
        let _ = writeln!(out, "T\t{count}\t{total}\t{max}\t{}\t{cat}\t{}", st.pid, clean(&name));
    }
    let mut buf = st.buf.lock().unwrap();
    buf.extend_from_slice(out.as_bytes());
    flush_locked(st, &mut buf);
}

/// A short description of this process for its `proc` record.
pub fn describe_process(verb: &str, args: &[String]) -> String {
    let mut s = format!("{verb} ({} args)", args.len());
    for a in args.iter().take(6) {
        s.push(' ');
        s.push_str(a);
    }
    if args.len() > 6 {
        s.push_str(" ...");
    }
    s
}

/// End a profiled run: the owner turns the raw records into the report.
/// No-op in every other process.
pub fn end_run() {
    if !OWNER.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(st) = state() else { return };
    let args: Vec<String> = std::env::args().skip(1).collect();
    finish_process(&describe_process("redo (top level)", &args));
    let raw = match std::fs::read(&st.path) {
        Ok(r) => String::from_utf8_lossy(&r).into_owned(),
        Err(e) => {
            eprintln!("redo-msh: --profile: cannot read {}: {e}", st.path.display());
            return;
        }
    };
    let report = render(&raw, st.pid);
    match std::fs::write(&st.path, report) {
        Ok(()) => eprintln!("redo-msh: profile written to {}", st.path.display()),
        Err(e) => eprintln!("redo-msh: --profile: cannot write {}: {e}", st.path.display()),
    }
    // A later run in this same process (tests) starts clean.
    std::env::remove_var(ENV_T0);
}

// ---- report ------------------------------------------------------------------

struct Rec<'a> {
    start: i64,
    dur: u64,
    pid: u32,
    cat: &'a str,
    name: &'a str,
    detail: &'a str,
}

#[derive(Default, Clone, Copy)]
struct Agg {
    count: u64,
    total: u64,
    max: u64,
}

impl Agg {
    fn add(&mut self, dur: u64) {
        self.count += 1;
        self.total += dur;
        self.max = self.max.max(dur);
    }
}

fn ms(us: u64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

fn mean(a: &Agg) -> String {
    if a.count == 0 {
        "-".into()
    } else {
        ms(a.total / a.count)
    }
}

/// Like `shorten`, but keeps the end (the distinguishing part of a path).
fn shorten_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(n - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// What each span category measures, for the report's legend. Categories
/// marked *inclusive* contain other spans (and waiting), so their totals
/// are not costs in themselves.
const LEGEND: &[(&str, &str)] = &[
    ("proc", "inclusive: one redo process, start of main to exit (name = REDO_TARGET it ran under)"),
    ("ctx.init", "process startup: root discovery, database open, redo.toml load, jobserver"),
    ("db.open", "opening one SQLite connection (pragmas + schema check)"),
    ("config.load", "reading redo.toml"),
    ("gc.temps / gc.waits / gc.logs", "top-level startup sweeps of crashed runs' leftovers"),
    ("top.build", "inclusive: the top-level build phase"),
    ("top.follower_tail", "time from build end until the log follower finished printing"),
    ("task", "inclusive: one target's whole ensure in one process (check, waits, build)"),
    ("check", "inclusive: deciding whether a target is out of date (includes check.wait)"),
    ("check.wait", "a checker waiting for a recorded dependency's task to settle"),
    ("token.wait", "waiting for a jobserver token (only recorded when one was not free)"),
    ("lock.wait", "waiting for another process to release the target's build lock"),
    ("build", "inclusive: build under the lock (prep + do-file + commit)"),
    ("build.prep", "from lock held to do-file spawn: guards, marker transaction, temp/log setup"),
    ("dofile.spawn", "spawning the do-file's interpreter"),
    ("dofile.run", "inclusive: the do-file, spawn to exit (includes its redo-ifchange children)"),
    ("commit", "after do-file exit: output checks, hash, fsync, rename, commit transaction"),
    ("hash", "content-hashing one file (detail = bytes)"),
    ("fsync / rename", "durability steps of the commit"),
    ("db.group", "inclusive: one group commit of queued small writes (detail = how many)"),
    ("db.txn", "one write transaction (name = call site; detail = wait for the write lock)"),
    ("ifchange.classify", "redo-ifchange: is each argument a target or a source"),
    ("ifchange.sources", "redo-ifchange: stamping source arguments"),
    ("ifchange.wait", "inclusive: redo-ifchange waiting for its dependencies to be brought up to date"),
    ("ifchange.record", "redo-ifchange: stamping deps and recording the edges"),
    ("drain", "waiting for (and abandoning) leftover speculative tasks"),
    ("waits.open", "opening the waits-graph database (.redom/waits.db, sync off)"),
    ("waits.liveness", "creating this process's liveness lock for the waits graph"),
];

fn render(raw: &str, top_pid: u32) -> String {
    let mut infos: Vec<(&str, &str)> = Vec::new();
    let mut spans: Vec<Rec> = Vec::new();
    let mut tallies: BTreeMap<(&str, &str), Agg> = BTreeMap::new();
    let mut bad = 0usize;
    for line in raw.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let ok = match f.first().copied() {
            Some("I") if f.len() == 3 => {
                infos.push((f[1], f[2]));
                true
            }
            Some("S") if f.len() == 7 => {
                match (f[1].parse(), f[2].parse(), f[3].parse()) {
                    (Ok(start), Ok(dur), Ok(pid)) => {
                        spans.push(Rec { start, dur, pid, cat: f[4], name: f[5], detail: f[6] });
                        true
                    }
                    _ => false,
                }
            }
            Some("T") if f.len() == 7 => {
                match (f[1].parse::<u64>(), f[2].parse::<u64>(), f[3].parse::<u64>()) {
                    (Ok(count), Ok(total), Ok(max)) => {
                        let a = tallies.entry((f[5], f[6])).or_default();
                        a.count += count;
                        a.total += total;
                        a.max = a.max.max(max);
                        true
                    }
                    _ => false,
                }
            }
            _ => line.is_empty(),
        };
        if !ok {
            bad += 1;
        }
    }
    spans.sort_by_key(|s| s.start);

    let mut out = String::new();
    let w = &mut out;
    let _ = writeln!(w, "# redo-msh profile\n");
    let _ = writeln!(
        w,
        "All times are wall-clock milliseconds. Spans nest and run in parallel \
         (threads and\nprocesses), so totals can exceed the elapsed time; categories \
         marked inclusive in the\nlegend contain other spans. Raw records follow the report.\n"
    );

    // ---- run ----
    let elapsed = spans
        .iter()
        .find(|s| s.cat == "proc" && s.pid == top_pid)
        .map(|s| s.dur)
        .unwrap_or(0);
    let _ = writeln!(w, "## Run\n");
    for (k, v) in &infos {
        let _ = writeln!(w, "{k:<14} {v}");
    }
    let _ = writeln!(w, "{:<14} {} ms", "elapsed", ms(elapsed));
    let procs: Vec<&Rec> = spans.iter().filter(|s| s.cat == "proc").collect();
    let mut by_verb: BTreeMap<&str, Agg> = BTreeMap::new();
    for p in &procs {
        let verb = p.detail.split(" (").next().unwrap_or(p.detail);
        by_verb.entry(verb).or_default().add(p.dur);
    }
    let _ = writeln!(w, "{:<14} {}", "processes", procs.len());
    for (verb, a) in &by_verb {
        let _ = writeln!(
            w,
            "  {:<22} {:>6} processes  total {:>10} ms  mean {:>8} ms  max {:>8} ms",
            verb,
            a.count,
            ms(a.total),
            mean(a),
            ms(a.max)
        );
    }
    if bad > 0 {
        let _ = writeln!(w, "{:<14} {bad} (skipped)", "bad records");
    }

    // ---- headline ----
    // Per-pid time spent waiting for dependencies inside redo-ifchange.
    let mut wait_by_pid: HashMap<u32, u64> = HashMap::new();
    for s in spans.iter().filter(|s| s.cat == "ifchange.wait") {
        *wait_by_pid.entry(s.pid).or_default() += s.dur;
    }
    // Per-target time its do-file spent inside child redo processes.
    let mut child_by_target: HashMap<String, u64> = HashMap::new();
    let mut child = Agg::default();
    let mut child_wait = 0u64;
    for p in procs.iter().filter(|p| p.pid != top_pid) {
        child.add(p.dur);
        child_wait += wait_by_pid.get(&p.pid).copied().unwrap_or(0).min(p.dur);
        *child_by_target.entry(p.name.to_ascii_lowercase()).or_default() += p.dur;
    }

    #[derive(Default)]
    struct T {
        name: String,
        task: u64,
        check: u64,
        check_wait: u64,
        token_wait: u64,
        lock_wait: u64,
        prep: u64,
        run: u64,
        runs: u64,
        commit: u64,
        in_child: u64,
    }
    let mut targets: HashMap<String, T> = HashMap::new();
    for s in &spans {
        let field: fn(&mut T) -> &mut u64 = match s.cat {
            "task" => |t| &mut t.task,
            "check" => |t| &mut t.check,
            "check.wait" => |t| &mut t.check_wait,
            "token.wait" => |t| &mut t.token_wait,
            "lock.wait" => |t| &mut t.lock_wait,
            "build.prep" => |t| &mut t.prep,
            "dofile.run" => |t| &mut t.run,
            "commit" => |t| &mut t.commit,
            _ => continue,
        };
        let t = targets.entry(s.name.to_ascii_lowercase()).or_default();
        if t.name.is_empty() {
            t.name = s.name.to_string();
        }
        *field(t) += s.dur;
        if s.cat == "dofile.run" {
            t.runs += 1;
        }
    }
    for (k, t) in targets.iter_mut() {
        t.in_child = child_by_target.get(k).copied().unwrap_or(0).min(t.run);
    }
    let self_of = |t: &T| t.run - t.in_child;
    // redo's own time on a target: everything in its tasks that is neither
    // the do-file nor waiting on something else.
    let redo_of = |t: &T| {
        t.task.saturating_sub(t.run + t.check_wait + t.token_wait + t.lock_wait)
    };
    let dofile_self: u64 = targets.values().map(self_of).sum();
    let task_redo: u64 = targets.values().map(redo_of).sum();
    let runs: u64 = targets.values().map(|t| t.runs).sum();
    let one = |cat: &str| -> u64 {
        spans.iter().filter(|s| s.cat == cat && s.pid == top_pid).map(|s| s.dur).sum()
    };

    let _ = writeln!(w, "\n## Headline\n");
    let _ = writeln!(w, "{:<52} {:>10} ms", "elapsed", ms(elapsed));
    let _ = writeln!(
        w,
        "{:<52} {:>10} ms   ({} do-file runs)",
        "do-files' own work, summed (run minus child redo)",
        ms(dofile_self),
        runs
    );
    let _ = writeln!(
        w,
        "{:<52} {:>10} ms   ({} processes, mean {} ms)",
        "child redo processes, summed",
        ms(child.total),
        child.count,
        mean(&child)
    );
    let _ = writeln!(w, "{:<52} {:>10} ms", "  of which waiting for dependencies", ms(child_wait));
    let own = Agg { count: child.count, total: child.total.saturating_sub(child_wait), max: 0 };
    let _ = writeln!(
        w,
        "{:<52} {:>10} ms   (mean {} ms per process)",
        "  of which the processes' own overhead",
        ms(own.total),
        mean(&own)
    );
    let _ = writeln!(
        w,
        "{:<52} {:>10} ms",
        "redo's per-target work, summed (check/prep/commit)",
        ms(task_redo)
    );
    let top_start = spans
        .iter()
        .find(|s| s.cat == "top.build" && s.pid == top_pid)
        .map(|s| s.start.max(0) as u64)
        .unwrap_or(0);
    let _ = writeln!(w, "{:<52} {:>10} ms", "top level: startup before the build phase", ms(top_start));
    let _ = writeln!(w, "{:<52} {:>10} ms", "top level: build phase", ms(one("top.build")));
    let _ = writeln!(
        w,
        "{:<52} {:>10} ms",
        "top level: log follower tail after the build",
        ms(one("top.follower_tail"))
    );

    // ---- categories ----
    let mut cats: BTreeMap<&str, Agg> = BTreeMap::new();
    for s in &spans {
        cats.entry(s.cat).or_default().add(s.dur);
    }
    let mut cat_rows: Vec<(&&str, &Agg)> = cats.iter().collect();
    cat_rows.sort_by(|a, b| b.1.total.cmp(&a.1.total));
    let _ = writeln!(w, "\n## Spans by category\n");
    let _ = writeln!(w, "{:<20} {:>8} {:>12} {:>10} {:>10}", "category", "count", "total", "mean", "max");
    for (cat, a) in cat_rows {
        let _ = writeln!(
            w,
            "{:<20} {:>8} {:>12} {:>10} {:>10}",
            cat,
            a.count,
            ms(a.total),
            mean(a),
            ms(a.max)
        );
    }

    // ---- tallies ----
    let mut tally_rows: Vec<(&(&str, &str), &Agg)> = tallies.iter().collect();
    tally_rows.sort_by(|a, b| b.1.total.cmp(&a.1.total));
    let _ = writeln!(w, "\n## Tallies (frequent operations, summed over all processes)\n");
    let _ = writeln!(
        w,
        "sql times come from SQLite's profiler, whose clock ticks in whole \
         milliseconds: counts\nare exact, totals undercount statements faster than \
         1 ms (db.txn spans are precise).\n"
    );
    let _ = writeln!(w, "{:>8} {:>12} {:>10} {:>10}  what", "count", "total", "mean", "max");
    let sql_total: Agg = tallies
        .iter()
        .filter(|((c, _), _)| *c == "sql")
        .fold(Agg::default(), |mut acc, (_, a)| {
            acc.count += a.count;
            acc.total += a.total;
            acc.max = acc.max.max(a.max);
            acc
        });
    if sql_total.count > 0 {
        let _ = writeln!(
            w,
            "{:>8} {:>12} {:>10} {:>10}  sql: ALL STATEMENTS",
            sql_total.count,
            ms(sql_total.total),
            mean(&sql_total),
            ms(sql_total.max)
        );
    }
    for ((cat, name), a) in tally_rows {
        let _ = writeln!(
            w,
            "{:>8} {:>12} {:>10} {:>10}  {}: {}",
            a.count,
            ms(a.total),
            mean(a),
            ms(a.max),
            cat,
            shorten(name, 150)
        );
    }

    // ---- targets ----
    let mut trows: Vec<&T> = targets.values().collect();
    trows.sort_by_key(|t| std::cmp::Reverse(self_of(t) + redo_of(t)));
    const TOP_TARGETS: usize = 40;
    let _ = writeln!(
        w,
        "\n## Targets (top {} of {} by do-file self + redo time)\n",
        TOP_TARGETS.min(trows.len()),
        trows.len()
    );
    let _ = writeln!(
        w,
        "self = do-file run minus time inside its child redo processes; redo = this \
         target's task\ntime that is neither do-file nor waiting (check, prep, commit, \
         bookkeeping).\n"
    );
    let _ = writeln!(
        w,
        "{:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>4}  target",
        "self", "redo", "run", "in-child", "dep-wait", "tok-wait", "lock-wait", "commit", "runs"
    );
    for t in trows.iter().take(TOP_TARGETS) {
        let _ = writeln!(
            w,
            "{:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>4}  {}",
            ms(self_of(t)),
            ms(redo_of(t)),
            ms(t.run),
            ms(t.in_child),
            ms(t.check_wait),
            ms(t.token_wait),
            ms(t.lock_wait),
            ms(t.commit),
            t.runs,
            t.name
        );
    }

    // ---- slowest leaf spans ----
    const INCLUSIVE: &[&str] = &[
        "proc", "task", "check", "build", "dofile.run", "ifchange.wait", "top.build", "check.wait",
        "db.group",
    ];
    let mut leaves: Vec<&Rec> = spans.iter().filter(|s| !INCLUSIVE.contains(&s.cat)).collect();
    leaves.sort_by_key(|s| std::cmp::Reverse(s.dur));
    let _ = writeln!(w, "\n## Slowest individual spans (inclusive categories excluded)\n");
    let _ = writeln!(w, "{:>10} {:>10} {:>8}  {:<18} name  [detail]", "dur", "start", "pid", "category");
    for s in leaves.iter().take(40) {
        let _ = writeln!(
            w,
            "{:>10} {:>10} {:>8}  {:<18} {}{}",
            ms(s.dur),
            ms(s.start.max(0) as u64),
            s.pid,
            s.cat,
            shorten_left(s.name, 70),
            if s.detail.is_empty() { String::new() } else { format!("  [{}]", shorten(s.detail, 60)) }
        );
    }

    // ---- legend + raw ----
    let _ = writeln!(w, "\n## Legend\n");
    for (cat, what) in LEGEND {
        let _ = writeln!(w, "{cat:<30} {what}");
    }
    let _ = writeln!(
        w,
        "\n## Raw records\n\n\
         S <start µs since run start> <duration µs> <pid> <category> <name> <detail>\n\
         T <count> <total µs> <max µs> <pid> <category> <name>\n\
         I <key> <value>\n\
         (tab-separated; S records sorted by start)\n"
    );
    for (k, v) in &infos {
        let _ = writeln!(w, "I\t{k}\t{v}");
    }
    for s in &spans {
        let _ = writeln!(
            w,
            "S\t{}\t{}\t{}\t{}\t{}\t{}",
            s.start, s.dur, s.pid, s.cat, s.name, s.detail
        );
    }
    for line in raw.lines().filter(|l| l.starts_with("T\t")) {
        let _ = writeln!(w, "{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_aggregates_spans_and_tallies() {
        let raw = "I\tcommand\tredo all\n\
                   S\t0\t100000\t1\tproc\t-\tredo (top level) (1 args) all\n\
                   S\t1000\t90000\t1\ttask\tall\t\n\
                   S\t2000\t80000\t1\tdofile.run\tall\tdemanded\n\
                   S\t3000\t30000\t2\tproc\tall\tifchange (1 args) a\n\
                   S\t4000\t20000\t2\tifchange.wait\tall\t\n\
                   T\t3\t600\t400\t2\tsql\tSELECT 1\n\
                   T\t2\t400\t300\t1\tsql\tSELECT 1\n\
                   garbage line\n";
        let r = render(raw, 1);
        assert!(r.contains("elapsed        100.0 ms"), "{r}");
        // 80ms run, 30ms of it inside the child redo process.
        assert!(r.contains("     50.0      10.0      80.0      30.0"), "{r}");
        // Tallies sum across processes: 5 statements, 1.0ms, max 0.4ms.
        assert!(r.contains("       5          1.0        0.2        0.4  sql: SELECT 1"), "{r}");
        assert!(r.contains("bad records    1 (skipped)"), "{r}");
        assert!(r.contains("S\t3000\t30000\t2\tproc\tall\tifchange (1 args) a"), "{r}");
    }

    #[test]
    fn disabled_spans_record_nothing() {
        // No REDO_PROFILE_T0 in the test environment: everything is a no-op.
        let mut s = span("task", "x");
        s.detail(|| unreachable!("detail must stay lazy when profiling is off"));
        assert_eq!(s.elapsed_us(), 0);
        let _t = tally("csum", "cached");
    }
}
