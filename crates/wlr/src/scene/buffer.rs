//! Read-only views of buffer scene nodes, and the options a buffer swap takes.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::geom::{FBox, Transform};
use crate::region::{Region, RegionRef};
use crate::render::{FilterMode, SyncTimeline};
use crate::scene::{NodeId, SceneNode};
use crate::sys;

/// What [`Runtime::set_scene_buffer`](crate::Runtime::set_scene_buffer) does
/// beyond swapping the pixels.
///
/// Every field is optional and the default is what wlroots reads out of a
/// zeroed `wlr_scene_buffer_set_buffer_options`: the whole buffer is damaged
/// and nothing is waited on.
#[derive(Debug, Default)]
pub struct SceneBufferOptions<'a> {
    damage: Option<&'a Region>,
    wait: Option<(&'a SyncTimeline, u64)>,
}

impl<'a> SceneBufferOptions<'a> {
    /// Options that damage the whole buffer and wait on nothing.
    pub fn new() -> SceneBufferOptions<'a> {
        SceneBufferOptions::default()
    }

    /// Damage only `region`, in **buffer-local** coordinates.
    ///
    /// An optimisation, and a sharp one: wlroots believes it. Under-reporting
    /// damage leaves stale pixels on screen rather than producing an error.
    pub fn damage(mut self, region: &'a Region) -> Self {
        self.damage = Some(region);
        self
    }

    /// Do not sample the buffer until `timeline` reaches `point`.
    ///
    /// Explicit synchronisation: the renderer waits on the timeline instead of
    /// on an implicit fence. Requires a renderer with timeline support — see
    /// [`RendererFeatures`](crate::RendererFeatures).
    pub fn wait(mut self, timeline: &'a SyncTimeline, point: u64) -> Self {
        self.wait = Some((timeline, point));
        self
    }

    /// The C struct these options describe, borrowing `self` for the call.
    ///
    /// The pointers it holds — the region and the timeline — are borrowed from
    /// `'a`, so the value must not outlive `self`. It is deliberately built at
    /// the call site rather than stored.
    pub(crate) fn as_c(&self) -> sys::wlr_scene_buffer_set_buffer_options {
        sys::wlr_scene_buffer_set_buffer_options {
            damage: self.damage.map_or(std::ptr::null(), |r| r.as_ptr()),
            wait_timeline: self
                .wait
                .map_or(std::ptr::null_mut(), |(timeline, _)| timeline.as_ptr()),
            wait_point: self.wait.map_or(0, |(_, point)| point),
        }
    }
}

/// A borrow-scoped view of one `wlr_scene_buffer`.
///
/// Read-only; every mutation goes through [`Runtime`](crate::Runtime) by
/// [`NodeId`]. Obtained from [`SceneNode::try_as_buffer`], which checks the
/// node's tag first — the C downcast is a bare pointer cast and undefined
/// behaviour on any other kind.
///
/// A buffer node is not only "pixels this crate uploaded": every mapped client
/// surface in the scene is one, which is what makes
/// [`Runtime::node_at`](crate::Runtime::node_at) able to answer "what is under
/// the cursor" at all.
pub struct SceneBuffer<'h> {
    raw: NonNull<sys::wlr_scene_buffer>,
    id: NodeId,
    _scope: PhantomData<&'h ()>,
}

