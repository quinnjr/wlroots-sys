//! Borrow-scoped Xwayland-surface handles and their stable ids.
//!
//! Same shape, and for the same reason, as [`Toplevel`](crate::Toplevel): a
//! `wlr_xwayland_surface` is freed whenever its X11 client (or the X server)
//! says so, so a handle that escapes the handler it was passed to is a
//! use-after-free. The lifetime and the private constructor make that a compile
//! error rather than a documented rule.
//!
//! Unlike a `wlr_xdg_toplevel`, though, an X11 window's identity is **not**
//! carried on an addon set. An X11 window exists before it has any
//! `wlr_surface` at all (the two-phase `associate`/`map` lifecycle — see
//! [`crate::ToplevelHandler::new_xwayland_surface`]), so there is no surface to
//! hang an id addon on when the window is first announced. The id is minted
//! from the crate's process-wide counter the moment
//! `wlr_xwayland.events.new_surface` fires and carried in each per-surface
//! listener's own `Bound`, exactly the way the pointer-constraints listeners
//! carry their identity — see `backend.rs`.

use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::Box2D;
use crate::sys;

/// The `_NET_WM_WINDOW_TYPE` an X11 window advertises, wrapped over the common
/// EWMH atoms a compositor keys placement and decoration on.
///
/// An X11 window may list several window-type atoms; [`XwaylandSurface::window_type`]
/// resolves them to the single most specific one it recognises (see that
/// method's own priority note), defaulting to [`Normal`](Self::Normal) when the
/// window sets none or only atoms this enum does not model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XwaylandWindowType {
    /// `_NET_WM_WINDOW_TYPE_NORMAL`, or no recognised type — an ordinary
    /// top-level window.
    Normal,
    /// `_NET_WM_WINDOW_TYPE_DIALOG` — a dialog, typically centered over its
    /// parent.
    Dialog,
    /// `_NET_WM_WINDOW_TYPE_UTILITY` — a persistent utility/tool window.
    Utility,
    /// `_NET_WM_WINDOW_TYPE_SPLASH` — a splash screen, typically centered on
    /// the output.
    Splash,
    /// `_NET_WM_WINDOW_TYPE_TOOLBAR` — a detached toolbar.
    Toolbar,
    /// `_NET_WM_WINDOW_TYPE_MENU` — a pinnable (torn-off) menu.
    Menu,
    /// `_NET_WM_WINDOW_TYPE_DROPDOWN_MENU` — a menu-bar dropdown.
    DropdownMenu,
    /// `_NET_WM_WINDOW_TYPE_POPUP_MENU` — a context (right-click) menu.
    PopupMenu,
    /// `_NET_WM_WINDOW_TYPE_TOOLTIP` — a tooltip.
    Tooltip,
    /// `_NET_WM_WINDOW_TYPE_COMBO` — a combo-box drop-down list.
    Combo,
    /// `_NET_WM_WINDOW_TYPE_NOTIFICATION` — a notification bubble.
    Notification,
    /// `_NET_WM_WINDOW_TYPE_DND` — a drag-and-drop feedback window.
    Dnd,
    /// `_NET_WM_WINDOW_TYPE_DESKTOP` — the desktop background window.
    Desktop,
    /// `_NET_WM_WINDOW_TYPE_DOCK` — a dock/panel.
    Dock,
}

/// Identifies an Xwayland surface for as long as the consumer chooses to
/// remember it.
///
/// Storable, comparable and hashable — unlike a handle. Ids are never reused
/// within a process, and an id held past its surface's destruction resolves to
/// nothing rather than to another window. Mirrors
/// [`ToplevelId`](crate::ToplevelId) in every respect, including the deliberate
/// absence of `PartialOrd`/`Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct XwaylandSurfaceId(pub(crate) u64);

