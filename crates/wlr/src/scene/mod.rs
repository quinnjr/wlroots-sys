//! The scene graph: nodes, trees, and the ids that survive their destruction.
//!
//! # What a node id is, and why it is not a pointer
//!
//! `wlr_scene_node_destroy` destroys the node **and every descendant**,
//! immediately and without announcing it to anyone who was not listening. Any
//! table this crate keeps of `*mut wlr_scene_rect` goes dangling the moment an
//! ancestor tree dies, and a later "remove" on such a row is a double free.
//!
//! [`NodeId`] closes that by hanging a payload off the node's own
//! `wlr_addon_set`. wlroots runs an addon's destroy hook for every node it
//! frees — including every node of a recursive cascade — so the payload's
//! [`Drop`] is the notification nothing else provides. Purging is therefore
//! exact rather than inferred from parentage, and it is exact for the whole
//! cascade rather than only for the node the caller named.
//!
//! # Owned, protected, foreign
//!
//! Not every node in the scene is this crate's to mutate, and a [`NodeId`]
//! alone does not say which is which — so the runtime records that alongside
//! it (`NodeOrigin`, crate-internal):
//!
//! - **owned** — created through [`Runtime::create_tree_in_band`](crate::Runtime::create_tree_in_band),
//!   [`Runtime::create_rect`](crate::Runtime::create_rect), [`Runtime::add_rect`](crate::Runtime::add_rect) and friends. Everything
//!   is permitted.
//! - **protected** — the scene root and the six stacking bands
//!   ([`Band`](crate::Band)). Readable, and [`Runtime::set_node_enabled`](crate::Runtime::set_node_enabled)
//!   works, but nothing may destroy, restack or reparent one: the bands' order
//!   is the crate's whole stacking guarantee (see `Graphics`' own doc), and
//!   destroying the root would free every node in the process at once.
//! - **foreign** — a node wlroots or another part of this crate owns, which
//!   got an id only because something *observed* it
//!   ([`Runtime::node_at`](crate::Runtime::node_at), [`Runtime::node_children`](crate::Runtime::node_children),
//!   [`Runtime::for_each_buffer`](crate::Runtime::for_each_buffer)). A toplevel's tree, a layer surface's tree,
//!   a client surface's buffer node. Their *placement* is read-only through
//!   this API; move and restack them through
//!   [`Runtime::raise_toplevel`](crate::Runtime::raise_toplevel) and its
//!   siblings, which keep the crate's own bookkeeping straight. Their
//!   *appearance* is not: opacity, filter, transform, the colour metadata, the
//!   source box, the destination size and the opaque region all apply, because
//!   fading or recolouring a client's window is the ordinary reason to hold one
//!   of these ids and none of it touches the bookkeeping the placement rule
//!   protects.
//!
//! So the rule is about placement, not ownership: every mutator that *moves,
//! restacks, reparents or destroys* a protected or foreign node returns `None`,
//! exactly as it does for an id that never existed. The one appearance-side
//! exception is [`Runtime::set_node_enabled`](crate::Runtime::set_node_enabled),
//! which refuses to hide [`Band::Lock`](crate::Band::Lock) while the session is
//! locked — see its own doc.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use crate::addon::{Addon, addon_kind};
use crate::id::next_id;
use crate::runtime::RuntimeInner;
use crate::sys;

mod buffer;
mod damage;
pub(crate) mod output;
mod rect;
mod surface;

pub use buffer::{SceneBuffer, SceneBufferOptions};
pub use damage::{DamageRing, DamageRingRef};
pub use output::{SceneOutput, SceneOutputId, SceneOutputStateOptions, SceneTimer};
pub use rect::SceneRect;
pub use surface::SceneSurface;

/// A moment, as wlroots' own `struct timespec`.
///
/// The crate hands wlroots timestamps in three places (a scene output's,
/// a scene surface's and a scene buffer's frame-done), and one conversion is
/// what keeps them from disagreeing about saturation. `Duration` is unsigned
/// and 64-bit-plus; `tv_sec` is a signed 64-bit `time_t` on every target this
/// crate builds for, so the clamp only ever fires on a value no clock produced.
pub(crate) fn timespec_of(when: std::time::Duration) -> sys::timespec {
    sys::timespec {
        tv_sec: i64::try_from(when.as_secs()).unwrap_or(i64::MAX) as _,
        tv_nsec: when.subsec_nanos() as _,
    }
}

