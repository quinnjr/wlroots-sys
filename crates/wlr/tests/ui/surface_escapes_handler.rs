use wlr::Surface;

/// Stands in for a handler: it receives a borrow-scoped handle.
///
/// Written as a borrow that outlives the call rather than as `*surface`,
/// because `Surface` is deliberately neither `Copy` nor `Clone` and a move
/// out of a shared reference would fail for a reason that has nothing to do
/// with the lifetime this fixture exists to pin.
fn handler<'h>(surface: &Surface<'h>, sink: &mut Vec<&'h Surface<'h>>) {
    // Storing the handle beyond the call must not compile.
    sink.push(surface);
}

fn main() {
    let mut sink: Vec<&Surface<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
