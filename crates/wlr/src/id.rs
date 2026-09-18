//! Stable object identity.
//!
//! A raw pointer is not an identity: wlroots may reuse an address after free, so
//! a pointer compared across a destroy can alias a different object. Instead a
//! monotonic id is attached to the C object with `wlr_addon`, wlroots' own
//! mechanism for data whose lifetime is bound to an object. wlroots runs our
//! destructor at exactly the right moment, so nothing has to be swept.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::addon::{Addon, AddonImpl, addon_kind};
use crate::sys;

/// Identifies an output for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike a handle, which cannot escape the
/// handler it was passed to. Ids are never reused within a process.
///
/// Deliberately does not derive `PartialOrd`/`Ord`: an opaque id ordering
/// silently promises creation-order semantics no consumer asked for, and the
/// hand-written API is frozen within a wlroots minor (see `CLAUDE.md`), so a
/// derive added here could not be withdrawn before 0.21. Adding it later is
/// non-breaking; the reversible direction is to leave it out for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub(crate) u64);

impl OutputId {
    /// An id no live output can have, for testing the "unknown id" path.
    ///
    /// Public for the same reason
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// is: "every by-id operation reports a miss rather than dereferencing" is
    /// a promise to consumers, and a promise nobody can write a test for is
    /// not one. Ids come from the process-wide counter that backs every id in
    /// this crate, which starts at 1, only increments and never reuses a
    /// value, so `u64::MAX` cannot be handed to a real output.
    ///
    /// Not for production code. An id from a real output is the one
    /// [`Output::id`](crate::Output::id) returns, and it stops resolving once
    /// the [`Backend::run_all`](crate::Backend::run_all) call that announced
    /// it has returned — at which point it behaves exactly like this one.
    pub fn dangling_for_test() -> OutputId {
        OutputId(dangling_test_id(0))
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Identifies an fd source for as long as the consumer chooses to remember it.
///
/// Unlike [`OutputId`] this is **not** backed by a `wlr_addon`, and it cannot
/// be: an fd source is a libwayland `wl_event_source`, not a wlroots object,
/// so there is no addon set to attach to and nothing that announces its own
/// death. It is drawn from the same process-wide monotonic counter instead, so
/// no `SourceId` can ever collide with itself, and ids are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(pub(crate) u64);

/// The next value from the counter that backs every id in this crate.
///
/// Shared with [`attach_id`] deliberately: one counter means an id printed in
/// a log is unambiguous about which object it names, and it costs one atomic
/// increment either way.
pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// The reserved band at the top of the id space that test-only "dangling" ids
/// are drawn from, shared by every id type that offers one.
///
/// Ids issued to real objects come from [`next_id`], a single process-wide
/// counter shared by every id type in this crate that starts at 1, only
/// increments and never reuses a value — so no process reaches the top of the
/// range, and a band parked there can never collide with a live object. `n` is
/// folded into a fixed 2^32-wide band immediately below `u64::MAX`
/// (`n % 2^32`) rather than subtracted unclamped, so even a very large `n`
/// (the suites probe `n = u64::MAX`) still lands inside the reserved range.
/// The band is 2^32 ids wide, far more than any test needs. `n = 0` aliases
/// `u64::MAX` itself, so callers wanting an id distinct from every other test
/// id (including `dangling_for_test`'s) must pass `n >= 1`.
pub(crate) fn dangling_test_id(n: u64) -> u64 {
    u64::MAX - (n % (1u64 << 32))
}

addon_kind!(
    /// The id payload's addon kind: a `u64` attached under the name wlroots
    /// prints when it walks a set.
    ///
    /// The name is part of the on-object representation and must not change:
    /// it is what distinguishes this crate's addons from another consumer's in
    /// a debugger, and `backend.rs`'s `ensure_id_raw` relies on `find` matching
    /// an addon attached by an earlier run of the same process.
    ID_ADDON_IMPL: u64 = c"wlr-rs-object-id"
);

addon_kind!(
    /// The generic surface-id payload's addon kind. A distinct kind from
    /// `ID_ADDON_IMPL` so a `wlr_surface` can carry both its role id and a
    /// [`SurfaceId`](crate::SurfaceId): `wlr_addon` keys on `(owner, impl)`, so
    /// two statics coexist on one set with no ordering or role check.
    ///
    /// A distinct name as well as a distinct static, for the reason
    /// `ID_ADDON_IMPL`'s own doc gives: the name is what a debugger — and
    /// `wlr_addon_find`'s `(owner, impl)` pair — uses to tell the two apart.
    SURFACE_ID_ADDON_IMPL: u64 = c"wlr-rs-surface-id"
);

/// The one attach/find core behind both id kinds in this module.
///
/// `ID_ADDON_IMPL` (role ids: toplevels, outputs, popups, …) and
/// `SURFACE_ID_ADDON_IMPL` (generic [`SurfaceId`](crate::SurfaceId)) differ
/// only in which `(owner, impl)` pair they key on, so every operation below is
/// this core with one of the two statics. The duplicate *policy* is not
/// shared: it stays explicit in the thin wrappers — [`attach_id`] asserts
/// (a duplicate on the role path is a programming error), while
/// [`attach_surface_id`] reports `None` (a duplicate on the announce path
/// must never abort the compositor) — so a reader sees the policy at the
/// call site rather than behind a flag.
///
/// # Safety
///
/// The same contract every wrapper documents: `set` must point at an
/// initialised `wlr_addon_set` belonging to a live object, and `kind` must be
/// one of this module's two addon kinds paired with a `u64` payload.
unsafe fn find_id_in(set: *const sys::wlr_addon_set, kind: &'static AddonImpl) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised. `wlr_addon_find`
    // only reads the set (it walks the addon list looking for a match); its C
    // signature takes `*mut wlr_addon_set` even though it performs no mutation,
    // so the cast back to `*mut` inside `Addon::find` is not a soundness
    // hazard.
    unsafe {
        let payload = Addon::<u64>::find(set, kind.owner(), kind);
        if payload.is_null() {
            return None;
        }
        Some(*Addon::data(payload))
    }
}

/// Mint and attach a fresh id under `kind`, without checking for a duplicate.
///
/// The unchecked half of the core: the caller owns the duplicate policy and
/// must have discharged it (an `assert!` in [`attach_id`], a `find_id_in`
/// check in [`attach_surface_id`]) before calling. Split out rather than
/// inlined so the "mint + attach" sequence exists exactly once.
///
/// # Safety
///
/// As for [`find_id_in`], plus: no addon may already be attached under
/// `(kind.owner(), kind)` — wlroots aborts on a duplicate `wlr_addon_init`.
unsafe fn attach_fresh_id(set: *mut sys::wlr_addon_set, kind: &'static AddonImpl) -> u64 {
    let id = next_id();
    // SAFETY: caller guarantees `set` is live and initialised, and that no
    // addon under this kind is attached yet (the duplicate policy above).
    unsafe {
        Addon::attach(set, kind.owner(), kind, id);
    }
    id
}

/// Attach a fresh surface id to `set` and return it, or `None` if the set
/// already carries one.
///
/// The duplicate is reported rather than asserted: this runs on the announce
/// path (via [`ensure_surface_id_raw`]), where aborting the compositor over an
/// id-bookkeeping surprise is never the right answer. The caller keeps the
/// no-duplicate invariant by treating `None` as "return the id that is
/// there".
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object.
pub(crate) unsafe fn attach_surface_id(set: *mut sys::wlr_addon_set) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised. Reading it
    // through a shared alias here is fine for the identical reason
    // `attach_id`'s own comment gives: `find_id_in` only calls
    // `wlr_addon_find`, which does not mutate the set, and the read completes
    // before `attach_fresh_id`'s own `wlr_addon_init` call performs any
    // mutation.
    unsafe {
        if find_id_in(set.cast_const(), &SURFACE_ID_ADDON_IMPL).is_some() {
            return None;
        }

        Some(attach_fresh_id(set, &SURFACE_ID_ADDON_IMPL))
    }
}

/// Retrieve the surface id attached to `set`, if any.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live object.
pub(crate) unsafe fn find_surface_id(set: *const sys::wlr_addon_set) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised; see
    // `find_id_in`'s own comment for why the `*mut` cast is not a soundness
    // hazard.
    unsafe { find_id_in(set, &SURFACE_ID_ADDON_IMPL) }
}

