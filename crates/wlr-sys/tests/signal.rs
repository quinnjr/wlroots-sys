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
use std::sync::atomic::{AtomicU32, Ordering};

use wayland_sys::common::wl_list;
use wayland_sys::server::{wl_listener, wl_signal};
use wlr_sys::{container_of, wl_list_for_each, wl_signal_add, wl_signal_emit, wl_signal_init};

/// Monotonic tick, so each handler invocation can record when it ran relative
/// to the others.
static CLOCK: AtomicU32 = AtomicU32::new(0);

/// A listener embedded in a larger struct, the way every wlroots consumer does it.
#[repr(C)]
struct Subscriber {
    calls: u32,
    listener: wl_listener,
    payload: u32,
    /// Tick at which this subscriber was last notified.
    last_tick: u32,
}

unsafe extern "C" fn on_event(listener: *mut wl_listener, data: *mut c_void) {
    // SAFETY: `listener` is the `listener` field of a live `Subscriber`, because
    // that is the only thing we ever link into the signal below.
    let subscriber: *mut Subscriber = unsafe { container_of!(listener, Subscriber, listener) };
    unsafe {
        (*subscriber).calls += 1;
        (*subscriber).payload = *(data as *mut u32);
        (*subscriber).last_tick = CLOCK.fetch_add(1, Ordering::SeqCst);
    }
}

/// A second notify fn, so `wl_signal_get`'s negative case can be exercised
/// against a populated list rather than only an empty one.
unsafe extern "C" fn other_event(_listener: *mut wl_listener, _data: *mut c_void) {}

/// A zeroed `wl_signal`. Every caller immediately `wl_signal_init`s it, which
/// overwrites both fields — the nulls are just a valid starting value.
fn signal() -> wl_signal {
    wl_signal {
        listener_list: wl_list {
            prev: std::ptr::null_mut(),
            next: std::ptr::null_mut(),
        },
    }
}

/// `n` subscribers with `payload` pre-set to their index, so iteration order is
/// observable.
///
/// The `Box` is load-bearing, not incidental: raw pointers to each `Subscriber`
/// get spliced into an intrusive list, so the address must stay put when the
/// `Vec` grows or is iterated. `Vec<Subscriber>` would move them.
#[allow(clippy::vec_box)]
fn subscribers(n: u32) -> Vec<Box<Subscriber>> {
    (0..n)
        .map(|index| {
            let mut sub = subscriber();
            sub.payload = index;
            sub
        })
        .collect()
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
        last_tick: 0,
    })
}

