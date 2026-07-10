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
//!
//! mshell note: external commands are built as argument *lists* and executed
//! with `!`/`;`/`?`. A bare token in a list can collide with an mshell builtin
//! or keyword (e.g. `mkdir`, or `maybe` — the Optional/`maybe` stack op), so we
//! quote *every* item: `['redo-msh' 'ifchange' 'a.txt']!`. This is the safe
//! form and lets target/dep names be anything. The same collision applies to
//! *variable* names: `o` is mshell's stdout-redirect operator, so binding it
//! (`args :2: o!`) fails on the pinned mshell with "Cannot set stdout behavior";
//! bind a longer name like `out` instead. To create a target's parent
//! directory (redo, like apenwarr, does not do this for you) a do-file uses the
//! `mkdirp` builtin (`"sub/deep" mkdirp`).

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
        self.redo_cmd(args).output().expect("failed to spawn redo-msh")
    }

    /// The configured-but-unspawned command, for tests that need extra env
    /// vars or incremental stderr. Ambient log-related variables are cleared
    /// so a developer's shell cannot change test behavior.
    fn redo_cmd(&self, args: &[&str]) -> Command {
        let bin_dir = Path::new(BIN).parent().unwrap();
        let mut paths = vec![bin_dir.to_path_buf()];
        if let Some(p) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&p));
        }
        let path = std::env::join_paths(paths).expect("join PATH");
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .current_dir(&self.dir)
            .env("PATH", path)
            .env_remove("REDO_KEEP_LOGS")
            .env_remove("REDO_LOG_PATH");
        cmd
    }

    /// Names of files currently in `.redom/logs`.
    fn log_files(&self) -> Vec<String> {
        std::fs::read_dir(self.dir.join(".redom").join("logs"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
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
        "args :2: out!\n\"r\\n\" \"app.log\" appendFile\n['redo-msh' 'ifchange' 'a.txt' 'b.txt']!\n\"a.txt\" readFile @out writeFile\n\"b.txt\" readFile @out appendFile\n",
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
        "args :2: out!\n\"r\\n\" \"gen.log\" appendFile\n['redo-msh' 'ifchange' 'a.txt']!\n\"GEN\" @out writeFile\n",
    );
    p.write(
        "app.txt.do",
        "args :2: out!\n\"r\\n\" \"app.log\" appendFile\n['redo-msh' 'ifchange' 'gen.txt']!\n\"gen.txt\" readFile @out writeFile\n",
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
        "args :2: out!\n\"r\\n\" \"out.log\" appendFile\n['redo-msh' 'ifchange' 'a.txt']!\n\"a.txt\" readFile @out writeFile\n",
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
    p.write("driver.do", "['redo-msh' 'ifchange' 'foo.dat']!\n");

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

/// An existing file that redo never generated is a source, even when a
/// matching `default.*.do` exists (djb's static-file rule, apenwarr
/// builder.py `_start_self`): the default rule must not run on it, must not
/// overwrite it, and must not drag in the rule's dependencies. A *missing*
/// file with the same extension still builds via the default rule.
#[test]
fn existing_source_ignores_default_rule() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("data.csv", "a,b\n1,2\n");
    // If this rule ever ran on data.csv it would fail (missing-input has no
    // rule and no file) and would log.
    p.write(
        "default.csv.do",
        "\"d\\n\" \"csv.log\" appendFile\n['redo-msh' 'ifchange' 'missing-input']!\n\"generated\" wl\n",
    );
    p.write("driver.do", "['redo-msh' 'ifchange' 'data.csv']!\n");

    let out = p.redo(&["ifchange", "driver"]);
    assert!(out.status.success(), "driver failed: {}", stderr(&out));
    assert_eq!(p.read("data.csv"), "a,b\n1,2\n", "source must not be overwritten");
    assert!(!p.exists("csv.log"), "default.csv.do must not run on an existing source");

    // Naming the source explicitly at the command line states unambiguous
    // intent to build it, so the static rule no longer silently no-ops (that
    // hid "why didn't my target rebuild?" whenever the db forgot a target was
    // generated). Instead the overwrite guard refuses, loudly, and leaves the
    // file alone; --yes would rebuild and adopt it.
    let out = p.redo(&["data.csv"]);
    assert!(!out.status.success(), "explicit redo of a source must refuse, not no-op");
    assert!(
        stderr(&out).contains("no record of ever building"),
        "refusal must explain itself: {}",
        stderr(&out)
    );
    assert_eq!(p.read("data.csv"), "a,b\n1,2\n", "refusal must not touch the file");
    assert!(!p.exists("csv.log"), "default.csv.do must not run without --yes");

    // A missing .csv target still builds via the default rule.
    p.write("missing-input", "x\n");
    let out = p.redo(&["ifchange", "gen.csv"]);
    assert!(out.status.success(), "gen.csv failed: {}", stderr(&out));
    assert_eq!(p.read("gen.csv"), "generated\n");
    assert_eq!(p.lines("csv.log"), 1);
}

/// An existing never-generated file with an *exact* `<target>.do` stays a
/// target (deliberate deviation from apenwarr, which treats it as static):
/// redo must not silently clobber the user's file — non-interactively it
/// fails with an explanatory error; `--yes` overwrites and adopts it.
#[test]
fn existing_source_with_exact_dofile_refuses_then_adopts() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("report.txt", "user data\n");
    p.write("report.txt.do", "\"built\" wl\n");

    // Non-tty, no --yes: refuse, explain, preserve the file.
    let out = p.redo(&["ifchange", "report.txt"]);
    assert!(!out.status.success(), "must refuse to overwrite a user-created file");
    assert!(
        stderr(&out).contains("no record of ever building"),
        "error should explain the conflict: {}",
        stderr(&out)
    );
    assert_eq!(p.read("report.txt"), "user data\n");

    // --yes overwrites and adopts it as a generated target.
    let out = p.redo(&["--yes", "report.txt"]);
    assert!(out.status.success(), "--yes build failed: {}", stderr(&out));
    assert_eq!(p.read("report.txt"), "built\n");

    // From here on it is a normal generated target: no prompt, no error.
    let out = p.redo(&["ifchange", "report.txt"]);
    assert!(out.status.success(), "rebuild failed: {}", stderr(&out));
    assert_eq!(p.read("report.txt"), "built\n");
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
        "['redo-msh' 'always']!\n\"x\\n\" \"stamp.log\" appendFile\n\"v\" wl\n",
    );
    p.write("top.do", "['redo-msh' 'ifchange' 'stamp']!\n");
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
    p.write("a.do", "['redo-msh' 'ifchange' 'b']!\n\"a\" wl\n");
    p.write("b.do", "['redo-msh' 'ifchange' 'a']!\n\"b\" wl\n");
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
    p.write("all.do", "['redo-msh' 'ifchange' 'real.txt']!\n");
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
            "args :2: out!\n['redo-msh' 'ifchange' 'gen.txt']!\n\"gen.txt\" readFile @out writeFile\n",
        );
    }
    let leaves: String = (1..=6).map(|i| format!("'leaf{i}.txt' ")).collect();
    p.write("all.do", &format!("['redo-msh' 'ifchange' {leaves}]!\n"));

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
        "args :2: out!\n['redo-msh' 'ifchange' 'a.txt']!\n\"a.txt\" readFile @out writeFile\n",
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
        "args :2: out!\n['redo-msh' 'ifchange' 'a.txt']!\n\"a.txt\" readFile @out writeFile\n",
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

