//! The scene node API against a real wlroots scene: structure, stacking,
//! coordinates, hit testing — and, at least as importantly, the calls that
//! must **refuse** rather than reach a wlroots `assert()`.
//!
//! Arch ships `libwlroots-0.20.so` with assertions enabled, so every refusal
//! below is the difference between a `None` and a process abort. A test here
//! that stops refusing does not fail with a message; the whole binary dies.
//! That is why each of those cases is asserted individually rather than folded
//! into a loop.
//!
//! No `Backend::run_all` anywhere in this file: nothing here needs an output,
//! a frame or a client, and a scene is fully usable the moment
//! `init_graphics` returns. `scene.rs` covers the frame path.

fn headless_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    });
}

/// A runtime with a real scene.
///
/// The `Display` and `Backend` are leaked deliberately: [`wlr::Runtime`]'s own
/// doc requires both to outlive it, and a test process is exactly the
/// "one per process, reclaimed at exit" case `Graphics` already documents for
/// the scene itself. `backend` is created after `display`, which is the drop
/// order the crate requires and which leaking makes moot either way.
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

/// Creation order is stacking order, because wlroots appends every new child
/// at the end of its parent's list. The crate's whole band design rests on
/// that, so it is asserted directly rather than inferred.
#[test]
fn children_are_reported_bottom_to_top_in_creation_order() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Top).expect("band id");

    let first = rt.create_tree_in_band(wlr::Band::Top).expect("first");
    let second = rt.create_tree_in_band(wlr::Band::Top).expect("second");
    let third = rt.create_tree_in_band(wlr::Band::Top).expect("third");

    assert_eq!(
        rt.node_children(band),
        Some(vec![first, second, third]),
        "the newest child is the topmost"
    );
    assert_eq!(rt.node_kind(first), Some(wlr::NodeKind::Tree));
    assert_eq!(rt.node_parent(first), Some(band));
}

/// The scene root's children are the five bands, in the fixed order
/// `Graphics` documents, and they are reachable by id.
#[test]
fn the_scene_root_reports_the_five_bands_in_order() {
    let rt = scene_runtime();
    let root = rt.scene_root_node().expect("scene root");
    let bands: Vec<wlr::NodeId> = [
        wlr::Band::Background,
        wlr::Band::Bottom,
        wlr::Band::Toplevel,
        wlr::Band::Top,
        wlr::Band::Overlay,
    ]
    .into_iter()
    .map(|b| rt.band_node(b).expect("band id"))
    .collect();

    assert_eq!(rt.node_children(root), Some(bands));
    assert_eq!(rt.node_parent(root), None, "the root has no parent");
}

/// Positions compose down the parent chain, and a disabled ancestor makes a
/// descendant's layout coordinates unavailable rather than zero — the
/// distinction `wlr_scene_node_coords`' boolean return carries.
#[test]
fn coordinates_compose_and_a_disabled_ancestor_hides_them() {
    let rt = scene_runtime();
    let outer = rt.create_tree_in_band(wlr::Band::Top).expect("outer");
    let inner = rt.create_tree_under(outer).expect("inner");
    let rect = rt
        .create_rect(inner, 10, 10, [1.0, 0.0, 0.0, 1.0])
        .expect("rect");

    assert_eq!(rt.set_node_position(outer, 100, 50), Some(()));
    assert_eq!(rt.set_node_position(inner, 5, 7), Some(()));
    assert_eq!(rt.set_node_position(rect, 1, 2), Some(()));

    assert_eq!(rt.node_position(rect), Some((1, 2)), "parent-relative");
    assert_eq!(rt.node_coords(rect), Some((106, 59)), "layout-local");

    assert_eq!(rt.set_node_enabled(outer, false), Some(()));
    assert_eq!(
        rt.node_coords(rect),
        None,
        "a disabled ancestor makes the whole subtree unplaced"
    );
    assert_eq!(
        rt.node_enabled(rect),
        Some(true),
        "without touching the descendant's own flag"
    );

    assert_eq!(rt.set_node_enabled(outer, true), Some(()));
    assert_eq!(rt.node_coords(rect), Some((106, 59)));
}

