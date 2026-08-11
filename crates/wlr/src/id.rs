//! Stable object identity.
//!
//! A raw pointer is not an identity: wlroots may reuse an address after free, so
//! a pointer compared across a destroy can alias a different object. Instead a
//! monotonic id is attached to the C object with `wlr_addon`, wlroots' own
//! mechanism for data whose lifetime is bound to an object. wlroots runs our
//! destructor at exactly the right moment, so nothing has to be swept.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Our addon payload: a `wlr_addon` header followed by the id.
///
/// `#[repr(C)]` with the addon first so `id_addon_from_raw` can recover the
/// payload from the `*mut wlr_addon` wlroots hands to the destroy hook via a
/// zero-offset cast. Enforced below by `const _`, since the cast itself
/// cannot detect a future field reordering.
#[repr(C)]
struct IdAddon {
    addon: sys::wlr_addon,
    id: u64,
}

// `id_addon_from_raw`'s cast is sound only while `addon` is `IdAddon`'s first
// field, at offset 0. This fails to compile if that ever stops being true.
const _: () = assert!(std::mem::offset_of!(IdAddon, addon) == 0);

/// `wlr_addon_interface` holds raw pointers, so it is not `Sync` by default.
/// Wrapping it lets us hold one immutable instance for the process.
struct AddonImpl(sys::wlr_addon_interface);

// SAFETY: the contents are never mutated after initialisation, and the `name`
// pointer targets a `'static` C string.
unsafe impl Sync for AddonImpl {}

static ID_ADDON_IMPL: AddonImpl = AddonImpl(sys::wlr_addon_interface {
    name: c"wlr-rs-object-id".as_ptr(),
    destroy: Some(id_addon_destroy),
});

/// Test-only witness that `id_addon_destroy` actually ran and freed its
/// `Box`. Production builds have no way to observe that beyond "did not
/// crash"; this gives the test something to assert on.
#[cfg(test)]
static DESTROY_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Serialises every test that attaches or destroys an id addon.
///
/// [`DESTROY_COUNT`] is process-wide and the test below asserts a *delta* across
/// its own work, so a second test destroying an id addon on another harness
/// thread at the same moment would inflate that delta and fail it for the wrong
/// reason. `backend.rs`'s delivery tests destroy id addons, so they take this
/// lock too — the alternative is a suite that passes or fails by scheduling.
///
/// **If you write a test that finishes an addon set carrying an id addon —
/// calling `wlr_addon_set_finish` directly, or through a fixture whose `Drop`
/// does — take this lock for the whole test.** Finishing the set runs
/// [`id_addon_destroy`], which bumps [`DESTROY_COUNT`]; a test that does so
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

/// Called by wlroots when the owning object is destroyed.
unsafe extern "C" fn id_addon_destroy(addon: *mut sys::wlr_addon) {
    // SAFETY: wlroots only invokes this for addons we registered, all of which
    // are the `addon` field of a boxed `IdAddon`.
    unsafe {
        let payload: *mut IdAddon = id_addon_from_raw(addon);
        sys::wlr_addon_finish(addon);
        drop(Box::from_raw(payload));
    }
    #[cfg(test)]
    DESTROY_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Downcast `addon` to the `IdAddon` that embeds it.
///
/// A plain pointer cast, not offset arithmetic: valid only while `addon` is
/// `IdAddon`'s first `#[repr(C)]` field (offset 0), which is checked at
/// compile time by the `const _` assertion next to the struct definition.
unsafe fn id_addon_from_raw(addon: *mut sys::wlr_addon) -> *mut IdAddon {
    // SAFETY: `addon` points at the `addon` field of a live `IdAddon`, and
    // that field is at offset 0 (enforced by the `const _` assertion above),
    // so the cast recovers the enclosing `IdAddon` exactly.
    addon.cast::<IdAddon>()
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

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let payload = Box::into_raw(Box::new(IdAddon {
            addon: std::mem::zeroed(),
            id,
        }));

        sys::wlr_addon_init(
            &raw mut (*payload).addon,
            set,
            (&raw const ID_ADDON_IMPL).cast::<c_void>(),
            &raw const ID_ADDON_IMPL.0,
        );
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
        let addon = sys::wlr_addon_find(
            set.cast_mut(),
            (&raw const ID_ADDON_IMPL).cast::<c_void>(),
            &raw const ID_ADDON_IMPL.0,
        );
        if addon.is_null() {
            return None;
        }
        Some((*id_addon_from_raw(addon)).id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
