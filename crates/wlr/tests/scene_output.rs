//! Scene outputs against a real headless wlroots: the viewport a scene renders
//! an output through, its damage ring, its timer, and what happens to an id
//! once the thing it names is gone.
//!
//! Assertions live outside the handlers, as everywhere else in this crate: a
//! failing assert under an `extern "C"` frame aborts rather than fails. What
//! makes that easy here is that a [`SceneOutputId`](wlr::SceneOutputId) is
//! **not** scoped to the run that produced it — the scene owns the scene
//! output, and the destroy listener that keeps the id honest belongs to the
//! runtime — so the interesting calls can be made after `run_all` returns.

use std::sync::Once;
use std::time::Duration;

use wlr::{
    Backend, Box2D, BufferId, Display, Error, LayerSurfaceId, NodeId, Output, OutputId, Runtime,
    SceneOutputId, SceneOutputStateOptions, SceneTimer, Until,
};

fn headless_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: single-threaded, before any other thread exists, and each
        // integration binary is its own process.
        unsafe {
            std::env::set_var("WLR_BACKENDS", "headless");
            std::env::set_var("WLR_HEADLESS_OUTPUTS", "1");
            std::env::set_var("WLR_RENDERER", "pixman");
        }
    });
}

/// Brings one headless output up and records what the scene-output API said
/// while it was the only thing running.
#[derive(Default)]
struct App {
    runtime: Option<Runtime>,
    outputs: Vec<OutputId>,
    scene_outputs: Vec<SceneOutputId>,
    /// What `add_scene_output` answered for an output that was already added.
    second_add: Option<Option<SceneOutputId>>,
    init_errors: Vec<Error>,
    commits: u32,
    turns: u32,
}

impl wlr::ToplevelHandler for App {}
impl wlr::SeatHandler for App {}
impl wlr::FdHandler for App {}

impl wlr::LoopHandler for App {
    fn should_stop(&mut self) -> bool {
        self.turns += 1;
        // A second bound under the `Until::Turns` one, not the only bound.
        // `should_stop` runs *between* dispatch turns, so it can only end a
        // run that keeps getting events — under `Until::Stop` a headless
        // output that stops producing frames leaves the loop blocked in
        // `poll` forever and this is never consulted. `Until::Turns` is what
        // actually makes this test hang-proof.
        self.turns >= 8
    }
}

impl wlr::OutputHandler for App {
    fn new_output(&mut self, output: &Output<'_>) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        self.outputs.push(output.id());
        if let Err(e) = output.enable_with_preferred_mode() {
            self.init_errors.push(e);
            return;
        }
        if let Err(e) = runtime.init_output(output) {
            self.init_errors.push(e);
            return;
        }
        if let Some(scene_output) = runtime.scene_output(output.id()) {
            self.scene_outputs.push(scene_output);
        }
        // `init_output` already added this output to the scene, and wlroots
        // asserts on a second add — so this must answer `None` rather than
        // aborting the test process.
        self.second_add = Some(runtime.add_scene_output(output.id()));
        output.schedule_frame();
    }

    fn frame(&mut self, output: &Output<'_>) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        if runtime.commit_output(output).is_ok() {
            self.commits += 1;
        }
        // Ask for the next one. `Until::Stop` blocks in `poll` between
        // events, and a headless output's frames are timer-driven, so a run
        // that stops asking stops receiving — and `should_stop`, which only
        // runs between turns, would never be consulted again. Re-arming here
        // is both what a real compositor does and what lets this run end.
        output.schedule_frame();
    }
}

