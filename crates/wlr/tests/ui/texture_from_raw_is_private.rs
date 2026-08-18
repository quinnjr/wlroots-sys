use wlr::Texture;

/// `Texture::from_raw` is the only way to mint a texture handle, and it is
/// `pub(crate)`. A consumer who could call it could give the texture a lifetime
/// of their own choosing and outlive the renderer that has to destroy it.
fn main() {
    let _ = Texture::<'static>::from_raw;
}
