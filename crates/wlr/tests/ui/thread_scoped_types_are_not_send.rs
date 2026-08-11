//! The `IN_HANDLER` guard in `dispatch.rs` is thread-local, and that only
//! closes the hole it exists for because `Display`, `EventLoop`, `Backend`
//! and `Output` are all `!Send` (and so, being single-threaded types, never
//! move to a thread where the flag reads clear). That currently holds
//! incidentally — from `NonNull` fields and `PhantomData<&Display>` — rather
//! than from anything stated. This pins it: each type must fail the `Send`
//! bound, so a future field that accidentally makes one `Send` (an `Arc`, a
//! plain `usize`) — or a well-meant `unsafe impl Send` — breaks this fixture
//! instead of silently voiding the guard.

fn requires_send<T: Send>() {}

fn main() {
    requires_send::<wlr::Display>();
    requires_send::<wlr::EventLoop<'static>>();
    requires_send::<wlr::Backend<'static>>();
    requires_send::<wlr::Output<'static>>();
}