impl XwaylandSurfaceId {
    /// An id no live Xwayland surface can have, for testing the "unknown id"
    /// path.
    ///
    /// Public for the identical reason
    /// [`ToplevelId::dangling_for_test`](crate::ToplevelId::dangling_for_test)
    /// is: "every by-id operation reports a miss rather than dereferencing" is
    /// a promise to consumers, and a promise nobody can write a test for is not
    /// one. Ids come from a process-wide counter that starts at 1, only ever
    /// increments, and never reuses a value, so `u64::MAX` cannot be handed to
    /// a real surface.
    pub fn dangling_for_test() -> XwaylandSurfaceId {
        XwaylandSurfaceId(u64::MAX)
    }

    /// A distinct id no live Xwayland surface can have, for testing.
    ///
    /// The same 2^32-wide reserved band just below `u64::MAX` that
    /// [`ToplevelId::dangling_nth_for_test`](crate::ToplevelId::dangling_nth_for_test)
    /// uses, and for the same reasons — including that `n = 0` aliases
    /// [`dangling_for_test`](Self::dangling_for_test) and that a very large `n`
    /// is folded (`n % 2^32`) so it cannot wrap past the band into real-id
    /// space.
    pub fn dangling_nth_for_test(n: u64) -> XwaylandSurfaceId {
        XwaylandSurfaceId(u64::MAX - (n % (1u64 << 32)))
    }
}

/// An Xwayland surface, borrowed for the duration of a handler call.
///
/// One X11 window. Its accessors read straight through the live
/// `wlr_xwayland_surface`; the borrow's lifetime is what stops the handle
/// outliving the handler it was passed to, exactly as for
/// [`Toplevel`](crate::Toplevel).
pub struct XwaylandSurface<'h> {
    raw: NonNull<sys::wlr_xwayland_surface>,
    id: XwaylandSurfaceId,
    _scope: PhantomData<&'h ()>,
}

/// Hand-written rather than derived, for the same reason
/// [`Toplevel`](crate::Toplevel)'s is: the `PhantomData` scope marker has no
/// value to print, and a raw pointer printed by a derive is neither useful nor
/// stable across runs.
impl std::fmt::Debug for XwaylandSurface<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XwaylandSurface")
            .field("id", &self.id)
            .field("title", &self.title())
            .field("class", &self.class())
            .field("override_redirect", &self.override_redirect())
            .finish()
    }
}

