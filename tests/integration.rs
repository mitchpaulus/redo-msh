//! End-to-end integration tests, ported from apenwarr redo's `t/` suite.
//!
//! The do-files are written in `mshell` (redo-msh's only do-file form); the
//! harness and assertions are Rust so the suite runs under `cargo test` on
//! every platform (Linux and Windows CI). Each test drives the real `redo-msh`
//! binary, which spawns real `msh` do-files, which call back into `redo-msh` —
//! exercising the full process tree.
//!
//! The do-files use only mshell built-ins (`readFile`/`writeFile`/`appendFile`/
//! `wl`), never external Unix tools, so they behave identically on both OSes.
//!
//! Requires `msh` on PATH; tests skip (pass) gracefully when it is absent.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_redo-msh");

/// Whether `msh` is available on PATH; tests no-op when it is not.
fn msh_available() -> bool {
    let exe = if cfg!(windows) { "msh.exe" } else { "msh" };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).exists()))
        .unwrap_or(false)
}

/// Guard at the top of each test. Returns true if the test should skip.
fn skip() -> bool {
    if !msh_available() {
        eprintln!("SKIP: `msh` not found on PATH");
        return true;
    }
    false
}

/// A throwaway project rooted in a unique temp directory.
struct Project {
    dir: PathBuf,
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Project {
    fn new() -> Project {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("redo-msh-it-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = Project { dir };
        let out = p.redo(&["root", "."]);
        assert!(out.status.success(), "redo-msh root failed: {}", stderr(&out));
        p
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.dir.join(rel).exists()
    }

    fn lines(&self, rel: &str) -> usize {
        std::fs::read_to_string(self.dir.join(rel))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Run `redo-msh <args>` in the project, with the binary's own directory on
    /// PATH (so do-files can invoke `redo-msh`). Uses the platform PATH
    /// separator via `join_paths`.
    fn redo(&self, args: &[&str]) -> Output {
        let bin_dir = Path::new(BIN).parent().unwrap();
        let mut paths = vec![bin_dir.to_path_buf()];
        if let Some(p) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&p));
        }
        let path = std::env::join_paths(paths).expect("join PATH");
        Command::new(BIN)
            .args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .output()
            .expect("failed to spawn redo-msh")
    }

    fn stdout_of(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.redo(args).stdout).into_owned()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

// ---- ported scenarios -------------------------------------------------------

/// Basic build from sources, then incremental: no-op rebuild, edit a source.
/// (ports the spirit of t/350-deps and t/110-compile)
#[test]
fn deps_and_incremental() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "alpha\n");
    p.write("b.txt", "beta\n");
    p.write(
        "app.txt.do",
        "args :2: out!\n\"r\\n\" \"app.log\" appendFile\n[redo-msh ifchange a.txt b.txt]!\n\"a.txt\" readFile @out writeFile\n\"b.txt\" readFile @out appendFile\n",
    );

    assert!(p.redo(&["ifchange", "app.txt"]).status.success());
    assert_eq!(p.read("app.txt"), "alpha\nbeta\n");
    assert_eq!(p.lines("app.log"), 1);

    // No-op: nothing changed -> no rebuild.
    assert!(p.redo(&["ifchange", "app.txt"]).status.success());
    assert_eq!(p.lines("app.log"), 1);

    // Edit a source -> rebuild, new content.
    p.write("a.txt", "ALPHA\n");
    assert!(p.redo(&["ifchange", "app.txt"]).status.success());
    assert_eq!(p.lines("app.log"), 2);
    assert_eq!(p.read("app.txt"), "ALPHA\nbeta\n");
}

