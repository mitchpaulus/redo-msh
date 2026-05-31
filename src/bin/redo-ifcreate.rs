//! `redo-ifcreate <paths...>` — record that the current target depends on these
//! paths NOT existing.
fn main() {
    redo_msh::forward(Some("ifcreate"));
}