impl<'h> XwaylandSurface<'h> {
    /// # Safety
    ///
    /// `raw` must be a live `wlr_xwayland_surface`, and the returned handle
    /// must not outlive the callback it was created for.
    pub(crate) unsafe fn from_raw_with_id(
        raw: *mut sys::wlr_xwayland_surface,
        id: XwaylandSurfaceId,
    ) -> XwaylandSurface<'h> {
        XwaylandSurface {
            raw: NonNull::new(raw).expect("wlroots handed us a null xwayland surface"),
            id,
            _scope: PhantomData,
        }
    }

    /// This surface's stable identity, safe to store beyond the handler.
    pub fn id(&self) -> XwaylandSurfaceId {
        self.id
    }

    /// The X11 window title (`_NET_WM_NAME`/`WM_NAME`), if one has been set.
    pub fn title(&self) -> Option<String> {
        // SAFETY: the handle's lifetime guarantees the surface is live;
        // wlroots leaves `title` null until the client sets one.
        unsafe { cstr_field((*self.raw.as_ptr()).title) }
    }

    /// The X11 `WM_CLASS` class (the "res_class" half), if set. This is the
    /// value a compositor maps to a Wayland `app_id`.
    pub fn class(&self) -> Option<String> {
        // SAFETY: as for `title`.
        unsafe { cstr_field((*self.raw.as_ptr()).class) }
    }

    /// The X11 `WM_CLASS` instance (the "res_name" half), if set — the
    /// fallback a compositor uses for `app_id` when `class` is absent.
    pub fn instance(&self) -> Option<String> {
        // SAFETY: as for `title`.
        unsafe { cstr_field((*self.raw.as_ptr()).instance) }
    }

    /// The X11 window role (`WM_WINDOW_ROLE`), if set.
    pub fn role(&self) -> Option<String> {
        // SAFETY: as for `title`.
        unsafe { cstr_field((*self.raw.as_ptr()).role) }
    }

    /// The client-requested geometry as a [`Box2D`]: the window's `x`/`y`
    /// position and `width`/`height`, in layout pixels.
    ///
    /// X11 clients self-position, so these are what the client asked for, not
    /// what the compositor has (or has not yet) granted. `x`/`y` are `i16` and
    /// `width`/`height` are `u16` on the wire; they widen losslessly into the
    /// `i32` fields of a `Box2D`.
    pub fn geometry(&self) -> Box2D {
        // SAFETY: the handle's lifetime guarantees the surface is live; these
        // are plain integer fields, always initialised on a live surface.
        unsafe {
            let s = self.raw.as_ptr();
            Box2D::new(
                (*s).x as i32,
                (*s).y as i32,
                (*s).width as i32,
                (*s).height as i32,
            )
        }
    }

    /// Whether this is an override-redirect surface — an X11 window that has
    /// asked the window manager not to manage it (menus, tooltips, drag icons,
    /// combo-box popups). Such surfaces are positioned at their own coordinates
    /// and bypass the managed-window model entirely.
    pub fn override_redirect(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the surface is live;
        // `override_redirect` is a plain `bool` field.
        unsafe { (*self.raw.as_ptr()).override_redirect }
    }

    /// Whether the window set the `_NET_WM_STATE_MODAL` hint — a modal dialog.
    pub fn is_modal(&self) -> bool {
        // SAFETY: as for `override_redirect`.
        unsafe { (*self.raw.as_ptr()).modal }
    }

    /// The window's `_NET_WM_WINDOW_TYPE`, resolved to the single most specific
    /// [`XwaylandWindowType`] this crate recognises.
    ///
    /// An X11 window may advertise several type atoms at once; this returns the
    /// first match in a fixed priority that puts the more placement-specific
    /// kinds first (dialog, then the menu family, tooltip, notification,
    /// utility/splash, drag-and-drop, desktop/dock, toolbar), falling back to
    /// [`Normal`](XwaylandWindowType::Normal) when the window sets no type — the
    /// common case — or only atoms this enum does not model. The atom → type
    /// mapping is wlroots' own (`wlr_xwayland_surface_has_window_type`, which
    /// compares against the `xwm`'s interned atoms), so a compositor never
    /// touches `xcb` to read it.
    pub fn window_type(&self) -> XwaylandWindowType {
        // A window with no type atoms is Normal without an FFI call — which
        // also keeps the accessor sound over a zeroed unit-test surface, whose
        // `window_type_len` is 0 and whose `xwm` back-pointer is null.
        // SAFETY: the handle's lifetime guarantees the surface is live;
        // `window_type_len` is a plain integer field.
        if unsafe { (*self.raw.as_ptr()).window_type_len } == 0 {
            return XwaylandWindowType::Normal;
        }
        use XwaylandWindowType as T;
        use sys::wlr_xwayland_net_wm_window_type as Atom;
        // Priority order: the most placement-specific kinds win when a window
        // lists several atoms.
        const TABLE: &[(Atom, T)] = &[
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DIALOG, T::Dialog),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_COMBO, T::Combo),
            (
                Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
                T::DropdownMenu,
            ),
            (
                Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_POPUP_MENU,
                T::PopupMenu,
            ),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_MENU, T::Menu),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_TOOLTIP, T::Tooltip),
            (
                Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_NOTIFICATION,
                T::Notification,
            ),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_UTILITY, T::Utility),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_SPLASH, T::Splash),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DND, T::Dnd),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DESKTOP, T::Desktop),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_DOCK, T::Dock),
            (Atom::WLR_XWAYLAND_NET_WM_WINDOW_TYPE_TOOLBAR, T::Toolbar),
        ];
        for &(atom, ty) in TABLE {
            // SAFETY: the handle's lifetime guarantees the surface is live;
            // `has_window_type` only reads the window's atom list (non-empty
            // here) and compares against the live `xwm`'s interned atoms.
            if unsafe { sys::wlr_xwayland_surface_has_window_type(self.raw.as_ptr(), atom) } {
                return ty;
            }
        }
        XwaylandWindowType::Normal
    }

    /// A heuristic for whether an override-redirect window wants keyboard focus
    /// (e.g. a `rofi`/`dmenu`-style launcher or a combo-box list), wrapping
    /// wlroots' `wlr_xwayland_surface_override_redirect_wants_focus`.
    ///
    /// Pure X11 window managers ignore override-redirect windows entirely, but
    /// some of them (menus that navigate by keyboard) still expect focus. This
    /// is wlroots' best-effort guess, based on the window type, of which ones a
    /// compositor should hand the keyboard while they are mapped. Meaningful
    /// only for [`override_redirect`](Self::override_redirect) surfaces.
    pub fn override_redirect_wants_focus(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the surface is live; this
        // only reads the window's type to return a bool.
        unsafe { sys::wlr_xwayland_surface_override_redirect_wants_focus(self.raw.as_ptr()) }
    }

    /// The pid of the X11 client that owns this window, from `_NET_WM_PID`.
    ///
    /// `None` when the client set no pid (the property is optional in X11), in
    /// which case wlroots leaves the field `0` or negative.
    pub fn pid(&self) -> Option<u32> {
        // SAFETY: the handle's lifetime guarantees the surface is live; `pid`
        // is a plain integer field.
        let pid = unsafe { (*self.raw.as_ptr()).pid };
        u32::try_from(pid).ok().filter(|&p| p != 0)
    }

    /// Whether this surface has an associated `wlr_surface` yet.
    ///
    /// An X11 window exists before its content surface does; this is `false`
    /// between [`new_xwayland_surface`](crate::ToplevelHandler::new_xwayland_surface)
    /// and the first
    /// [`associate`](crate::ToplevelHandler::xwayland_surface_associate), and
    /// again after an
    /// [`unassociate`](crate::ToplevelHandler::xwayland_surface_unassociate)
    /// until it re-associates.
    pub fn has_surface(&self) -> bool {
        // SAFETY: the handle's lifetime guarantees the surface is live;
        // `surface` is a plain pointer field, null until `associate`.
        unsafe { !(*self.raw.as_ptr()).surface.is_null() }
    }
}

