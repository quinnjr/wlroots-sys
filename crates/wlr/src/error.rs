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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create(op) | Error::Operation(op) => write!(f, "{op} failed"),
            Error::Destroyed(what) => write!(f, "{what} was destroyed"),
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

    #[test]
    fn error_is_a_std_error() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<Error>();
    }
}
