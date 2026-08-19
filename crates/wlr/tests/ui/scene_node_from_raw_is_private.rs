use wlr::SceneNode;

/// `SceneNode::from_raw_with_id` is the only way to mint a handle, and it is
/// `pub(crate)` — a consumer must not be able to name it, let alone call it to
/// manufacture a handle with a lifetime of their own choosing. A `SceneNode`
/// minted with `'static` would outlive the destroy cascade that frees the node
/// under it.
fn main() {
    let _ = SceneNode::<'static>::from_raw_with_id;
}