/// Copy a wlroots-owned C string field out, or `None` if it is null.
///
/// # Safety
///
/// `p` must be null or a live, NUL-terminated C string owned by wlroots.
unsafe fn cstr_field(p: *mut std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `p` is a live NUL-terminated string; this
    // copies it out and never frees it.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::{XwaylandSurface, XwaylandSurfaceId};
    use crate::sys;
    use std::alloc::{Layout, alloc_zeroed, dealloc};

    /// A zeroed, heap-allocated `wlr_xwayland_surface`, wired up just enough
    /// for the accessors to read real values through it.
    ///
    /// `alloc_zeroed` rather than `std::mem::zeroed`, for the same reason
    /// `toplevel.rs`'s `ScratchToplevel` uses it: the struct embeds
    /// `wl_signal`/`wl_listener` machinery with bare function pointers, a bit
    /// pattern `std::mem::zeroed` refuses to materialise as a *value*.
    /// Allocating the bytes directly and only ever touching them through a raw
    /// pointer sidesteps that — nothing here reads a listener field, only the
    /// plain scalars and string pointers the accessors read.
    struct ScratchSurface {
        surface: *mut sys::wlr_xwayland_surface,
    }

    impl ScratchSurface {
        fn new() -> Self {
            let layout = Layout::new::<sys::wlr_xwayland_surface>();
            // SAFETY: the type has nonzero size, so `alloc_zeroed` returns
            // either null (checked) or a suitably aligned zeroed allocation.
            let ptr = unsafe { alloc_zeroed(layout) }.cast::<sys::wlr_xwayland_surface>();
            assert!(!ptr.is_null(), "allocation failed");
            Self { surface: ptr }
        }

        /// # Safety
        ///
        /// The returned handle borrows this allocation and must not outlive it.
        unsafe fn handle(&self, id: XwaylandSurfaceId) -> XwaylandSurface<'_> {
            // SAFETY: `self.surface` is a live, zeroed allocation for as long
            // as `self` is; the caller upholds the lifetime bound.
            unsafe { XwaylandSurface::from_raw_with_id(self.surface, id) }
        }
    }

    impl Drop for ScratchSurface {
        fn drop(&mut self) {
            // SAFETY: allocated by `alloc_zeroed` with the matching layout in
            // `new`, still exclusively owned, nothing else frees or aliases it.
            unsafe {
                dealloc(
                    self.surface.cast(),
                    Layout::new::<sys::wlr_xwayland_surface>(),
                );
            }
        }
    }

    #[test]
    fn geometry_reads_the_client_requested_box() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives every use of the handle below, and the
        // fields written are plain scalars in bounds of the allocation.
        unsafe {
            (*scratch.surface).x = 100;
            (*scratch.surface).y = 200;
            (*scratch.surface).width = 640;
            (*scratch.surface).height = 480;
            let s = scratch.handle(XwaylandSurfaceId(0));
            let g = s.geometry();
            assert_eq!((g.x, g.y, g.width, g.height), (100, 200, 640, 480));
        }
    }

    #[test]
    fn empty_surface_reports_no_strings_no_pid_no_surface() {
        let scratch = ScratchSurface::new();
        // SAFETY: `scratch` outlives the handle; the zeroed surface has null
        // string pointers, a zero pid and a null `surface`.
        unsafe {
            let s = scratch.handle(XwaylandSurfaceId(7));
            assert_eq!(s.title(), None);
            assert_eq!(s.class(), None);
            assert_eq!(s.instance(), None);
            assert_eq!(s.role(), None);
            assert_eq!(s.pid(), None);
            assert!(!s.override_redirect());
            assert!(!s.is_modal());
            assert!(!s.has_surface());
            // `window_type_len` is 0 on the zeroed surface, so `window_type`
            // short-circuits to `Normal` without the FFI call that would
            // dereference the null `xwm` back-pointer.
            assert_eq!(s.window_type(), super::XwaylandWindowType::Normal);
            assert_eq!(s.id(), XwaylandSurfaceId(7));
        }
    }

    #[test]
    fn flags_and_pid_read_through() {
        let scratch = ScratchSurface::new();
        // SAFETY: as above; plain scalar writes in bounds.
        unsafe {
            (*scratch.surface).override_redirect = true;
            (*scratch.surface).modal = true;
            (*scratch.surface).pid = 4321;
            let s = scratch.handle(XwaylandSurfaceId(1));
            assert!(s.override_redirect());
            assert!(s.is_modal());
            assert_eq!(s.pid(), Some(4321));
        }
    }

    /// Both test constructors sit at the top of the id space, which a
    /// process-wide counter starting at 1 and only ever incrementing can never
    /// reach — so neither can collide with a real surface's id.
    #[test]
    fn test_constructors_never_collide_with_a_real_id() {
        assert!(XwaylandSurfaceId::dangling_for_test().0 > u32::MAX as u64);
        for n in 1..=8 {
            assert!(XwaylandSurfaceId::dangling_nth_for_test(n).0 > u32::MAX as u64);
        }
    }

    #[test]
    fn nth_for_test_is_distinguishable_by_n() {
        let ids: Vec<XwaylandSurfaceId> = (1..=8)
            .map(XwaylandSurfaceId::dangling_nth_for_test)
            .collect();
        for i in 0..ids.len() {
            for j in 0..ids.len() {
                assert_eq!(i == j, ids[i] == ids[j]);
            }
        }
    }

    #[test]
    fn nth_for_test_zero_aliases_dangling_for_test() {
        assert_eq!(
            XwaylandSurfaceId::dangling_nth_for_test(0),
            XwaylandSurfaceId::dangling_for_test()
        );
    }

    /// The fold must keep even an extreme `n` inside the reserved band, so it
    /// can never alias a value a real (small, counter-issued) id could reach.
    #[test]
    fn nth_for_test_stays_in_band_at_the_extremes() {
        let band_floor = u64::MAX - ((1u64 << 32) - 1);
        for n in [u64::MAX, u64::MAX - 5] {
            assert!(XwaylandSurfaceId::dangling_nth_for_test(n).0 >= band_floor);
        }
    }
}