/// Identifies a solid-colour rect in the scene.
///
/// The representation is frozen: this type was published in 0.20.1 and the
/// hand-written API cannot change within a wlroots minor (see `CLAUDE.md`).
/// It is a plain counter value, *not* the id of the node's addon — but since
/// 0.20.19 the node underneath one does carry a [`NodeId`], and the addon that
/// backs that id is what purges this rect's row when a cascade frees it.
/// [`Runtime::rect_node`](crate::Runtime::rect_node) hands the [`NodeId`] out, which is how a rect
/// reaches the node API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RectId(pub(crate) u64);

impl RectId {
    /// An id no live rect can have, for testing that an unknown id misses
    /// cleanly rather than crashing.
    ///
    /// Ids come from a counter starting at 1, so the top of the range is
    /// unreachable in practice. Mirrors
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test).
    pub fn dangling_for_test() -> RectId {
        RectId(u64::MAX)
    }
}

/// Identifies a scene node for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike [`SceneNode`], which cannot
/// escape the borrow that produced it. Ids are never reused within a process,
/// and one goes stale the instant its node is freed, whether that was an
/// explicit [`Runtime::destroy_node`](crate::Runtime::destroy_node) or an ancestor's destroy cascade. Every
/// by-id call reports `None` from then on.
///
/// Deliberately no `PartialOrd`/`Ord`, for the reason
/// [`OutputId`](crate::OutputId) records: ordering would imply creation-order
/// semantics nobody asked for, and adding it later is the non-breaking
/// direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) u64);

impl NodeId {
    /// An id no live node can have, for testing that an unknown id misses
    /// cleanly rather than crashing.
    ///
    /// The same reserved top of the counter's range
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// uses, and for the same reason.
    pub fn dangling_for_test() -> NodeId {
        NodeId(u64::MAX)
    }
}

/// What a scene node actually is.
///
/// wlroots models this as C-style embedding plus a tag: `wlr_scene_tree`,
/// `wlr_scene_rect` and `wlr_scene_buffer` each embed a `wlr_scene_node` at
/// offset 0, and the downcast helpers are **undefined behaviour** on a
/// mismatched tag. This enum is that tag, and the `try_as_*` methods on
/// [`SceneNode`] are the only downcast this crate performs.
///
/// `#[non_exhaustive]`: wlroots may add a node type in a later patch release,
/// and a `match` that would then stop compiling is not a service to anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NodeKind {
    /// A `wlr_scene_tree` — the only kind with children.
    Tree,
    /// A `wlr_scene_rect` — a solid premultiplied-RGBA rectangle.
    Rect,
    /// A `wlr_scene_buffer` — pixels, a client surface, or a drag icon.
    Buffer,
}

impl NodeKind {
    /// The kind `type_` names, or `None` for a tag this build does not know.
    ///
    /// Never panics: an unrecognised tag is what a wlroots patch release
    /// adding a node type looks like from here, and the caller treats it as
    /// "not a node I can work with" rather than aborting a compositor.
    pub(crate) fn from_raw(type_: sys::wlr_scene_node_type) -> Option<NodeKind> {
        match type_ {
            sys::wlr_scene_node_type::WLR_SCENE_NODE_TREE => Some(NodeKind::Tree),
            sys::wlr_scene_node_type::WLR_SCENE_NODE_RECT => Some(NodeKind::Rect),
            sys::wlr_scene_node_type::WLR_SCENE_NODE_BUFFER => Some(NodeKind::Buffer),
            _ => None,
        }
    }
}

/// A legacy id whose table row dies with its node.
///
/// [`RectId`] and [`BufferId`](crate::BufferId) predate [`NodeId`] and their
/// representation is frozen, so they cannot *be* addon-backed. They are made
/// self-cleaning instead: the node's [`NodePurge`] payload remembers which
/// legacy row (if any) names the same node, and drops it in the same hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyId {
    /// A row in `RuntimeInner::rects`.
    Rect(RectId),
    /// A row in `RuntimeInner::buffers`.
    Buffer(crate::BufferId),
}