// ---- additional ported scenarios (apenwarr t/) -----------------------------

/// `$1`/`$2`/`$3` reach the do-file with the right values. For a
/// `default.EXT.do`, `$2` is the target with the matched extension stripped;
/// for an exact `name.do`, `$2 == $1`. (ports t/100-args)
#[test]
fn args_passed_to_dofile() {
    if skip() {
        return;
    }
    let p = Project::new();
    // default.args.do builds foo.args; $2 should be "foo".
    p.write(
        "default.args.do",
        "args :0: a1!\nargs :1: a2!\n@a1 \"got1\" writeFile\n@a2 \"got2\" writeFile\n",
    );
    assert!(p.redo(&["foo.args"]).status.success());
    assert_eq!(p.read("got1"), "foo.args");
    assert_eq!(p.read("got2"), "foo", "default.*.do strips the matched extension for $2");

    // An exact do-file: $2 == $1.
    p.write(
        "bar.args.do",
        "args :0: a1!\nargs :1: a2!\n@a1 \"egot1\" writeFile\n@a2 \"egot2\" writeFile\n",
    );
    assert!(p.redo(&["bar.args"]).status.success());
    assert_eq!(p.read("egot1"), "bar.args");
    assert_eq!(p.read("egot2"), "bar.args", "exact name.do gives $2 == $1");
}

/// An explicit zero-byte `$3` creates an empty target; no output at all creates
/// no file. (ports t/102-empty)
#[test]
fn zero_byte_vs_no_output() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("empty.txt.do", "args :2: out!\n\"\" @out writeFile\n");
    assert!(p.redo(&["empty.txt"]).status.success());
    assert!(p.exists("empty.txt"), "an explicit zero-byte $3 must create the file");
    assert_eq!(p.read("empty.txt"), "");

    p.write("nofile.do", "\"ran\\n\" \"nofile.log\" appendFile\n");
    assert!(p.redo(&["nofile"]).status.success());
    assert!(!p.exists("nofile"), "no output must not create a file");
}

