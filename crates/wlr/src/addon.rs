//! Rust payloads whose lifetime wlroots manages.
//!
//! `wlr_addon` is wlroots' mechanism for hanging data off one of its objects
//! and having it torn down at exactly the moment that object dies — including
//! when the death is a cascade nobody told us about, such as a scene subtree
//! being destroyed with its parent. That is the only mechanism in wlroots that
//! runs *our* destructor at the right time; a table swept on a destroy signal
//! is strictly weaker, because not every object announces its own death.
//!
//! [`id`](crate::id) has used this since 0.20.2 for object ids. This module is
//! that code generalised so a second payload kind costs a `static` rather than
//! a copy of the whole pattern.
//!
//! # The three rules
//!
//! 1. **The payload must be boxed and must not move.** wlroots links the
//!    embedded `wlr_addon` into an intrusive list; the address is the identity.
//! 2. **`owner` must be a stable per-purpose sentinel**, never the payload's
//!    own address. `(owner, impl)` is the key [`Addon::find`] looks up, so an
//!    owner that differs per attachment makes lookup impossible. This crate
//!    uses the address of the [`AddonImpl`] static itself
//!    ([`AddonImpl::owner`]), which is stable, unique per purpose, and needs
//!    nothing declared alongside it.
//! 3. **The vtable's `destroy` must call `wlr_addon_finish`** before freeing
//!    anything. wlroots documents this; skipping it leaves a freed addon linked
//!    into the set, which the next walk dereferences.
//!
//! `(owner, impl)` must also be unique *within a set*, and getting that wrong is
//! not a soft failure. `wlr_addon_init` walks the set and ends at
//! `assert(0 && "Can't have two addons of the same type with the same owner")`
//! — verified by disassembling the installed `libwlroots-0.20.so`, whose
//! `wlr_addon_init` reaches `__assert_fail` on the duplicate path. So a second
//! attach with the same pair **aborts the process** on an assertions-enabled
//! build, which is what distributions ship; only under `NDEBUG` does it
//! degrade to two entries and a `wlr_addon_find` that returns whichever the
//! list walk reaches first.
//!
//! Callers that cannot rule out a second attach must therefore `find` first —
//! see [`crate::id::attach_id`], whose own `assert!` fails with a message
//! naming this crate before wlroots' fires, and `backend.rs`'s `ensure_id_raw`,
//! which is idempotent instead because it runs inside a C callback where any
//! abort is the whole compositor.

use std::ffi::c_void;

use crate::sys;

/// A boxed payload plus the `wlr_addon` header wlroots links into a set.
///
/// `#[repr(C)]` with the header first, so the `*mut wlr_addon` wlroots hands
/// the destroy hook can be turned back into this allocation with a plain
/// zero-offset cast.
#[repr(C)]
pub(crate) struct Addon<T> {
    raw: sys::wlr_addon,
    data: T,
}

// The zero-offset cast in `addon_destroy` and `Addon::find` is sound only while
// `raw` is the first field. `#[repr(C)]` fixes field order for every `T`, so
// checking one instantiation checks them all; `u64` is the one `id.rs` uses.
const _: () = assert!(std::mem::offset_of!(Addon<u64>, raw) == 0);

/// A `wlr_addon_interface` that can live in a `static`.
///
/// `wlr_addon_interface` holds a `*const c_char` and a function pointer, so it
/// is not `Sync` by default. Wrapping it lets one immutable instance serve the
/// whole process, which is also what makes its address usable as the `owner`
/// sentinel.
pub(crate) struct AddonImpl(pub(crate) sys::wlr_addon_interface);

// SAFETY: the contents are never mutated after initialisation, and `name`
// targets a `'static` C string literal.
unsafe impl Sync for AddonImpl {}

impl AddonImpl {
    /// The `owner` sentinel this crate pairs with `self`.
    ///
    /// The address of the impl static itself: stable for the process, distinct
    /// per purpose, and requiring no second static to keep in sync with this
    /// one. See rule 2 in the module docs for why the payload's own address
    /// would be wrong.
    pub(crate) fn owner(&'static self) -> *const () {
        (&raw const *self).cast::<()>()
    }

    /// The vtable pointer wlroots stores in the addon.
    fn interface(&'static self) -> *const sys::wlr_addon_interface {
        &raw const self.0
    }
}

