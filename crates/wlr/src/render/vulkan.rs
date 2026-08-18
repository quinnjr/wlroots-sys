//! The Vulkan renderer's own surface.
//!
//! Gated on `wlr_has_vulkan_renderer`. Note the asymmetry with GLES2: wlroots
//! has `wlr_render_timer_is_gles2` but **no** `wlr_render_timer_is_vk`, so
//! there is no Vulkan counterpart to
//! `RenderTimer::is_gles2` to write.
//!
//! The view-type pattern and the reason for it are in `pixman.rs`.
//!
//! # Vulkan handles cross unchanged
//!
//! `VkInstance`, `VkPhysicalDevice` and `VkDevice` are dispatchable handles
//! (pointers); `VkImage` is a non-dispatchable one, which is a pointer on
//! 64-bit targets and a `u64` on 32-bit. They are handed out exactly as
//! `wlr-sys` generated them, with no normalisation and no `ash` dependency:
//! this crate wraps wlroots' use of Vulkan, and re-typing a handle would put
//! two definitions of it in one process. `VkImageLayout` and `VkFormat` are C
//! enums and cross as their raw integers.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::sys;

/// A [`Renderer`](crate::Renderer) that has been proved to be the Vulkan
/// renderer.
///
/// Obtained from [`Renderer::as_vulkan`](crate::Renderer::as_vulkan). Borrows the
/// renderer, so it cannot outlive it.
#[derive(Clone, Copy)]
pub struct Vk<'r> {
    raw: NonNull<sys::wlr_renderer>,
    _renderer: PhantomData<&'r ()>,
}

impl<'r> Vk<'r> {
    /// # Safety
    ///
    /// `raw` must point at a live `wlr_renderer` that outlives `'r` and for
    /// which `wlr_renderer_is_vk` returned true.
    pub(crate) unsafe fn from_raw(raw: *mut sys::wlr_renderer) -> Vk<'r> {
        Vk {
            raw: NonNull::new(raw).expect("wlroots handed us a null wlr_renderer"),
            _renderer: PhantomData,
        }
    }

    /// The `VkInstance` wlroots created.
    ///
    /// # Safety
    ///
    /// **Borrowed** — the renderer owns it and destroys it with itself, so it
    /// must not be destroyed here, and it is invalid once the renderer is
    /// dropped. Everything else about using it is Vulkan's contract, which this
    /// crate knows nothing about.
    pub unsafe fn instance(&self) -> sys::VkInstance {
        // SAFETY: `'r` guarantees the renderer is live and the type test that
        // produced this view proved it is the Vulkan one; the call is a field
        // read.
        unsafe { sys::wlr_vk_renderer_get_instance(self.raw.as_ptr()) }
    }

    /// The `VkPhysicalDevice` wlroots selected.
    ///
    /// # Safety
    ///
    /// As [`instance`](Vk::instance): borrowed, valid only while the renderer
    /// is.
    pub unsafe fn physical_device(&self) -> sys::VkPhysicalDevice {
        // SAFETY: as above.
        unsafe { sys::wlr_vk_renderer_get_physical_device(self.raw.as_ptr()) }
    }

    /// The `VkDevice` wlroots created.
    ///
    /// # Safety
    ///
    /// As [`instance`](Vk::instance): borrowed, valid only while the renderer
    /// is. Submitting work on it concurrently with wlroots' own is subject to
    /// Vulkan's external-synchronisation rules.
    pub unsafe fn device(&self) -> sys::VkDevice {
        // SAFETY: as above.
        unsafe { sys::wlr_vk_renderer_get_device(self.raw.as_ptr()) }
    }

    /// The index of the queue family wlroots submits on.
    ///
    /// A plain integer, so unlike the handles above this needs no `unsafe`.
    pub fn queue_family(&self) -> u32 {
        // SAFETY: as above.
        unsafe { sys::wlr_vk_renderer_get_queue_family(self.raw.as_ptr()) }
    }
}

impl std::fmt::Debug for Vk<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vk")
            .field("raw", &self.raw)
            .field("queue_family", &self.queue_family())
            .finish()
    }
}

/// How a Vulkan texture is backed: `wlr_vk_image_attribs`.
///
/// From [`Texture::vulkan_attribs`](crate::Texture::vulkan_attribs).
#[derive(Debug, Clone, Copy)]
pub struct VkImageAttribs {
    /// The `VkImage` holding the texture's pixels. Borrowed — the texture owns
    /// it.
    pub image: sys::VkImage,
    /// The image's current `VkImageLayout`, as its raw integer.
    pub layout: u32,
    /// The image's `VkFormat`, as its raw integer.
    pub format: u32,
}