/// Deleting a built target forces a rebuild on the next ifchange. (t/102-empty)
#[test]
fn rebuild_when_target_deleted() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "out.txt.do",
        "args :2: out!\n\"r\\n\" \"out.log\" appendFile\n\"hi\\n\" @out writeFile\n",
    );
    assert!(p.redo(&["ifchange", "out.txt"]).status.success());
    assert_eq!(p.lines("out.log"), 1);
    assert_eq!(p.read("out.txt"), "hi\n");

    std::fs::remove_file(p.dir.join("out.txt")).unwrap();
    assert!(p.redo(&["ifchange", "out.txt"]).status.success());
    assert_eq!(p.lines("out.log"), 2, "deleted target must be rebuilt");
    assert_eq!(p.read("out.txt"), "hi\n");
}

/// Non-ASCII characters in the do-file and target paths. (ports t/103-unicode)
#[test]
fn unicode_paths() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("café.txt.do", "args :2: out!\n\"é\\n\" @out writeFile\n");
    let out = p.redo(&["café.txt"]);
    assert!(out.status.success(), "unicode path build failed: {}", stderr(&out));
    assert_eq!(p.read("café.txt"), "é\n");
}

/// Spaces in directory and target names. (ports t/104-space)
#[test]
fn spaces_in_paths() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("space dir/out.txt.do", "args :2: out!\n\"x\\n\" @out writeFile\n");
    let out = p.redo(&["space dir/out.txt"]);
    assert!(out.status.success(), "spaced path build failed: {}", stderr(&out));
    assert_eq!(p.read("space dir/out.txt"), "x\n");
}

/// `default.EXT.do` is preferred over `default.do` by longest matching
/// extension. (ports t/120-defaults-flat)
#[test]
fn default_extension_precedence() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("default.c.do", "args :2: out!\n\"c-rule\\n\" @out writeFile\n");
    p.write("default.do", "args :2: out!\n\"any-rule\\n\" @out writeFile\n");
    assert!(p.redo(&["x.c"]).status.success());
    assert_eq!(p.read("x.c"), "c-rule\n", "x.c should use default.c.do");
    assert!(p.redo(&["y.q"]).status.success());
    assert_eq!(p.read("y.q"), "any-rule\n", "y.q should fall back to default.do");
}

/// Nested default resolution: a closer `default.z.do` wins, and the root
/// `default.do` receives a subdir-prefixed `$1`. (ports t/121-defaults-nested)
#[test]
fn nested_default_resolution() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("a/default.z.do", "args :2: out!\n\"az\\n\" @out writeFile\n");
    p.write("default.do", "args :0: t!\nargs :2: out!\n@t @out writeFile\n");

    assert!(p.redo(&["a/file.z"]).status.success());
    assert_eq!(p.read("a/file.z"), "az\n", "a/file.z should use a/default.z.do");

    assert!(p.redo(&["a/file"]).status.success());
    assert_eq!(p.read("a/file"), "a/file", "root default.do sees $1 = a/file");
}

/// A `default.do` in one directory must NOT build a target in a sibling
/// directory; that target has no do-file and must fail. (ports t/122-defaults-parent)
#[test]
fn default_does_not_cross_dirs() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("inner/default.do", "args :2: out!\n\"in\\n\" @out writeFile\n");
    // inner/foo builds via inner/default.do.
    assert!(p.redo(&["inner/foo"]).status.success());
    assert_eq!(p.read("inner/foo"), "in\n");
    // x/foo has no do-file anywhere; inner/default.do must not apply.
    let out = p.redo(&["x/foo"]);
    assert!(!out.status.success(), "inner/default.do must not build x/foo");
}

/// The committed target gets normal (0644) permissions, not the private mode of
/// the temp file. (ports t/130-mode)
#[cfg(unix)]
#[test]
fn output_mode_is_644() {
    use std::os::unix::fs::PermissionsExt;
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("m.txt.do", "\"hi\" wl\n");
    assert!(p.redo(&["m.txt"]).status.success());
    let mode = std::fs::metadata(p.dir.join("m.txt")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "committed target should be 0644, got {mode:o}");
}

