//! What an fd source asks to be woken for, and what it was woken for.
//!
//! libwayland spells both as a bitmask of `WL_EVENT_*`. Those constants live
//! in `wayland-server-core.h` as a plain C enum, which neither `wayland-sys`
//! nor this branch's bindgen allowlist binds, so they are restated here.
//! They are ABI, fixed since libwayland 1.0, and `interest_round_trips_the_mask`
//! below pins the values against libwayland's own behaviour rather than
//! against this comment.

/// `WL_EVENT_READABLE`.
pub(crate) const READABLE: u32 = 0x01;
/// `WL_EVENT_WRITABLE`.
pub(crate) const WRITABLE: u32 = 0x02;
/// `WL_EVENT_HANGUP`.
pub(crate) const HANGUP: u32 = 0x04;
/// `WL_EVENT_ERROR`.
pub(crate) const ERROR: u32 = 0x08;

/// What an fd source asks the event loop to watch for.
///
/// Deliberately not a `bitflags` type and deliberately without public fields:
/// the set of things libwayland lets a source *request* is exactly readable
/// and writable (hangup and error are always reported and cannot be asked
/// for), so an open-ended flags type would advertise combinations that do not
/// exist. The three constants are the whole domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interest {
    mask: u32,
}

impl Interest {
    /// Wake when the fd has data to read.
    pub const READABLE: Interest = Interest { mask: READABLE };
    /// Wake when the fd can accept a write.
    pub const WRITABLE: Interest = Interest { mask: WRITABLE };
    /// Wake for either.
    pub const READ_WRITE: Interest = Interest {
        mask: READABLE | WRITABLE,
    };

    pub(crate) fn mask(self) -> u32 {
        self.mask
    }
}

/// Why an fd source was woken.
///
/// Hangup and error arrive whether or not they were asked for, so this is a
/// superset of the [`Interest`] that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Readiness {
    mask: u32,
}

impl Readiness {
    pub(crate) fn from_mask(mask: u32) -> Readiness {
        Readiness { mask }
    }

    /// The fd has data to read.
    pub fn readable(self) -> bool {
        self.mask & READABLE != 0
    }

    /// The fd can accept a write.
    pub fn writable(self) -> bool {
        self.mask & WRITABLE != 0
    }

    /// The peer closed its end.
    pub fn hangup(self) -> bool {
        self.mask & HANGUP != 0
    }

    /// The fd is in an error state.
    pub fn error(self) -> bool {
        self.mask & ERROR != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_decodes_each_bit_independently() {
        let r = Readiness::from_mask(READABLE | HANGUP);
        assert!(r.readable());
        assert!(r.hangup());
        assert!(!r.writable());
        assert!(!r.error());
    }

    #[test]
    fn read_write_is_the_union_of_the_two_singletons() {
        assert_eq!(
            Interest::READ_WRITE.mask(),
            Interest::READABLE.mask() | Interest::WRITABLE.mask()
        );
    }
}
