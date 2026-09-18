//! xdg-decoration negotiation, against a real headless compositor with no
//! client.
//!
//! Same shape as `toplevels.rs`'s test file: what is provable without a
//! client library is that the decoration manager global can be created, that
//! the id-keyed mutator rejects an id that was never issued rather than
//! dereferencing it, and that the new handler method is additive.

mod common;

#[test]
fn decoration_manager_creates_once_on_a_display() {
    let _serial = common::headless_guard();
    common::headless_env();

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    // `create_xdg_shell` requires graphics to exist first (see its own doc);
    // `toplevels.rs`'s test carries the identical prerequisite.
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("shell");
    runtime
        .create_xdg_decoration_manager(&display)
        .expect("decoration manager");
}

#[test]
fn set_decoration_mode_on_a_dead_id_is_none() {
    common::headless_env();
    let runtime = wlr::Runtime::new().expect("runtime");
    assert_eq!(
        runtime.set_decoration_mode(
            wlr::ToplevelId::dangling_for_test(),
            wlr::DecorationMode::ServerSide
        ),
        None
    );
}

/// The polarity trap 0.20.8 shipped, pinned shut by the type system.
///
/// In 0.20.8 both sides of the negotiation were `bool` with *opposite*
/// meanings — the handler's `true` meant client-side, the mutator's `true`
/// meant server-side — so the natural "honour whatever the client asked
/// for" body passed the value straight through and did the exact opposite.
/// It compiled silently; that is why it survived review and reached
/// crates.io.
///
/// Now both sides speak [`wlr::DecorationMode`], so pass-through *is* the
/// honouring implementation and the inverted one cannot be written by
/// accident. Compiling is necessary but not sufficient evidence of that,
/// though — a body that compiles but is never actually run proves nothing
/// about which way values flow through it, which is exactly the gap the
/// 0.20.8 bug lived in (it compiled too). So this test also *calls*
/// `request_decoration_mode` directly, bypassing FFI the same way
/// `runtime.rs`'s own staging tests do for `set_decoration_mode`, and
/// asserts on what it captures — not on the unrelated fact that the two
/// variants compare unequal.
///
/// `set_decoration_mode` itself is not called here: it stages onto a real
/// `wlr_xdg_toplevel_decoration_v1`/`wlr_xdg_toplevel` pair (see
/// `runtime.rs`'s `set_decoration_mode_stages_rather_than_sends_before_...`
/// tests), and fabricating one well enough to survive that call without a
/// live client is exactly the FFI risk this test is trying to avoid. So
/// `App` records the mode it *would* forward to the mutator instead of
/// calling the mutator — enough to prove the forwarding is unmodified
/// pass-through, without needing FFI-shaped fixtures to prove it.
#[test]
fn honouring_the_client_is_a_pass_through() {
    use wlr::ToplevelHandler as _;

    struct App {
        forwarded: std::cell::Cell<Option<wlr::DecorationMode>>,
    }
    impl wlr::ToplevelHandler for App {
        fn request_decoration_mode(
            &mut self,
            _id: wlr::ToplevelId,
            preference: Option<wlr::DecorationMode>,
        ) {
            // No negation, no mapping table, no remembering which way a
            // bool points — the value the client stated is the value this
            // records as what would be forwarded to `set_decoration_mode`.
            self.forwarded
                .set(Some(preference.unwrap_or(wlr::DecorationMode::ServerSide)));
        }
    }

    let mut app = App {
        forwarded: std::cell::Cell::new(None),
    };
    app.request_decoration_mode(
        wlr::ToplevelId::dangling_for_test(),
        Some(wlr::DecorationMode::ClientSide),
    );

    // The inverted 0.20.8 body would have recorded `ServerSide` here — this
    // is the assertion that fails on that regression, not `assert_ne!` on
    // the two variants (which holds regardless of what the handler does).
    assert_eq!(
        app.forwarded.get(),
        Some(wlr::DecorationMode::ClientSide),
        "the client's stated preference must reach the mutator unmodified"
    );
}

#[test]
fn the_new_handler_method_is_additive() {
    struct Old;
    impl wlr::OutputHandler for Old {}
    impl wlr::ToplevelHandler for Old {}
    impl wlr::SeatHandler for Old {}
    impl wlr::FdHandler for Old {}
    impl wlr::LoopHandler for Old {}
    fn takes_handlers<S: wlr::Handlers>(_s: &S) {}
    takes_handlers(&Old);
}