/// A failing do-file fails the build, and an `ifchange` of a missing,
/// unbuildable dep fails. (ports t/201-fail and t/350-deps ifchange-fail)
#[test]
fn failing_dofile_and_missing_dep() {
    if skip() {
        return;
    }
    let p = Project::new();
    // A do-file whose ifchange of a nonexistent, unbuildable dep fails.
    p.write("boom.do", "['redo-msh' 'ifchange' 'no-such-dep']!\n");
    assert!(!p.redo(&["boom"]).status.success(), "failing do-file must fail the build");
    // ifchange of a missing, unbuildable target fails directly.
    assert!(
        !p.redo(&["ifchange", "no-such-dep"]).status.success(),
        "ifchange of a missing source/target must fail"
    );
    // ifcreate of a nonexistent path from inside a do-file succeeds.
    // (NB: the path name must not collide with an mshell reserved word such as
    // `maybe`; see the module note on reserved words.)
    p.write("ic.do", "['redo-msh' 'ifcreate' 'notyet']!\n\"ok\\n\" \"ic.log\" appendFile\n");
    assert!(p.redo(&["ic"]).status.success(), "ifcreate of a nonexistent path should succeed");
}

/// A dependency whose do-file fails must stay out of date: the next run must
/// retry (and keep failing) rather than accept a leftover output file from an
/// earlier successful build. (regression: A -> B, B fails, second `redo A`
/// silently accepted the stale B)
#[test]
fn failed_dep_is_retried_not_accepted() {
    if skip() {
        return;
    }
    let p = Project::new();
    // Build b.txt successfully once, so a leftover file and db row exist.
    p.write("b.txt.do", "args :2: out!\n\"beta\\n\" @out writeFile\n");
    p.write(
        "a.txt.do",
        "args :2: out!\n['redo-msh' 'ifchange' 'b.txt']!\n\"b.txt\" readFile @out writeFile\n",
    );
    assert!(p.redo(&["a.txt"]).status.success());
    assert_eq!(p.read("b.txt"), "beta\n");

    // Break b's do-file (ifchange of a missing, unbuildable dep fails).
    p.write("b.txt.do", "['redo-msh' 'ifchange' 'no-such-dep']!\n");
    assert!(
        !p.redo(&["a.txt"]).status.success(),
        "a must fail while b's do-file fails"
    );
    // The stale leftover b.txt must not be accepted on the next run.
    assert!(
        !p.redo(&["a.txt"]).status.success(),
        "second run must retry the failed dependency, not accept the stale b.txt"
    );
    let ood = p.stdout_of(&["ood"]);
    assert!(
        ood.lines().any(|l| l == "b.txt"),
        "b.txt must be listed as out of date after a failed build: {ood:?}"
    );

    // Fix the do-file: everything builds again and b is committed fresh.
    p.write("b.txt.do", "args :2: out!\n\"fixed\\n\" @out writeFile\n");
    let out = p.redo(&["a.txt"]);
    assert!(out.status.success(), "build after fixing b.do failed: {}", stderr(&out));
    assert_eq!(p.read("b.txt"), "fixed\n");
    assert_eq!(p.read("a.txt"), "fixed\n");
}

/// A path recorded via `ifcreate` makes the target out-of-date once it is
/// created. (ports t/220-ifcreate)
///
/// We check this through `redo-msh ood` rather than by rebuilding: a real
/// `ifcreate` do-file guards the call (`if exists -> ifchange, else ifcreate`)
/// because `redo-ifcreate <existing>` is *defined* to fail (apenwarr's t/220
/// expects exactly that). Driving the read-only `ood` check exercises the
/// dependency mechanism without re-running the unguarded recipe.
#[test]
fn ifcreate_marks_ood_when_created() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("t.do", "['redo-msh' 'ifcreate' 'marker']!\n\"r\\n\" \"t.log\" appendFile\n");
    assert!(p.redo(&["t"]).status.success());
    assert_eq!(p.lines("t.log"), 1);

    // marker absent -> target is up to date.
    let ood = p.stdout_of(&["ood"]);
    assert!(!ood.lines().any(|l| l == "t"), "t must not be ood while marker is absent: {ood:?}");

    // Creating the ifcreate path makes the target out-of-date.
    p.write("marker", "x\n");
    let ood = p.stdout_of(&["ood"]);
    assert!(ood.lines().any(|l| l == "t"), "creating an ifcreate path must make t ood: {ood:?}");
}

/// Building a target whose parent directory does not yet exist: the do-file
/// creates the directory itself. (ports t/250-makedir)
///
/// Like apenwarr redo, redo-msh does NOT auto-create a target's parent dir — by
/// design that is the do-file's job (apenwarr's recipe does
/// `mkdir -p $(dirname $1)`; here the mshell recipe uses the `mkdirp` builtin).
#[test]
fn dofile_creates_target_subdir() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "default.txt.do",
        "args :2: out!\n\"sub/deep\" mkdirp\n\"hi\\n\" @out writeFile\n",
    );
    let out = p.redo(&["sub/deep/x.txt"]);
    assert!(
        out.status.success(),
        "build into a do-file-created subdir should work: {}",
        stderr(&out)
    );
    assert_eq!(p.read("sub/deep/x.txt"), "hi\n");
}