/// A rebuild that produces byte-identical output must not cascade downstream
/// (the automatic redo-stamp replacement). Ports the spirit of t/s60-stamp.
#[test]
fn identical_rebuild_prunes_downstream() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "v1\n");
    // gen emits a CONSTANT regardless of a.txt, so its output never changes.
    p.write(
        "gen.txt.do",
        "args :2: out!\n\"r\\n\" \"gen.log\" appendFile\n[redo-msh ifchange a.txt]!\n\"GEN\" @out writeFile\n",
    );
    p.write(
        "app.txt.do",
        "args :2: out!\n\"r\\n\" \"app.log\" appendFile\n[redo-msh ifchange gen.txt]!\n\"gen.txt\" readFile @out writeFile\n",
    );

    assert!(p.redo(&["ifchange", "app.txt"]).status.success());
    assert_eq!(p.lines("gen.log"), 1);
    assert_eq!(p.lines("app.log"), 1);

    // Change a.txt: gen rebuilds, but its output is identical -> app stays.
    p.write("a.txt", "v2\n");
    assert!(p.redo(&["ifchange", "app.txt"]).status.success());
    assert_eq!(p.lines("gen.log"), 2, "gen should rebuild");
    assert_eq!(p.lines("app.log"), 1, "app must NOT rebuild on identical gen output");
}

/// Rewriting a source with identical bytes (new mtime) does not rebuild.
#[test]
fn touch_does_not_rebuild() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "data\n");
    p.write(
        "out.txt.do",
        "args :2: out!\n\"r\\n\" \"out.log\" appendFile\n[redo-msh ifchange a.txt]!\n\"a.txt\" readFile @out writeFile\n",
    );
    assert!(p.redo(&["ifchange", "out.txt"]).status.success());
    assert_eq!(p.lines("out.log"), 1);

    p.write("a.txt", "data\n"); // identical content, new mtime
    assert!(p.redo(&["ifchange", "out.txt"]).status.success());
    assert_eq!(p.lines("out.log"), 1, "identical content must not rebuild");
}

/// default.EXT.do builds a target, and creating a more-specific .do invalidates
/// it via the recorded ifcreate dependency. (ports t/120-defaults, t/220-ifcreate)
#[test]
fn defaults_and_ifcreate() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "default.dat.do",
        "\"d\\n\" \"dat.log\" appendFile\n\"from-default\" wl\n",
    );
    p.write("driver.do", "[redo-msh ifchange foo.dat]!\n");

    assert!(p.redo(&["ifchange", "driver"]).status.success());
    assert_eq!(p.read("foo.dat"), "from-default\n");
    assert_eq!(p.lines("dat.log"), 1);

    // No-op.
    assert!(p.redo(&["ifchange", "driver"]).status.success());
    assert_eq!(p.lines("dat.log"), 1);

    // Creating a specific foo.dat.do invalidates foo.dat (ifcreate fired).
    p.write("foo.dat.do", "\"s\\n\" \"dat.log\" appendFile\n\"from-specific\" wl\n");
    assert!(p.redo(&["ifchange", "driver"]).status.success());
    assert_eq!(p.read("foo.dat"), "from-specific\n");
    assert_eq!(p.lines("dat.log"), 2);
}

/// `redo-msh always` rebuilds the target on every run. (ports t/640-always)
#[test]
fn always_rebuilds() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "stamp.do",
        "[redo-msh always]!\n\"x\\n\" \"stamp.log\" appendFile\n\"v\" wl\n",
    );
    p.write("top.do", "[redo-msh ifchange stamp]!\n");
    for expected in 1..=3 {
        assert!(p.redo(&["ifchange", "top"]).status.success());
        assert_eq!(p.lines("stamp.log"), expected);
    }
}

/// A dependency cycle errors cleanly instead of hanging. (ports t/355-deps-cyclic)
#[test]
fn cycle_errors_without_hang() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.do", "[redo-msh ifchange b]!\n\"a\" wl\n");
    p.write("b.do", "[redo-msh ifchange a]!\n\"b\" wl\n");
    let out = p.redo(&["a"]);
    assert!(!out.status.success(), "cycle must fail");
    assert!(
        stderr(&out).contains("cycle"),
        "expected a cycle error, got: {}",
        stderr(&out)
    );
}

