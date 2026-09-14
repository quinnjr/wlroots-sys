//! The `wlr_fixes` global: wlroots' compatibility fixes for older clients.
//!
//! wlroots exposes a handful of behaviour fixes some clients still depend on —
//! a `wl_surface`-level workaround older toolkits need to render correctly — as
//! one optional global. A compositor that wants them advertises it with
//! [`Runtime::create_fixes`] before the run; a runtime that never calls this
//! exposes nothing and clients fall back to the unfixed behaviour.
//!
//! The global is display-owned: wlroots creates it, links its own display
//! teardown and frees it with the display, so this crate only keeps the pointer
//! it used to discard.

use std::ptr::NonNull;

use crate::{Display, Error, Result, Runtime, sys};

impl Runtime {
    /// Create the `wlr_fixes` global, advertising wlroots' client
    /// compatibility fixes. Errors if called twice.
    ///
    /// `version` is the protocol version to advertise. The global lives and
    /// dies with `display`; this crate never frees it, and there is no other
    /// call against it — advertising it is the whole API.
    pub fn create_fixes(&self, display: &Display, version: u32) -> Result<()> {
        if self.inner.fixes.borrow().is_some() {
            return Err(Error::Operation("Runtime::create_fixes called twice"));
        }
        // SAFETY: `display` is live for the call; wlroots owns the returned
        // global and its display-destroy listener frees it with the display.
        let raw = unsafe { sys::wlr_fixes_create(display.as_ptr(), version) };
        let raw = NonNull::new(raw).ok_or(Error::Create("wlr_fixes_create"))?;
        *self.inner.fixes.borrow_mut() = Some(raw);
        Ok(())
    }

    /// The `wlr_fixes` global, once created. Kept so the create-once guard and
    /// any future accessor can reach it without re-walking the display.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn fixes_ptr(&self) -> Option<NonNull<sys::wlr_fixes>> {
        *self.inner.fixes.borrow()
    }
}

#[cfg(test)]
mod tests {
    use crate::Runtime;

    /// A runtime that never creates the global reports no pointer, and the
    /// accessor is a plain miss rather than a fabrication.
    #[test]
    fn fixes_ptr_misses_before_create() {
        let runtime = Runtime::new().expect("runtime");
        assert!(runtime.fixes_ptr().is_none());
    }
}