/// Two targets sharing one static source both rebuild when it changes.
/// (ports t/350-deps doublestatic)
#[test]
fn shared_static_dep_rebuilds_all() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("static.in", "1\n");
    for n in [1, 2] {
        p.write(
            &format!("s{n}.txt.do"),
            "args :2: out!\n['redo-msh' 'ifchange' 'static.in']!\n\"s\\n\" \"s.log\" appendFile\n\"static.in\" readFile @out writeFile\n",
        );
    }
    assert!(p.redo(&["ifchange", "s1.txt", "s2.txt"]).status.success());
    assert_eq!(p.lines("s.log"), 2);

    p.write("static.in", "2\n");
    assert!(p.redo(&["ifchange", "s1.txt", "s2.txt"]).status.success());
    assert_eq!(p.lines("s.log"), 4, "both dependents must rebuild when shared source changes");
}

/// A do-file in a subdirectory can depend on a target via a relative `../`
/// path, and that edge is recorded correctly. (ports t/550-chdir)
#[test]
fn cross_dir_relative_dependency() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "top.txt.do",
        "args :2: out!\n\"t\\n\" \"top.log\" appendFile\n\"top\\n\" @out writeFile\n",
    );
    p.write(
        "sub/leaf.do",
        "['redo-msh' 'ifchange' '../top.txt']!\n\"l\\n\" \"leaf.log\" appendFile\n",
    );

    assert!(p.redo(&["ifchange", "sub/leaf"]).status.success());
    assert_eq!(p.lines("top.log"), 1, "top.txt built once");
    assert_eq!(p.lines("sub/leaf.log"), 1);

    // No change -> nothing rebuilds.
    assert!(p.redo(&["ifchange", "sub/leaf"]).status.success());
    assert_eq!(p.lines("top.log"), 1);
    assert_eq!(p.lines("sub/leaf.log"), 1);

    // Change top's recipe -> top rebuilds, and leaf (which depends on ../top.txt)
    // rebuilds too.
    p.write(
        "top.txt.do",
        "args :2: out!\n\"t\\n\" \"top.log\" appendFile\n\"TOP\\n\" @out writeFile\n",
    );
    assert!(p.redo(&["ifchange", "sub/leaf"]).status.success());
    assert_eq!(p.lines("top.log"), 2, "top rebuilt after recipe change");
    assert_eq!(
        p.lines("sub/leaf.log"),
        2,
        "leaf must rebuild when its ../ dependency changes"
    );
}

// ---- drop-in compatibility: foreign interpreter + named forwarders ----------

/// Whether `sh` is available; the drop-in tests no-op when it is not (e.g. on a
/// Windows runner without a POSIX shell).
fn sh_available() -> bool {
    let exe = if cfg!(windows) { "sh.exe" } else { "sh" };
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(exe).exists()))
        .unwrap_or(false)
}

/// Run an arbitrary command in the project with the redo-msh bin dir prepended
/// to PATH (so the `redo`/`redo-ifchange`/... forwarders resolve).
fn run_in(dir: &Path, program: &str, args: &[&str]) -> Output {
    let bin_dir = Path::new(BIN).parent().unwrap();
    let mut paths = vec![bin_dir.to_path_buf()];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    let path = std::env::join_paths(paths).expect("join PATH");
    Command::new(bin_dir.join(program))
        .args(args)
        .current_dir(dir)
        .env("PATH", path)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {program}: {e}"))
}