/// `place_above`/`place_below` reorder siblings, and the read-back through
/// `node_children` is what proves it — not the return value, which only says
/// the call was accepted.
#[test]
fn placing_a_node_reorders_its_siblings() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band id");
    let a = rt.create_tree_in_band(wlr::Band::Overlay).expect("a");
    let b = rt.create_tree_in_band(wlr::Band::Overlay).expect("b");
    let c = rt.create_tree_in_band(wlr::Band::Overlay).expect("c");
    assert_eq!(rt.node_children(band), Some(vec![a, b, c]));

    assert_eq!(rt.place_node_above(a, c), Some(()));
    assert_eq!(rt.node_children(band), Some(vec![b, c, a]));

    assert_eq!(rt.place_node_below(a, b), Some(()));
    assert_eq!(rt.node_children(band), Some(vec![a, b, c]));

    assert_eq!(rt.raise_node_to_top(a), Some(()));
    assert_eq!(rt.node_children(band), Some(vec![b, c, a]));

    assert_eq!(rt.lower_node_to_bottom(a), Some(()));
    assert_eq!(rt.node_children(band), Some(vec![a, b, c]));
}

/// Raising and lowering touch siblings only, so nothing inside a band can
/// escape it. This is the property the five-band stacking guarantee is built
/// on; if it ever stopped holding, a `Top` panel could be covered by a
/// toplevel.
#[test]
fn raising_within_a_band_cannot_move_a_node_across_bands() {
    let rt = scene_runtime();
    let root = rt.scene_root_node().expect("root");
    let bottom_band = rt.band_node(wlr::Band::Bottom).expect("band id");
    let child = rt.create_tree_in_band(wlr::Band::Bottom).expect("child");

    let root_children_before = rt.node_children(root).expect("root children");
    assert_eq!(rt.raise_node_to_top(child), Some(()));

    assert_eq!(
        rt.node_parent(child),
        Some(bottom_band),
        "raising kept the node inside its own band"
    );
    assert_eq!(
        rt.node_children(root),
        Some(root_children_before),
        "and left the band order at the root untouched"
    );
}

/// Reparenting moves a node between bands, which is the one restacking
/// operation `raise_to_top` deliberately cannot express.
#[test]
fn reparenting_moves_a_node_between_bands() {
    let rt = scene_runtime();
    let toplevel_band = rt.band_node(wlr::Band::Toplevel).expect("toplevel band");
    let overlay_band = rt.band_node(wlr::Band::Overlay).expect("overlay band");
    let tree = rt.create_tree_in_band(wlr::Band::Toplevel).expect("tree");
    let rect = rt
        .create_rect(tree, 4, 4, [0.0, 1.0, 0.0, 1.0])
        .expect("rect");

    assert_eq!(rt.node_parent(tree), Some(toplevel_band));
    assert_eq!(rt.reparent_node(tree, overlay_band), Some(()));
    assert_eq!(rt.node_parent(tree), Some(overlay_band));
    assert_eq!(
        rt.node_parent(rect),
        Some(tree),
        "the subtree came with it, unchanged"
    );
    assert!(
        !rt.node_children(toplevel_band)
            .expect("children")
            .contains(&tree)
    );
    assert!(
        rt.node_children(overlay_band)
            .expect("children")
            .contains(&tree)
    );
}

/// Hit testing reports the topmost node at a point, its kind, and coordinates
/// relative to that node — and reports nothing where there is nothing.
#[test]
fn node_at_finds_rects_and_buffers_and_nothing_else() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band id");
    let rect = rt
        .create_rect(band, 20, 20, [1.0, 0.0, 0.0, 1.0])
        .expect("rect");
    assert_eq!(rt.set_node_position(rect, 100, 100), Some(()));

    let (hit, nx, ny) = rt.node_at(105.0, 108.0).expect("a hit inside the rect");
    assert_eq!(hit, rect);
    assert_eq!(rt.node_kind(hit), Some(wlr::NodeKind::Rect));
    assert_eq!((nx, ny), (5.0, 8.0), "coordinates are relative to the node");

    assert_eq!(
        rt.node_at(99.0, 99.0),
        None,
        "just outside the rect there is nothing"
    );

    // A buffer node with no buffer covers nothing, so give it a destination
    // size: that is what makes it hittable at all.
    let buffer = rt.create_scene_buffer(band, None).expect("buffer node");
    assert_eq!(rt.set_scene_buffer_dest_size(buffer, 30, 30), Some(()));
    assert_eq!(rt.set_node_position(buffer, 300, 300), Some(()));
    let (hit, _, _) = rt.node_at(310.0, 310.0).expect("a hit inside the buffer");
    assert_eq!(hit, buffer);
    assert_eq!(rt.node_kind(hit), Some(wlr::NodeKind::Buffer));
}

