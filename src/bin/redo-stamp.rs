//! `redo-stamp` — drain stdin and exit (no-op; redo-msh content-hashes outputs
//! by default). See `build::stamp`.
fn main() {
    redo_msh::forward(Some("stamp"));
}