/// The surface id attached to `set`, attaching a fresh one if absent.
///
/// The public entry point for keeping `wlr_surface` identity stable while
/// wlroots may announce one surface through more than one role/announce path:
/// the caller gets the same id back every time.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object.
pub(crate) unsafe fn ensure_surface_id_raw(set: *mut sys::wlr_addon_set) -> u64 {
    // SAFETY: the caller's guarantee is exactly what every call below
    // requires. Nothing can attach between the `find_surface_id` check and the
    // `attach_surface_id` call: the wlroots event loop, and therefore every
    // caller, is single-threaded — the same argument `backend.rs`'s
    // `ensure_id_raw` makes — so a duplicate can never appear between the two
    // lookups, and no reachable path aborts: a duplicate there is re-read as
    // the id that is already there rather than tripping an `assert!` on the
    // announce path.
    unsafe {
        if let Some(id) = find_surface_id(set.cast_const()) {
            return id;
        }
        if let Some(id) = attach_surface_id(set) {
            return id;
        }
        // Unreachable single-threaded: the two returns above cover "present"
        // and "absent then attached", so a triple miss means an addon appeared
        // and vanished between two calls on the same thread. `expect` rather
        // than `debug_assert!(false)` + retry: loud in all builds if the
        // single-threaded threading invariant ever breaks, instead of an
        // infinite loop in release.
        find_surface_id(set.cast_const()).expect(
            "a surface id addon changed under a find and an attach on the same thread: \
             wlroots event-loop callers are single-threaded",
        )
    }
}
/// Serialises every test that attaches or destroys *any* addon this crate
/// declares, not only an id one.
///
/// [`crate::addon::DESTROY_COUNT`] is process-wide and shared by every addon
/// kind, and the tests below assert a *delta* across their own work, so a second
/// test destroying an addon on another harness thread at the same moment would
/// inflate that delta and fail it for the wrong reason. `backend.rs`'s delivery
/// tests destroy id addons, so they take this lock too — the alternative is a
/// suite that passes or fails by scheduling.
///
/// **If you write a test that finishes an addon set carrying one of this
/// crate's addons — calling `wlr_addon_set_finish` directly, or through a
/// fixture whose `Drop` does — take this lock for the whole test.** Finishing
/// the set runs [`crate::addon::addon_destroy`], which bumps the counter; a test that does so
/// without holding the lock will not fail itself, it will fail whichever test
/// happens to be measuring the delta at that moment, intermittently and
/// somewhere else.
#[cfg(test)]
pub(crate) fn id_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that fails while holding this poisons it. The guarded data is `()`,
    // so there is nothing that could have been left inconsistent, and refusing
    // to run the remaining tests would turn one real failure into several
    // spurious ones.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Attach a fresh id to `set` and return it.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live
