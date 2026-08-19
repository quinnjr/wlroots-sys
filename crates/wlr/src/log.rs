//! wlroots' logger.
//!
//! wlroots logs a great deal at `Debug` — backend probing, DRM mode selection,
//! renderer capability negotiation — and by default it all goes to stderr. This
//! module lets a consumer point it at their own logging crate instead, and read
//! back the verbosity wlroots is actually filtering at.
//!
//! # Two constraints from the C side
//!
//! **There is no user-data pointer.** `wlr_log_init` takes a bare function
//! pointer, so the sink is a *process-global singleton* with nowhere to hang a
//! context. It lives in a `static` here; installing a second one replaces the
//! first. That is wlroots' shape, not a simplification.
//!
//! **The callback is varargs.** wlroots hands the sink a `printf` format string
//! and a `va_list`, and Rust cannot read a `va_list` on stable. The only
//! portable answer is to hand it straight back to C, which this module does
//! through a private `vsnprintf` declaration and a fixed 4 KiB stack buffer.
//! Two-pass sizing would need `va_copy`, also unavailable, so a line longer than
//! that is **truncated** — wlroots' own default logger has no such limit, and a
//! consumer who needs every byte of a 4 KiB log line should keep the default.
//!
//! # The verbosity filter is the *default logger's*, not wlroots'
//!
//! `wlr_log_init`'s doc says "only messages less than or equal to the supplied
//! verbosity will be logged", which is true of the logger wlroots ships and
//! *not* of a callback you install: `_wlr_log` hands every line to the callback
//! and the `verbosity > log_importance` test lives inside `log_stderr`. A sink
//! installed at [`LogLevel::Silent`] therefore still sees every `Info` line —
//! verified against wlroots 0.20 by installing one and bringing up a backend.
//!
//! [`init_logging`] applies the filter in its own trampoline instead, so the
//! level means the same thing whether or not a sink is installed. The
//! alternative — passing everything through and documenting the surprise —
//! would make `LogLevel` decorative for the only consumers who pass it.
//!
//! # Emitting into wlroots' logger is not offered
//!
//! `wlr_log()` is a variadic macro over `_wlr_log()`, which is `static inline`;
//! neither survives into `bindings.rs`, and neither is something a compositor
//! needs. Log with your own crate.

use std::ffi::{c_char, c_int};
use std::panic::AssertUnwindSafe;
use std::sync::RwLock;

use crate::sys;

/// How chatty wlroots should be: `enum wlr_log_importance`.
///
/// Ordered by verbosity, so `LogLevel::Debug > LogLevel::Info` — the same
/// direction as the underlying C values, and the direction `wlr_log_init`'s
/// "only messages less than or equal to the supplied verbosity" filter uses.
///
/// `WLR_LOG_IMPORTANCE_LAST` is deliberately absent: it is an array-sizing
/// sentinel, not a level, and admitting it would let a consumer ask wlroots to
/// filter at a value wlroots itself treats as out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LogLevel {
    /// Log nothing.
    Silent,
    /// Errors only.
    Error,
    /// Errors and notable events.
    Info,
    /// Everything, including per-frame and per-probe detail.
    Debug,
}

impl LogLevel {
    /// Decode a raw `wlr_log_importance`.
    ///
    /// `None` for the `WLR_LOG_IMPORTANCE_LAST` sentinel and anything beyond
    /// it, which is also how a level added by a future wlroots patch release
    /// arrives here. Never panics.
    pub fn from_raw(value: u32) -> Option<LogLevel> {
        Some(match value {
            0 => LogLevel::Silent,
            1 => LogLevel::Error,
            2 => LogLevel::Info,
            3 => LogLevel::Debug,
            _ => return None,
        })
    }

    /// The raw `wlr_log_importance` value.
    pub fn to_raw(self) -> u32 {
        match self {
            LogLevel::Silent => 0,
            LogLevel::Error => 1,
            LogLevel::Info => 2,
            LogLevel::Debug => 3,
        }
    }

    fn to_c(self) -> sys::wlr_log_importance {
        sys::wlr_log_importance(self.to_raw())
    }
}

/// A leaked, thread-safe consumer closure.
///
/// `&'static` rather than a `Box` that could be dropped: replacing the sink
/// while another thread is inside it would be a use-after-free, and wlroots
/// gives no way to know whether one is. The old sink is leaked instead, which
/// costs one closure per [`init_logging`] call and is bounded by how often a
/// program reconfigures its logger.
type Sink = &'static (dyn Fn(LogLevel, &str) + Send + Sync);

