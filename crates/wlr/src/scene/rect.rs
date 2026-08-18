//! Read-only views of solid-colour scene nodes.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::scene::{NodeId, SceneNode};
use crate::sys;

/// A borrow-scoped view of one `wlr_scene_rect`.
///
/// Read-only; every mutation goes through
/// [`Runtime`](crate::Runtime) by [`NodeId`] (or by
/// [`RectId`](crate::RectId), for a rect made with the 0.20.1 API). Obtained
/// from [`SceneNode::try_as_rect`], which checks the node's tag first — the C
/// downcast is a bare pointer cast and undefined behaviour on any other kind.
pub struct SceneRect<'h> {
    raw: NonNull<sys::wlr_scene_rect>,
    id: NodeId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneRect<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_rect`.
    /// * `id` must be the id attached to its embedded node.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_scene_rect,
        id: NodeId,
    ) -> SceneRect<'h> {
        SceneRect {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_rect"),
            id,
            _scope: PhantomData,
        }
    }

    /// This rect's node id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// This rect as a plain node.
    pub fn node(&self) -> SceneNode<'h> {
        // SAFETY: `wlr_scene_rect` embeds its node at offset 0 and the handle's
        // lifetime guarantees the rect is live; the id is the node's own.
        unsafe { SceneNode::from_raw_with_id(&raw mut (*self.raw.as_ptr()).node, self.id) }
    }

    /// The rect's size, in layout pixels before any ancestor transform.
    pub fn size(&self) -> (i32, i32) {
        // SAFETY: the handle's lifetime guarantees the rect is live.
        unsafe { ((*self.raw.as_ptr()).width, (*self.raw.as_ptr()).height) }
    }

    /// The rect's colour, as the **premultiplied** RGBA
    /// [`Runtime::add_rect`](crate::Runtime::add_rect) takes.
    pub fn color(&self) -> [f32; 4] {
        // SAFETY: the handle's lifetime guarantees the rect is live; `color`
        // is a by-value `[f32; 4]` in the C struct, so this copies it.
        unsafe { (*self.raw.as_ptr()).color }
    }

    /// The raw rect. Valid only for as long as the handle is.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_rect {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneRect<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneRect")
            .field("id", &self.id)
            .field("size", &self.size())
            .field("color", &self.color())
            .finish()
    }
}
