//! Mode configuration (game-modes plan T2): the per-mode rule parameters a
//! `Game` is built from, plus the terminal-reason vocabulary modes report.
//!
//! [`ModeConfig::default`] reproduces today's Marathon exactly — `Game::new`
//! is `Game::with_config(seed, &ModeConfig::default())` and the T1 golden
//! test gates that equivalence. All times in a config are logical ticks:
//! the core never reads a clock (plan constraint 3).
//!
//! Design notes:
//! - `GameSnapshot`/`MatchSnapshot` gain **no** fields from modes (wire
//!   stability until T19); terminal state is exposed via
//!   [`crate::game::Game::finished_reason`].
//! - `T12` will extend this module with a `GarbageFeed` field; keep
//!   `ModeConfig` open to additional fields.

use serde::{Deserialize, Serialize};

use crate::board::{Board, COLS, HIDDEN_ROWS, ROWS};
use crate::piece::Piece;
use crate::prng::Rng;

/// Win condition evaluated by the core (T3 wires `GarbageCleared` firing
/// checks beyond the initial wiring in T2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Goal {
    /// Reached when `lines_cleared >= n` at a lock (Sprint: `Lines(40)`).
    Lines(u32),
    /// Reached when [`crate::game::Game::garbage_rows_left`] hits 0 after a
    /// lock (Dig).
    ///
    /// Retire semantics: a settled row counts as a *garbage row* while it
    /// contains at least one [`Piece::Garbage`] cell — a single garbage cell
    /// on an otherwise empty row still counts. A garbage row retires the
    /// moment a line clear removes it (any garbage cell in the row retires
    /// the whole row, because clearing requires the row to complete), and
    /// the surviving garbage rows shift down with the stack, still counted.
    /// The goal is evaluated after this lock's line clear, so the winning
    /// lock's event batch emits `LineCleared` before `GoalReached`.
    ///
    /// A game that starts with zero garbage rows satisfies this on its first
    /// lock; modes using this goal must start with
    /// [`StartBoard::BuriedGarbage`] (or an equivalent hand-made garbage
    /// board).
    GarbageCleared,
}

/// Non-empty initial playfield assembled from the game seed at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartBoard {
    /// Bottom `rows` *visible-field* rows (board rows
    /// `ROWS - rows..ROWS`) are filled with [`Piece::Garbage`] except
    /// exactly one hole per row, and no two adjacent rows share a hole
    /// column. Hole columns are drawn from a splitmix64 stream derived
    /// from the game seed (salted so it never aliases the bag stream).
    /// `rows` is clamped to the visible field height
    /// (`ROWS - HIDDEN_ROWS = 20`) so hidden spawn rows stay empty and
    /// every spawn state fits.
    BuriedGarbage { rows: usize },
}

impl StartBoard {
    /// Materialize this start board for `seed`. Deterministic: same seed +
    /// same variant always yields the identical board.
    pub fn build(self, seed: u64) -> Board {
        match self {
            StartBoard::BuriedGarbage { rows } => buried_garbage_board(rows, seed),
        }
    }
}

/// Seed-stream salt for buried-garbage hole columns: keeps the hole stream
/// independent of the 7-bag's `Rng::new(seed)` stream.
const BURIED_GARBAGE_SALT: u64 = 0xFF51_AFD2_AFED_DAB9;

fn buried_garbage_board(rows: usize, seed: u64) -> Board {
    let mut board = Board::new();
    // Clamp to the visible field: hidden spawn rows must stay empty.
    let rows = rows.min(ROWS - HIDDEN_ROWS);
    let mut rng = Rng::new(seed ^ BURIED_GARBAGE_SALT);
    let mut prev_hole: Option<usize> = None;
    for r in (ROWS - rows)..ROWS {
        let mut hole = rng.next_below(COLS as u64) as usize;
        // Invariant: no two adjacent rows share a hole column.
        while Some(hole) == prev_hole {
            hole = rng.next_below(COLS as u64) as usize;
        }
        prev_hole = Some(hole);
        for c in 0..COLS {
            if c != hole {
                board.set(r, c, Some(Piece::Garbage));
            }
        }
    }
    board
}

/// What a block-out (spawn collision) does to the game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockOutBehavior {
    /// Freeze the game and emit `GameEvent::GameOver` (Marathon et al.).
    #[default]
    End,
    /// Plumb-only (T2): Zen's wipe-and-continue behavior is **wired in T14**
    /// (stack cleared, colliding piece respawned, `GameEvent::StackWiped`).
    /// Until then it behaves exactly like [`BlockOutBehavior::End`].
    WipeAndContinue,
}

/// Why a game froze. Exposed through [`crate::game::Game::finished_reason`]
/// (the snapshot carries no such field — wire stability).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// Block-out at spawn (or garbage pushed past the ceiling). The
    /// snapshot's `game_over` flag is `true` exactly in this case.
    TopOut,
    /// The [`Goal`] was met; the game froze without `game_over`.
    GoalReached,
    /// The `clock_ticks` budget expired; the game froze without `game_over`.
    TimeUp,
}

