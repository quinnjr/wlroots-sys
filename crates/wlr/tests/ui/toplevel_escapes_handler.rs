use wlr::Toplevel;

/// Stands in for a handler: it receives a borrow-scoped handle.
///
/// Written as a borrow that outlives the call rather than as `*toplevel`,
/// because `Toplevel` is deliberately neither `Copy` nor `Clone` and a move
/// out of a shared reference would fail for a reason that has nothing to do
/// with the lifetime this fixture exists to pin.
fn handler<'h>(toplevel: &Toplevel<'h>, sink: &mut Vec<&'h Toplevel<'h>>) {
    // Storing the handle beyond the call must not compile.
    sink.push(toplevel);
}

fn main() {
    let mut sink: Vec<&Toplevel<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
