//! Stable object identity.
//!
//! A raw pointer is not an identity: wlroots may reuse an address after free, so
//! a pointer compared across a destroy can alias a different object. Instead a
//! monotonic id is attached to the C object with `wlr_addon`, wlroots' own
//! mechanism for data whose lifetime is bound to an object. wlroots runs our
//! destructor at exactly the right moment, so nothing has to be swept.
//!
//! `attach_id` and `find_id` have no callers outside this module's tests yet —
//! `Output` (Task 5) and its constructors (Task 7/8) are what will wire them
//! up — so the module is allowed to be dead code for now rather than muted
//! item-by-item.
#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sys;

/// Identifies an output for as long as the consumer chooses to remember it.
///
/// Storable, comparable and hashable — unlike a handle, which cannot escape the
/// handler it was passed to. Ids are never reused within a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub(crate) u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Our addon payload: a `wlr_addon` header followed by the id.
///
/// `#[repr(C)]` with the addon first so `container_of!` can recover the payload
/// from the `*mut wlr_addon` wlroots hands to the destroy hook.
#[repr(C)]
struct IdAddon {
    addon: sys::wlr_addon,
    id: u64,
}

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

/// Called by wlroots when the owning object is destroyed.
unsafe extern "C" fn id_addon_destroy(addon: *mut sys::wlr_addon) {
    // SAFETY: wlroots only invokes this for addons we registered, all of which
    // are the `addon` field of a boxed `IdAddon`.
    unsafe {
        let payload: *mut IdAddon = wlr_sys_container_of(addon);
        sys::wlr_addon_finish(addon);
        drop(Box::from_raw(payload));
    }
}

/// Recover the `IdAddon` from its embedded `wlr_addon`.
///
/// A local helper rather than `wlr-sys`'s `container_of!`, because the macro is
/// exported from whichever versioned crate is selected and this keeps the
/// version-specific path out of the call site.
unsafe fn wlr_sys_container_of(addon: *mut sys::wlr_addon) -> *mut IdAddon {
    // SAFETY: `addon` points at the `addon` field of a live `IdAddon`, which is
    // `#[repr(C)]` with that field first, so the offset is zero.
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
        Some((*wlr_sys_container_of(addon)).id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the id addon against a standalone `wlr_addon_set`. This needs
    /// no display, backend or output — `wlr_addon_set_init` works on any set.
    #[test]
    fn ids_are_unique_stable_and_self_cleaning() {
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

            // Finishing the set runs our destroy hook and frees the addon.
            sys::wlr_addon_set_finish(&raw mut set);
            sys::wlr_addon_set_finish(&raw mut other);
        }
    }
}