/// Everything that distinguishes one solo mode's rules from another. A mode
/// is pure data: `Game::with_config(seed, &config)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeConfig {
    /// Level the game starts at; sets the gravity from tick 0. Values
    /// below 1 behave as level 1 (matches `gravity::interval_for`
    /// clamping).
    pub start_level: u32,
    /// `true` (Marathon): every 10 lines raise the level and emit
    /// `GameEvent::LevelUp`. `false` (Sprint/Dig): the level and gravity
    /// interval are pinned to `start_level` and no `LevelUp` is ever
    /// emitted.
    pub levels_advance: bool,
    /// Optional win condition; checked at each lock.
    pub goal: Option<Goal>,
    /// Optional logical-tick budget; `tick()` emits
    /// `GameEvent::TimeUp { tick }` and freezes once `ticks >= n`
    /// (Ultra: `Some(7200)`). `None` = no clock.
    pub clock_ticks: Option<u64>,
    /// Optional non-empty initial playfield.
    pub start_board: Option<StartBoard>,
    /// What a block-out does.
    pub on_block_out: BlockOutBehavior,
}

impl Default for ModeConfig {
    /// Today's Marathon exactly: start at level 1 with the line-count
    /// level curve, no goal, no clock, empty board, end on block-out.
    fn default() -> Self {
        Self {
            start_level: 1,
            levels_advance: true,
            goal: None,
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::End,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Game;

    #[test]
    fn default_config_is_marathon() {
        let c = ModeConfig::default();
        assert_eq!(c.start_level, 1);
        assert!(c.levels_advance);
        assert_eq!(c.goal, None);
        assert_eq!(c.clock_ticks, None);
        assert_eq!(c.start_board, None);
        assert_eq!(c.on_block_out, BlockOutBehavior::End);
    }

    /// Buried-garbage start-board invariants over 100 seeds:
    /// every buried row has exactly one hole, no two adjacent rows share
    /// a hole column, and at 10 buried rows every possible first piece
    /// still spawns without collision.
    #[test]
    fn buried_garbage_invariants_over_100_seeds() {
        for seed in 0..100u64 {
            let board = StartBoard::BuriedGarbage { rows: 10 }.build(seed);
            let mut prev_hole: Option<usize> = None;
            for r in (ROWS - 10)..ROWS {
                let holes: Vec<usize> = (0..COLS).filter(|&c| board.get(r, c).is_none()).collect();
                assert_eq!(holes.len(), 1, "seed {seed} row {r}: {holes:?} holes");
                let hole = holes[0];
                assert_ne!(
                    Some(hole),
                    prev_hole,
                    "seed {seed}: rows {} and {r} share hole column {hole}",
                    r - 1
                );
                prev_hole = Some(hole);
                // All other cells are garbage (never a real piece mark).
                for c in 0..COLS {
                    if c != hole {
                        assert_eq!(board.get(r, c), Some(Piece::Garbage));
                    }
                }
            }
            // Rows above the buried band are empty, including hidden rows.
            for r in 0..(ROWS - 10) {
                for c in 0..COLS {
                    assert_eq!(board.get(r, c), None, "seed {seed} cell {r},{c}");
                }
            }
            // The first piece must spawn without collision (all 7 pieces,
            // whatever the bag deals — checked for every piece here).
            for piece in Piece::ALL {
                let st = crate::piece::spawn_state(piece);
                assert!(
                    !board.collides(&st),
                    "seed {seed}: {piece:?} cannot spawn over 10 buried rows"
                );
            }
        }
    }

    #[test]
    fn buried_garbage_first_spawn_fits_for_100_seeds() {
        // Same invariant through the real Game path: the dealt first piece
        // (not just every piece) spawns live.
        let config = ModeConfig {
            start_board: Some(StartBoard::BuriedGarbage { rows: 10 }),
            ..ModeConfig::default()
        };
        for seed in 0..100u64 {
            let g = Game::with_config(seed, &config);
            let s = g.snapshot();
            assert!(
                s.active.is_some() && !s.game_over,
                "seed {seed}: first piece must spawn over 10 buried rows"
            );
            assert_eq!(g.garbage_rows_left(), 10);
        }
    }

    #[test]
    fn buried_garbage_is_seed_derivated_deterministic() {
        let a = StartBoard::BuriedGarbage { rows: 10 }.build(7);
        let b = StartBoard::BuriedGarbage { rows: 10 }.build(7);
        assert_eq!(a.get(12, 0), b.get(12, 0));
        let mut same = true;
        for r in 0..ROWS {
            for c in 0..COLS {
                same &= a.get(r, c) == b.get(r, c);
            }
        }
        assert!(same, "same seed must build identical buried boards");
    }

    #[test]
    fn buried_garbage_clamps_rows_to_visible_field() {
        let board = StartBoard::BuriedGarbage { rows: 25 }.build(1);
        // Hidden spawn rows stay empty even when over-requested.
        for r in 0..HIDDEN_ROWS {
            for c in 0..COLS {
                assert_eq!(board.get(r, c), None);
            }
        }
        assert_eq!(
            (0..COLS)
                .filter(|&c| board.get(ROWS - HIDDEN_ROWS, c).is_none())
                .count(),
            1
        );
    }
}