/// The payload wlroots frees when a scene node dies.
///
/// Its [`Drop`] is the entire point: it runs inside wlroots'
/// `wlr_addon_set_finish` on the dying node, for every node of a destroy
/// cascade, which is the only moment at which this crate can learn that a
/// node it recorded is gone.
pub(crate) struct NodePurge {
    /// Cleared first, and unconditionally.
    ///
    /// The table removal below is best-effort (see [`Drop`]); this is not. A
    /// cleared flag is what makes a lookup miss even if its row survived, so
    /// no path can hand out a pointer to a freed node.
    alive: Rc<Cell<bool>>,
    /// Weak, never strong: a strong reference would be a cycle through the
    /// very tables this frees rows from, and would keep a `Runtime` alive
    /// past its last handle.
    runtime: Weak<RuntimeInner>,
    id: NodeId,
    legacy: Option<LegacyId>,
}

#[cfg(test)]
thread_local! {
    /// Test-only witness that node payloads are freed exactly once.
    ///
    /// Deliberately *not* [`crate::addon::DESTROY_COUNT`] — see
    /// [`crate::addon::addon_destroy_untracked`] for why this kind needs its
    /// own — and deliberately **per thread** rather than process-wide.
    ///
    /// A process-wide counter would have exactly the defect that motivated the
    /// untracked kind in the first place: the test harness runs each test on
    /// its own thread, several of this crate's tests destroy scene nodes, and a
    /// test asserting a *delta* across a shared counter fails whenever another
    /// one happens to destroy something between its two reads. That flaked
    /// roughly one run in ten. Every node this crate tracks is created and
    /// destroyed on one thread (the handles are `!Send`, and a cascade runs
    /// synchronously inside the `wlr_scene_node_destroy` that started it), so
    /// counting per thread measures exactly the cascade under test and nothing
    /// else. With `--test-threads=1` the tests run sequentially on one thread,
    /// where a delta across a single call is equally unpolluted.
    static NODE_DESTROY_COUNT: Cell<usize> = const { Cell::new(0) };
}

/// How many node payloads this thread has freed. Test-only.
#[cfg(test)]
pub(crate) fn node_destroy_count() -> usize {
    NODE_DESTROY_COUNT.with(Cell::get)
}

