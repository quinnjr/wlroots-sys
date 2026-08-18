//! Safe bindings to wlroots.
//!
//! # The ownership model, in one page
//!
//! wlroots owns nearly everything you touch. Its 0.20 API has 117
//! `wlr_*_create` functions but only 38 `wlr_*_destroy`, and 106 types that
//! announce their own death through a `destroy` signal — so an object is
//! typically handed to you in a callback and freed whenever wlroots decides.
//! A reference that outlives that moment is a use-after-free, and no amount of
//! care at the call site prevents it.
//!
//! This crate's answer is that **handles are borrow-scoped and cannot be
//! stored**. [`Output`] carries a lifetime bound to the handler call it was
//! passed to, and its constructor is private, so a handle escaping a handler is
//! a compile error rather than a documented rule. Nothing about that has to be
//! remembered.
//!
//! What you store instead is an [`OutputId`]: stable, comparable, hashable, and
//! valid to hold for as long as you like. Long-lived state is yours, keyed by
//! id:
//!
//! ```
//! use std::collections::HashMap;
//! use wlr::{Output, OutputHandler, OutputId};
//!
//! #[derive(Default)]
//! struct App {
//!     outputs: HashMap<OutputId, MyOutput>,
//! }
//!
//! #[derive(Default)]
//! struct MyOutput {
//!     frames: u64,
//! }
//!
//! impl OutputHandler for App {
//!     fn new_output(&mut self, output: &Output<'_>) {
//!         self.outputs.insert(output.id(), MyOutput::default());
//!     }
//!
//!     fn frame(&mut self, output: &Output<'_>) {
//!         if let Some(mine) = self.outputs.get_mut(&output.id()) {
//!             mine.frames += 1;
//!         }
//!         let _ = output.commit();
//!     }
//!
//!     fn destroyed(&mut self, id: OutputId) {
//!         // Removing an id you never saw is harmless, and it can happen —
//!         // see `OutputHandler::destroyed`. Indexing here would abort.
//!         self.outputs.remove(&id);
//!     }
//! }
//! ```
//!
//! The cost, stated plainly: you key your own state, and an operation spanning
//! two objects means two lookups. That is the price of dangling being
//! impossible rather than merely unlikely.
//!
//! Two rules follow from the same place and are worth knowing before you write
//! a handler. wlroots emits signals *synchronously*, from inside its own API
//! calls, so a handler always runs underneath one — which means a handler must
//! not drive the event loop ([`EventLoop::dispatch`] refuses, returning
//! [`Error::Reentrant`]), and a handler must not panic, because it runs under an
//! `extern "C"` frame and a panic there aborts the process. Record the problem
//! in your own state and act on it once control is back in your hands.
//!
//! # Versioning
//!
//! `wlr`'s minor version tracks the wlroots minor it binds; see this crate's
//! README. `docs/superpowers/specs/2026-08-03-wlr-safe-wrapper-design.md` in the
//! repository records why, along with the rest of the design.

#![warn(missing_docs)]

pub(crate) mod sys;

mod addon;
mod backend;
mod buffer;
mod decoration;
mod dispatch;
mod display;
mod error;
mod geom;
mod handler;
mod id;
mod interest;
mod layer;
mod log;
mod output;
mod region;
mod render;
mod runtime;
mod scene;
mod seat;
mod toplevel;

pub use backend::{Backend, Until};
pub use buffer::{Buffer, BufferId};
pub use decoration::DecorationMode;
pub use display::{Display, EventLoop};
pub use error::{Error, Result};
pub use geom::{Box2D, FBox, Transform};
pub use handler::{FdHandler, Handlers, LoopHandler, OutputHandler, SeatHandler, ToplevelHandler};
pub use id::{OutputId, SourceId};
pub use interest::{Interest, Readiness};
pub use layer::{Anchor, Layer, LayerSurface, LayerSurfaceId};
pub use log::{LogLevel, init_logging, log_verbosity};
pub use output::Output;
pub use region::{Region, RegionRef};
pub use render::{
    Allocator, AllocatorRef, AlphaMode, BlendMode, BufferCaps, BufferPassOptions, ChromaLocation,
    Cie1931Xy, ColorEncoding, ColorEncodings, ColorLuminances, ColorPrimaries, ColorRange,
    ColorTransform, DMABUF_MAX_PLANES, DmabufAttributes, DmabufAttributesRef, DmabufPlane,
    DrmFormat, DrmFormatRef, DrmFormatSet, DrmFormatSetRef, Egl, FilterMode, FourCc, LockedBuffer,
    Modifier, NamedPrimaries, OwnedBuffer, Pixman, ReadPixels, RectOptions, RenderColor,
    RenderPass, RenderTimer, Renderer, RendererFeatures, RendererRef, SWAPCHAIN_CAP, Swapchain,
    SyncFlags, SyncTimeline, SyncWaiter, Texture, TextureOptions, TransferFunction,
    TransferFunctions,
};
#[cfg(wlr_has_gles2_renderer)]
pub use render::{Gles2, Gles2TextureAttribs};
#[cfg(wlr_has_vulkan_renderer)]
pub use render::{Vk, VkImageAttribs};
pub use runtime::{Band, Runtime};
pub use scene::RectId;
pub use seat::{KeyEvent, Modifiers};
pub use toplevel::{Edges, Toplevel, ToplevelId};

/// The wlroots version this build of `wlr` binds, as `(major, minor)`.
///
/// Read from `wlr-sys`'s own header constants rather than from this crate's
/// version, so a dependency that does not match the branch is observable
/// instead of silent.
pub fn wlroots_version() -> (u32, u32) {
    (sys::WLR_VERSION_MAJOR, sys::WLR_VERSION_MINOR)
}
