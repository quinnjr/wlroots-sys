//! Failure reporting.
//!
//! wlroots signals failure by returning null or `false` and provides no detail,
//! so `Error` names the operation that failed and nothing more. Inventing detail
//! would be less truthful than admitting the C API does not supply it.

use std::fmt;

/// An operation that wlroots reported as failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A wlroots constructor returned null. The payload is the C function name.
    Create(&'static str),
    /// A wlroots operation returned `false`. The payload is the C function name.
    Operation(&'static str),
    /// The wlroots object named by the payload was destroyed, so the call was
    /// never attempted.
    ///
    /// Distinct from [`Error::Operation`] on purpose. `Operation` means C was
    /// called and said no; this means C was *not* called, because the object it
    /// would have been called on is gone. Reporting a C function name here
    /// would name a function that never ran — exactly the invented detail this
    /// module's own docs argue against — and a consumer cannot retry or
    /// diagnose the two the same way.
    ///
    /// The payload is the C type name of the object that died, not a function.
    Destroyed(&'static str),

    /// The named entry point was called while an outer call to it was still
    /// running, and refused rather than proceeding.
    ///
    /// wlroots emits signals synchronously from inside its own API calls, so a
    /// handler runs *underneath* the call that is dispatching it. An entry point
    /// that sets up per-call state a handler can reach cannot tolerate a second
    /// copy of that state existing at the same time, so it reports this instead.
    ///
    /// The payload is the Rust entry point that was re-entered — no C function
    /// was called, so naming one would be the invented detail this module's own
    /// docs argue against, and it is not [`Error::Operation`] for the same
    /// reason.
    ///
    /// **The payload string is diagnostic, not contractual.** Match on the
    /// variant, not its contents: which entry points refuse re-entry may grow,
    /// and their names may be reworded, without that being a breaking change.
    Reentrant(&'static str),

    /// Two values that had to name the same underlying object did not.
    ///
    /// Distinct from every other variant because no C call happened and
    /// nothing died: the caller passed a [`Display`](crate::Display) that does
    /// not own the [`Backend`](crate::Backend)'s event loop, which is a
    /// programming mistake rather than a runtime condition. The payload is
    /// the Rust entry point that refused, and it is diagnostic rather than
    /// contractual — match on the variant, not its contents.
    Mismatch(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create(op) | Error::Operation(op) => write!(f, "{op} failed"),
            Error::Destroyed(what) => write!(f, "{what} was destroyed"),
            Error::Reentrant(what) => write!(f, "{what} was called re-entrantly"),
            Error::Mismatch(what) => write!(f, "{what} was given mismatched arguments"),
        }
    }
}

impl std::error::Error for Error {}

/// Shorthand for results from this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_the_failed_operation() {
        let e = Error::Create("wlr_backend_autocreate");
        assert_eq!(e.to_string(), "wlr_backend_autocreate failed");
    }

    /// A destroyed object must not be reported as a failed call: a consumer
    /// deciding whether to retry needs to tell "wlroots said no" apart from
    /// "the object is gone".
    #[test]
    fn destruction_is_distinguishable_from_a_failed_operation() {
        let destroyed = Error::Destroyed("wlr_backend");
        assert_eq!(destroyed.to_string(), "wlr_backend was destroyed");
        assert_ne!(destroyed, Error::Operation("wlr_backend_start"));
    }

    /// Re-entry is neither a failed C call nor a dead object: a consumer that
    /// gets this one has a structural mistake in their handler, and retrying
    /// the same call from the same place will fail identically.
    #[test]
    fn reentrancy_is_distinguishable_from_the_other_failures() {
        let reentrant = Error::Reentrant("Backend::run");
        assert_eq!(
            reentrant.to_string(),
            "Backend::run was called re-entrantly"
        );
        assert_ne!(reentrant, Error::Operation("wlr_backend_start"));
        assert_ne!(reentrant, Error::Destroyed("wlr_backend"));
    }

    /// A mismatch is neither a failed call, a dead object, nor re-entry: the
    /// caller wired two values together that do not belong to each other.
    #[test]
    fn a_mismatch_is_distinguishable_from_every_other_failure() {
        let m = Error::Mismatch("Backend::run_all");
        assert_eq!(m.to_string(), "Backend::run_all was given mismatched arguments");
        assert_ne!(m, Error::Operation("wlr_backend_start"));
        assert_ne!(m, Error::Destroyed("wlr_backend"));
        assert_ne!(m, Error::Reentrant("Backend::run"));
    }

    #[test]
    fn error_is_a_std_error() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<Error>();
    }
}