/// `for_each_buffer` walks root to leaves, which is back to front — the order
/// a compositor paints in.
#[test]
fn for_each_buffer_visits_every_buffer_node_in_render_order() {
    let rt = scene_runtime();
    let tree = rt.create_tree_in_band(wlr::Band::Overlay).expect("tree");
    let lower = rt.create_scene_buffer(tree, None).expect("lower");
    let upper = rt.create_scene_buffer(tree, None).expect("upper");
    // A rect in the middle must not show up: this iterator is buffers only.
    let _rect = rt.create_rect(tree, 2, 2, [0.0; 4]).expect("rect");
    assert_eq!(rt.set_node_position(lower, 1, 2), Some(()));
    assert_eq!(rt.set_node_position(upper, 3, 4), Some(()));

    let mut seen = Vec::new();
    assert_eq!(
        rt.for_each_buffer(tree, |id, x, y| seen.push((id, x, y))),
        Some(())
    );
    assert_eq!(seen, vec![(lower, 1, 2), (upper, 3, 4)]);
}

/// Read-only handles see the same state the by-id queries report, and the
/// tag-checked downcasts only succeed for the matching kind.
#[test]
fn handles_observe_the_node_they_were_minted_for() {
    let rt = scene_runtime();
    let tree = rt.create_tree_in_band(wlr::Band::Top).expect("tree");
    let rect = rt
        .create_rect(tree, 6, 7, [0.25, 0.5, 0.75, 1.0])
        .expect("rect");
    assert_eq!(rt.set_node_position(rect, 8, 9), Some(()));

    let observed = rt
        .with_node(rect, |node| {
            (
                node.id(),
                node.kind(),
                node.position(),
                node.enabled(),
                node.parent(),
                node.try_as_rect().map(|r| (r.size(), r.color())),
                node.try_as_tree().is_some(),
                node.try_as_buffer().is_some(),
            )
        })
        .expect("the rect is borrowable");

    assert_eq!(observed.0, rect);
    assert_eq!(observed.1, Some(wlr::NodeKind::Rect));
    assert_eq!(observed.2, (8, 9));
    assert!(observed.3);
    assert_eq!(observed.4, Some(tree));
    assert_eq!(observed.5, Some(((6, 7), [0.25, 0.5, 0.75, 1.0])));
    assert!(!observed.6, "a rect is not a tree");
    assert!(!observed.7, "a rect is not a buffer node");

    let children = rt
        .with_tree(tree, |t| (t.child_count(), t.children()))
        .expect("the tree is borrowable");
    assert_eq!(children, (1, vec![rect]));

    assert_eq!(
        rt.with_tree(rect, |_| ()),
        None,
        "with_tree refuses a node that is not a tree"
    );
}

/// The destroy guard: while a handle is borrowed, nothing may free the node
/// under it. Without this the handle would dangle for the rest of the closure
/// and every accessor on it would be a use-after-free.
#[test]
fn a_live_node_borrow_refuses_every_destroy() {
    let rt = scene_runtime();
    let tree = rt.create_tree_in_band(wlr::Band::Top).expect("tree");
    let rect = rt.add_rect(4, 4, [0.0; 4]).expect("legacy rect");
    let buffer = rt.add_buffer(1, 1, &[0, 0, 0, 255]).expect("legacy buffer");

    let refusals = rt
        .with_node(tree, |node| {
            (
                rt.destroy_node(node.id()),
                rt.reparent_node(node.id(), rt.band_node(wlr::Band::Overlay).unwrap()),
                rt.remove_rect(rect),
                rt.remove_buffer(buffer),
            )
        })
        .expect("borrowable");
    assert_eq!(refusals, (None, None, None, None));

    // And everything works again once the borrow is over.
    assert_eq!(rt.destroy_node(tree), Some(()));
    assert_eq!(rt.remove_rect(rect), Some(()));
    assert_eq!(rt.remove_buffer(buffer), Some(()));
}