/// A project whose do-files are plain `sh` scripts selected via `redo.toml`,
/// invoking the bare `redo-ifchange` forwarder, built via the bare `redo`
/// command with no arguments (the `all` default). Exercises the entire drop-in
/// path: interpreter config, PATH injection, named forwarders, `all` default.
#[test]
fn sh_dofiles_via_config_and_forwarders() {
    if !sh_available() {
        eprintln!("SKIP: `sh` not found on PATH");
        return;
    }
    let p = Project::new();
    // Map every platform the suite runs on, including Windows (Git Bash `sh` is
    // present on the runners). Without a `[platform.windows]` entry the do-files
    // would fall back to the built-in `msh` interpreter on Windows and the sh
    // recipes would not run.
    p.write("redo.toml", "[platform.linux]\ndefault = [\"sh\", \"-e\"]\n\n[platform.macos]\ndefault = [\"sh\", \"-e\"]\n\n[platform.wsl]\ndefault = [\"sh\", \"-e\"]\n\n[platform.windows]\ndefault = [\"sh\", \"-e\"]\n");
    // No shebangs anywhere; the interpreter comes entirely from redo.toml.
    p.write("all.do", "redo-ifchange greeting.txt\n");
    p.write(
        "greeting.txt.do",
        "redo-ifchange name.txt\necho \"hello, $(cat name.txt)\"\n",
    );
    p.write("name.txt", "world\n");

    // Bare `redo` with no targets builds `all`, which builds greeting.txt.
    let out = run_in(&p.dir, "redo", &[]);
    assert!(out.status.success(), "redo (all) failed: {}", stderr(&out));
    assert_eq!(p.read("greeting.txt"), "hello, world\n");

    // Incremental: nothing changed -> greeting.txt is up to date.
    assert!(p.stdout_of(&["ood"]).trim().is_empty(), "nothing should be ood");

    // Edit the source -> rebuild propagates through the sh do-files.
    p.write("name.txt", "redo\n");
    let out = run_in(&p.dir, "redo-ifchange", &["greeting.txt"]);
    assert!(out.status.success(), "redo-ifchange failed: {}", stderr(&out));
    assert_eq!(p.read("greeting.txt"), "hello, redo\n");
}

/// A failing `redo-ifchange` (here, a missing dependency with no do-file) must
/// propagate failure through `sh -e` and fail the parent build.
#[test]
fn sh_dofile_failure_propagates() {
    if !sh_available() {
        eprintln!("SKIP: `sh` not found on PATH");
        return;
    }
    let p = Project::new();
    // Map every platform the suite runs on, including Windows (Git Bash `sh` is
    // present on the runners). Without a `[platform.windows]` entry the do-files
    // would fall back to the built-in `msh` interpreter on Windows and the sh
    // recipes would not run.
    p.write("redo.toml", "[platform.linux]\ndefault = [\"sh\", \"-e\"]\n\n[platform.macos]\ndefault = [\"sh\", \"-e\"]\n\n[platform.wsl]\ndefault = [\"sh\", \"-e\"]\n\n[platform.windows]\ndefault = [\"sh\", \"-e\"]\n");
    // out.txt depends on a nonexistent source with no do-file -> ifchange fails,
    // and `sh -e` must abort the do-file rather than continuing to write output.
    p.write(
        "out.txt.do",
        "redo-ifchange missing-source\necho should-not-happen\n",
    );
    let out = run_in(&p.dir, "redo", &["out.txt"]);
    assert!(!out.status.success(), "build should fail on missing dep");
    assert!(!p.exists("out.txt"), "no output on failed build");
}

/// Every shipped executable must respond to `-h`/`--help`/`--version` without
/// trying to build anything. Needs no interpreter, so it runs everywhere.
#[test]
fn all_commands_have_help_and_version() {
    let bin_dir = Path::new(BIN).parent().unwrap();
    let tmp = std::env::temp_dir();
    let cmds = [
        ("redo", "build targets"),
        ("redo-ifchange", "build dependencies"),
        ("redo-ifcreate", "NOT existing"),
        ("redo-always", "always out of date"),
        ("redo-stamp", "drain standard input"),
        ("redo-msh", "cross-platform"),
    ];
    for (prog, needle) in cmds {
        for flag in ["-h", "--help"] {
            let out = Command::new(bin_dir.join(prog))
                .arg(flag)
                .current_dir(&tmp)
                .output()
                .unwrap();
            assert!(out.status.success(), "{prog} {flag} should exit 0");
            let s = String::from_utf8_lossy(&out.stdout);
            assert!(s.contains(needle), "{prog} {flag} help missing {needle:?}:\n{s}");
            assert!(s.contains("-h, --help"), "{prog} {flag} should document --help");
        }
        let v = Command::new(bin_dir.join(prog))
            .arg("--version")
            .current_dir(&tmp)
            .output()
            .unwrap();
        assert!(v.status.success(), "{prog} --version should exit 0");
        assert!(
            String::from_utf8_lossy(&v.stdout).contains(prog),
            "{prog} --version should name itself"
        );
    }
}

// ---- live log streaming -------------------------------------------------

