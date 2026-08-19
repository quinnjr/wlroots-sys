//! What happens to ids when nodes die.
//!
//! `wlr_scene_node_destroy` frees the node **and every descendant**, with no
//! notification to anyone not listening. Every test here is about the same
//! question from a different angle: after that cascade, does an id this crate
//! handed out still resolve to memory wlroots has reclaimed?
//!
//! The answer must be "no, and asking is free". A failure here is not a wrong
//! value — it is a use-after-free that a fuzzer, a compositor under load, or
//! nothing at all will find later.

fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    });
}

/// See `scene_tree.rs`'s copy for why the display and backend are leaked.
fn scene_runtime() -> wlr::Runtime {
    headless_env();
    let display: &'static wlr::Display = Box::leak(Box::new(wlr::Display::new().expect("display")));
    let backend: &'static wlr::Backend<'static> = Box::leak(Box::new(
        wlr::Backend::autocreate(&display.event_loop()).expect("backend"),
    ));
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime
        .init_graphics(display, backend)
        .expect("init_graphics");
    runtime
}

/// Assert that `id` names nothing, through every entry point that would
/// dereference it. Any one of these resolving would be reading freed memory.
fn assert_id_is_dead(rt: &wlr::Runtime, id: wlr::NodeId, what: &str) {
    assert_eq!(rt.node_kind(id), None, "{what}: node_kind");
    assert_eq!(rt.node_coords(id), None, "{what}: node_coords");
    assert_eq!(rt.node_position(id), None, "{what}: node_position");
    assert_eq!(rt.node_enabled(id), None, "{what}: node_enabled");
    assert_eq!(rt.node_parent(id), None, "{what}: node_parent");
    assert_eq!(rt.node_children(id), None, "{what}: node_children");
    assert_eq!(rt.set_node_enabled(id, true), None, "{what}: set_enabled");
    assert_eq!(rt.set_node_position(id, 1, 1), None, "{what}: set_position");
    assert_eq!(rt.raise_node_to_top(id), None, "{what}: raise_to_top");
    assert_eq!(rt.lower_node_to_bottom(id), None, "{what}: lower_to_bottom");
    assert_eq!(rt.create_tree_under(id), None, "{what}: create_tree_under");
    assert_eq!(rt.set_node_rect_size(id, 1, 1), None, "{what}: rect_size");
    assert_eq!(
        rt.set_scene_buffer_opacity(id, 0.5),
        None,
        "{what}: buffer_opacity"
    );
    assert_eq!(
        rt.for_each_buffer(id, |_, _, _| ()),
        None,
        "{what}: iterate"
    );
    assert_eq!(rt.with_node(id, |_| ()), None, "{what}: with_node");
    assert_eq!(
        rt.destroy_node(id),
        None,
        "{what}: destroy_node must miss, not double free"
    );
}

/// The central claim: destroy the top of a three-deep tree and *every* id
/// beneath it goes stale at once, not just the one named.
#[test]
fn destroying_a_tree_invalidates_every_descendant_id() {
    let rt = scene_runtime();
    let top = rt.create_tree_in_band(wlr::Band::Top).expect("top");
    let middle = rt.create_tree_under(top).expect("middle");
    let bottom = rt.create_tree_under(middle).expect("bottom");

    let rects: Vec<wlr::NodeId> = [top, middle, bottom]
        .iter()
        .map(|&parent| {
            rt.create_rect(parent, 4, 4, [1.0, 0.0, 0.0, 1.0])
                .expect("rect")
        })
        .collect();
    let buffer = rt.create_scene_buffer(bottom, None).expect("buffer node");

    // Everything resolves while the tree is alive.
    for id in [top, middle, bottom, buffer] {
        assert!(rt.node_kind(id).is_some());
    }

    assert_eq!(rt.destroy_node(top), Some(()));

    assert_id_is_dead(&rt, top, "the destroyed tree");
    assert_id_is_dead(&rt, middle, "a child tree");
    assert_id_is_dead(&rt, bottom, "a grandchild tree");
    assert_id_is_dead(&rt, buffer, "a buffer node three levels down");
    for (depth, rect) in rects.into_iter().enumerate() {
        assert_id_is_dead(&rt, rect, &format!("a rect at depth {depth}"));
    }
}