#[test]
fn signal_delivers_to_listeners_in_order() {
    let mut signal = signal();
    let mut first = subscriber();
    let mut second = subscriber();
    let mut data: u32 = 0xfeed;

    // SAFETY: `signal` and both subscribers outlive every call below, and each
    // listener is linked in exactly once and unlinked before the boxes drop.
    unsafe {
        wl_signal_init(&raw mut signal);
        wl_signal_add(&raw mut signal, &raw mut first.listener);
        wl_signal_add(&raw mut signal, &raw mut second.listener);

        wl_signal_emit(&raw mut signal, (&raw mut data).cast::<c_void>());
        wl_signal_emit(&raw mut signal, (&raw mut data).cast::<c_void>());

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
    // `wl_signal_add` links at `listener_list.prev`, i.e. the tail, so listeners
    // fire in the order they subscribed. Both handlers stamp a shared counter,
    // so this compares when they actually ran rather than just that they ran.
    assert!(
        first.last_tick < second.last_tick,
        "listeners should fire in subscription order (first={}, second={})",
        first.last_tick,
        second.last_tick
    );
}

#[test]
fn signal_get_finds_a_linked_listener() {
    let mut signal = signal();
    let mut sub = subscriber();

    // SAFETY: as above; the listener is unlinked before `sub` drops. Results are
    // captured and asserted *outside* the block: an assertion failing between
    // `wl_signal_add` and `wl_list_remove` would unwind and free the boxed
    // `Subscriber` while its `link` was still spliced into `signal`.
    let (empty, found, other) = unsafe {
        wl_signal_init(&raw mut signal);
        let empty = wlr_sys::wl_signal_get(&raw mut signal, on_event);

        wl_signal_add(&raw mut signal, &raw mut sub.listener);
        let found = wlr_sys::wl_signal_get(&raw mut signal, on_event);
        // The negative case with a *populated* list — this is the branch where
        // the comparison logic actually runs.
        let other = wlr_sys::wl_signal_get(&raw mut signal, other_event);

        ffi_wl_list_remove(&raw mut sub.listener.link);
        (empty, found, other)
    };

    assert!(empty.is_null(), "an empty signal should have no listeners");
    assert_eq!(
        found, &raw mut sub.listener,
        "wl_signal_get should return the listener we linked"
    );
    assert!(
        other.is_null(),
        "wl_signal_get should not match a different notify fn"
    );
}

#[test]
fn iterating_an_empty_list_yields_nothing() {
    let mut signal = signal();

    // SAFETY: `signal` is initialised below and outlives the iteration.
    let seen: Vec<u32> = unsafe {
        wl_signal_init(&raw mut signal);
        wl_list_for_each!(&raw mut signal.listener_list, Subscriber, listener.link)
            .map(|sub| (*sub).payload)
            .collect()
    };

    assert!(
        seen.is_empty(),
        "an initialised but empty list should yield no entries"
    );
}

#[test]
fn wl_list_iteration_walks_every_entry() {
    let mut signal = signal();
    let mut subs = subscribers(3);

    // SAFETY: the list head and every entry outlive the iteration, and the list
    // is not modified while iterating.
    let seen: Vec<u32> = unsafe {
        wl_signal_init(&raw mut signal);
        for sub in subs.iter_mut() {
            wl_signal_add(&raw mut signal, &raw mut sub.listener);
        }

        let payloads = wl_list_for_each!(&raw mut signal.listener_list, Subscriber, listener.link)
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

/// `wl_list_iter` advances its cursor before yielding, so it behaves like C's
/// `wl_list_for_each_safe`: unlinking the element you were just handed is
/// supported. Destroy handlers do exactly this, so the guarantee is load-bearing.
#[test]
fn wl_list_iteration_tolerates_removing_the_current_entry() {
    let mut signal = signal();
    let mut subs = subscribers(4);

    // SAFETY: every entry outlives the iteration, and we only unlink entries
    // the iterator has already yielded — never the one ahead of the cursor.
    let seen: Vec<u32> = unsafe {
        wl_signal_init(&raw mut signal);
        for sub in subs.iter_mut() {
            wl_signal_add(&raw mut signal, &raw mut sub.listener);
        }

        wl_list_for_each!(&raw mut signal.listener_list, Subscriber, listener.link)
            .map(|sub| {
                let payload = (*sub).payload;
                ffi_wl_list_remove(&raw mut (*sub).listener.link);
                payload
            })
            .collect()
    };

    assert_eq!(
        seen,
        vec![0, 1, 2, 3],
        "unlinking the just-yielded entry must not truncate iteration"
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

/// The two macros take *different* fields, and it matters which.
///
/// A callback is handed a `*mut wl_listener` pointing at `Subscriber.listener`,
/// so `container_of!` subtracts `offset_of!(Subscriber, listener)`. The list
/// nodes, by contrast, are the `link` fields *inside* those listeners, so
/// `wl_list_for_each!` subtracts `offset_of!(Subscriber, listener.link)`.
///
/// Those two offsets are equal today only because `wl_listener` declares `link`
/// first. This test records the dependency: if `wayland-sys` ever reorders that
/// struct, this fails loudly instead of every iteration silently yielding
/// pointers off by a constant.
#[test]
fn listener_and_link_offsets_coincide_only_because_link_is_first() {
    assert_eq!(
        offset_of!(wl_listener, link),
        0,
        "wl_listener.link is no longer the first field; every wl_list_for_each! \
         and container_of! call site must be rechecked for which field it names"
    );
    assert_eq!(
        offset_of!(Subscriber, listener),
        offset_of!(Subscriber, listener.link)
    );
}

/// `wl_list_remove` reached through `ffi_dispatch!`, so this test behaves the
/// same whether or not `wayland-sys` was built with `dlopen`.
unsafe fn ffi_wl_list_remove(link: *mut wl_list) {
    use wayland_sys::ffi_dispatch;
    // Load-bearing without `dlopen`, where `ffi_dispatch!` calls `wl_list_remove`
    // as a bare name; unused with it, where the call goes through the handle's
    // function table. `allow`, not `expect` — `expect` would fire in the default
    // build, where the import is genuinely used.
    #[allow(unused_imports)]
    use wayland_sys::server::*;
    unsafe {
        ffi_dispatch!(
            wayland_sys::server::wayland_server_handle(),
            wl_list_remove,
            link
        );
    }
}
