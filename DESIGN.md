This is going to be an implementation of DJBs redo.

Requirements and what will be different:


- This will be cross-platform, it must work on Windows, using native, low-level Windows primitives for things like atomic writes.
- It will use the cross-platform shell language `mshell` as the default do-file form. This allows us to not worry about things like handling she-bangs on Windows. To support dropping redo-msh onto existing projects whose do-files target another interpreter, a project may commit a `redo.toml` at its root mapping do-files to the command that runs them (e.g. `sh -e`, `python3`), per-platform (linux/wsl/windows/macos). We never parse shebangs; the interpreter mapping is always explicit and lives in version control. See `src/config.rs`.
- The redo command interface (`redo`, `redo-ifchange`, `redo-ifcreate`, `redo-always`, `redo-stamp`) is provided as separate, individually named executables that ship beside `redo-msh` and forward into it. redo-msh prepends its own directory to a do-file's `PATH` so these resolve without a system install. There is no symlink/argv[0] trick (it does not port to Windows). `redo-stamp` is a compatibility no-op (drains stdin) because redo-msh content-hashes every committed output by default.
- `redo` / `redo-msh` with no targets builds `all` (i.e. `all.do`).
- It must have ability to run in parallel, and do so robustly.
- It will have the notion of a "project base", not just recording in the current directory.
- It will not do the 'symlink' trick for the different executable names, again, because this doesn't work well on Windows.

- It will be written in Rust
- It will handle manually updated files with more interaction if run from tty. It will ask the user if it should be overwritten by build.
- It can use SQLite as the data store. `.redom/redo-msh.db` in the "project root".

- Support both stdout and `$3` output styles. Error if both received.
- `$1` target, `$2` base from default, and `$3` temporary output path are passed to `mshell` via arguments.

- Environment passed to do-files (`REDO_DEPTH` for indentation, target name, project root).
- Do files always run in the location they are defined in.

## Project Root

The project root will be the `.git` root, or somewhere that has the `.redom/redo-msh.db` manually set up.
Can be set up using `redo-msh root` command in the directory of choice.

Searching for default do files stops at project root.

## Live Build Logs

Do-file stderr streams to the terminal *while it runs* (a long EnergyPlus run
shows its progress live), and parallel builds never interleave. The design is
apenwarr redo's log linearizer, rebuilt fork-free and fd-free so it works
identically on Windows; the full invariants (I1–I5) are documented at the top
of `src/logs.rs`.

Mechanism, in one paragraph: every target's do-file writes stderr into its own
append-only log file (`.redom/logs/t.<key>.log`, where `<key>` is the same
blake3 hash that names the target's build lock). The recursion trace is
embedded in those same files as structured `@REDOM1:...@` event lines: a `do`
event in the parent's log (appended only after the child's log exists) tells
the follower to descend; a `done` event in the target's own log (appended
before the build lock is released) terminates it. A single follower thread in
the top-level process — the only writer to the terminal — walks the trace
depth-first, replaying finished targets and live-tailing the one it is on.
EOF plus a free build lock plus no `done` proves the builder died and is
reported as `(crashed)`. Events travel by *path* (`REDO_LOG_PATH` in the
do-file environment), never by inherited fd, so a do-file that redirects its
own stderr can neither receive nor pollute trace events.

Logs are consumed and deleted as the follower replays them; the run trace
(`run.<session>.log`, flock-held for the run's lifetime) is removed at exit,
and a lock-probing GC at startup sweeps anything a crashed run left behind.
Set `REDO_KEEP_LOGS=1` to keep everything for post-mortem inspection.