/// The whole non-destructive surface, in one bring-up: the id exists, the
/// viewport reads back, a commit skips when nothing changed and renders when
/// something did, the damage ring is reachable, and a timer plugged into the
/// options survives a real commit.
#[test]
fn a_scene_output_answers_for_its_viewport_its_damage_and_its_timing() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    // Something for the scene to actually draw, so `for_each_buffer` has a
    // buffer node to visit. A rect is not a buffer node; a pixel buffer is.
    let pixels = vec![0xffu8; 32 * 32 * 4];
    let buffer = runtime.add_buffer(32, 32, &pixels).expect("pixel buffer");
    runtime
        .lower_buffer_to_bottom(buffer)
        .expect("known buffer");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Stop)
        .expect("run_all");

    assert_eq!(app.init_errors, Vec::new(), "bring-up must not fail");
    assert_eq!(app.outputs.len(), 1, "one headless output was announced");
    assert_eq!(
        app.scene_outputs.len(),
        1,
        "`init_output` puts the output in the scene, so it has a scene output"
    );
    assert_eq!(
        app.second_add,
        Some(None),
        "adding an output the scene already has must miss, not abort"
    );
    assert!(app.commits >= 1, "the output produced frames");

    // The id outlives the run that produced it — unlike an `OutputId`, whose
    // table the run clears on the way out.
    let scene_output = app.scene_outputs[0];
    let stale_output = app.outputs[0];
    assert_eq!(
        runtime.scene_output(stale_output),
        None,
        "an OutputId is only good for the run that announced it"
    );

    // The viewport, set and read back.
    assert_eq!(runtime.scene_output_position(scene_output), Some((0, 0)));
    runtime
        .set_scene_output_position(scene_output, 40, 25)
        .expect("known scene output");
    assert_eq!(runtime.scene_output_position(scene_output), Some((40, 25)));
    let seen = runtime
        .with_scene_output(scene_output, |so| (so.id(), so.position()))
        .expect("known scene output");
    assert_eq!(seen, (scene_output, (40, 25)));
    runtime
        .set_scene_output_position(scene_output, 0, 0)
        .expect("known scene output");

    // A commit with nothing new to draw reports the skip rather than an error.
    // Committing twice in a row guarantees the second one has nothing left.
    let _ = runtime.commit_scene_output(scene_output, &SceneOutputStateOptions::new());
    assert_eq!(
        runtime.scene_output_needs_frame(scene_output),
        Some(false),
        "nothing has changed since that commit"
    );
    assert_eq!(
        runtime.commit_scene_output(scene_output, &SceneOutputStateOptions::new()),
        Ok(false),
        "a skipped commit is not a failure"
    );

    // Damaging through the scene output's own ring is one of the two ways to
    // make it need a frame again; moving a node is the other.
    runtime
        .with_scene_output(scene_output, |so| {
            so.damage_ring().add_box(Box2D::new(0, 0, 8, 8));
        })
        .expect("known scene output");
    // The ring is what the scene consults when it builds a commit, but
    // `needs_frame` is driven by the scene's own damage bookkeeping, so move a
    // node too and check the pair of them together.
    runtime.set_buffer_position(buffer, 3, 3).expect("buffer");
    assert_eq!(runtime.scene_output_needs_frame(scene_output), Some(true));

    let timer = SceneTimer::new();
    let options = SceneOutputStateOptions::new().timer(&timer);
    assert_eq!(
        runtime.commit_scene_output(scene_output, &options),
        Ok(true),
        "a commit with damage renders"
    );
    // Never a panic, whatever the renderer supports: the pixman renderer has no
    // render timer, so this is the pre-render duration alone.
    match timer.duration_ns() {
        None => {}
        Some(ns) => assert!(ns >= 0, "a duration is never negative: {ns}"),
    }
    assert!(timer.pre_render_duration_ns() >= 0);

    // The commit built a render list, which is what `for_each_buffer` walks.
    let mut visited: Vec<(NodeId, i32, i32)> = Vec::new();
    runtime
        .scene_output_for_each_buffer(scene_output, |node, x, y| visited.push((node, x, y)))
        .expect("known scene output");
    assert!(
        !visited.is_empty(),
        "the pixel buffer is on screen, so the output's walk must see it"
    );

    // Frame-done is a separate call from the commit at this level.
    runtime
        .send_scene_output_frame_done(scene_output, Duration::from_secs(1))
        .expect("known scene output");
}

/// Destroying a scene output leaves its id naming nothing, on every call, with
/// no crash — the destroy listener is what removes the row.
#[test]
fn a_destroyed_scene_output_misses_on_every_call() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    let scene_output = *app.scene_outputs.first().expect("a scene output");

    assert_eq!(runtime.destroy_scene_output(scene_output), Some(()));
    assert_miss(&runtime, scene_output);

    // A second destroy misses rather than double-freeing.
    assert_eq!(runtime.destroy_scene_output(scene_output), None);
}

/// A borrow refuses the destroy that would pull the ground out from under it.
///
/// The hazard is not what `SceneOutput`'s own methods can do — they only read
/// fields and damage the ring. It is that the closure captures the `Runtime`
/// it was called on, so it can call `destroy_scene_output` directly and then
/// keep using the handle it was just given. Without the borrow guard that is
/// a use-after-free reachable with no `unsafe` at the call site.
///
/// The second half is the part that discriminates: asserting only that the
/// destroy returned `None` would also pass if the destroy had run and merely
/// reported failure. Using the handle afterwards, and finding the scene output
/// still alive when the borrow ends, is what shows nothing was freed.
#[test]
fn a_live_borrow_refuses_a_destroy_of_the_scene_output_it_names() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    let scene_output = *app.scene_outputs.first().expect("a scene output");

    let refused = runtime
        .with_scene_output(scene_output, |so| {
            let destroyed = runtime.destroy_scene_output(scene_output);
            // Touch the handle *after* the attempted destroy: this is the
            // dereference that would be reading freed memory.
            let _ = so.position();
            so.damage_ring().add_box(Box2D::new(0, 0, 4, 4));
            destroyed
        })
        .expect("known scene output");
    assert_eq!(
        refused, None,
        "a destroy under a live borrow must be refused"
    );

    // Still alive once the borrow ends — nothing was freed behind the refusal.
    assert!(runtime.scene_output_position(scene_output).is_some());
    // And the destroy works again now that nothing is borrowing it.
    assert_eq!(runtime.destroy_scene_output(scene_output), Some(()));
}

