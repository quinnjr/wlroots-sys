use wlr::{Buffer, BufferPassOptions, RenderPass, Renderer};

/// A render pass must not outlive either the renderer it was begun on or the
/// buffer it draws into: dropping it *submits* it, and submitting through a
/// freed renderer is a use-after-free.
///
/// `RenderPass<'r, 'b>` borrows both, so a pass smuggled out of the scope that
/// made it does not compile.
fn escape<'b>(buffer: &'b Buffer<'b>) -> RenderPass<'static, 'b> {
    let renderer = Renderer::pixman().expect("pixman renderer");
    renderer
        .begin_buffer_pass(buffer, &BufferPassOptions::new())
        .expect("pass")
}

fn main() {
    let _ = escape;
}