/// Every wlroots `assert()` reachable from this API, refused. The process
/// surviving this test *is* the assertion; the `assert_eq!`s only confirm the
/// refusal was reported rather than silently swallowed.
#[test]
fn calls_that_would_abort_wlroots_are_refused() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band");
    let root = rt.scene_root_node().expect("root");
    let a = rt.create_tree_in_band(wlr::Band::Overlay).expect("a");
    let b = rt.create_tree_in_band(wlr::Band::Top).expect("b");
    let child = rt.create_tree_under(a).expect("child of a");
    let rect = rt.create_rect(a, 4, 4, [0.0; 4]).expect("rect");

    // `wlr_scene_node_place_above` asserts node != sibling.
    assert_eq!(rt.place_node_above(a, a), None);
    assert_eq!(rt.place_node_below(a, a), None);
    // ... and asserts they share a parent.
    assert_eq!(rt.place_node_above(a, b), None);
    assert_eq!(rt.place_node_below(a, b), None);

    // `wlr_scene_node_reparent` asserts the new parent is not the node or one
    // of its descendants.
    assert_eq!(rt.reparent_node(a, a), None);
    assert_eq!(rt.reparent_node(a, child), None);
    // ... and a rect is not a tree, so it cannot be a parent at all.
    assert_eq!(rt.reparent_node(a, rect), None);

    // `wlr_scene_rect_set_size` and `wlr_scene_buffer_set_dest_size` assert
    // non-negative.
    assert_eq!(rt.set_node_rect_size(rect, -1, 4), None);
    assert_eq!(rt.set_node_rect_size(rect, 4, -1), None);
    assert_eq!(rt.create_rect(a, -1, 4, [0.0; 4]), None);
    let buffer = rt.create_scene_buffer(a, None).expect("buffer node");
    assert_eq!(rt.set_scene_buffer_dest_size(buffer, -1, 1), None);

    // Not a wlroots assert, but this crate's own: an out-of-range opacity is
    // a silently wrong image rather than an error, so it is refused.
    assert_eq!(rt.set_scene_buffer_opacity(buffer, 1.5), None);
    assert_eq!(rt.set_scene_buffer_opacity(buffer, f32::NAN), None);
    assert_eq!(rt.set_scene_buffer_opacity(buffer, 0.5), Some(()));

    // The scene root and the bands are structurally untouchable.
    for protected in [root, band] {
        assert_eq!(rt.destroy_node(protected), None);
        assert_eq!(rt.reparent_node(protected, a), None);
        assert_eq!(rt.raise_node_to_top(protected), None);
        assert_eq!(rt.lower_node_to_bottom(protected), None);
        assert_eq!(rt.place_node_above(protected, a), None);
        assert_eq!(rt.set_node_position(protected, 1, 1), None);
        // ... but still readable, and still enable-able.
        assert_eq!(rt.node_kind(protected), Some(wlr::NodeKind::Tree));
        assert_eq!(rt.set_node_enabled(protected, true), Some(()));
    }
}

