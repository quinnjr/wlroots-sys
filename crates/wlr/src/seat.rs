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
use crate::toplevel::ToplevelId;

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

impl KeyEvent<'static> {
    /// Build a synthetic key event, bound to no keyboard and no handler
    /// call, for a consumer's own tests of
    /// [`SeatHandler::key`](crate::SeatHandler::key).
    ///
    /// `'static` rather than the borrowed `'h` every real event carries:
    /// this event is not scoped to any callback (there is no live keyboard
    /// or handler frame behind it), so nothing would bound a shorter
    /// lifetime, and pinning it at `'static` is what lets a consumer's test
    /// build one, store it in a local, and pass it to their own handler
    /// method without fighting a lifetime a real event never actually
    /// needs there.
    ///
    /// Identical construction to the crate-internal `KeyEvent::new` — same
    /// fields, same `PhantomData` — this is simply that constructor made
    /// `pub` under a name that says "test double", so a consumer's own
    /// key-binding logic can be exercised without a live wlroots keyboard
    /// at all.
    pub fn for_test(
        keysym: u32,
        modifiers: Modifiers,
        pressed: bool,
        time_msec: u32,
    ) -> KeyEvent<'static> {
        KeyEvent::new(keysym, modifiers, pressed, time_msec)
    }
}

/// Which class of input device raised a `cursor-shape-v1` request.
///
/// Mirrors `wlr_cursor_shape_manager_v1_device_type`. A third value
/// (`WLR_CURSOR_SHAPE_MANAGER_V1_DEVICE_TYPE_POINTER`'s tablet-tool sibling
/// exhausts the wire enum today) is not expected — `wlr_cursor_shape_manager_v1_request_set_shape_event::device_type`
/// only ever carries one of these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShapeDevice {
    /// A pointer (mouse, touchpad, ...).
    Pointer,
    /// A tablet tool.
    TabletTool,
}

/// A named cursor image, the way `cursor-shape-v1` names it.
///
/// Mirrors `wp_cursor_shape_device_v1.shape` (wlroots' `wp_cursor_shape_device_v1_shape`
/// C enum) one variant per wire value, including the `dnd_ask`/`all_resize`
/// pair `cursor-shape-v1` version 2 added — this crate advertises version 2
/// (see [`crate::Runtime::create_cursor_shape_manager`]), so both are
/// reachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorShape {
    /// The platform-default pointer.
    Default,
    /// A context menu is available.
    ContextMenu,
    /// Help is available.
    Help,
    /// The platform-default pointer, requested explicitly rather than
    /// implicitly (distinct wire value from [`CursorShape::Default`]).
    Pointer,
    /// A progress indicator.
    Progress,
    /// The program is busy.
    Wait,
    /// A cell or set of cells may be selected.
    Cell,
    /// Crosshair.
    Crosshair,
    /// Text may be selected.
    Text,
    /// Vertical text may be selected.
    VerticalText,
    /// An alias or shortcut is to be created.
    Alias,
    /// Something is to be copied.
    Copy,
    /// Something is to be moved.
    Move,
    /// An item may not be dropped here.
    NoDrop,
    /// The requested action is not allowed.
    NotAllowed,
    /// Something can be grabbed.
    Grab,
    /// Something is being grabbed (e.g. dragged).
    Grabbing,
    /// East resize.
    EResize,
    /// North resize.
    NResize,
    /// North-east resize.
    NeResize,
    /// North-west resize.
    NwResize,
    /// South resize.
    SResize,
    /// South-east resize.
    SeResize,
    /// South-west resize.
    SwResize,
    /// West resize.
    WResize,
    /// Bidirectional east-west resize.
    EwResize,
    /// Bidirectional north-south resize.
    NsResize,
    /// Bidirectional north-east/south-west resize.
    NeswResize,
    /// Bidirectional north-west/south-east resize.
    NwseResize,
    /// Column resize.
    ColResize,
    /// Row resize.
    RowResize,
    /// Something can be scrolled in any direction.
    AllScroll,
    /// Something can be zoomed in.
    ZoomIn,
    /// Something can be zoomed out.
    ZoomOut,
    /// A drag-and-drop action asks the user to choose.
    DndAsk,
    /// Something can be resized in any direction.
    AllResize,
}