impl<'h> SceneBuffer<'h> {
    /// # Safety
    ///
    /// * `raw` must point at a live `wlr_scene_buffer`.
    /// * `id` must be the id attached to its embedded node.
    /// * The handle must not outlive the borrow that established both.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_scene_buffer,
        id: NodeId,
    ) -> SceneBuffer<'h> {
        SceneBuffer {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_scene_buffer"),
            id,
            _scope: PhantomData,
        }
    }

    /// This buffer node's id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// This buffer node as a plain node.
    pub fn node(&self) -> SceneNode<'h> {
        // SAFETY: `wlr_scene_buffer` embeds its node at offset 0 and the
        // handle's lifetime guarantees the node is live; the id is its own.
        unsafe { SceneNode::from_raw_with_id(&raw mut (*self.raw.as_ptr()).node, self.id) }
    }

    /// Whether a buffer is currently attached.
    ///
    /// A scene buffer with none is legal — it simply draws nothing — and is
    /// what [`Runtime::create_scene_buffer`](crate::Runtime::create_scene_buffer)
    /// makes when handed `None`.
    pub fn has_buffer(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the node is live.
        !unsafe { (*self.raw.as_ptr()).buffer }.is_null()
    }

    /// The opacity multiplier applied when this node is composited. `1.0` by
    /// default.
    pub fn opacity(&self) -> f32 {
        // SAFETY: the handle's lifetime guarantees the node is live.
        unsafe { (*self.raw.as_ptr()).opacity }
    }

    /// The on-screen size the buffer is scaled to.
    ///
    /// `None` when it is wlroots' default of zero, which means "use the
    /// buffer's own size" — the distinction matters, because `(0, 0)` is not a
    /// size a caller would ever have set deliberately.
    pub fn dest_size(&self) -> Option<(i32, i32)> {
        // SAFETY: the handle's lifetime guarantees the node is live.
        let (w, h) = unsafe {
            (
                (*self.raw.as_ptr()).dst_width,
                (*self.raw.as_ptr()).dst_height,
            )
        };
        (w != 0 || h != 0).then_some((w, h))
    }

    /// The region of the buffer that is sampled.
    ///
    /// `None` when the whole buffer is — wlroots' default, spelled as an empty
    /// source box.
    pub fn source_box(&self) -> Option<FBox> {
        // SAFETY: the handle's lifetime guarantees the node is live;
        // `wlr_fbox` and `FBox` have identical layout (asserted in `geom.rs`).
        let raw = unsafe { (*self.raw.as_ptr()).src_box };
        let b = FBox::new(raw.x, raw.y, raw.width, raw.height);
        (!b.empty()).then_some(b)
    }

    /// The transform applied to the buffer.
    ///
    /// `None` for a value this build does not know — a wlroots patch release
    /// widening `wl_output_transform` is not a reason to abort.
    pub fn transform(&self) -> Option<Transform> {
        // SAFETY: the handle's lifetime guarantees the node is live.
        Transform::try_from(unsafe { (*self.raw.as_ptr()).transform }).ok()
    }

    /// How the buffer is sampled when scaled.
    pub fn filter(&self) -> FilterMode {
        // SAFETY: the handle's lifetime guarantees the node is live.
        match unsafe { (*self.raw.as_ptr()).filter_mode } {
            sys::wlr_scale_filter_mode::WLR_SCALE_FILTER_NEAREST => FilterMode::Nearest,
            // Bilinear is wlroots' own default and the zero value; anything
            // else this build does not know reads as the default rather than
            // as a panic, matching every other "from the future" case here.
            _ => FilterMode::Bilinear,
        }
    }

    /// The region declared fully opaque, as a hint to the renderer.
    ///
    /// Borrowed from the node, so it cannot outlive this handle.
    pub fn opaque_region(&self) -> RegionRef<'_> {
        // SAFETY: the handle's lifetime guarantees the node is live, and
        // `opaque_region` is a `pixman_region32` embedded in it by value —
        // initialised for the node's whole life by wlroots.
        unsafe { RegionRef::from_raw(&raw mut (*self.raw.as_ptr()).opaque_region) }
    }

    /// The raw buffer node. Valid only for as long as the handle is.
    pub fn as_ptr(&self) -> *mut sys::wlr_scene_buffer {
        self.raw.as_ptr()
    }
}

impl std::fmt::Debug for SceneBuffer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SceneBuffer")
            .field("id", &self.id)
            .field("has_buffer", &self.has_buffer())
            .field("opacity", &self.opacity())
            .field("dest_size", &self.dest_size())
            .finish()
    }
}