/// Every entry point must miss cleanly on an id no runtime ever issued — the
/// crate's standing promise for by-id calls, and the precondition for the
/// destroy-storm fuzz target to mean anything.
#[test]
fn a_dangling_node_id_misses_on_every_entry_point() {
    let rt = scene_runtime();
    let dead = wlr::NodeId::dangling_for_test();
    let live = rt.create_tree_in_band(wlr::Band::Top).expect("tree");

    assert_eq!(rt.node_kind(dead), None);
    assert_eq!(rt.node_coords(dead), None);
    assert_eq!(rt.node_position(dead), None);
    assert_eq!(rt.node_enabled(dead), None);
    assert_eq!(rt.node_parent(dead), None);
    assert_eq!(rt.node_children(dead), None);
    assert_eq!(rt.destroy_node(dead), None);
    assert_eq!(rt.set_node_enabled(dead, true), None);
    assert_eq!(rt.set_node_position(dead, 0, 0), None);
    assert_eq!(rt.place_node_above(dead, live), None);
    assert_eq!(rt.place_node_below(live, dead), None);
    assert_eq!(rt.raise_node_to_top(dead), None);
    assert_eq!(rt.lower_node_to_bottom(dead), None);
    assert_eq!(rt.reparent_node(dead, live), None);
    assert_eq!(rt.reparent_node(live, dead), None);
    assert_eq!(rt.create_tree_under(dead), None);
    assert_eq!(rt.create_rect(dead, 1, 1, [0.0; 4]), None);
    assert_eq!(rt.create_scene_buffer(dead, None), None);
    assert_eq!(rt.set_node_rect_size(dead, 1, 1), None);
    assert_eq!(rt.set_node_rect_color(dead, [0.0; 4]), None);
    assert_eq!(rt.set_scene_buffer_dest_size(dead, 1, 1), None);
    assert_eq!(rt.set_scene_buffer_opacity(dead, 0.5), None);
    assert_eq!(
        rt.set_scene_buffer_filter(dead, wlr::FilterMode::Nearest),
        None
    );
    assert_eq!(rt.set_scene_buffer_source_box(dead, None), None);
    assert_eq!(rt.set_scene_buffer_opaque_region(dead, None), None);
    assert_eq!(
        rt.set_scene_buffer_transform(dead, wlr::Transform::Normal),
        None
    );
    assert_eq!(
        rt.set_scene_buffer(dead, None, &wlr::SceneBufferOptions::new()),
        None
    );
    assert_eq!(
        rt.set_scene_buffer_transfer_function(dead, wlr::TransferFunction::Srgb),
        None
    );
    assert_eq!(
        rt.set_scene_buffer_primaries(dead, wlr::NamedPrimaries::Srgb),
        None
    );
    assert_eq!(
        rt.set_scene_buffer_color_encoding(dead, wlr::ColorEncoding::Bt709),
        None
    );
    assert_eq!(
        rt.set_scene_buffer_color_range(dead, wlr::ColorRange::Full),
        None
    );
    assert_eq!(rt.for_each_buffer(dead, |_, _, _| ()), None);
    assert_eq!(rt.with_node(dead, |_| ()), None);
    assert_eq!(rt.with_tree(dead, |_| ()), None);
    assert_eq!(rt.with_rect(dead, |_| ()), None);
    assert_eq!(rt.with_scene_buffer(dead, |_| ()), None);
    assert_eq!(rt.rect_node(wlr::RectId::dangling_for_test()), None);
}

/// The buffer-node setters take effect and read back, including the two whose
/// "unset" value is not a value a caller would ever choose (a zero
/// destination size, an empty source box).
#[test]
fn buffer_node_properties_round_trip() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band");
    let node = rt.create_scene_buffer(band, None).expect("buffer node");

    let observed = rt
        .with_scene_buffer(node, |b| {
            (b.has_buffer(), b.dest_size(), b.source_box(), b.opacity())
        })
        .expect("borrowable");
    assert_eq!(
        observed,
        (false, None, None, 1.0),
        "a fresh buffer node has no buffer and every default"
    );

    assert_eq!(rt.set_scene_buffer_dest_size(node, 40, 30), Some(()));
    assert_eq!(rt.set_scene_buffer_opacity(node, 0.25), Some(()));
    assert_eq!(
        rt.set_scene_buffer_filter(node, wlr::FilterMode::Nearest),
        Some(())
    );
    assert_eq!(
        rt.set_scene_buffer_transform(node, wlr::Transform::R90),
        Some(())
    );
    assert_eq!(
        rt.set_scene_buffer_source_box(node, Some(wlr::FBox::new(1.0, 2.0, 3.0, 4.0))),
        Some(())
    );
    let opaque = wlr::Region::from_box(wlr::Box2D::new(0, 0, 10, 10));
    assert_eq!(
        rt.set_scene_buffer_opaque_region(node, Some(&opaque)),
        Some(())
    );

    let observed = rt
        .with_scene_buffer(node, |b| {
            (
                b.dest_size(),
                b.opacity(),
                b.filter(),
                b.transform(),
                b.source_box(),
                b.opaque_region().extents(),
            )
        })
        .expect("borrowable");
    assert_eq!(observed.0, Some((40, 30)));
    assert_eq!(observed.1, 0.25);
    assert_eq!(observed.2, wlr::FilterMode::Nearest);
    assert_eq!(observed.3, Some(wlr::Transform::R90));
    assert_eq!(observed.4, Some(wlr::FBox::new(1.0, 2.0, 3.0, 4.0)));
    assert_eq!(observed.5, wlr::Box2D::new(0, 0, 10, 10));

    // Clearing the two "unset means something" properties restores the
    // defaults rather than recording a zero.
    assert_eq!(rt.set_scene_buffer_dest_size(node, 0, 0), Some(()));
    assert_eq!(rt.set_scene_buffer_source_box(node, None), Some(()));
    let observed = rt
        .with_scene_buffer(node, |b| (b.dest_size(), b.source_box()))
        .expect("borrowable");
    assert_eq!(observed, (None, None));
}

