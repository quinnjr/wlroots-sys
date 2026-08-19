//! Workspace maintenance commands.
//!
//! `cargo xtask coverage` prints the safe crate's burn-down against the
//! generated wlroots bindings. It shares one implementation with the CI gate in
//! `crates/wlr/tests/coverage_audit.rs` — both include
//! `crates/wlr/src/coverage.rs` by path — so the human-facing percentage and the
//! pass/fail decision can never drift apart.
//!
//! The report reads whichever `bindings.rs` is newest under `target/`, or the
//! file `WLR_COVERAGE_BINDINGS` names. Build `wlr-sys --all-features` first if
//! you want the number the spec defines 100% against.

#[path = "../../wlr/src/coverage.rs"]
mod coverage;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match parse(std::env::args().skip(1)) {
        Ok(Command::Coverage { check }) => coverage_report(check),
        Err(err) => {
            eprintln!("{err}");
            eprintln!("usage: cargo xtask coverage [--check]");
            ExitCode::from(2)
        }
    }
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Coverage { check: bool },
}

/// Parse the command line, rejecting anything not understood.
///
/// Rejecting is the whole point. `--check` used to be recognised by comparing
/// one argument for equality and ignoring everything else, so
/// `cargo xtask coverage --chek` — or `--check` with a stray extra argument —
/// quietly became the *reporting* mode and exited 0 over a dirty ledger. A
/// gate that answers "fine" when it was never asked to check is worse than no
/// gate, because the green tick is what people read.
fn parse(args: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut args = args;
    let Some(cmd) = args.next() else {
        return Err("no command given".to_string());
    };
    if cmd != "coverage" {
        return Err(format!("unknown command `{cmd}`"));
    }
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" if !check => check = true,
            "--check" => return Err("`--check` given twice".to_string()),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Command::Coverage { check })
}

fn coverage_report(check: bool) -> ExitCode {
    let root = match coverage::workspace_root(Path::new(env!("CARGO_MANIFEST_DIR"))) {
        Ok(root) => root,
        Err(err) => return fail(&err),
    };
    let wlr = root.join("crates/wlr");

    let bindings = match coverage::find_bindings(&root) {
        Ok(path) => path,
        Err(err) => return fail(&err),
    };
    let symbols = match read(&bindings) {
        Ok(src) => coverage::extract(&src),
        Err(err) => return fail(&err),
    };

    let wrapped = match read(&wlr.join("coverage/wrapped.toml"))
        .and_then(|src| coverage::parse_wrapped(&src).map_err(|e| format!("wrapped.toml: {e}")))
    {
        Ok(entries) => entries,
        Err(err) => return fail(&err),
    };
    let waived = match read(&wlr.join("coverage/waived.toml"))
        .and_then(|src| coverage::parse_waived(&src).map_err(|e| format!("waived.toml: {e}")))
    {
        Ok(entries) => entries,
        Err(err) => return fail(&err),
    };

    let sys_uses = match coverage::scan_sys_uses(&wlr.join("src")) {
        Ok(uses) => uses,
        Err(err) => return fail(&format!("scanning crates/wlr/src: {err}")),
    };

    let mut report = coverage::reconcile(&symbols, &wrapped, &waived, &sys_uses);
    println!("ground truth: {}", bindings.display());
    println!();
    print!("{}", report.render_table());

    // A ledger row absent from *these* bindings is either stale or merely
    // feature-gated, and only the all-features build can tell the two apart —
    // exactly the distinction `coverage_audit.rs` draws. Demote it to a note
    // here for the same reason, so a report run after a default build does not
    // fail `--check` over vulkan rows.
    if std::env::var_os("WLR_COVERAGE_ALL_FEATURES").is_none() && !report.unknown_symbol.is_empty()
    {
        println!(
            "\nnote: {} ledger row(s) name a symbol these bindings do not contain. \
             Build `wlr-sys --all-features` and set WLR_COVERAGE_ALL_FEATURES=1 to \
             tell a gated row from a stale one:",
            report.unknown_symbol.len()
        );
        for name in &report.unknown_symbol {
            println!("  {name}");
        }
        report.unknown_symbol.clear();
    }

    if !report.is_clean() {
        println!();
        print!("{}", report.render_failures());
        if check {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("xtask: {message}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};

    fn go(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn coverage_defaults_to_reporting() {
        assert_eq!(go(&["coverage"]), Ok(Command::Coverage { check: false }));
    }

    #[test]
    fn check_is_recognised() {
        assert_eq!(
            go(&["coverage", "--check"]),
            Ok(Command::Coverage { check: true })
        );
    }

    /// The bug this parser exists for: a typo used to leave `check` false and
    /// exit 0, so CI reported a clean ledger it had never inspected.
    #[test]
    fn a_typo_is_rejected_rather_than_silently_reporting() {
        assert!(go(&["coverage", "--chek"]).is_err());
        assert!(go(&["coverage", "-check"]).is_err());
        assert!(go(&["coverage", "check"]).is_err());
    }

    #[test]
    fn a_stray_extra_argument_is_rejected() {
        assert!(go(&["coverage", "--check", "extra"]).is_err());
        assert!(go(&["coverage", "extra"]).is_err());
    }

    #[test]
    fn an_unknown_or_missing_command_is_rejected() {
        assert!(go(&["covrage"]).is_err());
        assert!(go(&[]).is_err());
    }
}