/// The installed sink, or `None` for wlroots' own stderr logger.
///
/// Read-locked only long enough to copy the reference out — never across the
/// call, so a sink that somehow provoked a wlroots log line would drop that
/// line rather than deadlocking on a re-entrant lock.
static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// The size of the formatting buffer. A wlroots log line longer than this is
/// truncated; the longest this crate has observed is a DRM format-modifier
/// dump, well under 1 KiB.
const BUF_LEN: usize = 4096;

// `vsnprintf` is the only way to consume the `va_list` wlroots hands the sink.
// Declared here rather than pulled in with a `libc` dependency: this is one
// signature, it is C89, and the crate otherwise has no non-wlroots dependency
// at all. `size_t` is `usize` on every target this crate builds for.
unsafe extern "C" {
    fn vsnprintf(
        str_: *mut c_char,
        size: usize,
        format: *const c_char,
        ap: *mut sys::__va_list_tag,
    ) -> c_int;
}

/// Install a process-global log sink, and set wlroots' verbosity.
///
/// Passing `None` for `sink` restores wlroots' own stderr logger, which is also
/// the default. Either way `level` takes effect — for an installed sink because
/// this crate filters on its behalf, wlroots itself not doing so; see this
/// module's docs, since it is the one place where the C documentation is
/// misleading rather than merely terse.
///
/// ```no_run
/// // The `None` case needs a type for `F`, since there is no value to infer
/// // it from. Any function type will do.
/// wlr::init_logging::<fn(wlr::LogLevel, &str)>(wlr::LogLevel::Info, None);
///
/// wlr::init_logging(
///     wlr::LogLevel::Debug,
///     Some(|level, line: &str| eprintln!("[{level:?}] {line}")),
/// );
/// ```
///
/// # The sink's obligations
///
/// - **It must not call back into this crate.** wlroots logs from inside its
///   own API calls, so a sink that touched [`Runtime`](crate::Runtime) or
///   [`Display`](crate::Display) would re-enter whatever call is currently
///   running.
/// - **It may be called from any thread.** wlroots makes no promise about
///   which, hence the `Send + Sync` bound.
/// - **It should be cheap.** It runs on the path of every log line, including
///   during backend probing where there are thousands.
///
/// # Panics
///
/// Never — neither this function nor the sink. A panic escaping `sink` is
/// caught and discarded: this is a logger, and a logger that aborts the process
/// is worse than one that loses a line. (A panic *would* abort regardless, the
/// sink being called from an `extern "C"` frame, so catching it is the only
/// behaviour that leaves anything running.)
///
/// # Truncation
///
/// A formatted line longer than 4 KiB is cut short; see this module's docs for
/// why there is a fixed limit at all.
pub fn init_logging<F>(level: LogLevel, sink: Option<F>)
where
    F: Fn(LogLevel, &str) + Send + Sync + 'static,
{
    let installed: Option<Sink> = sink.map(|f| Box::leak(Box::new(f)) as Sink);

    // Install before telling wlroots about the trampoline, so there is no
    // window in which the trampoline is live with a stale sink behind it.
    // A poisoned lock means a sink panicked past `catch_unwind`, which cannot
    // happen; recover rather than turn it into an abort here.
    let mut guard = SINK.write().unwrap_or_else(|p| p.into_inner());
    *guard = installed;
    drop(guard);

    // Null does *not* restore wlroots' default logger, whatever its header
    // says: `wlr_log_init` ignores a null callback rather than storing it (see
    // `log_trampoline`'s own comment for the disassembly), so the trampoline
    // stays installed for the process's life once it has been installed once.
    // Passing null is still right — it is what keeps a first-ever `None` call
    // on wlroots' real default — and the `None`-after-`Some` case is handled
    // in the trampoline, which is the only place that can still see the line.
    let callback: sys::wlr_log_func_t = installed.map(|_| {
        log_trampoline
            as unsafe extern "C" fn(sys::wlr_log_importance, *const c_char, *mut sys::__va_list_tag)
    });
    // SAFETY: `wlr_log_init` stores the verbosity and the function pointer in
    // globals; the pointer is to a `'static` function, and passing null (the
    // `None` case) is documented as selecting wlroots' default logger.
    unsafe { sys::wlr_log_init(level.to_c(), callback) };
}