/// A destroyed subtree must also leave its former parent's child list, or
/// `node_children` would keep handing out an id whose node is gone.
#[test]
fn a_destroyed_subtree_leaves_its_parents_child_list() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band");
    let keep = rt.create_tree_in_band(wlr::Band::Overlay).expect("keep");
    let doomed = rt.create_tree_in_band(wlr::Band::Overlay).expect("doomed");
    assert_eq!(rt.node_children(band), Some(vec![keep, doomed]));

    assert_eq!(rt.destroy_node(doomed), Some(()));
    assert_eq!(rt.node_children(band), Some(vec![keep]));
}

/// The frozen 0.20.1/0.20.3 tables purge with the cascade too: a rect and a
/// pixel buffer parented into a tree that dies must leave no row behind, or
/// `remove_rect` would later call `wlr_scene_node_destroy` a second time on
/// memory wlroots has already reclaimed.
#[test]
fn a_cascade_purges_the_legacy_rect_and_buffer_tables() {
    let rt = scene_runtime();
    let rect = rt
        .add_rect_in_band(wlr::Band::Top, 4, 4, [1.0, 0.0, 0.0, 1.0])
        .expect("rect");
    let buffer = rt.add_buffer(1, 1, &[1, 2, 3, 4]).expect("buffer");

    let rect_node = rt.rect_node(rect).expect("rect node id");
    let buffer_node = rt.buffer_node(buffer).expect("buffer node id");

    // Reparent both into a tree of our own, then destroy that tree — the
    // cascade is the only thing that frees them.
    let doomed = rt.create_tree_in_band(wlr::Band::Overlay).expect("doomed");
    assert_eq!(rt.reparent_node(rect_node, doomed), Some(()));
    assert_eq!(rt.reparent_node(buffer_node, doomed), Some(()));

    assert_eq!(rt.destroy_node(doomed), Some(()));

    assert_id_is_dead(&rt, rect_node, "a legacy rect caught in a cascade");
    assert_id_is_dead(&rt, buffer_node, "a legacy buffer caught in a cascade");
    assert_eq!(
        rt.remove_rect(rect),
        None,
        "remove_rect must miss, not double-destroy"
    );
    assert_eq!(
        rt.remove_buffer(buffer),
        None,
        "remove_buffer must miss, not double-destroy"
    );
    assert_eq!(rt.rect_node(rect), None);
    assert_eq!(rt.buffer_node(buffer), None);
    assert_eq!(rt.set_rect_position(rect, 1, 1), None);
    assert_eq!(rt.set_rect_size(rect, 1, 1), None);
    assert_eq!(rt.set_buffer_position(buffer, 1, 1), None);
    assert_eq!(rt.update_buffer(buffer, 1, 1, &[0, 0, 0, 0]), None);
}

/// Destroying the same node twice must be a miss, not a double free — the
/// property every "remove" in this crate promises, restated for the node API
/// because a cascade gives many more ways to reach an already-dead node.
#[test]
fn a_second_destroy_of_the_same_node_misses() {
    let rt = scene_runtime();
    let tree = rt.create_tree_in_band(wlr::Band::Top).expect("tree");
    assert_eq!(rt.destroy_node(tree), Some(()));
    assert_eq!(rt.destroy_node(tree), None);
    assert_eq!(rt.destroy_node(tree), None);
}