impl CursorShape {
    /// Decode a `wp_cursor_shape_device_v1_shape` wire value, or `None` for
    /// one this build's headers do not know (there is no such value today —
    /// wlroots validates the client's wire value before emitting the event —
    /// but a `match` this crate does not control over another crate's enum
    /// should never be a hidden panic).
    pub(crate) fn from_raw(raw: sys::wp_cursor_shape_device_v1_shape) -> Option<CursorShape> {
        use sys::wp_cursor_shape_device_v1_shape as W;
        Some(match raw {
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT => CursorShape::Default,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CONTEXT_MENU => CursorShape::ContextMenu,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_HELP => CursorShape::Help,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_POINTER => CursorShape::Pointer,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_PROGRESS => CursorShape::Progress,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_WAIT => CursorShape::Wait,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CELL => CursorShape::Cell,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CROSSHAIR => CursorShape::Crosshair,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_TEXT => CursorShape::Text,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_VERTICAL_TEXT => CursorShape::VerticalText,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALIAS => CursorShape::Alias,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COPY => CursorShape::Copy,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_MOVE => CursorShape::Move,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NO_DROP => CursorShape::NoDrop,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NOT_ALLOWED => CursorShape::NotAllowed,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRAB => CursorShape::Grab,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRABBING => CursorShape::Grabbing,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_E_RESIZE => CursorShape::EResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_N_RESIZE => CursorShape::NResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NE_RESIZE => CursorShape::NeResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NW_RESIZE => CursorShape::NwResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_S_RESIZE => CursorShape::SResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_SE_RESIZE => CursorShape::SeResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_SW_RESIZE => CursorShape::SwResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_W_RESIZE => CursorShape::WResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_EW_RESIZE => CursorShape::EwResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NS_RESIZE => CursorShape::NsResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NESW_RESIZE => CursorShape::NeswResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NWSE_RESIZE => CursorShape::NwseResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COL_RESIZE => CursorShape::ColResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ROW_RESIZE => CursorShape::RowResize,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALL_SCROLL => CursorShape::AllScroll,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_IN => CursorShape::ZoomIn,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_OUT => CursorShape::ZoomOut,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DND_ASK => CursorShape::DndAsk,
            W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALL_RESIZE => CursorShape::AllResize,
            _ => return None,
        })
    }

    /// Encode back to the `wp_cursor_shape_device_v1_shape` wire value
    /// [`sys::wlr_cursor_shape_v1_name`] wants, for
    /// [`crate::Runtime::set_cursor_shape`].
    pub(crate) fn to_raw(self) -> sys::wp_cursor_shape_device_v1_shape {
        use sys::wp_cursor_shape_device_v1_shape as W;
        match self {
            CursorShape::Default => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT,
            CursorShape::ContextMenu => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CONTEXT_MENU,
            CursorShape::Help => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_HELP,
            CursorShape::Pointer => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_POINTER,
            CursorShape::Progress => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_PROGRESS,
            CursorShape::Wait => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_WAIT,
            CursorShape::Cell => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CELL,
            CursorShape::Crosshair => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_CROSSHAIR,
            CursorShape::Text => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_TEXT,
            CursorShape::VerticalText => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_VERTICAL_TEXT,
            CursorShape::Alias => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALIAS,
            CursorShape::Copy => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COPY,
            CursorShape::Move => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_MOVE,
            CursorShape::NoDrop => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NO_DROP,
            CursorShape::NotAllowed => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NOT_ALLOWED,
            CursorShape::Grab => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRAB,
            CursorShape::Grabbing => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_GRABBING,
            CursorShape::EResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_E_RESIZE,
            CursorShape::NResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_N_RESIZE,
            CursorShape::NeResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NE_RESIZE,
            CursorShape::NwResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NW_RESIZE,
            CursorShape::SResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_S_RESIZE,
            CursorShape::SeResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_SE_RESIZE,
            CursorShape::SwResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_SW_RESIZE,
            CursorShape::WResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_W_RESIZE,
            CursorShape::EwResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_EW_RESIZE,
            CursorShape::NsResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NS_RESIZE,
            CursorShape::NeswResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NESW_RESIZE,
            CursorShape::NwseResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NWSE_RESIZE,
            CursorShape::ColResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_COL_RESIZE,
            CursorShape::RowResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ROW_RESIZE,
            CursorShape::AllScroll => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALL_SCROLL,
            CursorShape::ZoomIn => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_IN,
            CursorShape::ZoomOut => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ZOOM_OUT,
            CursorShape::DndAsk => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DND_ASK,
            CursorShape::AllResize => W::WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_ALL_RESIZE,
        }
    }
}

/// Fields wlroots recorded when a client made this activation token, handed
/// to a compositor's [`crate::SeatHandler::request_activate`] override so it
/// can apply its own focus-steal policy.
///
/// wlroots has already validated the token before this handler runs: it only
/// reaches `request_activate` for a `set_serial`/`set_surface` a client
/// actually sent on a token it actually created through
/// `xdg_activation_v1.get_activation_token`. Nothing here is raw,
/// unvalidated client input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivationToken {
    /// The serial the token was created with (`set_serial`'s first argument),
    /// or `0` if the client never called `set_serial` — check
    /// [`ActivationToken::has_seat`] to tell the two apart, since `0` is
    /// also a value a client could (pointlessly) supply.
    pub serial: u32,
    /// Whether the client supplied a seat (`set_serial`'s second argument).
    /// `false` means the token carries no evidence of a real user action —
    /// most commonly a token minted for another process to redeem — which a
    /// focus-steal policy should usually treat as a reason to refuse.
    pub has_seat: bool,
    /// The surface that requested the token (`set_surface`), mapped to this
    /// crate's own toplevel id when it names a live, tracked toplevel.
    /// `None` when the token named no surface, or named one this crate does
    /// not track (a subsurface, or a toplevel already destroyed).
    pub requesting_toplevel: Option<ToplevelId>,
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
