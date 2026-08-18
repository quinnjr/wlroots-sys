use wlr::{Buffer, BufferPassOptions, RenderPass, Renderer};

/// A `RenderTimer` named by a pass's options must outlive the pass: wlroots
/// stores the pointer on the pass and writes through it from inside `submit`
/// (`render/gles2/pass.c`), so destroying the timer first is a use-after-free
/// that only shows up on a GPU renderer.
///
/// `begin_buffer_pass` takes its options at the pass's own lifetime, so a timer
/// that dies with the call does not compile.
fn timed<'r, 'b>(renderer: &'r Renderer, buffer: &'b Buffer<'b>) -> RenderPass<'r, 'b> {
    let timer = renderer.create_timer().expect("timer");
    let options = BufferPassOptions::new().timer(&timer);
    renderer
        .begin_buffer_pass(buffer, &options)
        .expect("render pass")
}

fn main() {
    let _ = timed;
}