/// The verbosity wlroots is currently filtering at.
///
/// A value this crate does not recognise — a level added by a future wlroots
/// patch release — reports as [`LogLevel::Debug`], the most verbose level it
/// knows, since anything numerically higher is by construction more verbose.
pub fn log_verbosity() -> LogLevel {
    // SAFETY: reads a global; no pointers are involved.
    let raw = unsafe { sys::wlr_log_get_verbosity() };
    LogLevel::from_raw(raw.0).unwrap_or(LogLevel::Debug)
}

/// The `wlr_log_func_t` handed to wlroots: format the varargs, then call the
/// installed sink.
///
/// # Safety
///
/// Called only by wlroots, with a live NUL-terminated `fmt` and the matching
/// `va_list`.
unsafe extern "C" fn log_trampoline(
    importance: sys::wlr_log_importance,
    fmt: *const c_char,
    args: *mut sys::__va_list_tag,
) {
    let sink = {
        let guard = match SINK.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard
    };
    // A level wlroots invented and this crate does not know: dropping the line
    // is the only honest option, since there is no `LogLevel` to report it as.
    let Some(level) = LogLevel::from_raw(importance.0) else {
        return;
    };
    // wlroots does not apply the verbosity filter before calling a custom
    // callback — see this module's docs — so it is applied here, before the
    // formatting work, which is also where it is cheapest.
    if level > log_verbosity() {
        return;
    }
    if fmt.is_null() {
        return;
    }

    let mut buf = [0u8; BUF_LEN];
    // SAFETY: `buf` is a live, exclusively-owned local of exactly `BUF_LEN`
    // bytes; `fmt` and `args` are wlroots' own and valid for this call.
    // `vsnprintf` consumes `args` once, which is why nothing else reads it.
    let written = unsafe { vsnprintf(buf.as_mut_ptr().cast::<c_char>(), buf.len(), fmt, args) };
    if written < 0 {
        return;
    }
    // The return is the length the line *would* have had, so it can exceed the
    // buffer; clamp to what was actually written, short of the NUL.
    let len = (written as usize).min(BUF_LEN - 1);
    let line = String::from_utf8_lossy(&buf[..len]);

    let Some(sink) = sink else {
        // No sink installed, but the trampoline is still wlroots' callback and
        // always will be — `wlr_log_init` *ignores* a null callback rather
        // than clearing it. Disassembling the shipped `libwlroots-0.20.so`
        // shows `test %rdx,%rdx ; je` skipping the store to `log_callback`,
        // and `_wlr_vlog` is an unconditional `jmp *log_callback`, so the
        // pointer can only ever be overwritten, never cleared — and wlroots
        // exports no default-logger symbol to overwrite it back with.
        //
        // So returning here dropped every wlroots line for the rest of the
        // process: no backend probe failures, no DRM errors, no
        // `wlr_log_errno`, silently, from a call documented as *restoring* the
        // default logger. Writing the line is the only way to keep that
        // promise, so this is that default: this crate's, not wlroots' own,
        // which is unreachable once overwritten. The format is deliberately
        // close but not claimed to be identical.
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("[wlr] [{level:?}] {line}\n"),
        );
        return;
    };

    // wlroots runs this under an `extern "C"` frame, where an unwind aborts.
    // `AssertUnwindSafe` is honest here: the only state the sink could leave
    // torn is the consumer's own, and a logger is not worth a process for.
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| sink(level, &line)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four levels are C ABI. Pin them against wlroots' own constants, and
    /// pin that the sentinel is *not* one of them.
    #[test]
    fn log_level_values_match_the_wlroots_header() {
        for (ours, theirs) in [
            (LogLevel::Silent, sys::wlr_log_importance::WLR_SILENT),
            (LogLevel::Error, sys::wlr_log_importance::WLR_ERROR),
            (LogLevel::Info, sys::wlr_log_importance::WLR_INFO),
            (LogLevel::Debug, sys::wlr_log_importance::WLR_DEBUG),
        ] {
            assert_eq!(ours.to_raw(), theirs.0, "{ours:?}");
            assert_eq!(LogLevel::from_raw(theirs.0), Some(ours));
        }

        let last = sys::wlr_log_importance::WLR_LOG_IMPORTANCE_LAST;
        assert_eq!(
            LogLevel::from_raw(last.0),
            None,
            "the array-sizing sentinel is not a level"
        );
    }

    /// Verbosity is a total order on how much is logged, which is what makes
    /// `<=` the right way to ask "would this be logged?".
    #[test]
    fn log_levels_order_by_verbosity() {
        assert!(LogLevel::Silent < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
    }
}
