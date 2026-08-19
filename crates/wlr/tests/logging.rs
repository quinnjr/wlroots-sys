//! wlroots' logger, driven by real wlroots log traffic.
//!
//! The sink and the verbosity are **process-global** — `wlr_log_init` has no
//! user-data parameter — so every test here takes `sink_lock()` and restores
//! the default logger before it returns. Two tests running at once would each
//! see the other's lines and neither would mean anything.
//!
//! What is actually under test is the `va_list` handoff. wlroots hands the sink
//! a `printf` format string and a `va_list`, which Rust cannot read; the crate
//! passes both straight back to C's `vsnprintf`. If that path were wrong — a
//! bad signature, the wrong ABI for `__va_list_tag` — the visible symptom would
//! be either a crash or lines still containing their `%s` and `%d`, and both
//! are asserted against below.

use std::sync::{Arc, Mutex, MutexGuard};

/// Serialises the process-global sink. Poison-tolerant: the guarded data is
/// `()`, and a panicking test should not take the rest of the file down with
/// it.
fn sink_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn headless_env() {
    // SAFETY: every test in this binary takes `sink_lock()` first, so no other
    // harness thread can observe a torn environment read.
    unsafe {
        std::env::set_var("WLR_BACKENDS", "headless");
        std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
    }
}

/// Restore wlroots' own stderr logger, so a later test in this binary is not
/// still feeding a sink whose captured `Vec` has gone out of scope.
fn restore_default_logger() {
    wlr::init_logging::<fn(wlr::LogLevel, &str)>(wlr::LogLevel::Error, None);
}

type Captured = Arc<Mutex<Vec<(wlr::LogLevel, String)>>>;

fn capture_at(level: wlr::LogLevel) -> Captured {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    wlr::init_logging(
        level,
        Some(move |level, line: &str| {
            // The sink runs under an `extern "C"` frame; a poisoned lock here
            // must not become a panic that the crate then has to swallow.
            let mut lines = sink.lock().unwrap_or_else(|p| p.into_inner());
            lines.push((level, line.to_owned()));
        }),
    );
    captured
}

#[test]
fn an_installed_sink_receives_wlroots_log_lines_with_their_arguments_substituted() {
    let _serialised = sink_lock();
    headless_env();

    let captured = capture_at(wlr::LogLevel::Debug);
    assert_eq!(
        wlr::log_verbosity(),
        wlr::LogLevel::Debug,
        "init_logging sets the verbosity wlroots filters at"
    );

    {
        let display = wlr::Display::new().expect("display");
        let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
        let runtime = wlr::Runtime::new().expect("runtime");
        runtime
            .init_graphics(&display, &backend)
            .expect("renderer, allocator and core globals");
    }

    restore_default_logger();
    let lines = captured.lock().unwrap_or_else(|p| p.into_inner()).clone();

    assert!(
        !lines.is_empty(),
        "bringing up a backend and a renderer logs something"
    );

    // Every wlroots log line goes through the `wlr_log` macro, which prepends
    // `"[%s:%d] "` and passes `__FILE__` and `__LINE__` as the first two
    // varargs. A line that arrived as `[render/egl.c:205] …` is therefore
    // direct evidence that both a `%s` and a `%d` were substituted by
    // `vsnprintf` — which is the whole of what this crate had to get right.
    let substituted = lines.iter().any(|(_, line)| {
        let Some(prefix) = line.strip_prefix('[').and_then(|l| l.split(']').next()) else {
            return false;
        };
        let Some((file, line_number)) = prefix.rsplit_once(':') else {
            return false;
        };
        file.ends_with(".c")
            && !line_number.is_empty()
            && line_number.chars().all(|c| c.is_ascii_digit())
    });
    assert!(
        substituted,
        "no line carried a substituted [file.c:line] prefix; got {:?}",
        lines.iter().take(3).collect::<Vec<_>>()
    );

    // The complement: nothing arrived as a raw format string.
    for (_, line) in &lines {
        for spec in ["%s", "%d", "%u", "%p", "%zu", "%08"] {
            assert!(
                !line.contains(spec),
                "an unsubstituted {spec} means vsnprintf did not run: {line}"
            );
        }
    }

    // `Debug` admits every level, and wlroots logs at more than one of them
    // during bring-up.
    let mut levels: Vec<wlr::LogLevel> = lines.iter().map(|(level, _)| *level).collect();
    levels.sort();
    levels.dedup();
    assert!(
        levels.len() > 1,
        "bring-up logs at more than one level: {levels:?}"
    );
    // This test requested `Debug`, which admits everything, so "nothing above
    // the verbosity arrived" is not a claim it can make — comparing against
    // `LogLevel::Debug` held for every possible value, and deleting the
    // trampoline's filter outright left it passing.
    //
    // Nor can it read `log_verbosity()` and compare against that: wlroots'
    // verbosity is process-global, these tests share a process, and another
    // test lowering it mid-run made exactly that version fail with
    // "verbosity (Error) ... got [Info, Debug]".
    //
    // So the filter is tested where it can actually be tested — by
    // `silent_filters_everything_and_the_verbosity_is_readable_back`, which
    // takes `sink_lock()` and so owns the verbosity for its own duration. What is asserted here is the thing
    // this test does establish: `Debug` really does admit the quieter levels,
    // so a filter that wrongly dropped them would show up as a missing level.
    assert!(
        levels.contains(&wlr::LogLevel::Error) || levels.contains(&wlr::LogLevel::Info),
        "Debug must admit the quieter levels too, not only Debug itself: {levels:?}"
    );
}