impl Drop for NodePurge {
    fn drop(&mut self) {
        // This runs under an `extern "C"` frame (wlroots' own
        // `wlr_addon_set_finish`), where an unwind aborts the process. Nothing
        // below can panic: `Cell::set`, `Weak::upgrade`, `try_borrow_mut` and
        // `HashMap::remove` are all total.
        self.alive.set(false);

        if let Some(inner) = self.runtime.upgrade() {
            // `try_borrow_mut`, not `borrow_mut`: a panic here would be an
            // abort rather than a catchable failure. The crate's own rule is
            // that no borrow of these tables is ever held across a call into
            // wlroots (see `runtime.rs`'s module doc), so this cannot fail —
            // and if it ever did, the cleared flag above has already made
            // every lookup miss, so the worst outcome is a leaked row rather
            // than a pointer to freed memory.
            if let Ok(mut nodes) = inner.nodes.try_borrow_mut() {
                nodes.remove(&self.id);
            }
            match self.legacy {
                Some(LegacyId::Rect(rect)) => {
                    if let Ok(mut rects) = inner.rects.try_borrow_mut() {
                        rects.remove(&rect);
                    }
                }
                Some(LegacyId::Buffer(buffer)) => {
                    if let Ok(mut buffers) = inner.buffers.try_borrow_mut() {
                        buffers.remove(&buffer);
                    }
                }
                None => {}
            }
        }

        // `try_with`, not `with`: this frame is `extern "C"`, and `with` panics
        // if the thread's TLS is already being destroyed. Nothing above may
        // unwind, and neither may this.
        #[cfg(test)]
        let _ = NODE_DESTROY_COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

addon_kind!(
    untracked
    /// The node id payload's addon kind.
    ///
    /// The name is part of the on-object representation a debugger prints
    /// when it walks a node's addon set, and distinguishes this crate's
    /// payload from any other consumer's.
    pub(crate) NODE_ADDON_IMPL: NodePurge = c"wlr-rs-scene-node"
);

/// The [`NodeId`] already attached to `node`, if any.
///
/// # Safety
///
/// `node` must point at a live `wlr_scene_node`.
pub(crate) unsafe fn find_node_id(node: *const sys::wlr_scene_node) -> Option<NodeId> {
    // SAFETY: the caller guarantees `node` is live, so its embedded
    // `wlr_addon_set` is initialised; `wlr_addon_find` only walks it.
    unsafe {
        let payload = Addon::<NodePurge>::find(
            &raw const (*node).addons,
            NODE_ADDON_IMPL.owner(),
            &NODE_ADDON_IMPL,
        );
        if payload.is_null() {
            return None;
        }
        Some(Addon::data(payload).id)
    }
}

/// Attach a fresh [`NodeId`] to `node`, returning it and its liveness flag.
///
/// # Safety
///
/// `node` must point at a live `wlr_scene_node` that does **not** already
/// carry one of these payloads — `wlr_addon_init` `assert()`s on a duplicate
/// and aborts. Callers that cannot rule one out go through
/// `Runtime::ensure_node_id`, which looks first.
pub(crate) unsafe fn attach_node_id(
    node: *mut sys::wlr_scene_node,
    runtime: &Rc<RuntimeInner>,
    legacy: Option<LegacyId>,
) -> (NodeId, Rc<Cell<bool>>) {
    let id = NodeId(next_id());
    let alive = Rc::new(Cell::new(true));
    // SAFETY: the caller guarantees `node` is live and carries no payload of
    // this kind, which is `Addon::attach`'s precondition. The payload's
    // `Drop` is what `NODE_ADDON_IMPL`'s destroy hook runs.
    unsafe {
        Addon::attach(
            &raw mut (*node).addons,
            NODE_ADDON_IMPL.owner(),
            &NODE_ADDON_IMPL,
            NodePurge {
                alive: Rc::clone(&alive),
                runtime: Rc::downgrade(runtime),
                id,
                legacy,
            },
        );
    }
    (id, alive)
}

/// A borrow-scoped view of one scene node.
///
/// Read-only, and cannot outlive the borrow that produced it — the usual
/// bargain in this crate (see the crate docs). Mutation goes through
/// [`Runtime`](crate::Runtime) by [`NodeId`], which misses cleanly on a node
/// that has since died; a handle cannot, which is exactly why it is scoped.
pub struct SceneNode<'h> {
    raw: NonNull<sys::wlr_scene_node>,
    id: NodeId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneNode<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_node`.
    /// * `id` must be the id attached to it.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_scene_node,
        id: NodeId,
    ) -> SceneNode<'h> {
        SceneNode {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_node"),
            id,
            _scope: PhantomData,
        }
    }

    /// This node's id — the storable half of the pair.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// What this node is.
    ///
    /// `None` for a tag this build of the crate does not know, which is what a
    /// wlroots patch release adding a node type looks like from here.
    pub fn kind(&self) -> Option<NodeKind> {
        // SAFETY: the handle's lifetime guarantees the node is live.
        NodeKind::from_raw(unsafe { (*self.raw.as_ptr()).type_ })
    }

    /// Whether this node is enabled *in itself*.
    ///
    /// A node with a disabled ancestor is not drawn even though this reads
    /// `true` — wlroots keeps each node's own flag and composes them at draw
    /// time. [`coords`](Self::coords) is the query that answers "is this
    /// actually visible".
    pub fn enabled(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the node is live.
        unsafe { (*self.raw.as_ptr()).enabled }
    }

    /// This node's position **relative to its parent**.
    pub fn position(&self) -> (i32, i32) {
        // SAFETY: the handle's lifetime guarantees the node is live.
        unsafe { ((*self.raw.as_ptr()).x, (*self.raw.as_ptr()).y) }
    }

    /// This node's layout-local coordinates.
    ///
    /// `None` when this node or any ancestor is disabled — wlroots' own
    /// `wlr_scene_node_coords` reports that as a `false` return rather than a
    /// coordinate, and this preserves the distinction instead of flattening it
    /// to `(0, 0)`.
    pub fn coords(&self) -> Option<(i32, i32)> {
        let mut lx = 0;
        let mut ly = 0;
        // SAFETY: the handle's lifetime guarantees the node is live; the two
        // out-parameters are live stack locals.
        let found =
            unsafe { sys::wlr_scene_node_coords(self.raw.as_ptr(), &raw mut lx, &raw mut ly) };
        found.then_some((lx, ly))
    }

    /// The id of this node's parent tree.
    ///
    /// `None` at the scene root, which has no parent — and also `None` for a
    /// parent that carries no [`NodeId`] yet, since a bare handle has no
    /// runtime to mint one through (the same limit
    /// [`SceneTree::children`] documents). Use
    /// [`Runtime::node_parent`](crate::Runtime::node_parent), which mints,
    /// when the two cases must be told apart.
    pub fn parent(&self) -> Option<NodeId> {
        // SAFETY: the handle's lifetime guarantees the node is live, and every
        // `parent` in a scene is either a live tree or null at the root.
        unsafe {
            let parent = (*self.raw.as_ptr()).parent;
            if parent.is_null() {
                return None;
            }
            find_node_id(&raw const (*parent).node)
        }
    }

    /// This node as a tree, if it is one.
    ///
    /// The tag is checked first. `wlr_scene_tree_from_node` is undefined
    /// behaviour on any other kind — it is a pointer cast, not a checked
    /// downcast — so this is the only shape in which the crate performs it.
    pub fn try_as_tree(&self) -> Option<SceneTree<'h>> {
        if self.kind() != Some(NodeKind::Tree) {
            return None;
        }
        // SAFETY: the tag says this node is a tree, which is exactly
        // `wlr_scene_tree_from_node`'s precondition, and the handle's lifetime
        // guarantees the node is live. The result inherits `'h`, which is
        // sound because it names the same allocation.
        Some(unsafe {
            SceneTree::from_raw_with_id(sys::wlr_scene_tree_from_node(self.raw.as_ptr()), self.id)
        })
    }

