//! Helpers for libwayland's intrusive doubly-linked list, `wl_list`.
//!
//! The C API for `wl_list` is half functions and half macros. The functions
//! (`wl_list_init`, `wl_list_insert`, `wl_list_remove`, ...) are real exported
//! symbols and come from [`wayland_sys::server`]. The macros — `wl_container_of`
//! and `wl_list_for_each` — have no symbols, and are provided here.

use std::marker::PhantomData;

use wayland_sys::common::wl_list;

/// Recover a pointer to the struct that embeds a given field.
///
/// This is libwayland's `wl_container_of` macro. It is how every wlroots
/// callback gets from the `*mut wl_listener` it is handed back to the struct
/// that owns the listener:
///
/// ```ignore
/// #[repr(C)]
/// struct Output {
///     wlr_output: *mut wlr_sys::wlr_output,
///     frame: wayland_sys::server::wl_listener,
/// }
///
/// unsafe extern "C" fn on_frame(listener: *mut wl_listener, _data: *mut c_void) {
///     let output: *mut Output = unsafe { wlr_sys::container_of!(listener, Output, frame) };
///     // ...
/// }
/// ```
///
/// # Safety
///
/// `$ptr` must point at the `$field` member of a live `$container`. Getting this
/// wrong produces a wild pointer, not a panic. The container type must be
/// `#[repr(C)]` (or otherwise have a stable layout) for the offset to mean
/// anything.
#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $container:ty, $field:ident) => {{
        let ptr = $ptr as *const u8;
        let offset = ::std::mem::offset_of!($container, $field);
        ptr.sub(offset) as *mut $container
    }};
}

/// An iterator over the containers linked into a `wl_list`.
///
/// This is the `wl_list_for_each` macro. It is *not* the safe variant: removing
/// the current element during iteration invalidates the iterator, exactly as in
/// C. Collect first if you intend to mutate the list.
pub struct wl_list_iter<T> {
    head: *mut wl_list,
    next: *mut wl_list,
    offset: usize,
    _marker: PhantomData<*mut T>,
}

impl<T> wl_list_iter<T> {
    /// Iterate `head`, yielding `*mut T` for each entry.
    ///
    /// `offset` is the byte offset of the `wl_list` link field within `T`, as
    /// produced by [`std::mem::offset_of!`].
    ///
    /// # Safety
    ///
    /// `head` must point at an initialised `wl_list` that is the sentinel head of
    /// the list (not one of its entries), every entry must be embedded in a live
    /// `T` at `offset`, and the list must not be modified while iterating.
    pub unsafe fn new(head: *mut wl_list, offset: usize) -> Self {
        Self {
            head,
            next: unsafe { (*head).next },
            offset,
            _marker: PhantomData,
        }
    }
}

impl<T> Iterator for wl_list_iter<T> {
    type Item = *mut T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.head {
            return None;
        }
        let link = self.next;
        // SAFETY: `link` is a live entry of the list, which the constructor's
        // contract guarantees is embedded in a `T` at `self.offset`.
        let item = unsafe { (link as *mut u8).sub(self.offset) as *mut T };
        self.next = unsafe { (*link).next };
        Some(item)
    }
}

/// Iterate a `wl_list` of `$container`s linked through `$field`.
///
/// Sugar over [`wl_list_iter::new`] that computes the field offset for you.
///
/// ```ignore
/// for output in unsafe { wlr_sys::wl_list_for_each!(&mut (*layout).outputs, wlr_output_layout_output, link) } {
///     // ...
/// }
/// ```
///
/// # Safety
///
/// Same contract as [`wl_list_iter::new`].
#[macro_export]
macro_rules! wl_list_for_each {
    ($head:expr, $container:ty, $field:ident) => {
        $crate::list::wl_list_iter::<$container>::new(
            $head,
            ::std::mem::offset_of!($container, $field),
        )
    };
}
