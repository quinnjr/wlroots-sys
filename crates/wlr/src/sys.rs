//! `wlr-sys`, re-exported under one name.
//!
//! Every module writes `use crate::sys;` rather than naming `wlr_sys` directly.
//! The indirection costs nothing and keeps a single edit point if a branch ever
//! needs to bind its `-sys` crate differently.

pub(crate) use wlr_sys::*;
