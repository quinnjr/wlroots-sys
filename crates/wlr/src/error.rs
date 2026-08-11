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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Create(op) | Error::Operation(op) => write!(f, "{op} failed"),
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

    #[test]
    fn error_is_a_std_error() {
        fn assert_error<T: std::error::Error>() {}
        assert_error::<Error>();
    }
}