/// The headline property of the live log: a long-running do-file's stderr
/// reaches the user *while it runs*, not as a block after it exits.
///
/// Methodology: the do-file writes PROGRESS-1 to stderr, then spins until the
/// test creates `go.txt` (bounded, so a regression fails rather than hangs),
/// then writes PROGRESS-2. The test reads redo's stderr incrementally and
/// only creates `go.txt` after observing PROGRESS-1 with the build still
/// running — which is impossible unless stderr streamed live.
#[test]
fn stderr_streams_during_build() {
    use std::io::Read;
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "slow.txt.do",
        "args :2: out!\n\
         \"PROGRESS-1\" wle\n\
         0 i!\n\
         (\n\
         \t\"go.txt\" fileExists if break end\n\
         \t@i 1 + i!\n\
         \t@i 2000000 >= if break end\n\
         ) loop\n\
         \"PROGRESS-2\" wle\n\
         \"done\\n\" @out writeFile\n",
    );

    let mut child = p
        .redo_cmd(&["slow.txt"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn redo-msh");
    let mut err = child.stderr.take().expect("stderr piped");

    // Read stderr incrementally until PROGRESS-1 appears (or a bounded amount
    // of time passes, in which case streaming is broken).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("PROGRESS-1") {
        assert!(
            std::time::Instant::now() < deadline,
            "PROGRESS-1 never streamed; stderr so far: {:?}",
            String::from_utf8_lossy(&seen)
        );
        let n = err.read(&mut buf).expect("read child stderr");
        assert!(n > 0, "stderr closed before PROGRESS-1");
        seen.extend_from_slice(&buf[..n]);
    }
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "build already finished when PROGRESS-1 arrived; that is replay, not streaming"
    );

    p.write("go.txt", "");
    let mut rest = Vec::new();
    err.read_to_end(&mut rest).expect("drain stderr");
    seen.extend_from_slice(&rest);
    let status = child.wait().expect("wait");
    let all = String::from_utf8_lossy(&seen);
    assert!(status.success(), "build failed: {all}");
    assert!(all.contains("PROGRESS-2"), "missing tail of stream: {all}");
    assert_eq!(p.read("slow.txt"), "done\n");
}

/// Parallel builds must not interleave: each target's stderr appears as one
/// contiguous block, even with four noisy do-files running under -j4.
#[test]
fn parallel_stderr_is_not_interleaved() {
    if skip() {
        return;
    }
    let p = Project::new();
    let mut all_do = String::from("['redo-msh' 'ifchange'");
    for t in ["t1", "t2", "t3", "t4"] {
        p.write(
            &format!("{t}.txt.do"),
            &format!(
                "args :2: out!\n\
                 0 i!\n\
                 (\n\
                 \t@i 30 >= if break end\n\
                 \t\"LINE-{t}\" wle\n\
                 \t@i 1 + i!\n\
                 ) loop\n\
                 \"x\\n\" @out writeFile\n"
            ),
        );
        all_do.push_str(&format!(" '{t}.txt'"));
    }
    all_do.push_str("]!\n");
    p.write("all.do", &all_do);

    let out = p.redo(&["-j4", "all"]);
    assert!(out.status.success(), "parallel build failed: {}", stderr(&out));
    let err = stderr(&out);

    // Collect the tag sequence of user lines; each tag must form exactly one
    // contiguous run, i.e. 4 runs total across 120 lines.
    let tags: Vec<&str> = err
        .lines()
        .filter_map(|l| l.strip_prefix("LINE-"))
        .collect();
    assert_eq!(tags.len(), 4 * 30, "every line must be shown exactly once:\n{err}");
    let runs = 1 + tags.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(runs, 4, "output interleaved; tag sequence: {tags:?}");
}

/// Nested builds are indented by depth, and a parent whose output continues
/// after a child block gets a `(resumed)` marker.
#[test]
fn nested_output_is_indented_and_resumed() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write(
        "a.txt.do",
        "args :2: out!\n\
         \"A-BEFORE\" wle\n\
         ['redo-msh' 'ifchange' 'b.txt']!\n\
         \"A-AFTER\" wle\n\
         \"a\\n\" @out writeFile\n",
    );
    p.write(
        "b.txt.do",
        "args :2: out!\n\"B-ERR\" wle\n\"b\\n\" @out writeFile\n",
    );

    let out = p.redo(&["a.txt"]);
    assert!(out.status.success(), "build failed: {}", stderr(&out));
    let err = stderr(&out);
    let lines: Vec<&str> = err.lines().collect();
    let pos = |needle: &str| {
        lines
            .iter()
            .position(|l| *l == needle)
            .unwrap_or_else(|| panic!("missing line {needle:?} in:\n{err}"))
    };
    // Top-level target at indent 0, its dependency at indent 1, and the
    // parent's stream resumes with a marker after the child block.
    let header_a = pos("redo  a.txt");
    let before = pos("A-BEFORE");
    let header_b = pos("  redo  b.txt");
    let b_err = pos("B-ERR");
    let resumed = pos("redo  a.txt  (resumed)");
    let after = pos("A-AFTER");
    assert!(header_a < before, "{err}");
    assert!(before < header_b, "{err}");
    assert!(header_b < b_err, "{err}");
    assert!(b_err < resumed, "{err}");
    assert!(resumed < after, "{err}");
}

