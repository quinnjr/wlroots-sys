//! Solid-colour scene nodes, addressed by id.
//!
//! A `wlr_scene_rect` is owned by the scene tree it was created in and is
//! freed with it, so this crate stores only the pointer and never a `Drop`.
//! [`Runtime::remove_rect`](crate::Runtime::remove_rect) (0.20.5) destroys
//! one explicitly; short of that, a rect lives as long as the
//! [`Runtime`](crate::Runtime) that made it, or as long as the toplevel it
//! is parented into for one created by
//! [`Runtime::add_rect_in_toplevel`](crate::Runtime::add_rect_in_toplevel).

/// Identifies a solid-colour rect in the scene.
///
/// Not addon-backed: a `wlr_scene_rect` has an addon set, but nothing
/// announces its destruction to this crate (it dies with its tree), so there
/// is no destroy hook to key off. The id comes from the same monotonic
/// counter every other id in this crate does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RectId(pub(crate) u64);
