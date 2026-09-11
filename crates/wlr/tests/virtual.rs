fn headless_env() {
    // SAFETY: libtest runs in parallel but all tests here set identical values.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

#[test]
fn virtual_devices_and_transient_seats_miss_cleanly() {
    use wlr::{TransientSeatAnswer, TransientSeatId, VirtualKeyboardId, VirtualPointerId};

    headless_env();
    let rt = wlr::Runtime::new().unwrap();
    // Nothing was ever announced, so every table rests empty.
    assert_eq!(rt.rt_debug_virtual_keyboard_count(), 0);
    assert_eq!(rt.rt_debug_virtual_pointer_count(), 0);
    assert_eq!(rt.rt_debug_transient_seat_count(), 0);
    // Null pointers miss without touching anything.
    // SAFETY: null is the documented miss case for every lookup.
    unsafe {
        assert!(rt.try_virtual_keyboard(std::ptr::null_mut()).is_none());
        assert!(rt.try_virtual_pointer(std::ptr::null_mut()).is_none());
        assert!(
            rt.try_virtual_keyboard_from_resource(std::ptr::null_mut())
                .is_none()
        );
        assert!(rt.try_transient_seat(std::ptr::null_mut()).is_none());
    }
    // Answering or destroying an unknown transient seat changes nothing.
    // An unknown id is fatal (`Unknown`), even with no seat: the miss is
    // about the id, not the seat.
    assert_eq!(
        rt.ready_transient_seat(TransientSeatId::dangling_nth_for_test(1)),
        TransientSeatAnswer::Unknown,
        "no request is pending, so ready must miss as unknown"
    );
    assert!(
        !rt.destroy_transient_seat(TransientSeatId::dangling_nth_for_test(1)),
        "nothing was ever requested, so destroy must miss"
    );
    assert!(!rt.destroy_transient_seat(TransientSeatId::dangling_nth_for_test(2)));

    // Dangling ids are distinct from each other.
    assert_ne!(
        VirtualKeyboardId::dangling_nth_for_test(1),
        VirtualKeyboardId::dangling_nth_for_test(2)
    );
    assert_ne!(
        VirtualPointerId::dangling_nth_for_test(1),
        VirtualPointerId::dangling_nth_for_test(2)
    );
    assert_ne!(
        TransientSeatId::dangling_nth_for_test(1),
        TransientSeatId::dangling_nth_for_test(2)
    );
}

#[test]
fn virtual_and_transient_handlers_compile() {
    use wlr::{SeatHandler, TransientSeatId, VirtualKeyboardId, VirtualPointerId};

    struct H {
        keyboards: Vec<VirtualKeyboardId>,
        pointers: Vec<VirtualPointerId>,
        seats: Vec<TransientSeatId>,
    }
    impl SeatHandler for H {
        fn virtual_keyboard_created(&mut self, id: VirtualKeyboardId) {
            self.keyboards.push(id);
        }
        fn virtual_pointer_created(&mut self, id: VirtualPointerId) {
            self.pointers.push(id);
        }
        fn transient_seat_requested(&mut self, id: TransientSeatId) {
            self.seats.push(id);
        }
    }

    // The defaulted methods are callable through an override, and unknown
    // ids are harmless to record — the events that carry them resolve at
    // delivery, so a handler must never assume the id is live.
    let mut h = H {
        keyboards: Vec::new(),
        pointers: Vec::new(),
        seats: Vec::new(),
    };
    let kid = VirtualKeyboardId::dangling_nth_for_test(1);
    let pid = VirtualPointerId::dangling_nth_for_test(1);
    let sid = TransientSeatId::dangling_nth_for_test(1);
    h.virtual_keyboard_created(kid);
    h.virtual_pointer_created(pid);
    h.transient_seat_requested(sid);
    assert_eq!(h.keyboards, vec![kid]);
    assert_eq!(h.pointers, vec![pid]);
    assert_eq!(h.seats, vec![sid]);
}