impl<T: 'static> Addon<T> {
    /// Box `data` and link it into `set`.
    ///
    /// The payload is owned by wlroots from here on: it is freed by
    /// [`addon_destroy`] when the object owning `set` dies, or when the set is
    /// finished. The returned pointer is a borrow, not a handle to free.
    ///
    /// # Safety
    ///
    /// `set` must point at an initialised `wlr_addon_set` belonging to a live
    /// object. `(owner, impl_)` must not already be present in `set` — wlroots
    /// `assert()`s against a duplicate and aborts the process, so this is a
    /// precondition to check rather than a case to recover from; see the module
    /// docs. `impl_`'s `destroy` must be
    /// [`addon_destroy::<T>`](addon_destroy), which is what [`addon_kind!`]
    /// guarantees.
    pub(crate) unsafe fn attach(
        set: *mut sys::wlr_addon_set,
        owner: *const (),
        impl_: &'static AddonImpl,
        data: T,
    ) -> *mut Addon<T> {
        let payload = Box::into_raw(Box::new(Addon {
            // SAFETY: `wlr_addon` is two pointers and a `wl_list`; a zeroed
            // value is a valid (if unlinked) one, and `wlr_addon_init`
            // overwrites every field before anything reads it.
            raw: unsafe { std::mem::zeroed() },
            data,
        }));

        // SAFETY: the caller guarantees `set` is live and initialised, and
        // `payload` is a fresh, uniquely-owned allocation whose address is
        // stable for as long as wlroots holds it (it is only freed by
        // `addon_destroy`, which unlinks first).
        unsafe {
            sys::wlr_addon_init(
                &raw mut (*payload).raw,
                set,
                owner.cast::<c_void>(),
                impl_.interface(),
            );
        }
        payload
    }

    /// The payload attached to `set` under `(owner, impl_)`, or null.
    ///
    /// # Safety
    ///
    /// `set` must point at an initialised `wlr_addon_set` belonging to a live
    /// object, and `(owner, impl_)` must be the pair some [`attach`](Self::attach)
    /// used with this same `T` — the cast back is unchecked, and a mismatched
    /// `T` reinterprets another kind's payload.
    pub(crate) unsafe fn find(
        set: *const sys::wlr_addon_set,
        owner: *const (),
        impl_: &'static AddonImpl,
    ) -> *mut Addon<T> {
        // SAFETY: the caller guarantees `set` is live and initialised.
        // `wlr_addon_find` only walks the addon list; its C signature takes a
        // `*mut` despite performing no mutation, so casting away `const` here
        // is not a soundness hazard.
        let addon = unsafe {
            sys::wlr_addon_find(set.cast_mut(), owner.cast::<c_void>(), impl_.interface())
        };
        // The `wlr_addon` is `Addon<T>`'s first field, at offset 0 (checked by
        // the `const _` above), so this recovers the enclosing allocation. Null
        // stays null.
        addon.cast::<Addon<T>>()
    }

    /// Borrow the payload.
    ///
    /// # Safety
    ///
    /// `p` must be a non-null pointer from [`attach`](Self::attach) or
    /// [`find`](Self::find) whose payload is still attached, and the returned
    /// borrow must not outlive it.
    pub(crate) unsafe fn data<'a>(p: *mut Addon<T>) -> &'a T {
        // SAFETY: the caller guarantees `p` is live for `'a`.
        unsafe { &(*p).data }
    }
}

