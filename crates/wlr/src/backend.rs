//! Backend creation and the wiring from wlroots signals to handler traits.
//!
//! This is where C calls back into Rust. Two rules govern everything below and
//! are worth stating once rather than repeating at every call site:
//!
//! 1. A `wl_listener` is linked into an intrusive list owned by the signal. The
//!    memory holding its `link` must not be freed while it is still linked, so
//!    every registration is an RAII guard ([`Registration`]) that unlinks in
//!    `Drop` — not an unlink statement at the end of a function, which the `?`
//!    on a fallible dispatch call would skip.
//! 2. Nothing in a callback may unwind. A panic escaping an `extern "C"` fn has
//!    aborted the process since Rust 1.81, so the code reached from one avoids
//!    panicking paths where the condition is recoverable; see [`ensure_id`].

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::dispatch::{Dispatcher, Event};
use crate::id::{attach_id, find_id};
use crate::{Display, Error, EventLoop, OutputHandler, OutputId, Result, sys};

/// A wlroots backend.
///
/// Deliberately has no `Drop`. `wlr_backend_autocreate` registers the backend
/// for destruction with the event loop it was created on, so the backend is torn
/// down by `wl_display_destroy` in [`Display`]'s own `Drop`. Calling
/// `wlr_backend_destroy` here as well would be a double free; the `'d` lifetime
/// is what guarantees the ordering, by keeping the display borrowed for as long
/// as the `Backend` exists.
pub struct Backend<'d> {
    raw: NonNull<sys::wlr_backend>,
    _loop: PhantomData<&'d ()>,
}

/// A listener plus the context needed to route its event.
///
/// `#[repr(C)]` with `listener` first so the C callback can recover this struct
/// from the `*mut wl_listener` wlroots passes it.
#[repr(C)]
struct Bound<S> {
    listener: sys::wl_listener,
    dispatcher: *const Dispatcher<S>,
}

// `bound_of`'s cast is sound only while `listener` is `Bound`'s first field, at
// offset 0. This fails to compile if that ever stops being true.
const _: () = assert!(std::mem::offset_of!(Bound<()>, listener) == 0);

/// A [`Bound`] currently linked into a signal, unlinked when it drops.
///
/// The box is what keeps the listener's address stable: a `wl_list` is
/// intrusive, so the signal stores a pointer *into* this allocation and moving
/// the `Registration` (which moves only the `Box`, not its contents) must not
/// disturb it.
struct Registration<S> {
    bound: Box<Bound<S>>,
}

impl<S> Registration<S> {
    /// Link a fresh listener for `notify` into `signal`.
    ///
    /// # Safety
    ///
    /// * `signal` must point at an initialised `wl_signal` that outlives the
    ///   returned `Registration`.
    /// * `dispatcher` must remain valid, and its `emit` contract must hold, for
    ///   every call `notify` makes through it — i.e. for as long as the returned
    ///   `Registration` is alive.
    /// * `notify` must be prepared to recover a `Bound<S>` from the listener it
    ///   is handed, which is what [`bound_of`] does.
    unsafe fn link(
        signal: *mut sys::wl_signal,
        notify: sys::wl_notify_func_t,
        dispatcher: *const Dispatcher<S>,
    ) -> Self {
        let mut bound = Box::new(Bound {
            listener: sys::wl_listener {
                // `wl_signal_add` overwrites both ends via `wl_list_insert`, so
                // these nulls are never read.
                link: sys::wl_list {
                    prev: std::ptr::null_mut(),
                    next: std::ptr::null_mut(),
                },
                notify,
            },
            dispatcher,
        });

        // SAFETY: the caller guarantees `signal` is an initialised `wl_signal`,
        // and the listener is a freshly boxed one that nothing else has linked
        // anywhere. Its address stays put until this `Registration` drops,
        // which unlinks it before the box is freed.
        unsafe { sys::wl_signal_add(signal, &raw mut bound.listener) };

        Registration { bound }
    }
}

impl<S> Drop for Registration<S> {
    fn drop(&mut self) {
        // SAFETY: `link` put this listener into a signal's list and nothing
        // else ever unlinks it, so it is still linked here; and the caller of
        // `link` guaranteed the signal outlives this `Registration`, so the
        // neighbours `wl_list_remove` writes through are live. This runs before
        // the box is freed, which is the whole reason the unlink lives in
        // `Drop` rather than at the end of `run`: an early `?` return there
        // would free a still-linked listener.
        unsafe { remove_listener(&raw mut self.bound.listener) };
    }
}

