//! shows-core — the pure, safe engine for the shows desktop: ordering, round
//! logic, scanning, update checks, the round/advance engine, the SQLite
//! replica, sync, the round-robin runner, and the API/auth clients. No Windows
//! GUI and no `unsafe`; this is where "never silently lose state" is enforced by
//! the compiler rather than by discipline.
#![forbid(unsafe_code)]

pub mod engine;
pub mod model;
pub mod ordering;
pub mod replica;
pub mod roundlogic;
pub mod runner;
pub mod scan;
pub mod sync;
pub mod update;
