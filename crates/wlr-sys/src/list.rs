//! Helpers for libwayland's intrusive doubly-linked list, `wl_list`.
//!
//! The C API for `wl_list` is half functions and half macros. The functions
//! (`wl_list_init`, `wl_list_insert`, `wl_list_remove`, ...) are real exported
//! symbols and come from [`wayland_sys::server`]. The macros — `wl_container_of`
//! and `wl_list_for_each_safe` — have no symbols, and are provided here.

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
///
/// The result inherits `$ptr`'s provenance, including its *permission*. A
/// `*const` input is accepted and still yields a `*mut`, but the cast launders
/// only the type, never the permission: writing through the result is sound only
/// when `$ptr` itself carried write permission — i.e. it came from `&raw mut` or
/// a `*mut`. A pointer from `&raw const` does not become writable here even if
/// the memory behind it is mutable. The normal case is unaffected, since
/// callbacks are handed a `*mut wl_listener`.
///
/// The subtraction must also stay inside the same allocated object, which is
/// implied by "`$ptr` points at the `$field` member of a live `$container`".
#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $container:ty, $($field:tt)+) => {{
        let ptr = $ptr as *const u8;
        let offset = ::core::mem::offset_of!($container, $($field)+);
        ptr.sub(offset) as *mut $container
    }};
}

/// An iterator over the containers linked into a `wl_list`.
///
/// This is C's `wl_list_for_each_safe`, not `wl_list_for_each`: the cursor
/// advances to the next link *before* an item is handed back, so unlinking (or
/// freeing) the element you were just given is supported — which is what
/// destroy handlers do.
///
/// What is *not* supported is disturbing the element after the current one.
/// `next()` has already cached a pointer to it, so removing or freeing it mid
/// iteration leaves the iterator holding a dangling link. Collect first if you
/// need to mutate the list ahead of the cursor.
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
    /// These are invariants over the iterator's whole lifetime, not just its
    /// construction — [`Iterator::next`] is a *safe* method that dereferences on
    /// every call, so a violation later is still on the caller:
    ///
    /// * `head` must point at an initialised `wl_list` that is the sentinel head
    ///   of the list (not one of its entries) and must outlive the iterator.
    /// * Every entry reachable from the cursor must remain live and embedded in
    ///   a `T` at `offset` for as long as the iterator exists.
    /// * The list may be modified only at or behind the cursor: permanently
    ///   unlinking (or freeing) an already-yielded element is fine; disturbing
    ///   the element *after* the cursor is not, because its address is already
    ///   cached. Re-inserting a yielded element into the same list is also not
    ///   supported — it will be yielded twice, or loop forever if re-inserted at
    ///   the tail.
    /// * In particular, do not dispatch the Wayland event loop or run any
    ///   wlroots destroy path while an iterator is outstanding: either can free
    ///   arbitrary entries. Collect first if you need to.
    ///
    /// Note that the cursor is primed here, at construction, not on the first
    /// [`Iterator::next`] call — so these constraints apply from this point, not
    /// from first use.
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
/// `$field` is a field *path*, so the `wl_list` may be nested. That matters more
/// than it looks: a `wl_signal`'s entries are linked through the `link` field of
/// an embedded `wl_listener`, so the path is `listener.link`, not `listener`.
/// Those two offsets agree today only because `wl_listener` happens to declare
/// `link` first — name the real path and the code stops depending on that.
///
/// ```ignore
/// // link field directly in the container
/// for output in unsafe { wlr_sys::wl_list_for_each!(&raw mut (*layout).outputs, wlr_output_layout_output, link) } {
///     // ...
/// }
///
/// // link field nested inside an embedded wl_listener
/// for sub in unsafe { wlr_sys::wl_list_for_each!(&raw mut (*signal).listener_list, Subscriber, listener.link) } {
///     // ...
/// }
/// ```
///
/// # Safety
///
/// Same contract as [`wl_list_iter::new`].
#[macro_export]
macro_rules! wl_list_for_each {
    ($head:expr, $container:ty, $($field:tt)+) => {
        $crate::list::wl_list_iter::<$container>::new(
            $head,
            ::core::mem::offset_of!($container, $($field)+),
        )
    };
}