/// Test-only witness that a destroy hook ran and freed its `Box`.
///
/// Production builds have no way to observe that beyond "did not crash". Every
/// addon kind in this crate shares the counter, which is why the tests that
/// measure a delta across it take [`crate::id::id_test_lock`].
#[cfg(test)]
pub(crate) static DESTROY_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The `destroy` half of every [`AddonImpl`] this crate declares.
///
/// Called by wlroots when the owning object dies, or by `wlr_addon_set_finish`.
/// Unlinks first (rule 3), then frees the box.
///
/// # Safety
///
/// `raw` must be the `raw` field of a live `Addon<T>` that was boxed by
/// [`Addon::attach`] and is still linked into a set. wlroots only ever calls
/// this for addons registered with an [`AddonImpl`] naming this instantiation,
/// which [`addon_kind!`] is what enforces.
///
/// `T`'s `Drop` must not unwind: this runs in an `extern "C"` frame, where an
/// unwind aborts the process rather than propagating.
pub(crate) unsafe extern "C" fn addon_destroy<T>(raw: *mut sys::wlr_addon) {
    // SAFETY: the caller guarantees `raw` is the header of a boxed `Addon<T>`
    // still linked into a set, which is exactly `finish_and_free`'s own
    // precondition.
    unsafe { finish_and_free::<T>(raw) };
    #[cfg(test)]
    DESTROY_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [`addon_destroy`] without the test-only [`DESTROY_COUNT`] bump.
///
/// [`DESTROY_COUNT`] is process-wide and several tests assert a *delta* across
/// it while holding [`crate::id::id_test_lock`]. That only works while every
/// addon this crate attaches is attached and destroyed by code that also takes
/// the lock. Scene node ids ([`crate::scene`]) are not such a payload: one is
/// attached to every scene node this crate creates *or merely observes*, and
/// destroyed by ordinary scene work in tests written before that payload
/// existed and which must keep passing unmodified —
/// `clear_toplevels_does_not_purge_a_band_rect` destroys a rect without taking
/// the lock, and would start inflating another test's delta the moment rects
/// carried a counted addon. So that kind is deliberately invisible to this
/// counter and brings its own
/// ([`crate::scene::node_destroy_count`](crate::scene::node_destroy_count)).
///
/// # Safety
///
/// Identical to [`addon_destroy`]'s.
pub(crate) unsafe extern "C" fn addon_destroy_untracked<T>(raw: *mut sys::wlr_addon) {
    // SAFETY: as for `addon_destroy`.
    unsafe { finish_and_free::<T>(raw) };
}

/// Unlink `raw` from its set (rule 3) and free the box it heads.
///
/// # Safety
///
/// `raw` must be the `raw` field of a live `Addon<T>` boxed by
/// [`Addon::attach`] and still linked into a set.
unsafe fn finish_and_free<T>(raw: *mut sys::wlr_addon) {
    // SAFETY: the caller (wlroots) passes an addon we registered, all of which
    // are the first field of a boxed `Addon<T>`; `wlr_addon_finish` unlinks it
    // from the set before the memory goes away.
    unsafe {
        let payload = raw.cast::<Addon<T>>();
        sys::wlr_addon_finish(raw);
        drop(Box::from_raw(payload));
    }
}

/// Declare an addon kind: one `static AddonImpl` naming its payload type.
///
/// The payload type is part of the declaration rather than only of the call
/// sites because the vtable's `destroy` is `addon_destroy::<T>` — writing the
/// two independently is exactly how a payload gets freed as the wrong type.
/// Here they cannot disagree.
///
/// `$name` is the sentinel too: pass `NAME.owner()` as `owner` at every call
/// site (module docs, rule 2).
///
/// The `untracked` form is identical except that its destroy hook does not
/// bump [`DESTROY_COUNT`] — see
/// [`addon_destroy_untracked`](crate::addon::addon_destroy_untracked) for when
/// that is the right choice and why it is not the default.
macro_rules! addon_kind {
    ($(#[$meta:meta])* $vis:vis $name:ident: $payload:ty = $cname:literal) => {
        $(#[$meta])*
        $vis static $name: $crate::addon::AddonImpl =
            $crate::addon::AddonImpl($crate::sys::wlr_addon_interface {
                name: $cname.as_ptr(),
                destroy: Some($crate::addon::addon_destroy::<$payload>),
            });
    };
    (untracked $(#[$meta:meta])* $vis:vis $name:ident: $payload:ty = $cname:literal) => {
        $(#[$meta])*
        $vis static $name: $crate::addon::AddonImpl =
            $crate::addon::AddonImpl($crate::sys::wlr_addon_interface {
                name: $cname.as_ptr(),
                destroy: Some($crate::addon::addon_destroy_untracked::<$payload>),
            });
    };
}

pub(crate) use addon_kind;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts payload drops, so "freed exactly once" is observable rather than
    /// inferred from the absence of a crash.
    static PAYLOAD_DROPS: AtomicUsize = AtomicUsize::new(0);

    struct Payload;

    impl Drop for Payload {
        fn drop(&mut self) {
            PAYLOAD_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    addon_kind!(TEST_ADDON_IMPL: Payload = c"wlr-rs-addon-test");

    /// A standalone `wlr_addon_set`, which is all `wlr_addon_*` needs — no
    /// display, backend or output is involved.
    ///
    /// Boxed rather than left on the stack: `wlr_addon_set_init` makes the
    /// embedded `wl_list` point at itself, so the value must not move
    /// afterwards, and a `Box` says that in the type rather than in a comment.
    struct ScratchSet(*mut sys::wlr_addon_set);

    impl ScratchSet {
        fn new() -> ScratchSet {
            // SAFETY: `wlr_addon_set` is a single `wl_list`; a zeroed value is
            // a valid one to hand to `wlr_addon_set_init`, which initialises
            // it in place at the address the `Box` fixed.
            let set = Box::into_raw(Box::new(unsafe {
                std::mem::zeroed::<sys::wlr_addon_set>()
            }));
            // SAFETY: `set` is a fresh, uniquely-owned allocation.
            unsafe { sys::wlr_addon_set_init(set) };
            ScratchSet(set)
        }
    }

    impl Drop for ScratchSet {
        fn drop(&mut self) {
            // SAFETY: the set was initialised in `new` and has not moved;
            // finishing it runs the destroy hook of anything still attached,
            // which is exactly what these tests are measuring.
            unsafe {
                sys::wlr_addon_set_finish(self.0);
                drop(Box::from_raw(self.0));
            }
        }
    }

    /// The case the mechanism exists for: the owning object dies, and wlroots
    /// frees our payload for us.
    #[test]
    fn finishing_the_set_runs_the_destroy_hook_exactly_once() {
        // `DESTROY_COUNT` is process-wide and this asserts a delta across it.
        let _serialised = crate::id::id_test_lock();

        let set = ScratchSet::new();
        let drops_before = PAYLOAD_DROPS.load(Ordering::Relaxed);
        let destroys_before = DESTROY_COUNT.load(Ordering::Relaxed);

        // SAFETY: `set.0` is live and initialised, nothing is attached under
        // this `(owner, impl)` pair yet, and the payload type matches the one
        // `TEST_ADDON_IMPL` was declared with.
        unsafe {
            let attached = Addon::attach(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL, Payload);
            assert!(!attached.is_null(), "attach returns the boxed payload");
            assert_eq!(
                Addon::<Payload>::find(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL),
                attached,
                "find returns the payload attach produced"
            );
        }

        drop(set);

        assert_eq!(
            PAYLOAD_DROPS.load(Ordering::Relaxed) - drops_before,
            1,
            "the payload was dropped exactly once"
        );
        assert_eq!(
            DESTROY_COUNT.load(Ordering::Relaxed) - destroys_before,
            1,
            "and the destroy hook is what did it"
        );
    }

    /// `wlr_addon_finish` inside the hook must unlink, so the set no longer
    /// walks into freed memory — and so finishing the set afterwards does not
    /// free the payload a second time.
    #[test]
    fn a_destroyed_addon_is_unlinked_and_not_freed_twice() {
        let _serialised = crate::id::id_test_lock();

        let set = ScratchSet::new();
        let drops_before = PAYLOAD_DROPS.load(Ordering::Relaxed);

        // SAFETY: as above. Invoking the vtable's `destroy` by hand is exactly
        // what `wlr_addon_set_finish` does, one addon at a time, so this is the
        // real destroy path rather than a stand-in for it.
        unsafe {
            let attached = Addon::attach(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL, Payload);
            let destroy = TEST_ADDON_IMPL
                .0
                .destroy
                .expect("the kind declares a destroy");
            destroy(&raw mut (*attached).raw);

            assert!(
                Addon::<Payload>::find(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL).is_null(),
                "the destroy hook unlinked the addon rather than merely freeing it"
            );
        }

        assert_eq!(
            PAYLOAD_DROPS.load(Ordering::Relaxed) - drops_before,
            1,
            "the payload was dropped by the hook"
        );

        // `ScratchSet::drop` finishes an empty set: nothing more to free.
        drop(set);

        assert_eq!(
            PAYLOAD_DROPS.load(Ordering::Relaxed) - drops_before,
            1,
            "finishing the set afterwards did not free it a second time"
        );
    }

    /// The duplicate hazard from the module docs, from the only side a test can
    /// observe it: a second attach under a pair already in the set reaches
    /// `assert(0 && "Can't have two addons of the same type with the same
    /// owner")` inside `wlr_addon_init`, which aborts the process — so nothing
    /// in this crate may ever get that far. [`crate::id::attach_id`] checks
    /// first, and this is what says the check is still there. Deleting it as
    /// "obviously redundant" turns a panic with a message into an abort with
    /// none.
    #[test]
    #[should_panic(expected = "an id addon is already attached")]
    fn a_second_attach_of_one_kind_is_refused_before_wlroots_can_abort() {
        let _serialised = crate::id::id_test_lock();

        // Dropped during the unwind, before the lock guard, so the first addon
        // is freed by its destroy hook rather than leaked into a dead set.
        let set = ScratchSet::new();

        // SAFETY: `set.0` is live and initialised. The second call is the one
        // under test; it panics before reaching `wlr_addon_init`.
        unsafe {
            crate::id::attach_id(set.0);
            crate::id::attach_id(set.0);
        }
    }

    /// Two kinds attached to one set do not collide: the `(owner, impl)` key
    /// separates them, which is what lets `id.rs` and a future payload share an
    /// object.
    #[test]
    fn distinct_kinds_coexist_on_one_set() {
        let _serialised = crate::id::id_test_lock();

        let set = ScratchSet::new();

        // SAFETY: `set.0` is live; the two attachments use different
        // `(owner, impl)` pairs, so neither shadows the other.
        unsafe {
            let id = crate::id::attach_id(set.0);
            let mine = Addon::attach(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL, Payload);

            assert_eq!(crate::id::find_id(set.0.cast_const()), Some(id));
            assert_eq!(
                Addon::<Payload>::find(set.0, TEST_ADDON_IMPL.owner(), &TEST_ADDON_IMPL),
                mine
            );
        }
    }
}