/// object, and must not already carry one of our id addons.
pub(crate) unsafe fn attach_id(set: *mut sys::wlr_addon_set) -> u64 {
    // SAFETY: caller guarantees `set` is live and initialised. Reading it
    // through a shared alias here is fine: `find_id_in` only calls
    // `wlr_addon_find`, which does not mutate the set, and the read completes
    // (and its borrow ends) before `attach_fresh_id`'s `wlr_addon_init` call
    // below performs any mutation.
    unsafe {
        assert!(
            find_id_in(set.cast_const(), &ID_ADDON_IMPL).is_none(),
            "an id addon is already attached to this object"
        );

        attach_fresh_id(set, &ID_ADDON_IMPL)
    }
}

/// Retrieve the id attached to `set`, if any.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live object.
pub(crate) unsafe fn find_id(set: *const sys::wlr_addon_set) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised; see
    // `find_id_in`'s own comment for why the `*mut` cast is not a soundness
    // hazard.
    unsafe { find_id_in(set, &ID_ADDON_IMPL) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addon::DESTROY_COUNT;

    /// Exercises the id addon against a standalone `wlr_addon_set`. This needs
    /// no display, backend or output — `wlr_addon_set_init` works on any set.
    #[test]
    fn ids_are_unique_stable_and_self_cleaning() {
        let _serialised = id_test_lock();

        // SAFETY: `set` is a live, exclusively-owned value for this scope, and
        // is finished before it drops.
        unsafe {
            let mut set = std::mem::zeroed::<sys::wlr_addon_set>();
            sys::wlr_addon_set_init(&raw mut set);

            assert_eq!(find_id(&raw const set), None, "empty set has no id");

            let a = attach_id(&raw mut set);
            assert_eq!(find_id(&raw const set), Some(a), "id is retrievable");
            assert_eq!(
                find_id(&raw const set),
                Some(a),
                "and stable across lookups"
            );

            let mut other = std::mem::zeroed::<sys::wlr_addon_set>();
            sys::wlr_addon_set_init(&raw mut other);
            let b = attach_id(&raw mut other);
            assert_ne!(a, b, "ids are unique across objects");

            let destroyed_before = DESTROY_COUNT.load(Ordering::Relaxed);

            // Finishing the set runs our destroy hook and frees the addon.
            sys::wlr_addon_set_finish(&raw mut set);
            // `wlr_addon_finish` does a `wl_list_remove`, which leaves a
            // finished set a valid, walkable, empty list head — so this
            // lookup is safe and proves the destroy hook unlinked the addon
            // rather than merely "the process did not crash".
            assert_eq!(
                find_id(&raw const set),
                None,
                "the destroy hook unlinked the addon"
            );

            sys::wlr_addon_set_finish(&raw mut other);

            assert_eq!(
                DESTROY_COUNT.load(Ordering::Relaxed) - destroyed_before,
                2,
                "the destroy hook ran, and freed the Box, for both addons"
            );
        }
    }

    /// A `wlr_surface` carries its role id and its `SurfaceId` as two separate
    /// addons on one set. `wlr_addon` keys on `(owner, impl)`, so the two kinds
    /// must resolve independently — `attach_surface_id` is not allowed to
    /// shadow or disturb a role id already present, and `ensure_surface_id_raw`
    /// must hand back the same value on every call.
    #[test]
    fn surface_and_role_ids_coexist_on_one_set() {
        let _serialised = id_test_lock();

        // SAFETY: `set` is a live, exclusively-owned value for this scope, and
        // is finished before it drops.
        unsafe {
            let mut set = std::mem::zeroed::<sys::wlr_addon_set>();
            sys::wlr_addon_set_init(&raw mut set);

            assert_eq!(find_surface_id(&raw const set), None, "empty set has none");

            let role = attach_id(&raw mut set);
            let surface = attach_surface_id(&raw mut set).expect("first attach succeeds");

            assert_eq!(find_id(&raw const set), Some(role), "the role id survives");
            assert_eq!(
                find_surface_id(&raw const set),
                Some(surface),
                "the surface id resolves alongside it"
            );
            assert_ne!(role, surface, "the two kinds never share a value");
            assert_eq!(
                attach_surface_id(&raw mut set),
                None,
                "a second attach reports the duplicate instead of aborting"
            );
            assert_eq!(
                ensure_surface_id_raw(&raw mut set),
                surface,
                "ensure is idempotent rather than attaching a second addon"
            );

            sys::wlr_addon_set_finish(&raw mut set);
            assert_eq!(find_surface_id(&raw const set), None);
            assert_eq!(find_id(&raw const set), None);
        }
    }
}
