use wlr::RegionRef;

/// Stands in for a wlroots callback: it receives a borrow-scoped view of a
/// region wlroots owns.
fn handler<'h>(region: &RegionRef<'h>, sink: &mut Vec<&'h RegionRef<'h>>) {
    // Storing the view beyond the call must not compile: the region belongs to
    // an object that is freed the moment the callback returns.
    sink.push(region);
}

fn main() {
    let mut sink: Vec<&RegionRef<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
