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
