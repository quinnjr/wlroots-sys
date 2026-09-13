//! Per-surface presentation feedback: the `wp_presentation` feedback a compositor
//! hands to a client once it has actually displayed that surface's buffer.
//!
//! Presentation feedback is a three-part contract. The compositor marks a
//! surface's current buffer as *sampled* when it uses those contents
//! ([`Surface::sampled`]); if the content reaches the screen, it reports *when*
//! and *how* via [`PresentationFeedback::send_presented`]; then it releases the
//! feedback. wlroots' `wlr_presentation_surface_sampled` detaches the feedback
//! from its per-surface bookkeeping and hands ownership to the compositor, so
//! the returned [`PresentationFeedback`] is **owned** and leaks if it is never
//! destroyed — hence its [`Drop`], which is the one place
//! `wlr_presentation_feedback_destroy` is called.
//!
//! The two shortcuts, [`Surface::textured_on_output`] and
//! [`Surface::scanned_out_on_output`], fold sampling and reporting into one
//! call: wlroots samples internally, attaches the output's present listeners,
//! and destroys the feedback itself once the output has reported (or never
//! will). Neither returns a handle, and neither is used here to create one.
//!
//! # What needs a client
//!
//! A feedback object exists only if the client bound `wp_presentation` and
//! asked for feedback on the surface, so [`Surface::sampled`] reports `None`
//! for a surface whose client never did. That is a miss, not an error: the
//! compositor's sampling path is identical whether or not anyone is listening.

use std::mem::MaybeUninit;
use std::ptr::NonNull;

use crate::output::{Output, PresentEvent, PresentFlags};
use crate::surface::{Surface, SurfaceId};
use crate::{OutputId, Runtime, sys};

/// One `wp_presentation` present event: the timestamp, refresh rate, sequence
/// number and flags a client is told when its buffer reached the screen.
///
/// An owned value snapshot rather than a borrow of wlroots memory — the C
/// `wlr_presentation_event` is a plain value the caller fills in, and
/// [`PresentEvent::as_c`] is its only in-crate producer besides
/// [`from_output`](Self::from_output) itself.
pub struct PresentationEvent {
    raw: sys::wlr_presentation_event,
}

impl std::fmt::Debug for PresentationEvent {
    /// Hand-written so the raw output pointer — which is neither useful nor
    /// stable across runs — is not printed; the id is, when one is attached.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentationEvent")
            .field("output_id", &self.output_id())
            .field("tv_sec", &self.tv_sec())
            .field("tv_nsec", &self.tv_nsec())
            .field("refresh", &self.refresh())
            .field("seq", &self.seq())
            .field("flags", &self.flags())
            .finish()
    }
}

impl PresentationEvent {
    /// Build a presentation event from an output's present event.
    ///
    /// `output` is the output the frame reached; `present` is the
    /// [`PresentEvent`] the compositor is already reporting through
    /// [`Output::send_present`]. This is the Rust face of
    /// `wlr_presentation_event_from_output`, which copies the timestamp,
    /// refresh, sequence and flags out of the output's own event.
    pub fn from_output(output: &Output<'_>, present: &PresentEvent) -> PresentationEvent {
        let output_event = present.as_c(output.as_ptr());
        // `wlr_presentation_event` is a plain aggregate (a pointer and five
        // integers), so every bit pattern is valid; `MaybeUninit` only avoids
        // materialising a zeroed value the C call is about to overwrite.
        let mut raw = MaybeUninit::<sys::wlr_presentation_event>::uninit();
        // SAFETY: `raw` points at a live local sized for exactly the event the
        // C function writes, and `output_event` is a live local the function
        // only reads. The call initialises every field of `raw`.
        unsafe {
            sys::wlr_presentation_event_from_output(raw.as_mut_ptr(), &output_event);
            PresentationEvent {
                raw: raw.assume_init(),
            }
        }
    }

    /// The output the frame reached, as its stable id, when one is attached.
    ///
    /// `None` when the event names no output, or names one this crate never
    /// registered — the same by-id miss every other lookup in this crate
    /// reports.
    pub fn output_id(&self) -> Option<OutputId> {
        if self.raw.output.is_null() {
            return None;
        }
        // SAFETY: `from_output` copied the output pointer the caller handed in,
        // so it is the live output the caller was working with; this reads its
        // addon set without mutating it.
        unsafe { crate::id::find_id(&raw const (*self.raw.output).addons).map(OutputId) }
    }

    /// Whole seconds of the presentation timestamp.
    pub fn tv_sec(&self) -> u64 {
        self.raw.tv_sec
    }

    /// Nanoseconds past [`tv_sec`](Self::tv_sec), always below one second.
    pub fn tv_nsec(&self) -> u32 {
        self.raw.tv_nsec
    }

    /// Refresh rate in mHz at presentation time.
    pub fn refresh(&self) -> u32 {
        self.raw.refresh
    }

    /// Presentation sequence counter.
    pub fn seq(&self) -> u64 {
        self.raw.seq
    }

    /// How the frame reached the screen.
    pub fn flags(&self) -> PresentFlags {
        PresentFlags::from_bits(self.raw.flags)
    }
}