/// A failing do-file is annotated with its exit code in the stream.
#[test]
fn failed_build_is_annotated_in_stream() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("bad.txt.do", "\"BAD-OUTPUT\" wle\n['redo-msh' 'ifchange' 'no-such-dep']!\n");
    let out = p.redo(&["bad.txt"]);
    assert!(!out.status.success(), "build should fail");
    let err = stderr(&out);
    assert!(err.contains("BAD-OUTPUT"), "do-file stderr must stream: {err}");
    assert!(err.contains("bad.txt  (failed, exit "), "missing failure annotation: {err}");
}

/// Logs are consumed and deleted by default — after any run, successful or
/// failed, `.redom/logs` holds nothing. `REDO_KEEP_LOGS=1` keeps them.
#[test]
fn logs_deleted_by_default_kept_on_request() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("ok.txt.do", "args :2: out!\n\"noise\" wle\n\"ok\\n\" @out writeFile\n");
    p.write("bad.txt.do", "['redo-msh' 'ifchange' 'no-such-dep']!\n");

    assert!(p.redo(&["ok.txt"]).status.success());
    assert_eq!(p.log_files(), Vec::<String>::new(), "logs must be deleted after success");

    assert!(!p.redo(&["bad.txt"]).status.success());
    assert_eq!(p.log_files(), Vec::<String>::new(), "logs must be deleted after failure");

    // Opt out of deletion: both the run trace and the target log survive.
    let out = p
        .redo_cmd(&["ok.txt"])
        .env("REDO_KEEP_LOGS", "1")
        .output()
        .expect("spawn redo-msh");
    assert!(out.status.success(), "keep-logs build failed: {}", stderr(&out));
    let kept = p.log_files();
    assert!(
        kept.iter().any(|n| n.starts_with("run.") && n.ends_with(".log")),
        "run trace should be kept: {kept:?}"
    );
    assert!(
        kept.iter().any(|n| n.starts_with("t.") && n.ends_with(".log")),
        "target log should be kept: {kept:?}"
    );

    // The next default run's GC sweeps what the kept run left behind.
    assert!(p.redo(&["ok.txt"]).status.success());
    assert_eq!(p.log_files(), Vec::<String>::new(), "GC must sweep kept logs: {:?}", p.log_files());
}

/// The lost-database scenario (observed in the field): a target built long
/// ago exists on disk, but the current database has no record that redo
/// generated it, and only a default.*.do matches. An explicit `redo <target>`
/// must not silently no-op (it used to — indistinguishable from a display
/// bug); it refuses loudly, `--yes` rebuilds, and the rebuild re-records the
/// target as generated so future runs need no ceremony.
#[test]
fn explicit_redo_of_forgotten_target_refuses_then_heals_with_yes() {
    if skip() {
        return;
    }
    let p = Project::new();
    p.write("default.txt.do", "args :2: out!\n\"sub\" mkdirp\n\"built\\n\" @out writeFile\n");
    assert!(p.redo(&["sub/gen.txt"]).status.success());
    assert_eq!(p.read("sub/gen.txt"), "built\n");

    // Simulate database loss: the file remains, redo's memory of it doesn't.
    std::fs::remove_dir_all(p.dir.join(".redom")).unwrap();
    assert!(p.redo(&["root", "."]).status.success());

    let out = p.redo(&["sub/gen.txt"]);
    assert!(!out.status.success(), "must refuse, not silently no-op");
    assert!(stderr(&out).contains("no record of ever building"), "{}", stderr(&out));

    let out = p.redo(&["--yes", "sub/gen.txt"]);
    assert!(out.status.success(), "--yes must rebuild: {}", stderr(&out));
    assert_eq!(p.read("sub/gen.txt"), "built\n");

    // Healed: the target is recorded as generated again, so an explicit
    // rebuild works without --yes...
    let out = p.redo(&["sub/gen.txt"]);
    assert!(out.status.success(), "healed target must rebuild freely: {}", stderr(&out));
    // ...and dependency traversal treats it as a target, not a source.
    p.write("use.txt.do", "args :2: out!\n['redo-msh' 'ifchange' 'sub/gen.txt']!\n\"sub/gen.txt\" readFile @out writeFile\n");
    assert!(p.redo(&["use.txt"]).status.success());
    assert_eq!(p.read("use.txt"), "built\n");
}
