use wlr::Surface;

/// `Surface::from_raw_with_id` is the only way to mint a handle, and it is
/// `pub(crate)` — a consumer must not be able to name it, let alone call it
/// to manufacture a handle with a lifetime of their own choosing.
fn main() {
    let _ = Surface::<'static>::from_raw_with_id;
}