/// The colour setters are accepted on a live buffer node. What they mean is
/// wlroots' business and only observable through a render; that they reach the
/// right node without aborting is this crate's.
#[test]
fn buffer_node_colour_metadata_is_accepted() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Overlay).expect("band");
    let node = rt.create_scene_buffer(band, None).expect("buffer node");

    assert_eq!(
        rt.set_scene_buffer_transfer_function(node, wlr::TransferFunction::St2084Pq),
        Some(())
    );
    assert_eq!(
        rt.set_scene_buffer_primaries(node, wlr::NamedPrimaries::Bt2020),
        Some(())
    );
    assert_eq!(
        rt.set_scene_buffer_color_encoding(node, wlr::ColorEncoding::Bt2020),
        Some(())
    );
    assert_eq!(
        rt.set_scene_buffer_color_range(node, wlr::ColorRange::Limited),
        Some(())
    );
}

/// A rect made with the frozen 0.20.1 API is a node like any other: it has an
/// id, it can be restacked through the node API, and the two views of it agree.
#[test]
fn a_legacy_rect_reaches_the_node_api_through_its_node_id() {
    let rt = scene_runtime();
    let band = rt.band_node(wlr::Band::Top).expect("band");
    let rect = rt
        .add_rect_in_band(wlr::Band::Top, 12, 34, [1.0, 1.0, 0.0, 1.0])
        .expect("rect");
    let node = rt.rect_node(rect).expect("node id");

    assert_eq!(rt.node_kind(node), Some(wlr::NodeKind::Rect));
    assert_eq!(rt.node_parent(node), Some(band));
    assert_eq!(
        rt.with_rect(node, |r| r.size()),
        Some((12, 34)),
        "the handle sees what add_rect_in_band asked for"
    );

    // The two size setters name the same rect.
    assert_eq!(rt.set_node_rect_size(node, 5, 6), Some(()));
    assert_eq!(rt.with_rect(node, |r| r.size()), Some((5, 6)));
    assert_eq!(rt.set_rect_size(rect, 7, 8), Some(()));
    assert_eq!(rt.with_rect(node, |r| r.size()), Some((7, 8)));

    assert_eq!(rt.remove_rect(rect), Some(()));
    assert_eq!(rt.node_kind(node), None, "and they go stale together");
}

/// Ids are per-process and never reused, so two runtimes in one process never
/// collide — the property `tests/scene.rs` already asserts for `RectId`,
/// restated for `NodeId` because the two now share the same counter.
#[test]
fn node_ids_are_unique_across_runtimes() {
    let first = scene_runtime();
    let second = scene_runtime();
    let a = first.create_tree_in_band(wlr::Band::Top).expect("a");
    let b = second.create_tree_in_band(wlr::Band::Top).expect("b");

    assert_ne!(a, b);
    assert_eq!(
        second.node_kind(a),
        None,
        "and a node from one runtime is unknown to the other"
    );
    assert_eq!(first.node_kind(b), None);
}
