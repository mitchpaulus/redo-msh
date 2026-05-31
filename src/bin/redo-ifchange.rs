//! `redo-ifchange <deps...>` — build deps if out of date and record them as
//! dependencies of the current target.
fn main() {
    redo_msh::forward(Some("ifchange"));
}