impl<'d> Backend<'d> {
    /// Create whichever backend suits the environment.
    pub fn autocreate(loop_: &EventLoop<'d>) -> Result<Self> {
        // SAFETY: the borrow guarantees the loop is live. Passing null for the
        // session pointer means "do not hand back a session", which this slice
        // does not need.
        let raw = unsafe { sys::wlr_backend_autocreate(loop_.as_ptr(), std::ptr::null_mut()) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_backend_autocreate"))?;
        Ok(Backend {
            raw,
            _loop: PhantomData,
        })
    }

    /// Start the backend. New outputs are announced after this returns.
    pub fn start(&self) -> Result<()> {
        // SAFETY: `self` is live.
        if unsafe { sys::wlr_backend_start(self.raw.as_ptr()) } {
            Ok(())
        } else {
            Err(Error::Operation("wlr_backend_start"))
        }
    }

    /// Wire up handlers and dispatch `iterations` turns of the event loop.
    ///
    /// Takes a count rather than blocking forever so tests terminate; a
    /// blocking loop belongs with signal handling in a later slice.
    pub fn run<S: OutputHandler>(
        &self,
        display: &Display,
        state: &mut S,
        iterations: u32,
    ) -> Result<()> {
        // `state` is consumed into a raw pointer here and never touched as a
        // reference again for the rest of this function, so no `&mut S` is live
        // while a callback delivers through the dispatcher.
        let dispatcher = Dispatcher::new(&raw mut *state);

        // Declared after `dispatcher`, so it drops (and therefore unlinks)
        // before the dispatcher it points at. Bound to a named `_new_output`
        // rather than `_`, which would drop it immediately and unregister
        // before the loop even runs.
        //
        // SAFETY: `self` is live for this call, so `events.new_output` is an
        // initialised signal that outlives the registration. `dispatcher` is a
        // local that outlives it too (drop order above), and `S` never learns
        // of it, so nothing can hold a reference to it across `emit` — the
        // aliasing condition `Dispatcher::emit` requires. `on_new_output::<S>`
        // is written against `Bound<S>`, which is what `link` builds.
        let _new_output = unsafe {
            Registration::link(
                &raw mut (*self.raw.as_ptr()).events.new_output,
                on_new_output::<S>,
                &raw const dispatcher,
            )
        };

        let loop_ = display.event_loop();
        for _ in 0..iterations {
            loop_.dispatch(0)?;
        }

        Ok(())
    }
}

/// # Safety
///
/// `listener` must be a listener currently linked into a signal's list, and the
/// list's other members must still be live.
unsafe fn remove_listener(listener: *mut sys::wl_listener) {
    use sys::wayland_sys::ffi_dispatch;
    // `wl_list_remove` is not re-exported by wlr-sys; it lives in wayland-sys
    // and must go through `ffi_dispatch!` so this works whether or not
    // wayland-sys was built with its `dlopen` feature.
    //
    // `allow`, not `expect`: this glob is unused only under the `dlopen`
    // expansion of `ffi_dispatch!`, which calls through a function-pointer
    // table instead of the bare name this import brings into scope. See the
    // identical comment on `Display::new` for the full explanation.
    #[allow(unused_imports)]
    use sys::wayland_sys::server::*;

    // SAFETY: caller guarantees the listener is linked and its neighbours live.
    unsafe {
        ffi_dispatch!(
            sys::wayland_sys::server::wayland_server_handle(),
            wl_list_remove,
            &raw mut (*listener).link
        );
    }
}

/// # Safety
///
/// `l` must be the `listener` field of a live `Bound<S>`, with the same `S`.
unsafe fn bound_of<S>(l: *mut sys::wl_listener) -> *mut Bound<S> {
    // `Bound<S>` is `#[repr(C)]` with `listener` first, so the offset is zero
    // and this is the `container_of` pattern with nothing to subtract. The
    // `const _` assertion next to the struct pins the field order.
    l.cast::<Bound<S>>()
}

/// Give the object owning `set` an identity, reusing one already attached.
///
/// Unlike [`attach_id`] this is idempotent, and that is deliberate rather than
/// incidental. `attach_id` asserts that no id addon is present; this function's
/// only callers are `extern "C"` callbacks, where a panic aborts the process
/// (defined behaviour since Rust 1.81, not UB — but an abort out of a
/// compositor's event loop all the same). Reusing the existing id is also the
/// semantically correct answer: an id is meant to be stable for the object's
/// whole life, so "already has one" is a satisfied postcondition, not a fault.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object.
unsafe fn ensure_id(set: *mut sys::wlr_addon_set) -> OutputId {
    // SAFETY: the caller's guarantee is exactly what both calls below require.
    // `attach_id`'s additional precondition — that no id addon is attached yet
    // — is discharged by the `find_id` check immediately preceding it, and
    // nothing can attach one in between: wlroots' event loop, and therefore
    // every caller of this function, is single-threaded.
    unsafe {
        match find_id(set.cast_const()) {
            Some(id) => OutputId(id),
            None => OutputId(attach_id(set)),
        }
    }
}

unsafe extern "C" fn on_new_output<S: OutputHandler>(
    l: *mut sys::wl_listener,
    data: *mut std::ffi::c_void,
) {
    // SAFETY: wlroots invokes this only for the listener `Backend::run`
    // registered, which is the `listener` field of a live `Bound<S>` whose
    // `dispatcher` is valid for as long as that registration exists. The
    // `new_output` signal carries a `*mut wlr_output`, so the cast of `data`
    // matches what wlroots documents it to pass.
    unsafe {
        let bound = bound_of::<S>(l);
        let output = data.cast::<sys::wlr_output>();

        // Give the output an identity before anyone can ask for one.
        let id = ensure_id(&raw mut (*output).addons);

        (*(*bound).dispatcher).emit(Event::NewOutput(id), deliver::<S>);
    }
}

/// Route an event to the matching handler method.
fn deliver<S: OutputHandler>(state: &mut S, ev: Event) {
    match ev {
        Event::NewOutput(id) | Event::OutputFrame(id) => {
            // Incomplete on purpose. Re-resolving from an id is what makes
            // deferral safe — an object destroyed between queueing and
            // delivery simply is not found — so these arms wait on the
            // id-to-output registry that the output-registration slice adds.
            // Until then a `NewOutput`/`OutputFrame` event is accepted and
            // dropped rather than delivered.
            let _ = id;
        }
        Event::OutputDestroyed(id) => state.destroyed(id),
    }
}