    /// This node as a rect, if it is one. Tag-checked, as
    /// [`try_as_tree`](Self::try_as_tree) explains.
    pub fn try_as_rect(&self) -> Option<SceneRect<'h>> {
        if self.kind() != Some(NodeKind::Rect) {
            return None;
        }
        // SAFETY: as for `try_as_tree`, for `WLR_SCENE_NODE_RECT`.
        Some(unsafe {
            SceneRect::from_raw_with_id(sys::wlr_scene_rect_from_node(self.raw.as_ptr()), self.id)
        })
    }

    /// This node as a buffer, if it is one. Tag-checked, as
    /// [`try_as_tree`](Self::try_as_tree) explains.
    pub fn try_as_buffer(&self) -> Option<SceneBuffer<'h>> {
        if self.kind() != Some(NodeKind::Buffer) {
            return None;
        }
        // SAFETY: as for `try_as_tree`, for `WLR_SCENE_NODE_BUFFER`.
        Some(unsafe {
            SceneBuffer::from_raw_with_id(
                sys::wlr_scene_buffer_from_node(self.raw.as_ptr()),
                self.id,
            )
        })
    }

    /// The raw node, for a consumer reaching past this crate.
    ///
    /// Valid only for as long as the handle is. Nothing done through it is
    /// this crate's business — in particular `wlr_scene_node_destroy` through
    /// this pointer leaves every table row naming the node stale, since the
    /// purge runs off the addon and the addon *would* fire, but the crate's
    /// own `destroy_node` guard would not have refused a protected node.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_node {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneNode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneNode")
            .field("id", &self.id)
            .field("kind", &self.kind())
            .field("enabled", &self.enabled())
            .field("position", &self.position())
            .finish()
    }
}

