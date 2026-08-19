use wlr::RegionRef;

/// `RegionRef::from_raw` is the only way to mint a view of a region wlroots
/// owns, and it is `pub(crate)` — a consumer must not be able to name it, let
/// alone call it to manufacture one with a lifetime of their own choosing.
fn main() {
    let _ = RegionRef::<'static>::from_raw;
}
