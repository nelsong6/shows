//! shows-core — the pure, safe engine for the shows desktop: ordering, round
//! logic, scanning, update checks, the round/advance engine, the SQLite
//! NAS database and round-robin runner. No Windows
//! GUI and no `unsafe`; this is where "never silently lose state" is enforced by
//! the compiler rather than by discipline.
#![forbid(unsafe_code)]

pub mod engine;
pub mod ordering;
pub mod replica;
pub mod roundlogic;
pub mod runner;
pub mod scan;
pub mod update;
