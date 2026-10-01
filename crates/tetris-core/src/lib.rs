//! `tetris-core` — pure, deterministic Tetris rules engine.
//!
//! No Bevy, no I/O, no wall-clock: everything is a pure function of state plus a
//! 60 Hz logical tick and discrete [`actions::Action`]s, so runs are replayable
//! from a fixed seed. Owned module map (single-owner per tetris-plan.md):
//!
//! - [`piece`], [`board`] — tetrominoes, 10×22 grid, collision/merge/line-scan (T2)
//! - [`prng`], [`bag`] — seedable PRNG and 7-bag randomizer (T3)
//! - [`srs`] — SRS rotation with wall kicks incl. 180° (T4)
//! - [`gravity`] — Tetris Worlds level curve (T5)
//! - [`actions`], [`lock`], [`hold`] — input schema, lock delay, hold (T6)
//! - [`score`], [`tspin`] — guideline scoring, B2B/combo, T-spin detection (T7)
//! - [`game`], [`event`] — deterministic facade and `GameEvent` contract (T8)
//! - [`mode`] — per-mode rule configuration: goals, clocks, start boards (T2)
//! - [`versus`] — deterministic 1v1 match wrapper over two games (T24)

pub mod actions;
pub mod bag;
pub mod board;
pub mod event;
pub mod game;
pub mod gravity;
pub mod hold;
pub mod lock;
pub mod mode;
pub mod piece;
pub mod prng;
pub mod score;
pub mod srs;
pub mod tspin;
pub mod versus;
