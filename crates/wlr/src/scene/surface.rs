//! Client surfaces in the scene: what the scene does for you once one is in it.
//!
//! # `wlr_scene_surface` is not a node
//!
//! It is an addon hanging off a [`SceneBuffer`](crate::SceneBuffer) — the node
//! is the buffer node, and the scene surface is the extra state wlroots keeps
//! for a buffer node whose pixels come from a client. So there is no
//! `SceneSurfaceId`: a scene surface is named by the [`NodeId`] of its buffer
//! node, and [`SceneSurface::buffer_node`] is the way back.
//!
//! # What the scene implements on your behalf
//!
//! `wlr_scene_surface_create`'s own documentation lists what putting a surface
//! in the scene silently buys a compositor, and it is worth reproducing rather
//! than linking, because it decides which protocols a compositor still has to
//! implement itself:
//!
//! - `wp_viewporter`
//! - `wp_presentation_time`
//! - `wp_fractional_scale_v1`
//! - `wp_alpha_modifier_v1`
//! - `wp_linux_drm_syncobj_v1`
//! - `zwp_linux_dmabuf_v1` presentation feedback (once the scene has been given
//!   a `linux_dmabuf_v1`)
//!
//! and, transparently: preferred buffer scale and transform (based on the
//! output with the largest visible area of the surface), xwayland restacking,
//! and `wl_surface.enter`/`leave` for outputs.
//!
//! Every toplevel and every layer surface this crate puts in the scene goes
//! through that path — `wlr_scene_xdg_surface_create` and
//! `wlr_scene_layer_surface_v1_create` build on it — so a compositor using this
//! crate has those behaviours already, and does **not** need to implement those
//! protocols itself to get them.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::scene::{NodeId, SceneBuffer};
use crate::sys;

/// A borrow-scoped view of one client surface in the scene.
///
/// Read-only. Obtained from
/// [`Runtime::with_scene_surface`](crate::Runtime::with_scene_surface), which
/// asks wlroots whether a buffer node is backed by a surface — that check is
/// `wlr_scene_surface_try_from_buffer`, which returns null rather than
/// misbehaving, so it is safe on any buffer node.
///
/// The `wlr_surface` itself is deliberately not exposed: this crate has no
/// safe surface type yet, and handing out the raw pointer as if it were an
/// identity would be coverage in name only. What a compositor can do with a
/// scene surface today — send frame-done, clip it, find it by hit test — is
/// here and on [`Runtime`](crate::Runtime).
pub struct SceneSurface<'h> {
    raw: NonNull<sys::wlr_scene_surface>,
    node: NodeId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneSurface<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_surface`.
    /// * `node` must be the id of the buffer node it hangs off.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_node(
        raw: *mut sys::wlr_scene_surface,
        node: NodeId,
    ) -> SceneSurface<'h> {
        SceneSurface {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_surface"),
            node,
            _scope: PhantomData,
        }
    }

    /// The id of the buffer node this surface is drawn by.
    ///
    /// The storable half: a `SceneSurface` cannot escape its borrow, but this
    /// id can, and every by-id call that names a scene surface takes it.
    pub fn buffer_node(&self) -> NodeId {
        self.node
    }

    /// This surface's buffer node, as a handle.
    pub fn buffer(&self) -> SceneBuffer<'h> {
        // SAFETY: the handle's lifetime guarantees the scene surface is live,
        // and `buffer` is the node it hangs off — non-null for the scene
        // surface's whole life, since the addon is destroyed with it.
        unsafe { SceneBuffer::from_raw_with_id((*self.raw.as_ptr()).buffer, self.node) }
    }

    /// The raw scene surface. Valid only for as long as the handle is.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_surface {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneSurface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneSurface")
            .field("buffer_node", &self.node)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use static_assertions::assert_not_impl_any;

    /// As for every other handle in this crate; see `dispatch.rs`.
    #[test]
    fn scene_surfaces_are_thread_bound() {
        assert_not_impl_any!(SceneSurface<'static>: Send, Sync);
    }
}