/// A `wlr_presentation_feedback` this crate owns.
///
/// Returned by [`Surface::sampled`] when the surface's client asked for
/// feedback, [`send_presented`](Self::send_presented) reports the presentation,
/// and dropping it releases wlroots' copy. Because wlroots detaches the feedback
/// from its own bookkeeping when it hands it over, nothing else will free it —
/// the `Drop` below is the only release path, and a leaked handle is the only
/// consequence of forgetting it.
pub struct PresentationFeedback {
    raw: NonNull<sys::wlr_presentation_feedback>,
}

impl std::fmt::Debug for PresentationFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentationFeedback")
            .finish_non_exhaustive()
    }
}

impl PresentationFeedback {
    /// Take ownership of a feedback wlroots just handed over.
    ///
    /// `NonNull` rather than a raw pointer so the null check lives in the one
    /// caller ([`Surface::sampled`]) whose lookup can miss, and this
    /// constructor stays free of `unwrap`/`expect` on the handler path.
    ///
    /// # Safety
    ///
    /// `raw` must be a feedback returned by `wlr_presentation_surface_sampled`
    /// and not yet destroyed, and the returned handle must be the only owner.
    pub(crate) unsafe fn from_non_null(
        raw: NonNull<sys::wlr_presentation_feedback>,
    ) -> PresentationFeedback {
        PresentationFeedback { raw }
    }

    /// Tell the client whose feedback this is when its buffer was presented.
    ///
    /// Consumes the feedback: wlroots' contract is to send once and then
    /// destroy, and this is the one shot. The `Drop` that runs when this
    /// returns is what performs the destroy (and sends `discarded` to any
    /// client that no longer needs the feedback). A caller that decides the
    /// content was *not* presented simply drops the feedback instead; the same
    /// destroy runs, so there is no way to forget either step.
    ///
    /// Sending through an event built for another output is not prevented here
    /// — wlroots does not check either — but the timestamp and refresh are what
    /// the client sees, so it is the caller's to get right.
    pub fn send_presented(self, event: &PresentationEvent) {
        // SAFETY: the handle owns a live feedback and has not sent yet (this
        // method consumes it), and `event` is a live local the call only reads.
        // The `self` drop at the end of this function then destroys it exactly
        // once.
        unsafe {
            sys::wlr_presentation_feedback_send_presented(self.raw.as_ptr(), &raw const event.raw);
        }
    }
}

impl Drop for PresentationFeedback {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the feedback (the constructor's
        // contract), and `Drop` runs exactly once, so the wlroots object is
        // freed exactly once. `wlr_presentation_feedback_destroy` also sends
        // `discarded` to any client still listening, which is why it — not a
        // bare `free` — is the release path.
        unsafe { sys::wlr_presentation_feedback_destroy(self.raw.as_ptr()) };
    }
}

impl Surface<'_> {
    /// Mark this surface's current buffer as sampled.
    ///
    /// Returns the feedback to report once the content is displayed, or `None`
    /// when the surface's client never requested presentation feedback. Taking
    /// the feedback detaches it from wlroots' bookkeeping, so a second call
    /// for the same sample returns `None` until the client asks again.
    pub fn sampled(&self) -> Option<PresentationFeedback> {
        // SAFETY: the handle borrows a live surface for its lifetime. The
        // returned feedback is owned by wlroots until this call detaches it;
        // `from_non_null` takes that ownership.
        let raw = unsafe { sys::wlr_presentation_surface_sampled(self.as_ptr()) };
        NonNull::new(raw).map(|raw| unsafe { PresentationFeedback::from_non_null(raw) })
    }

    /// Mark this surface's current buffer as textured onto `output`.
    ///
    /// The one-call form of sampling and reporting: wlroots samples internally,
    /// watches `output`'s commit/present signals, and destroys the feedback
    /// once the output reports it (or is destroyed). Use this only when the
    /// content will be copied into a buffer shown on `output` before that
    /// output's next commit.
    pub fn textured_on_output(&self, output: &Output<'_>) {
        // SAFETY: the handle borrows a live surface, `output` a live output;
        // wlroots stores listeners on the output and takes ownership of the
        // feedback it samples internally, so nothing here is retained by the
        // caller.
        unsafe {
            sys::wlr_presentation_surface_textured_on_output(self.as_ptr(), output.as_ptr());
        }
    }

    /// Mark this surface's current buffer as scanned out directly onto `output`.
    ///
    /// [`textured_on_output`](Self::textured_on_output) with the zero-copy flag
    /// set, so the feedback tells the client its buffer was shown without a
    /// copy.
    pub fn scanned_out_on_output(&self, output: &Output<'_>) {
        // SAFETY: as in `textured_on_output`.
        unsafe {
            sys::wlr_presentation_surface_scanned_out_on_output(self.as_ptr(), output.as_ptr());
        }
    }
}

impl Runtime {
    /// The [`Surface::sampled`] path, resolved from a stored [`SurfaceId`].
    ///
    /// `None` when no live surface has `id` — the by-id miss every operation in
    /// this crate promises — or when that surface's client never requested
    /// feedback.
    pub fn sample_presentation(&self, id: SurfaceId) -> Option<PresentationFeedback> {
        self.surface(id)?.sampled()
    }
}

