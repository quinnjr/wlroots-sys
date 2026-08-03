//! Exercises the hand-written pieces that bindgen cannot generate: the
//! `wl_signal` helpers, `container_of!`, and `wl_list` iteration.
//!
//! These matter more than they look. `wl_signal_add` writes into a `wl_list`
//! whose layout comes from `wayland-sys` while the surrounding structs come from
//! bindgen, and `container_of!` computes a raw offset. If either were wrong the
//! failure would be a wild pointer at runtime inside a compositor, not a
//! compile error.

use std::ffi::c_void;
use std::mem::offset_of;

use wayland_sys::common::wl_list;
use wayland_sys::server::{wl_listener, wl_signal};
use wlr_sys::{
    container_of, wl_list_for_each, wl_signal_add, wl_signal_emit_mutable, wl_signal_init,
};

/// A listener embedded in a larger struct, the way every wlroots consumer does it.
#[repr(C)]
struct Subscriber {
    calls: u32,
    listener: wl_listener,
    payload: u32,
}

unsafe extern "C" fn on_event(listener: *mut wl_listener, data: *mut c_void) {
    // SAFETY: `listener` is the `listener` field of a live `Subscriber`, because
    // that is the only thing we ever link into the signal below.
    let subscriber: *mut Subscriber = unsafe { container_of!(listener, Subscriber, listener) };
    unsafe {
        (*subscriber).calls += 1;
        (*subscriber).payload = *(data as *mut u32);
    }
}

fn subscriber() -> Box<Subscriber> {
    Box::new(Subscriber {
        calls: 0,
        listener: wl_listener {
            link: wl_list {
                prev: std::ptr::null_mut(),
                next: std::ptr::null_mut(),
            },
            notify: on_event,
        },
        payload: 0,
    })
}

#[test]
fn signal_delivers_to_listeners_in_order() {
    let mut signal = wl_signal {
        listener_list: wl_list {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
        },
    };
    let mut first = subscriber();
    let mut second = subscriber();
    let mut data: u32 = 0xfeed;

    // SAFETY: `signal` and both subscribers outlive every call below, and each
    // listener is linked in exactly once and unlinked before the boxes drop.
    unsafe {
        wl_signal_init(&raw mut signal);
        wl_signal_add(&raw mut signal, &raw mut first.listener);
        wl_signal_add(&raw mut signal, &raw mut second.listener);

        wl_signal_emit_mutable(&raw mut signal, (&raw mut data).cast::<c_void>());
        wl_signal_emit_mutable(&raw mut signal, (&raw mut data).cast::<c_void>());

        // Unlink before the listeners are freed, exactly as a compositor must.
        ffi_wl_list_remove(&raw mut first.listener.link);
        ffi_wl_list_remove(&raw mut second.listener.link);
    }

    assert_eq!(first.calls, 2, "first listener should fire on every emit");
    assert_eq!(second.calls, 2, "second listener should fire on every emit");
    assert_eq!(
        first.payload, 0xfeed,
        "container_of! recovered the wrong struct"
    );
    assert_eq!(second.payload, 0xfeed);
}

#[test]
fn signal_get_finds_a_linked_listener() {
    let mut signal = wl_signal {
        listener_list: wl_list {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
        },
    };
    let mut sub = subscriber();

    // SAFETY: as above; the listener is unlinked before `sub` drops.
    unsafe {
        wl_signal_init(&raw mut signal);
        assert!(
            wlr_sys::wl_signal_get(&raw mut signal, on_event).is_null(),
            "an empty signal should have no listeners"
        );

        wl_signal_add(&raw mut signal, &raw mut sub.listener);
        assert_eq!(
            wlr_sys::wl_signal_get(&raw mut signal, on_event),
            &raw mut sub.listener,
            "wl_signal_get should return the listener we linked"
        );

        ffi_wl_list_remove(&raw mut sub.listener.link);
    }
}

#[test]
fn wl_list_iteration_walks_every_entry() {
    let mut signal = wl_signal {
        listener_list: wl_list {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
        },
    };
    let mut subs: Vec<Box<Subscriber>> = (0..3).map(|_| subscriber()).collect();
    for (index, sub) in subs.iter_mut().enumerate() {
        sub.payload = index as u32;
    }

    // SAFETY: the list head and every entry outlive the iteration, and the list
    // is not modified while iterating.
    let seen: Vec<u32> = unsafe {
        wl_signal_init(&raw mut signal);
        for sub in subs.iter_mut() {
            wl_signal_add(&raw mut signal, &raw mut sub.listener);
        }

        let payloads = wl_list_for_each!(&raw mut signal.listener_list, Subscriber, listener)
            .map(|sub| (*sub).payload)
            .collect();

        for sub in subs.iter_mut() {
            ffi_wl_list_remove(&raw mut sub.listener.link);
        }
        payloads
    };

    assert_eq!(
        seen,
        vec![0, 1, 2],
        "iteration should yield every entry in insertion order"
    );
}

#[test]
fn container_of_matches_offset_of() {
    let sub = subscriber();
    let listener = &raw const sub.listener;

    // SAFETY: `listener` points at the `listener` field of the live `sub`.
    let recovered: *mut Subscriber = unsafe { container_of!(listener, Subscriber, listener) };

    assert_eq!(recovered.cast_const(), &raw const *sub);
    assert_eq!(
        listener as usize - (&raw const *sub) as usize,
        offset_of!(Subscriber, listener)
    );
}

/// `wl_list_remove` reached through `ffi_dispatch!`, so this test behaves the
/// same whether or not `wayland-sys` was built with `dlopen`.
unsafe fn ffi_wl_list_remove(link: *mut wl_list) {
    use wayland_sys::ffi_dispatch;
    use wayland_sys::server::*;
    unsafe {
        ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_list_remove,
            link
        );
    }
}
