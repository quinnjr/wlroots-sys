//! Keyboard and pointer input, translated far enough to be usable and no
//! further.
//!
//! The translation this crate does is exactly the part a compositor cannot
//! sensibly do itself: turning a keycode into a keysym needs the keyboard's
//! compiled `xkb_keymap`, which is inside wlroots. Everything past that —
//! what a keysym *means*, which window is under the pointer *in the
//! compositor's own model* — is the compositor's decision and is not made
//! here.

use std::marker::PhantomData;

use crate::sys;

/// The modifier keys held when a key event was produced.
///
/// Accessors rather than public fields, and `#[non_exhaustive]`-in-spirit
/// through privacy: a public field set could never grow (a struct literal
/// would stop compiling), and there are more modifiers than these four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    logo: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl Modifiers {
    /// The Super / Windows / Command key.
    pub fn logo(self) -> bool {
        self.logo
    }
    /// Control.
    pub fn ctrl(self) -> bool {
        self.ctrl
    }
    /// Alt.
    pub fn alt(self) -> bool {
        self.alt
    }
    /// Shift.
    pub fn shift(self) -> bool {
        self.shift
    }

    /// Decode a `wlr_keyboard_get_modifiers` mask.
    ///
    /// Takes the mask rather than the keyboard itself, and is read at
    /// **emission** time (inside `on_key`) rather than at delivery time: a
    /// key event may be deferred behind another handler, and a deferred key
    /// must report the modifiers that were held *when it was pressed*, not
    /// whatever the keyboard's live state has moved on to by the time
    /// delivery runs. The mask travels inside the `Event`, so this
    /// constructor needs no `unsafe` and no live keyboard at all — decoding a
    /// stale mask copied out earlier is exactly as sound as decoding a fresh
    /// one.
    ///
    /// wlroots' own `wlr_keyboard_modifier` bit values, which are ABI within
    /// a wlroots minor and are checked by this crate's own tests rather than
    /// trusted from this comment.
    pub(crate) fn from_mask(mask: u32) -> Modifiers {
        Modifiers {
            shift: mask & sys::wlr_keyboard_modifier::WLR_MODIFIER_SHIFT.0 != 0,
            ctrl: mask & sys::wlr_keyboard_modifier::WLR_MODIFIER_CTRL.0 != 0,
            alt: mask & sys::wlr_keyboard_modifier::WLR_MODIFIER_ALT.0 != 0,
            logo: mask & sys::wlr_keyboard_modifier::WLR_MODIFIER_LOGO.0 != 0,
        }
    }
}

/// A key press or release, borrowed for the duration of the handler call.
pub struct KeyEvent<'h> {
    keysym: u32,
    modifiers: Modifiers,
    pressed: bool,
    time_msec: u32,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived so the `PhantomData` scope marker — an
/// implementation detail with no value to print — stays out of the output.
impl std::fmt::Debug for KeyEvent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEvent")
            .field("keysym", &format_args!("{:#x}", self.keysym))
            .field("modifiers", &self.modifiers)
            .field("pressed", &self.pressed)
            .field("time_msec", &self.time_msec)
            .finish()
    }
}

