//! Solid-colour scene nodes, addressed by id.
//!
//! A `wlr_scene_rect` is owned by the scene tree it was created in and is
//! freed with it, so this crate stores only the pointer and never a `Drop`.
//! There is no removal by id in 0.20.1: a rect lives as long as the
//! [`Runtime`](crate::Runtime) that made it. That is exactly what the two
//! consumers need — a background, and (at parity) decoration strips — and
//! adding removal later is additive.

/// Identifies a solid-colour rect in the scene.
///
/// Not addon-backed: a `wlr_scene_rect` has an addon set, but nothing
/// announces its destruction to this crate (it dies with its tree), so there
/// is no destroy hook to key off. The id comes from the same monotonic
/// counter every other id in this crate does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RectId(pub(crate) u64);
