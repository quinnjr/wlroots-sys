use wlr::Renderer;

/// A backend view is a proof about a renderer, not a thing of its own: every
/// call on it dereferences the `wlr_renderer` it was minted from. So `Gles2<'r>`
/// borrows the renderer, and outliving it is this compile error rather than a
/// use-after-free at the first accessor.
fn main() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let view = renderer.as_gles2();

    drop(renderer);

    let _ = view.map(|view| view.check_ext("GL_OES_EGL_image_external"));
}
