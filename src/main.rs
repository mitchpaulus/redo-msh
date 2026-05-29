//! redo-msh: a cross-platform, parallel, mshell-based implementation of DJB redo.
//!
//! Command dispatch follows redo's convention: the first argument is a verb
//! (`root`, `ifchange`, `ifcreate`, `always`, `sources`, `targets`, `ood`); if
//! it is not a known verb, every argument is treated as a target to build
//! (`redo-msh <targets...>`). This replaces the multi-binary symlink trick,
//! which does not port to Windows.

mod build;
mod db;
mod dofile;
mod fsguard;
mod jobserver;
mod lock;
mod paths;
mod root;
mod stamp;

use anyhow::{Context, Result};
use build::Ctx;
use root::Root;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("redo-msh: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run(raw_args: &[String]) -> Result<()> {
    let (jobs, args) = extract_jobs(raw_args)?;
    let (verb, rest) = match args.split_first() {
        Some((v, r)) => (v.as_str(), r.to_vec()),
        None => {
            print_usage();
            return Ok(());
        }
    };

    match verb {
        "root" => cmd_root(&rest),
        "ifchange" => build::ifchange(&Ctx::from_env()?, &rest),
        "ifcreate" => build::ifcreate(&Ctx::from_env()?, &rest),
        "always" => build::always(&Ctx::from_env()?),
        "sources" | "targets" | "ood" => {
            anyhow::bail!("`{verb}` is not implemented yet (coming in M7)")
        }
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        "--version" => {
            println!("redo-msh {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        // No recognized verb: treat all args as targets to build.
        _ => build::redo(&args, jobs),
    }
}

/// Extract `-j N` / `-jN` / `--jobs N` / `--jobs=N` from the argument list,
/// returning the parallelism (default 1) and the remaining args.
fn extract_jobs(args: &[String]) -> Result<(usize, Vec<String>)> {
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
        } else {
            rest.push(a.clone());
        }
    }
    Ok((jobs.max(1), rest))
}

/// `redo-msh root [dir]` — initialize a project root.
fn cmd_root(args: &[String]) -> Result<()> {
    let dir = args.first().map(Path::new).unwrap_or_else(|| Path::new("."));
    let root = Root::init(dir)?;
    println!(
        "redo-msh: initialized project root at {}",
        root.dir.display()
    );
    println!("  database: {}", root.db_path().display());
    Ok(())
}

fn print_usage() {
    eprintln!(
        "redo-msh {} — DJB redo, cross-platform, mshell do-files

USAGE:
    redo-msh <targets...>        build targets (force)
    redo-msh ifchange <deps...>  declare + build dependencies   [M2]
    redo-msh ifcreate <paths...> declare non-existence deps     [M2]
    redo-msh always              declare an always-build dep    [M2]
    redo-msh root [dir]          initialize a project root
    redo-msh sources|targets|ood introspection                  [M7]

Only `root` is implemented so far.",
        env!("CARGO_PKG_VERSION")
    );
}
