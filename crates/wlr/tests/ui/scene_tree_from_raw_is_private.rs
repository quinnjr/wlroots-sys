use wlr::SceneTree;

/// The `SceneTree` half of `scene_node_from_raw_is_private`: the tree handle
/// has its own private constructor, and widening it would let a consumer mint
/// a tree view over freed memory just as surely.
fn main() {
    let _ = SceneTree::<'static>::from_raw_with_id;
}
