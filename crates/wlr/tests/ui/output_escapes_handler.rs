use wlr::Output;

/// Stands in for a handler: it receives a borrow-scoped handle.
fn handler<'h>(out: &Output<'h>, sink: &mut Vec<&'h Output<'h>>) {
    // Storing the handle beyond the call must not compile.
    sink.push(out);
}

fn main() {
    let mut sink: Vec<&Output<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
