//! Linear ARGB8888 format helper for the output integration-test binaries.

/// The linear ARGB8888 format cursor buffers are allocated in, so the pixman
/// allocator hands out mappable buffers a cursor can take.
pub fn argb() -> wlr::DrmFormat {
    wlr::DrmFormat::new(wlr::FourCc::ARGB8888, [wlr::Modifier::LINEAR])
}