/// The other way a scene output dies: the `wlr_output` under it goes. wlroots
/// destroys the scene output from its own `output_destroy` listener, which is
/// the emission this crate's watch is linked into.
#[test]
fn destroying_the_output_underneath_a_scene_output_makes_its_id_miss() {
    headless_env();
    let display = Display::new().expect("display");
    let backend = Backend::autocreate(&display.event_loop()).expect("backend");
    let runtime = Runtime::new().expect("runtime");
    runtime.init_graphics(&display, &backend).expect("graphics");

    let mut app = App {
        runtime: Some(runtime.clone()),
        ..App::default()
    };
    backend
        .run_all(&display, &mut app, &runtime, Until::Turns(4))
        .expect("run_all");
    let scene_output = *app.scene_outputs.first().expect("a scene output");

    // The unplug, spelled through the raw pointers because this crate offers no
    // safe way to destroy an output — a compositor does not destroy its own
    // monitors, hardware does.
    //
    // SAFETY: the id resolves, so the scene output is live, and its `output`
    // field names the live `wlr_output` it was created for.
    unsafe {
        let raw = runtime
            .with_scene_output(scene_output, |so| so.as_ptr())
            .expect("known scene output");
        wlr_sys::wlr_output_destroy((*raw).output);
    }

    assert_miss(&runtime, scene_output);
}

/// Every by-id call must miss cleanly on an id no runtime ever issued.
#[test]
fn a_dangling_scene_output_id_misses_everywhere() {
    let runtime = Runtime::new().expect("runtime");
    assert_miss(&runtime, SceneOutputId::dangling_for_test());
}

/// The shared "this id names nothing" check, so the three callers above cannot
/// drift apart on which calls they cover.
fn assert_miss(runtime: &Runtime, scene_output: SceneOutputId) {
    assert_eq!(runtime.scene_output_position(scene_output), None);
    assert_eq!(runtime.scene_output_needs_frame(scene_output), None);
    assert_eq!(runtime.set_scene_output_position(scene_output, 1, 2), None);
    assert_eq!(
        runtime.send_scene_output_frame_done(scene_output, Duration::ZERO),
        None
    );
    assert_eq!(
        runtime.scene_output_for_each_buffer(scene_output, |_, _, _| {
            panic!("a destroyed scene output has no buffers to visit");
        }),
        None
    );
    assert_eq!(
        runtime.with_scene_output(scene_output, |_| unreachable!()),
        None::<()>
    );
    assert_eq!(
        runtime.commit_scene_output(scene_output, &SceneOutputStateOptions::new()),
        Err(Error::Destroyed("wlr_scene_output"))
    );
    // The node-side call that names a scene output too.
    assert_eq!(
        runtime.send_scene_buffer_frame_done(
            NodeId::dangling_for_test(),
            scene_output,
            Duration::ZERO
        ),
        None
    );
}

/// The surface-side helpers must miss on ids that name nothing, without a
/// scene or a run in sight.
#[test]
fn surface_helpers_miss_on_ids_that_name_nothing() {
    let runtime = Runtime::new().expect("runtime");
    let node = NodeId::dangling_for_test();

    assert_eq!(
        runtime.with_scene_surface(node, |_| unreachable!()),
        None::<()>
    );
    assert_eq!(
        runtime.send_scene_surface_frame_done(node, Duration::ZERO),
        None
    );
    assert_eq!(runtime.set_subsurface_tree_clip(node, None), None);
    assert_eq!(
        runtime.set_subsurface_tree_clip(node, Some(Box2D::new(0, 0, 4, 4))),
        None
    );

    let mut usable = Box2D::new(0, 0, 100, 100);
    assert_eq!(
        runtime.configure_scene_layer_surface(
            LayerSurfaceId::dangling_for_test(),
            Box2D::new(0, 0, 100, 100),
            &mut usable,
        ),
        None,
        "a layer surface that is not there configures nothing"
    );
    assert_eq!(
        usable,
        Box2D::new(0, 0, 100, 100),
        "and leaves the caller's usable area untouched"
    );

    // Observation is a no-op with no run to deliver through.
    assert_eq!(runtime.observe_scene_buffer(node), None);
    assert_eq!(runtime.unobserve_scene_buffer(node), None);
    assert!(!runtime.scene_buffer_observed(node));
    assert_eq!(runtime.scene_buffer_active_outputs(node), None);

    // And the legacy-id bridges, for good measure.
    assert_eq!(runtime.buffer_node(BufferId::dangling_for_test()), None);
}
