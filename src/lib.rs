//! redo-msh: a cross-platform, parallel, mshell-based implementation of DJB redo.
//!
//! The crate is structured as a library plus several thin binaries. The
//! `redo-msh` umbrella binary dispatches on a leading verb; the individually
//! named binaries (`redo`, `redo-ifchange`, ...) are two-line forwarders that
//! call into this library, recreating redo's multi-command interface without
//! the symlink/argv[0] trick (which does not port cleanly to Windows).

pub mod build;
pub mod config;
pub mod db;
pub mod dofile;
pub mod fsguard;
pub mod jobserver;
pub mod lock;
pub mod logs;
pub mod paths;
pub mod root;
pub mod stamp;
pub mod winjob;

use anyhow::{Context, Result};
use build::Ctx;
use root::Root;
use std::path::Path;

/// Dispatch the `redo-msh` umbrella command line. The first argument is a verb
/// (`root`, `ifchange`, `ifcreate`, `always`, `sources`, `targets`, `ood`); if
/// it is not a known verb, every argument is treated as a target to build.
pub fn run(raw_args: &[String]) -> Result<()> {
    // Help and version are recognized in any position, before other parsing.
    // (A target literally named `-h` must be given as a path, e.g. `./-h`.)
    if raw_args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return Ok(());
    }
    if raw_args.iter().any(|a| a == "--version") {
        println!("redo-msh {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (jobs, args) = extract_jobs(raw_args)?;
    let (verb, rest) = match args.split_first() {
        Some((v, r)) => (v.as_str(), r.to_vec()),
        // No arguments: build the default target `all`.
        None => return build_targets(&[], jobs),
    };

    match verb {
        "root" => cmd_root(&rest),
        "ifchange" => build::ifchange(&Ctx::from_env()?, &rest),
        "ifcreate" => build::ifcreate(&Ctx::from_env()?, &rest),
        "always" => build::always(&Ctx::from_env()?),
        "stamp" => build::stamp(),
        "sources" => build::cmd_sources(),
        "targets" => build::cmd_targets(),
        "ood" => build::cmd_ood(),
        "help" => {
            print_usage();
            Ok(())
        }
        // No recognized verb: treat all args as targets to build.
        _ => build::redo(&args, jobs),
    }
}

/// Entry point for the named forwarder binaries (`redo`, `redo-ifchange`,
/// `redo-ifcreate`, `redo-always`, `redo-stamp`). `verb = None` is the
/// top-level `redo` build command; otherwise the fixed verb applies to all
/// arguments. Handles its own error reporting and process exit.
pub fn forward(verb: Option<&str>) -> ! {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let prog = prog_name(verb);

    // Help and version are recognized by every command, in any position, before
    // any other parsing. (A target/dep literally named `-h` must be given as a
    // path, e.g. `./-h`.)
    if raw.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", forward_usage(verb));
        std::process::exit(0);
    }
    if raw.iter().any(|a| a == "--version") {
        println!("{prog} {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    let code = match run_forward(verb, &raw) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{prog}: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

/// The program name for diagnostics/usage: `redo` for the top-level build,
/// `redo-<verb>` otherwise.
fn prog_name(verb: Option<&str>) -> String {
    verb.map(|v| format!("redo-{v}")).unwrap_or_else(|| "redo".into())
}

/// Per-command help text for the named forwarder binaries.
fn forward_usage(verb: Option<&str>) -> String {
    let ver = env!("CARGO_PKG_VERSION");
    // Shared trailer for commands that take target/dependency names.
    let dash_note = "\nTo name a target/dependency beginning with '-', give it a path,\n\
                     e.g. `./--my-file`.\n";
    match verb {
        None => format!(
            "redo {ver} — build targets (forced); with no targets, builds `all`.

USAGE:
    redo [options] [targets...]

OPTIONS:
    -j N, --jobs N   build up to N targets in parallel (default 1)
    -y, --yes        overwrite hand-edited targets without asking
    -h, --help       show this help
    --version        print version
{dash_note}"
        ),
        Some("ifchange") => format!(
            "redo-ifchange {ver} — build dependencies if out of date and record them as
dependencies of the current target. Call from within a do-file.

USAGE:
    redo-ifchange <deps...>

OPTIONS:
    -h, --help   show this help
    --version    print version
{dash_note}"
        ),
        Some("ifcreate") => format!(
            "redo-ifcreate {ver} — record that the current target depends on the given
paths NOT existing. Call from within a do-file.

USAGE:
    redo-ifcreate <paths...>

OPTIONS:
    -h, --help   show this help
    --version    print version
{dash_note}"
        ),
        Some("always") => format!(
            "redo-always {ver} — mark the current target as always out of date. Call from
within a do-file.

USAGE:
    redo-always

OPTIONS:
    -h, --help   show this help
    --version    print version
"
        ),
        Some("stamp") => format!(
            "redo-stamp {ver} — drain standard input and exit. Compatibility no-op:
redo-msh content-hashes every committed output by default, so an explicit
stamp is unnecessary.

USAGE:
    <command> | redo-stamp

OPTIONS:
    -h, --help   show this help
    --version    print version
"
        ),
        Some(other) => format!("redo-{other}: no help available\n"),
    }
}

fn run_forward(verb: Option<&str>, raw: &[String]) -> Result<()> {
    match verb {
        // `redo [opts] [targets...]`; no targets builds `all`.
        None => {
            let (jobs, targets) = extract_jobs(raw)?;
            build_targets(&targets, jobs)
        }
        Some("ifchange") => build::ifchange(&Ctx::from_env()?, raw),
        Some("ifcreate") => build::ifcreate(&Ctx::from_env()?, raw),
        Some("always") => build::always(&Ctx::from_env()?),
        Some("stamp") => build::stamp(),
        Some(other) => anyhow::bail!("internal: unknown forwarder verb {other:?}"),
    }
}

/// Build the requested targets, defaulting to `all` when none are given.
fn build_targets(targets: &[String], jobs: usize) -> Result<()> {
    if targets.is_empty() {
        build::redo(&["all".to_string()], jobs)
    } else {
        build::redo(targets, jobs)
    }
}

/// Extract `-j N` / `-jN` / `--jobs N` / `--jobs=N` from the argument list,
/// returning the parallelism (default 1) and the remaining args.
pub fn extract_jobs(args: &[String]) -> Result<(usize, Vec<String>)> {
    let mut jobs = 1usize;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-j" || a == "--jobs" {
            let v = it
                .next()
                .ok_or_else(|| anyhow::anyhow!("{a} requires a number"))?;
            jobs = v.parse().with_context(|| format!("invalid job count {v:?}"))?;
        } else if let Some(v) = a.strip_prefix("--jobs=") {
            jobs = v.parse().with_context(|| format!("invalid job count {v:?}"))?;
        } else if let Some(v) = a.strip_prefix("-j") {
            jobs = v.parse().with_context(|| format!("invalid job count {v:?}"))?;
        } else if a == "--yes" || a == "-y" {
            // Auto-overwrite hand-edited targets; inherited by child processes.
            std::env::set_var("REDO_YES", "1");
        } else {
            rest.push(a.clone());
        }
    }
    Ok((jobs.max(1), rest))
}

/// `redo-msh root [dir]` — initialize a project root.
pub fn cmd_root(args: &[String]) -> Result<()> {
    let dir = args.first().map(Path::new).unwrap_or_else(|| Path::new("."));
    let root = Root::init(dir)?;
    println!(
        "redo-msh: initialized project root at {}",
        root.dir.display()
    );
    println!("  database: {}", root.db_path().display());
    Ok(())
}

pub fn print_usage() {
    println!(
        "redo-msh {} — DJB redo, cross-platform, mshell do-files

USAGE:
    redo-msh [options] <targets...>   (re)build the given targets
                                      (no targets builds `all`)

COMMANDS:
    redo-msh <targets...>         build targets (forced)
    redo-msh ifchange <deps...>   build deps if out of date; record them as
                                  dependencies of the current target (do-files)
    redo-msh ifcreate <paths...>  record that the current target depends on
                                  these paths NOT existing
    redo-msh always               mark the current target as always out of date
    redo-msh stamp                drain stdin (compat no-op; outputs are hashed)
    redo-msh root [dir]           initialize a project root (.redom/) here or at dir
    redo-msh sources              list known source files
    redo-msh targets              list known generated targets
    redo-msh ood                  list out-of-date targets (without building)

These are also available as the standalone commands `redo`, `redo-ifchange`,
`redo-ifcreate`, `redo-always`, and `redo-stamp` (shipped alongside redo-msh),
which existing do-files call directly.

OPTIONS:
    -j N, --jobs N                build up to N targets in parallel (default 1)
    -y, --yes                     overwrite hand-edited targets without asking
    --version                     print version
    -h, --help                    show this help

Do-files are run with `msh <dofile> $1 $2 $3` by default; a project may commit
a `redo.toml` to run some or all do-files with another interpreter (e.g.
`sh -e`). $1 = target, $2 = target without extension, $3 = temp output path.

To name a target beginning with '-', give it a path, e.g. `./--my-file`.",
        env!("CARGO_PKG_VERSION")
    );
}
