use wlr::SceneNode;

/// Stands in for `Runtime::with_node`'s closure: it receives a borrow-scoped
/// handle to a node that may be destroyed the moment the closure returns.
///
/// Written as a borrow that outlives the call rather than as `*node`, because
/// `SceneNode` is deliberately neither `Copy` nor `Clone` and a move out of a
/// shared reference would fail for a reason unrelated to the lifetime this
/// fixture exists to pin.
fn handler<'h>(node: &SceneNode<'h>, sink: &mut Vec<&'h SceneNode<'h>>) {
    // Storing the handle beyond the call must not compile.
    sink.push(node);
}

fn main() {
    let mut sink: Vec<&SceneNode<'_>> = Vec::new();
    let _ = &mut sink;
    let _ = handler;
}