/// A borrow-scoped view of one scene tree.
///
/// Same bargain as [`SceneNode`]: read-only, cannot escape the borrow.
pub struct SceneTree<'h> {
    raw: NonNull<sys::wlr_scene_tree>,
    id: NodeId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneTree<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_tree`.
    /// * `id` must be the id attached to its embedded node.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_scene_tree,
        id: NodeId,
    ) -> SceneTree<'h> {
        SceneTree {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_tree"),
            id,
            _scope: PhantomData,
        }
    }

    /// This tree's id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// This tree as a plain node.
    pub fn node(&self) -> SceneNode<'h> {
        // SAFETY: `wlr_scene_tree` embeds its node at offset 0 and the handle's
        // lifetime guarantees the tree is live; the id is the node's own.
        unsafe { SceneNode::from_raw_with_id(&raw mut (*self.raw.as_ptr()).node, self.id) }
    }

    /// How many children this tree has, tracked or not.
    pub fn child_count(&self) -> usize {
        let mut n = 0;
        // SAFETY: the handle's lifetime guarantees the tree is live, so
        // `children` is an initialised, walkable `wl_list` head.
        unsafe {
            for _ in sys::wl_list_iter::<sys::wlr_scene_node>::new(
                &raw mut (*self.raw.as_ptr()).children,
                std::mem::offset_of!(sys::wlr_scene_node, link),
            ) {
                n += 1;
            }
        }
        n
    }

    /// This tree's children that carry a [`NodeId`], bottom to top.
    ///
    /// Sibling order **is** stacking order: wlroots appends each new child at
    /// the end of the list, so the last element is the topmost.
    ///
    /// Children with no id are skipped — a node only has one once this crate
    /// created it or something observed it (see the module docs). Use
    /// [`Runtime::node_children`](crate::Runtime::node_children), which gives
    /// every child an id first, when the full list is what you want.
    pub fn children(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        // SAFETY: the handle's lifetime guarantees the tree is live, and every
        // element of `children` is a live `wlr_scene_node`.
        unsafe {
            for child in sys::wl_list_iter::<sys::wlr_scene_node>::new(
                &raw mut (*self.raw.as_ptr()).children,
                std::mem::offset_of!(sys::wlr_scene_node, link),
            ) {
                if let Some(id) = find_node_id(child) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// The raw tree. Valid only for as long as the handle is.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_tree {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneTree<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneTree")
            .field("id", &self.id)
            .field("child_count", &self.child_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    /// The `IN_HANDLER` argument in `dispatch.rs` rests on every handle being
    /// thread-bound; a new one that is not would silently void it.
    #[test]
    fn scene_handles_are_thread_bound() {
        assert_not_impl_any!(SceneNode<'static>: Send, Sync);
        assert_not_impl_any!(SceneTree<'static>: Send, Sync);
        assert_not_impl_any!(SceneRect<'static>: Send, Sync);
        assert_not_impl_any!(SceneBuffer<'static>: Send, Sync);
    }

    /// The three tags this crate knows, pinned against the header rather than
    /// trusted from a comment — the same discipline
    /// `capability_bits_match_the_wayland_protocol` applies to seat caps.
    #[test]
    fn node_kind_matches_the_wlroots_tags() {
        assert_eq!(
            NodeKind::from_raw(sys::wlr_scene_node_type::WLR_SCENE_NODE_TREE),
            Some(NodeKind::Tree)
        );
        assert_eq!(
            NodeKind::from_raw(sys::wlr_scene_node_type::WLR_SCENE_NODE_RECT),
            Some(NodeKind::Rect)
        );
        assert_eq!(
            NodeKind::from_raw(sys::wlr_scene_node_type::WLR_SCENE_NODE_BUFFER),
            Some(NodeKind::Buffer)
        );
    }

    /// A tag from the future must miss, not panic: this runs on the hit-test
    /// path, which a compositor reaches from a C frame.
    #[test]
    fn an_unknown_node_tag_is_none_rather_than_a_panic() {
        assert_eq!(NodeKind::from_raw(sys::wlr_scene_node_type(9999)), None);
    }

    /// Dangling ids must sit in the reserved top of the counter's range, well
    /// clear of anything a real process issues.
    #[test]
    fn dangling_ids_cannot_collide_with_issued_ones() {
        assert_eq!(NodeId::dangling_for_test(), NodeId(u64::MAX));
        assert_eq!(RectId::dangling_for_test(), RectId(u64::MAX));
        assert!(next_id() < u64::MAX / 2);
    }
}
