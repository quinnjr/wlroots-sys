//! The shared core for the crate's hand-rolled `u32` capability bitmasks.
//!
//! [`WmCapabilities`](crate::WmCapabilities),
//! [`WorkspaceGroupCapabilities`](crate::WorkspaceGroupCapabilities) and
//! [`WorkspaceCapabilities`](crate::WorkspaceCapabilities) are three
//! spellings of one shape — named `u32` constants plus
//! `contains`/`bits`/`from_raw` and `BitOr`/`BitOrAssign`, hand-rolled
//! rather than a `bitflags` dependency. `define_capability_mask!` generates
//! that shape from one definition, so the three types cannot drift apart: a
//! fourth bitmask of this shape belongs here too, not in a fourth copy.

/// Define one `u32`-backed capability bitmask: the struct, its named
/// constants, and the shared `contains`/`bits`/`from_raw` plus
/// `BitOr`/`BitOrAssign` core.
///
/// The per-type docs and constant values are macro inputs, so the generated
/// items render exactly as hand-written ones would; only the identical method
/// and operator bodies are shared. `contains` means "every bit of `other`",
/// never "any"; `from_raw` keeps unknown bits rather than dropping them,
/// because the mask is handed straight back to wlroots by the setter that
/// interprets it.
macro_rules! define_capability_mask {
    (
        $(#[$type_meta:meta])*
        $vis:vis struct $name:ident(u32);
        $(
            $(#[$const_meta:meta])*
            $const_vis:vis const $const_name:ident = $const_value:expr
        ),*
        $(,)?
    ) => {
        $(#[$type_meta])*
        $vis struct $name(u32);

        impl $name {
            $(
                $(#[$const_meta])*
                $const_vis const $const_name: $name = $name($const_value);
            )*

            /// Whether **every** bit of `other` is set here — not "any".
            #[must_use]
            pub fn contains(self, other: $name) -> bool {
                self.0 & other.0 == other.0
            }

            /// The raw mask, as the protocol numbers it.
            #[must_use]
            pub fn bits(self) -> u32 {
                self.0
            }

            /// Build from the raw protocol value.
            ///
            /// Unknown bits are kept, not dropped: the mask is handed
            /// straight back to wlroots by the setter that interprets it, so
            /// silently clearing a bit would change the caller's request
            /// rather than merely fail to describe it.
            pub(crate) fn from_raw(raw: u32) -> $name {
                $name(raw)
            }
        }

        impl std::ops::BitOr for $name {
            type Output = $name;

            fn bitor(self, rhs: $name) -> $name {
                $name(self.0 | rhs.0)
            }
        }

        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: $name) {
                self.0 |= rhs.0;
            }
        }
    };
}