#[cfg(test)]
mod tests {
    use super::{PresentationEvent, PresentationFeedback};
    use crate::output::{Output, PresentEvent, PresentFlags};
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::ptr::NonNull;

    /// A zeroed `wlr_output` with an initialised addon set, enough for
    /// `wlr_presentation_event_from_output` to read its fields and for
    /// `output_id` to walk it. Same shape as `output.rs`'s scratch output.
    struct ScratchOutput(*mut sys::wlr_output);

    impl ScratchOutput {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_output>();
            // SAFETY: `wlr_output` is non-zero-sized, so `alloc_zeroed` returns
            // null (checked) or a suitably aligned, zeroed allocation.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_output>();
            assert!(!ptr.is_null(), "allocation failed");
            // SAFETY: `ptr` is a fresh, exclusively-owned allocation sized for
            // the addon set it embeds; `wlr_addon_set_init` writes only the two
            // `wl_list` fields it owns, in bounds.
            unsafe { sys::wlr_addon_set_init(&raw mut (*ptr).addons) };
            Self(ptr)
        }
    }

    impl Drop for ScratchOutput {
        fn drop(&mut self) {
            // SAFETY: the addon set was initialised in `new` and no addon is
            // attached, so finishing it exactly undoes that init.
            unsafe { sys::wlr_addon_set_finish(&raw mut (*self.0).addons) };
            // SAFETY: `self.0` was allocated by `alloc_zeroed` with this layout.
            unsafe { dealloc(self.0.cast::<u8>(), Layout::new::<sys::wlr_output>()) };
        }
    }

    #[test]
    fn from_output_copies_the_output_events_fields() {
        let scratch = ScratchOutput::new();
        // SAFETY: `scratch` outlives the handle and the event.
        let output = unsafe { Output::from_raw(scratch.0) };
        let present = PresentEvent {
            commit_seq: 7,
            presented: true,
            when: std::time::Duration::new(12, 345_678_901),
            seq: 42,
            refresh: 60_000,
            flags: PresentFlags::VSYNC | PresentFlags::ZERO_COPY,
        };

        let event = PresentationEvent::from_output(&output, &present);

        assert_eq!(event.tv_sec(), 12);
        assert_eq!(event.tv_nsec(), 345_678_901);
        assert_eq!(event.refresh(), 60_000);
        assert_eq!(event.seq(), 42);
        assert_eq!(
            event.flags(),
            PresentFlags::VSYNC | PresentFlags::ZERO_COPY,
            "the copied flags round-trip through the C field"
        );
        assert_eq!(
            event.output_id(),
            None,
            "a scratch output carries no id addon, so the lookup misses"
        );
    }

    /// wlroots frees the feedback in `wlr_presentation_feedback_destroy`, and
    /// this handle owns it. A malloc'd, list-initialised feedback is exactly the
    /// object wlroots returns, so both release paths run for real here: even
    /// iterations `send_presented` (which consumes the handle and destroys it
    /// when it returns), odd ones drop without sending. Under ASan a double
    /// release or a use after one is reported. `calloc` rather than a Rust
    /// allocation because the C `free` inside wlroots must match.
    #[test]
    fn dropping_a_feedback_runs_the_wlroots_destroy_exactly_once() {
        for i in 0..8 {
            // SAFETY: `calloc` returns null or a zeroed, suitably aligned block
            // of exactly one `wlr_presentation_feedback`.
            let raw = unsafe {
                calloc(1, std::mem::size_of::<sys::wlr_presentation_feedback>())
                    .cast::<sys::wlr_presentation_feedback>()
            };
            assert!(!raw.is_null(), "calloc failed");
            // SAFETY: `raw` is a live, exclusively-owned, zeroed feedback. An
            // empty `wl_list` points at itself; `send_presented` and destroy
            // both require the resources list to be initialised.
            unsafe {
                (*raw).resources.prev = &raw mut (*raw).resources;
                (*raw).resources.next = &raw mut (*raw).resources;
            }

            // SAFETY: `raw` is a feedback wlroots would have handed over, and
            // this is its only owner.
            let feedback = unsafe {
                PresentationFeedback::from_non_null(
                    NonNull::new(raw).expect("calloc returned a feedback"),
                )
            };
            // An all-zero event: the empty resource list makes `send_presented`
            // a no-op, so no field of it is read, and no output is needed.
            let event = PresentationEvent {
                raw: unsafe { std::mem::zeroed() },
            };
            if i % 2 == 0 {
                // Consumes the handle; the destroy runs when the method returns.
                feedback.send_presented(&event);
            } else {
                // The no-send path: drop alone destroys.
                drop(feedback);
            }
        }
    }

    // SAFETY: the declaration matches libc's `calloc`; the test above upholds
    // its contract, and the memory is released by wlroots' own `free`.
    unsafe extern "C" {
        fn calloc(nmemb: usize, size: usize) -> *mut std::ffi::c_void;
    }
}
