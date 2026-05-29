This is going to be an implementation of DJBs redo.

Requirements and what will be different:


- This will be cross-platform, it must work on Windows, using native, low-level Windows primitives for things like atomic writes.
- It will use the cross-platform shell language `mshell` as the only do-file form. This allows us to not worry about things like handling she-bangs on Windows.
- It must have ability to run in parallel, and do so robustly.
- It will have the notion of a "project base", not just recording in the current directory.
- It will not do the 'symlink' trick for the different executable names, again, because this doesn't work well on Windows.

- It will be written in Rust
- It will handle manually updated files with more interaction if run from tty. It will ask the user if it should be overwritten by build.
- It can use SQLite as the data store. `.redo/redo-msh.db` in the "project root".

- Support both stdout and `$3` output styles. Error if both received.
- `$1` target, `$2` base from default, and `$3` temporary output path are passed to `mshell` via arguments.

- Environment passed to do-files (`REDO_DEPTH` for indentation, target name, project root).
- Do files always run in the location they are defined in.

## Project Root

The project root will be the `.git` root, or somewhere that has the `.redo/redo-msh.db` manually set up.
Can be set up using `redo-msh root` command in the directory of choice.

Searching for default do files stops at project root.
