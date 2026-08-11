//! Handler traits.
//!
//! Consumers implement these on one state struct, which the dispatcher hands to
//! every handler as `&mut S`. Every method is defaulted, so a consumer
//! implements only what they use.

use crate::{Output, OutputId};

/// Output lifecycle and frame events.
pub trait OutputHandler {
    /// A new output was attached. The handle is valid only for this call;
    /// remember [`Output::id`] if you need to refer to it later.
    fn new_output(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// It is a good time to render a frame for this output.
    ///
    /// wlroots expects rendering to happen before this returns, so this is one
    /// of the paths that is never deferred.
    fn frame(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// The output is gone. Only the id is passed, because there is no longer an
    /// object to borrow.
    fn destroyed(&mut self, id: OutputId) {
        let _ = id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A consumer implementing nothing at all must still satisfy the trait.
    struct Minimal;
    impl OutputHandler for Minimal {}

    #[test]
    fn every_handler_method_is_defaulted() {
        fn accepts<S: OutputHandler>(_: &S) {}
        accepts(&Minimal);
    }
}