/// wlroots applies its verbosity filter inside its *own* stderr logger, not
/// before the callback, so an installed sink would see `Info` lines even at
/// `Silent`. The crate filters in its trampoline instead — this is the test
/// that says so, and the one that would start failing if that were dropped as
/// redundant.
#[test]
fn silent_filters_everything_and_the_verbosity_is_readable_back() {
    let _serialised = sink_lock();
    headless_env();

    let captured = capture_at(wlr::LogLevel::Silent);
    assert_eq!(wlr::log_verbosity(), wlr::LogLevel::Silent);

    {
        let display = wlr::Display::new().expect("display");
        let _backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    }

    restore_default_logger();
    assert_eq!(wlr::log_verbosity(), wlr::LogLevel::Error);

    let lines = captured.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(
        lines.is_empty(),
        "the crate's trampoline drops everything above the verbosity — wlroots \
         hands a custom callback every line regardless, so nothing but this \
         filter is between the sink and them: {lines:?}"
    );
}

/// A logger that aborts the process is worse than one that loses a line, so the
/// crate catches a panic escaping the sink and discards it. Nothing else in
/// this crate does that — a panicking *handler* is meant to abort — which is
/// why it is worth a test of its own.
///
/// The test's evidence is simply that it returns: a panic reaching the
/// `extern "C"` frame wlroots calls through would abort the whole harness, and
/// no `#[should_panic]` or `catch_unwind` here could observe it.
#[test]
fn a_panicking_sink_does_not_take_the_process_down() {
    let _serialised = sink_lock();
    headless_env();

    let calls = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&calls);
    wlr::init_logging(
        wlr::LogLevel::Debug,
        Some(move |_level, _line: &str| {
            *counter.lock().unwrap_or_else(|p| p.into_inner()) += 1;
            panic!("a sink that panics on every line");
        }),
    );

    {
        let display = wlr::Display::new().expect("display");
        let _backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    }

    restore_default_logger();

    let calls = *calls.lock().unwrap_or_else(|p| p.into_inner());
    assert!(
        calls > 1,
        "the sink was called repeatedly and kept being called after it panicked, \
         rather than the first panic tearing the logger down: {calls}"
    );
}