impl<'h> KeyEvent<'h> {
    pub(crate) fn new(
        keysym: u32,
        modifiers: Modifiers,
        pressed: bool,
        time_msec: u32,
    ) -> KeyEvent<'h> {
        KeyEvent {
            keysym,
            modifiers,
            pressed,
            time_msec,
            _scope: PhantomData,
        }
    }

    /// The **layout-agnostic, unshifted** keysym for the key.
    ///
    /// Deliberately not the shifted symbol, and this is the single most
    /// error-prone decision in the whole input path. A compositor's key
    /// bindings are written against unshifted symbols — `Super+Shift+q` is
    /// bound on `q` (0x71), not on `Q` (0x51) — so reporting the shifted
    /// symbol makes every binding that includes Shift unreachable, and makes
    /// every other binding stop working when Caps Lock is on. Both were real
    /// bugs in this project's predecessor.
    ///
    /// Read from the keyboard's compiled `xkb_keymap` directly — layout
    /// index 0, level 0 — rather than from its live `xkb_state`, which is
    /// what makes it *layout-agnostic*: an `xkb_state` read reflects whatever
    /// group the user has switched to (`xkb_state_key_get_layout`), so the
    /// same physical key would report a different keysym depending on which
    /// layout happened to be active when it was pressed. Fixing the layout
    /// index at 0 means a binding written once holds regardless. A
    /// compositor that wants the *typed character*, respecting the active
    /// layout and modifiers, wants text input, not this.
    ///
    /// `0` (`XKB_KEY_NoSymbol`) if the keyboard has no compiled keymap yet —
    /// this crate always sets one from the environment when a keyboard is
    /// announced (see `backend.rs`'s `on_new_input`), so in practice this
    /// only happens if that compile itself failed — or if the keycode has no
    /// symbol at level 0 of layout 0.
    ///
    /// With more than one keyboard attached, this is read from **the seat's
    /// active keyboard** (`wlr_seat_get_keyboard`), not necessarily the
    /// physical device that produced this particular event — every keyboard
    /// funnels through one logical keyboard identity at the seat (see
    /// `backend.rs`'s `on_key`), which is what lets a client see one
    /// keyboard regardless of how many are plugged in. The practical
    /// consequence: two keyboards compiled with different layouts do not
    /// each report their own layout's keysym for the same physical key —
    /// both report whichever keyboard is currently active at the seat.
    pub fn keysym(&self) -> u32 {
        self.keysym
    }

    /// The modifiers held when this event was produced.
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// `true` for a press, `false` for a release.
    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// The event's timestamp, in the millisecond clock the Wayland protocol
    /// uses.
    pub fn time_msec(&self) -> u32 {
        self.time_msec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the bit values this module's doc comment claims are ABI within a
    /// wlroots minor, so a future bindgen regeneration that silently
    /// reordered them would fail this test rather than mis-decode every
    /// modifier key.
    #[test]
    fn modifier_bit_values_match_wlroots_headers() {
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_SHIFT.0, 1);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_CAPS.0, 2);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_CTRL.0, 4);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_ALT.0, 8);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_MOD2.0, 16);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_MOD3.0, 32);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_LOGO.0, 64);
        assert_eq!(sys::wlr_keyboard_modifier::WLR_MODIFIER_MOD5.0, 128);
    }

    #[test]
    fn from_mask_decodes_each_bit_independently() {
        let none = Modifiers::from_mask(0);
        assert!(!none.shift() && !none.ctrl() && !none.alt() && !none.logo());

        let shift_ctrl = Modifiers::from_mask(
            sys::wlr_keyboard_modifier::WLR_MODIFIER_SHIFT.0
                | sys::wlr_keyboard_modifier::WLR_MODIFIER_CTRL.0,
        );
        assert!(shift_ctrl.shift());
        assert!(shift_ctrl.ctrl());
        assert!(!shift_ctrl.alt());
        assert!(!shift_ctrl.logo());

        let logo = Modifiers::from_mask(sys::wlr_keyboard_modifier::WLR_MODIFIER_LOGO.0);
        assert!(logo.logo());
        assert!(!logo.shift());
    }

    #[test]
    fn key_event_accessors_return_what_was_constructed() {
        let ev = KeyEvent::new(0x71, Modifiers::from_mask(0), true, 12345);
        assert_eq!(ev.keysym(), 0x71);
        assert!(ev.pressed());
        assert_eq!(ev.time_msec(), 12345);
        assert_eq!(ev.modifiers(), Modifiers::default());
    }

    /// Every public type is `Debug` (Rust API guidelines C-DEBUG), and a
    /// compositor logging an unmatched key wants the keysym in hex — it is
    /// how every keysym table, `xkbcommon-keysyms.h` included, spells them.
    /// The `PhantomData` scope marker must not appear.
    #[test]
    fn a_key_event_prints_its_keysym_in_hex_and_hides_the_scope_marker() {
        let ev = KeyEvent::new(0xff1b, Modifiers::from_mask(0), true, 7);
        let s = format!("{ev:?}");
        assert!(s.contains("keysym: 0xff1b"), "{s}");
        assert!(s.contains("pressed: true"), "{s}");
        assert!(!s.contains("PhantomData"), "{s}");
        assert!(!s.contains("_scope"), "{s}");
    }
}
