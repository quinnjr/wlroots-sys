//! Stable object identity.
//!
//! A raw pointer is not an identity: wlroots may reuse an address after free, so
//! a pointer compared across a destroy can alias a different object. Instead a
//! monotonic id is attached to the C object with `wlr_addon`, wlroots' own
//! mechanism for data whose lifetime is bound to an object. wlroots runs our
//! destructor at exactly the right moment, so nothing has to be swept.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::addon::{Addon, addon_kind};
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
    // through a shared alias here is fine: `find_id` only calls
    // `wlr_addon_find`, which does not mutate the set, and the read completes
    // (and its borrow ends) before this function's own `wlr_addon_init` call
    // below performs any mutation.
    unsafe {
        assert!(
            find_id(set.cast_const()).is_none(),
            "an id addon is already attached to this object"
        );

        let id = next_id();
        Addon::attach(set, ID_ADDON_IMPL.owner(), &ID_ADDON_IMPL, id);
        id
    }
}

/// Retrieve the id attached to `set`, if any.
///
/// # Safety
///
/// `set` must point at an initialised `wlr_addon_set` belonging to a live object.
pub(crate) unsafe fn find_id(set: *const sys::wlr_addon_set) -> Option<u64> {
    // SAFETY: caller guarantees `set` is live and initialised. `wlr_addon_find`
    // only reads the set (it walks the addon list looking for a match); its C
    // signature takes `*mut wlr_addon_set` even though it performs no mutation,
    // so the cast back to `*mut` here is not a soundness hazard.
    unsafe {
        let payload = Addon::<u64>::find(set, ID_ADDON_IMPL.owner(), &ID_ADDON_IMPL);
        if payload.is_null() {
            return None;
        }
        Some(*Addon::data(payload))
    }
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
}