/// The dangerous interleaving: destroy a node whose id something else is still
/// holding, then create new nodes. The new nodes must not reuse the dead id,
/// and the dead id must not resolve to the new node — the ABA hazard a bare
/// pointer would have, and the whole reason ids come from a monotonic counter.
#[test]
fn a_new_node_never_inherits_a_destroyed_ones_id() {
    let rt = scene_runtime();
    let first = rt.create_tree_in_band(wlr::Band::Top).expect("first");
    assert_eq!(rt.destroy_node(first), Some(()));

    for _ in 0..32 {
        let next = rt.create_tree_in_band(wlr::Band::Top).expect("next");
        assert_ne!(next, first, "ids are never reused");
        assert_eq!(rt.destroy_node(next), Some(()));
    }
    assert_eq!(rt.node_kind(first), None);
}

/// A rect destroyed by `remove_rect` — the published 0.20.5 path — must take
/// its node row with it, not only its `RectId` row.
#[test]
fn remove_rect_and_remove_buffer_purge_the_node_table_too() {
    let rt = scene_runtime();
    let rect = rt.add_rect(4, 4, [0.0; 4]).expect("rect");
    let buffer = rt.add_buffer(1, 1, &[9, 9, 9, 255]).expect("buffer");
    let rect_node = rt.rect_node(rect).expect("rect node");
    let buffer_node = rt.buffer_node(buffer).expect("buffer node");

    assert_eq!(rt.remove_rect(rect), Some(()));
    assert_eq!(rt.remove_buffer(buffer), Some(()));

    assert_id_is_dead(&rt, rect_node, "a rect removed through remove_rect");
    assert_id_is_dead(&rt, buffer_node, "a buffer removed through remove_buffer");
}

/// Hit testing and iteration mint ids for nodes this crate never created. Those
/// ids must die with their nodes exactly like any other — a foreign id that
/// outlived its node would be the same use-after-free wearing a different hat.
#[test]
fn ids_minted_by_observation_die_with_their_nodes() {
    let rt = scene_runtime();
    let owner = rt.create_tree_in_band(wlr::Band::Overlay).expect("owner");
    let rect = rt
        .create_rect(owner, 20, 20, [1.0, 0.0, 0.0, 1.0])
        .expect("rect");
    assert_eq!(rt.set_node_position(rect, 0, 0), Some(()));

    // Reach the rect the way a compositor's pointer handler would, and confirm
    // observation hands back the same id rather than minting a second one.
    let (hit, _, _) = rt.node_at(5.0, 5.0).expect("hit");
    assert_eq!(hit, rect);

    assert_eq!(rt.destroy_node(owner), Some(()));
    assert_id_is_dead(&rt, hit, "an id first seen through node_at");
    assert_eq!(rt.node_at(5.0, 5.0), None, "and nothing is there any more");
}

/// A subtree that is *moved* rather than destroyed keeps every id, and the
/// tree it was moved out of stops naming it. Reparenting is the operation most
/// likely to desynchronise a purge table, so it gets its own destroy test
/// rather than only a structural one.
#[test]
fn reparenting_out_of_a_doomed_tree_saves_the_subtree() {
    let rt = scene_runtime();
    let doomed = rt.create_tree_in_band(wlr::Band::Top).expect("doomed");
    let rescued = rt.create_tree_under(doomed).expect("rescued");
    let rect = rt.create_rect(rescued, 4, 4, [0.0; 4]).expect("rect");
    let safety = rt.band_node(wlr::Band::Overlay).expect("overlay band");

    assert_eq!(rt.reparent_node(rescued, safety), Some(()));
    assert_eq!(rt.destroy_node(doomed), Some(()));

    assert_id_is_dead(&rt, doomed, "the destroyed tree");
    assert_eq!(
        rt.node_kind(rescued),
        Some(wlr::NodeKind::Tree),
        "the rescued subtree survived"
    );
    assert_eq!(rt.node_kind(rect), Some(wlr::NodeKind::Rect));
    assert_eq!(rt.node_parent(rescued), Some(safety));
}