/// A real client states client-side chrome, the compositor honors it, and
/// `decoration_state` reads back exactly the mode `set_decoration_mode`
/// sent — the full negotiation round trip, not just the decoding table
/// `decoration.rs`'s unit tests pin.
#[test]
fn decoration_state_returns_the_mode_set_via_set_decoration_mode() {
    let _serial = common::headless_guard();
    common::headless_env();
    common::isolated_runtime_dir();

    struct App {
        runtime: wlr::Runtime,
        id: Option<wlr::ToplevelId>,
        preference: Option<Option<wlr::DecorationMode>>,
        /// `decoration_state` sampled on every surface commit while the
        /// decoration is live. Sampled in-run rather than queried after the
        /// run: the client's disconnect destroys the decoration (purging its
        /// entry) and the server may dispatch that hangup before `run_all`
        /// returns, so a post-run query is inherently racy where these
        /// samples are not.
        state_samples: Vec<Option<wlr::DecorationMode>>,
        /// `decoration_configure` sampled alongside, one entry per commit in
        /// the same order: the generic commit listener runs before the role
        /// listener's flush, so every commit sample sees a settled queue —
        /// empty on the initial commit (the answer is staged, not yet
        /// queued) and empty again once the client acked and committed. The
        /// queued window in between is sampled from `should_stop` below,
        /// which runs every turn.
        configure_samples: Vec<Option<wlr::DecorationMode>>,
        /// Whether any between-turn `should_stop` observed the queued
        /// configure: after the initial-commit flush and before the client's
        /// ack+commit drains it, the queue holds the answered mode for whole
        /// turns, so at least one `should_stop` must see it.
        saw_queued: bool,
        client: Option<std::thread::JoinHandle<common::client::ClientEvents>>,
    }
    impl wlr::OutputHandler for App {}
    impl wlr::ToplevelHandler for App {
        fn request_decoration_mode(
            &mut self,
            id: wlr::ToplevelId,
            preference: Option<wlr::DecorationMode>,
        ) {
            self.id = Some(id);
            self.preference = Some(preference);
            // Honour the client: the value stated is the value answered.
            self.runtime
                .set_decoration_mode(id, preference.unwrap_or(wlr::DecorationMode::ServerSide));
        }

        fn surface_committed(&mut self, _surface: &wlr::Surface<'_>) {
            if let Some(id) = self.id {
                self.state_samples.push(self.runtime.decoration_state(id));
                self.configure_samples
                    .push(self.runtime.decoration_configure(id));
            }
        }
    }
    impl wlr::SeatHandler for App {}
    impl wlr::FdHandler for App {}
    impl wlr::LoopHandler for App {
        fn should_stop(&mut self) -> bool {
            // Between-turn sample of the queued window: the flush went out
            // during the initial commit's turn and wlroots pops the queue
            // only once the client's ack+commit is processed, so whole turns
            // in between must observe the queued mode here.
            if let Some(id) = self.id
                && self.runtime.decoration_configure(id) == Some(wlr::DecorationMode::ClientSide)
            {
                self.saw_queued = true;
            }
            self.client.as_ref().is_some_and(|h| h.is_finished())
        }
    }

    let display = wlr::Display::new().expect("display");
    let backend = wlr::Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = wlr::Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");
    runtime.create_xdg_shell(&display, 6).expect("shell");
    runtime
        .create_xdg_decoration_manager(&display)
        .expect("decoration manager");
    let socket = display.add_socket_auto().expect("socket");

    let mut app = App {
        runtime: runtime.clone(),
        id: None,
        preference: None,
        state_samples: Vec::new(),
        configure_samples: Vec::new(),
        saw_queued: false,
        client: Some(common::client::spawn_decoration_client(&socket)),
    };
    backend
        .run_all(&display, &mut app, &runtime, wlr::Until::Stop)
        .expect("run_all");
    let events = app
        .client
        .take()
        .expect("client handle")
        .join()
        .expect("client thread");

    assert!(
        events.configure_events >= 1 && events.acked_configures >= 1,
        "the client's xdg configure arrived and was acked"
    );
    assert_eq!(
        app.preference,
        Some(Some(wlr::DecorationMode::ClientSide)),
        "the client's stated preference reached the handler"
    );
    assert!(
        events.decoration_configures >= 1,
        "the server's answering decoration configure reached the client"
    );
    app.id.expect("a decoration request was announced");
    assert!(
        app.state_samples.len() >= 2,
        "both client commits were observed: {:?}",
        app.state_samples
    );
    assert_eq!(
        app.state_samples.last(),
        Some(&Some(wlr::DecorationMode::ClientSide)),
        "decoration_state returns the mode set via set_decoration_mode once the client acked"
    );
    // `decoration_configure` is live, not just decodable: a between-turn
    // sample sees the queued mode after the initial-commit flush and before
    // the client's ack+commit drains it.
    assert!(
        app.saw_queued,
        "decoration_configure reports the queued mode while the client has yet to ack it"
    );
    // The drained-queue `None` lands on a live id — `decoration_state` on the
    // same commit is `Some` — so this pins the empty-queue arm, distinct from
    // the ghost-id arm `set_decoration_mode_on_a_dead_id_is_none` covers.
    assert_eq!(
        app.configure_samples.last(),
        Some(&None),
        "decoration_configure is None once the client acked and committed"
    );
}
