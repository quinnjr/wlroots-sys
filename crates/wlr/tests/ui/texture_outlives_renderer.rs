use wlr::{FourCc, Renderer};

/// A texture must not outlive the renderer that made it: wlroots' own
/// `wlr_renderer_destroy` documents that textures are destroyed separately, and
/// the pixman renderer destroys every texture still on its list when it goes —
/// so a `wlr_texture_destroy` afterwards is a double free.
///
/// `Texture<'r>` borrows the renderer, which is what turns that into this
/// compile error rather than a rule to remember.
fn main() {
    let renderer = Renderer::pixman().expect("pixman renderer");
    let pixels = vec![0u8; 4 * 4 * 4];
    let texture = renderer
        .texture_from_pixels(FourCc::ARGB8888, 16, 4, 4, &pixels)
        .expect("texture");

    drop(renderer);

    let _ = texture.width();
}
