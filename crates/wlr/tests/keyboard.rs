fn headless_env() {
    // SAFETY: libtest runs in parallel but all tests here set identical values.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

#[test]
fn dangling_keyboard_group_misses_cleanly() {
    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    assert!(rt.keyboard_state().is_none());
    assert!(rt.pending_keyboard_state().is_none());
}

#[test]
fn shortcuts_inhibit_and_tablet_handlers_compile() {
    use wlr::{InhibitorId, SeatHandler, ShortcutsInhibitorId, TabletPadId, TabletToolId, ToolId};

    struct H {
        toggles: Vec<(ShortcutsInhibitorId, bool)>,
        tools: Vec<TabletToolId>,
        pads: Vec<TabletPadId>,
    }
    impl SeatHandler for H {
        fn shortcuts_inhibitor_toggled(&mut self, _id: ShortcutsInhibitorId, _active: bool) {
            self.toggles.push((_id, _active));
        }
        fn tablet_tool_event(&mut self, _id: TabletToolId) {
            self.tools.push(_id);
        }
        fn tablet_pad_event(&mut self, _id: TabletPadId) {
            self.pads.push(_id);
        }
    }

    // Also via the alias names the plan's failing assertion uses.
    struct H2 {
        toggles: u32,
        tools: u32,
    }
    impl SeatHandler for H2 {
        fn shortcuts_inhibitor_toggled(&mut self, _id: InhibitorId, _active: bool) {
            let _ = (_id, _active);
            self.toggles += 1;
        }
        fn tablet_tool_event(&mut self, _id: ToolId) {
            let _ = _id;
            self.tools += 1;
        }
    }

    // The defaulted methods are callable through an override, and unknown
    // ids are harmless to record — the events that carry them resolve at
    // delivery, so a handler must never assume the id is live.
    let mut h = H {
        toggles: Vec::new(),
        tools: Vec::new(),
        pads: Vec::new(),
    };
    let iid = ShortcutsInhibitorId::dangling_nth_for_test(1);
    let tid = TabletToolId::dangling_nth_for_test(1);
    let pid = TabletPadId::dangling_nth_for_test(1);
    h.shortcuts_inhibitor_toggled(iid, true);
    h.tablet_tool_event(tid);
    h.tablet_pad_event(pid);
    assert_eq!(h.toggles, vec![(iid, true)]);
    assert_eq!(h.tools, vec![tid]);
    assert_eq!(h.pads, vec![pid]);

    let mut h2 = H2 {
        toggles: 0,
        tools: 0,
    };
    h2.shortcuts_inhibitor_toggled(iid, false);
    h2.tablet_tool_event(tid);
    assert_eq!((h2.toggles, h2.tools), (1, 1));

    // Dangling ids are distinct from each other.
    assert_ne!(iid, ShortcutsInhibitorId::dangling_nth_for_test(2));
    assert_ne!(tid, TabletToolId::dangling_nth_for_test(2));
    assert_ne!(pid, TabletPadId::dangling_nth_for_test(2));
}

#[test]
fn fresh_runtime_tracks_no_inhibitors_or_tablets() {
    use wlr::ShortcutsInhibitorId;

    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    // No inhibitor was ever announced, so nothing is inhibited.
    assert!(!rt.shortcuts_inhibited());
    assert_eq!(rt.rt_debug_tablet_tool_count(), 0);
    assert_eq!(rt.rt_debug_tablet_pad_count(), 0);
    // Null hardware pointers miss without touching anything.
    // SAFETY: null is the documented miss case for both lookups.
    unsafe {
        assert!(rt.try_tablet_tool(std::ptr::null_mut()).is_none());
        assert!(rt.try_tablet_pad(std::ptr::null_mut()).is_none());
    }
    // Driving wire state for an unknown inhibitor changes nothing.
    assert!(
        !rt.set_shortcuts_inhibitor_active(ShortcutsInhibitorId::dangling_nth_for_test(1), true)
    );
    assert!(!rt.shortcuts_inhibited());
}
