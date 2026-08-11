//! Handler traits.
//!
//! Consumers implement these on one state struct, which the dispatcher hands to
//! every handler as `&mut S`. Every method is defaulted, so a consumer
//! implements only what they use.

use crate::{Output, OutputId};

/// Output lifecycle and frame events.
///
/// # Panics
///
/// Every method here is called from C, underneath an `extern "C"` frame, so a
/// panic escaping one **aborts the process**. That has been defined behaviour
/// rather than undefined since Rust 1.81, but it is still an abort out of a
/// compositor's event loop.
///
/// It is also the intended outcome, so do not read it as a defect to be papered
/// over: unwinding back through wlroots' C frames is not possible, and catching
/// the panic and returning into wlroots would resume a compositor whose state
/// is half-updated and whose invariants the handler just abandoned. Aborting is
/// the honest end.
///
/// The consequence for an implementor is that a handler is not a place for
/// `assert!`, `unwrap`, or indexing that might be out of range. Record the
/// problem in your own state and check it once control is back in your hands.
pub trait OutputHandler {
    /// A new output was attached. The handle is valid only for this call;
    /// remember [`Output::id`] if you need to refer to it later.
    fn new_output(&mut self, output: &Output<'_>) {
        let _ = output;
    }

    /// It is a good time to render a frame for this output.
    ///
    /// # Timeliness
    ///
    /// This is delivered like every other event, which means it **may be
    /// deferred**. wlroots emits signals synchronously from inside its own API
    /// calls, so a frame arriving while another handler is already running is
    /// queued and delivered once that handler returns — after wlroots' own
    /// emission has returned, and therefore outside the window wlroots intended
    /// the rendering to happen in.
    ///
    /// That is a genuine cost of this crate's dispatch model and not a bug to
    /// be reported, because the alternative is unsound rather than merely
    /// awkward: delivering directly from inside a running handler would hand
    /// out a second `&mut Self` while the first is still live, which is
    /// undefined behaviour. No rendering deadline is worth that, so the
    /// deferral wins and this is the price.
    ///
    /// A slice that needs the guarantee will have to change the model — a
    /// render-scheduling API that does not put a `&mut Self` on the stack in
    /// the first place, say — rather than special-case this method.
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