/// A phony target (no $3 and no stdout) produces no file but succeeds.
#[test]
fn phony_target() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "x\n");
    p.write("real.txt.do", "args :2: out!\n\"a.txt\" readFile @out writeFile\n");
    p.write("all.do", "[redo-msh ifchange real.txt]!\n");
    assert!(p.redo(&["all"]).status.success());
    assert!(p.exists("real.txt"));
    assert!(!p.exists("all"), "phony target must not create a file");
}

/// Writing to both stdout and $3 is an error. (ports the spirit of t/200-shell)
#[test]
fn stdout_and_dollar3_is_error() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "bad.txt.do",
        "args :2: out!\n\"to3\\n\" @out writeFile\n\"toStdout\" wl\n",
    );
    let out = p.redo(&["bad.txt"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("stdout") && stderr(&out).contains("$3"),
        "expected stdout/$3 error, got: {}",
        stderr(&out)
    );
}

/// A parallel diamond builds the shared node exactly once. (ports the spirit of
/// t/010-jobserver)
#[test]
fn parallel_diamond_builds_shared_once() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "gen.txt.do",
        "args :2: out!\n\"g\\n\" \"gen.log\" appendFile\n\"GEN\" @out writeFile\n",
    );
    for i in 1..=6 {
        p.write(
            &format!("leaf{i}.txt.do"),
            "args :2: out!\n[redo-msh ifchange gen.txt]!\n\"gen.txt\" readFile @out writeFile\n",
        );
    }
    let leaves: String = (1..=6).map(|i| format!("leaf{i}.txt ")).collect();
    p.write("all.do", &format!("[redo-msh ifchange {leaves}]!\n"));

    assert!(p.redo(&["-j6", "all"]).status.success());
    assert_eq!(p.lines("gen.log"), 1, "shared gen must build exactly once");
    for i in 1..=6 {
        assert!(p.exists(&format!("leaf{i}.txt")));
    }
}

/// A hand-edited target is not silently overwritten; --yes overrides.
#[test]
fn manual_edit_protected() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "src\n");
    p.write(
        "out.txt.do",
        "args :2: out!\n[redo-msh ifchange a.txt]!\n\"a.txt\" readFile @out writeFile\n",
    );
    assert!(p.redo(&["out.txt"]).status.success());

    std::fs::write(p.dir.join("out.txt"), "HAND\n").unwrap();
    // Non-tty, no --yes: must refuse and preserve the edit.
    let out = p.redo(&["out.txt"]);
    assert!(!out.status.success());
    assert_eq!(p.read("out.txt"), "HAND\n");

    // --yes overwrites.
    assert!(p.redo(&["--yes", "out.txt"]).status.success());
    assert_eq!(p.read("out.txt"), "src\n");
}

/// `sources`, `targets`, and `ood` introspection.
#[test]
fn introspection() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a.txt", "1\n");
    p.write(
        "out.txt.do",
        "args :2: out!\n[redo-msh ifchange a.txt]!\n\"a.txt\" readFile @out writeFile\n",
    );
    assert!(p.redo(&["out.txt"]).status.success());

    let sources = p.stdout_of(&["sources"]);
    let targets = p.stdout_of(&["targets"]);
    assert!(sources.lines().any(|l| l == "a.txt"), "sources: {sources:?}");
    assert!(targets.lines().any(|l| l == "out.txt"), "targets: {targets:?}");

    // Up to date -> no ood output.
    let ood = p.stdout_of(&["ood"]);
    assert!(ood.trim().is_empty(), "expected nothing ood, got: {ood:?}");

    // Change the source -> out.txt becomes ood (without building).
    p.write("a.txt", "2\n");
    let ood = p.stdout_of(&["ood"]);
    assert!(ood.lines().any(|l| l == "out.txt"), "expected out.txt ood, got: {ood:?}");
}
