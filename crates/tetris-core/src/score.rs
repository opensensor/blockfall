//! Guideline scoring with level multiplier, B2B ×1.5, capped combo (T7).
//!
//! Implements the PRD §6.7 table exactly:
//!
//! | Action | Base points (× level) |
//! | --- | --- |
//! | Single / Double / Triple / Tetris | 100 / 300 / 500 / 800 |
//! | T-spin mini / T-spin (no lines) | 100 / 400 |
//! | T-spin single / double / triple | 800 / 1200 / 1600 |
//! | Back-to-back Tetris/T-spin | ×1.5 |
//! | Combo | 50 × combo count × level (capped 10) |
//! | Soft drop / hard drop | 1 / 2 per cell (flat) |
//! | Perfect clear (single piece) | 3500 |
//!
//! Interpretation decisions (the PRD table is terse; pinned by tests):
//!
//! - **×level** multiplies every line-clear/T-spin base. Drop points are
//!   flat: the caller (T8) pre-computes them (soft drop 1/cell, hard drop
//!   2/cell) and they are added unchanged, never scaled by level.
//! - **Mini with lines**: the table has no mini+line row, so a line-clearing
//!   T-spin mini scores the T-spin single/double/triple row literally
//!   (800/1200/1600) and counts as "difficult" for B2B like a full spin.
//! - **Rounding**: B2B ×1.5 is applied after the level multiply and rounds
//!   down (floor). All table bases are even, so real rows divide exactly;
//!   the floor rule keeps odd intermediates deterministic.
//! - **B2B**: armed by any *difficult* line clear (Tetris, or a T-spin mini
//!   or full clearing ≥1 line). The next difficult clear scores ×1.5 and
//!   sustains the chain; any plain (non-T-spin, non-Tetris) line clear
//!   breaks it. Locks that clear no lines (including T-spins with 0 lines)
//!   leave B2B untouched.
//! - **Combo**: increments on each consecutive line-clearing lock, resets on
//!   a lock that clears nothing. `ScoreDelta::combo_now` is the PRD
//!   "combo count": 0 after the first clear of a chain (no bonus), then
//!   1, 2, … The bonus is `50 × min(combo, 10) × level` and is *not*
//!   B2B-multiplied.
//! - **Perfect clear**: flat 3500 (no level multiplier) added on top of
//!   everything else whenever the board is empty after merge + line clear.
//!   It is neither difficult nor combo-affecting on its own: a PC Tetris
//!   under B2B scores `800×1.5×level + combo + 3500` (PRD lists no special
//!   interaction, so the parts simply stack).

use serde::{Deserialize, Serialize};

use crate::tspin::TSpinKind;

/// Points awarded for one lock plus the resulting chain states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreDelta {
    /// Total points for this lock (line clear + B2B + combo + perfect
    /// clear + caller-supplied drop points).
    pub points: u64,
    /// Back-to-back armed after this lock.
    pub b2b_now: bool,
    /// PRD combo count after this lock (0 = no active combo).
    pub combo_now: u32,
}

/// Stateful scoring component: one [`ScoreState::on_lock`] call per piece
/// lock, owned and persisted by T8 across a game.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreState {
    /// Running score total.
    pub total: u64,
    /// Back-to-back chain armed (previous line clear was difficult).
    pub b2b: bool,
    /// Consecutive line-clearing locks, first clear of a chain included
    /// (0 = chain inactive). The PRD combo count is `chain - 1`.
    pub combo: u32,
}

/// `floor(x × 1.5)` — the B2B multiplier with the floor rounding rule.
fn b2b_scale(x: u64) -> u64 {
    x + x / 2
}

/// Base points from the PRD §6.7 table, before ×level and B2B.
fn base_points(lines: usize, tspin: TSpinKind) -> u64 {
    match (tspin, lines) {
        (TSpinKind::None, n) => match n {
            0 => 0,
            1 => 100,
            2 => 300,
            3 => 500,
            _ => 800,
        },
        (TSpinKind::Mini, 0) => 100,
        (TSpinKind::Full, 0) => 400,
        (TSpinKind::Mini | TSpinKind::Full, 1) => 800,
        (TSpinKind::Mini | TSpinKind::Full, 2) => 1200,
        (TSpinKind::Mini | TSpinKind::Full, _) => 1600,
    }
}

impl ScoreState {
    /// Fresh game: zero score, no chains.
    pub fn new() -> Self {
        Self::default()
    }

    /// Back to [`ScoreState::new`] (game restart).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Score one piece lock and advance the B2B/combo chains.
    ///
    /// T8 calls this once per lock with:
    /// - `lines` — rows cleared by this lock (0..=4),
    /// - `tspin` — [`crate::tspin::detect_tspin`] result for the lock,
    /// - `drop_points` — pre-computed flat drop score (1/cell soft,
    ///   2/cell hard; never level-scaled),
    /// - `level` — current level (≥1),
    /// - `board_empty_after_clear` — `Board::is_empty()` *after* merge and
    ///   line-clear (the perfect-clear condition).
    pub fn on_lock(
        &mut self,
        lines: usize,
        tspin: TSpinKind,
        drop_points: u64,
        level: u32,
        board_empty_after_clear: bool,
    ) -> ScoreDelta {
        let level = level as u64;
        let difficult = lines > 0 && (lines >= 4 || tspin != TSpinKind::None);
        let applied_b2b = difficult && self.b2b;

        let mut points = base_points(lines, tspin) * level;
        if applied_b2b {
            points = b2b_scale(points);
        }

        // Chain updates: no-clear locks leave B2B untouched.
        if difficult {
            self.b2b = true;
        } else if lines > 0 {
            self.b2b = false;
        }

        if lines > 0 {
            self.combo += 1;
        } else {
            self.combo = 0;
        }
        let combo_now = self.combo.saturating_sub(1);
        if combo_now > 0 {
            points += 50 * u64::from(combo_now.min(10)) * level;
        }

        if board_empty_after_clear {
            points += 3500;
        }
        points += drop_points;

        self.total += points;
        ScoreDelta {
            points,
            b2b_now: self.b2b,
            combo_now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{b2b_scale, ScoreState};
    use crate::tspin::TSpinKind;

    /// PRD §6.7 table rows: (lines, T-spin kind, base points before ×level).
    const TABLE: [(usize, TSpinKind, u64); 13] = [
        (0, TSpinKind::None, 0),
        (1, TSpinKind::None, 100),
        (2, TSpinKind::None, 300),
        (3, TSpinKind::None, 500),
        (4, TSpinKind::None, 800),
        (0, TSpinKind::Mini, 100),
        (0, TSpinKind::Full, 400),
        (1, TSpinKind::Mini, 800),
        (2, TSpinKind::Mini, 1200),
        (3, TSpinKind::Mini, 1600),
        (1, TSpinKind::Full, 800),
        (2, TSpinKind::Full, 1200),
        (3, TSpinKind::Full, 1600),
    ];

    #[test]
    fn prd_table_at_level_1_and_2() {
        // Fresh state per row so B2B/combo never distort the base table.
        for level in [1u32, 2] {
            for &(lines, t, base) in &TABLE {
                let mut s = ScoreState::new();
                let d = s.on_lock(lines, t, 0, level, false);
                assert_eq!(
                    d.points,
                    base * level as u64,
                    "lines {lines} {t:?} at level {level}"
                );
            }
        }
    }

    #[test]
    fn b2b_tetris_chain_scores_1_5x() {
        for level in [1u32, 2] {
            let mut s = ScoreState::new();
            let l = level as u64;
            assert_eq!(
                s.on_lock(4, TSpinKind::None, 0, level, false).points,
                800 * l
            );
            let d = s.on_lock(4, TSpinKind::None, 0, level, false);
            assert_eq!(d.points, 1200 * l + 50 * l, "2nd tetris: 800*1.5 + combo");
            assert!(d.b2b_now);
            assert_eq!(d.combo_now, 1);
        }
    }

    #[test]
    fn b2b_chain_tetris_then_tsd_then_tetris() {
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(4, TSpinKind::None, 0, 1, false).points, 800);
        // TSD is difficult: it gets B2B (1200*1.5 = 1800) and sustains the chain.
        assert_eq!(s.on_lock(2, TSpinKind::Full, 0, 1, false).points, 1800 + 50);
        // 3rd difficult clear still in B2B.
        assert_eq!(
            s.on_lock(4, TSpinKind::None, 0, 1, false).points,
            1200 + 100
        );
    }

    #[test]
    fn b2b_broken_by_plain_line_clear() {
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(4, TSpinKind::None, 0, 1, false).points, 800);
        assert_eq!(s.on_lock(1, TSpinKind::None, 0, 1, false).points, 100 + 50);
        let d = s.on_lock(4, TSpinKind::None, 0, 1, false);
        assert_eq!(d.points, 800 + 100, "B2B was broken: no 1.5x");
        assert!(d.b2b_now, "this tetris re-arms B2B");
    }

    #[test]
    fn zero_line_locks_neither_trigger_nor_break_b2b() {
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(4, TSpinKind::None, 0, 1, false).points, 800);
        // T-spin with no lines: scores 400, is not a line clear: B2B state
        // unchanged, combo reset.
        let d = s.on_lock(0, TSpinKind::Full, 0, 1, false);
        assert_eq!(d.points, 400);
        assert!(d.b2b_now, "no-clear placement must not break B2B");
        assert_eq!(d.combo_now, 0);
        let d = s.on_lock(4, TSpinKind::None, 0, 1, false);
        assert_eq!(d.points, 1200, "chain survived the no-clear placement");
    }

    #[test]
    fn tspin_mini_single_is_difficult_for_b2b() {
        let mut s = ScoreState::new();
        let d = s.on_lock(1, TSpinKind::Mini, 0, 1, false);
        assert_eq!(d.points, 800);
        assert!(d.b2b_now);
        assert_eq!(s.on_lock(4, TSpinKind::None, 0, 1, false).points, 1200 + 50);
    }

    #[test]
    fn combo_bonuses_and_cap() {
        let mut s = ScoreState::new();
        // 12 consecutive singles at level 1: bonuses 0,50,...,500,500 (cap n=10).
        let expected = [0u64, 50, 100, 150, 200, 250, 300, 350, 400, 450, 500, 500];
        for (i, bonus) in expected.iter().enumerate() {
            let d = s.on_lock(1, TSpinKind::None, 0, 1, false);
            assert_eq!(d.points, 100 + bonus, "clear #{}", i + 1);
            assert_eq!(d.combo_now, i as u32, "combo_now after clear #{}", i + 1);
        }
        assert_eq!(s.on_lock(1, TSpinKind::None, 0, 1, false).points, 100 + 500);
    }

    #[test]
    fn combo_scales_with_level_and_resets() {
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(2, TSpinKind::None, 0, 2, false).points, 600);
        let d = s.on_lock(2, TSpinKind::None, 0, 2, false);
        assert_eq!(d.points, 600 + 100, "50*1*2 combo bonus");
        assert_eq!(d.combo_now, 1);
        // A placement that clears no lines resets the combo.
        let d = s.on_lock(0, TSpinKind::None, 3, 2, false);
        assert_eq!(d.combo_now, 0);
        let d = s.on_lock(2, TSpinKind::None, 0, 2, false);
        assert_eq!(d.points, 600, "combo restarted: no bonus");
        assert_eq!(d.combo_now, 0);
    }

    #[test]
    fn perfect_clear_is_flat_3500_and_stacks() {
        // Flat: identical at level 1 and 5.
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(0, TSpinKind::None, 0, 1, true).points, 3500);
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(0, TSpinKind::None, 0, 5, true).points, 3500);
        // Stacks on top of the (level-multiplied) line clear + combo.
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(1, TSpinKind::None, 0, 2, true).points, 200 + 3500);
        // PC + Tetris under B2B: B2B applies to the line-clear part only.
        let mut s = ScoreState::new();
        assert_eq!(s.on_lock(4, TSpinKind::None, 0, 1, false).points, 800);
        let d = s.on_lock(4, TSpinKind::None, 0, 1, true);
        assert_eq!(d.points, 1200 + 50 + 3500);
        assert!(d.b2b_now, "PC must not break B2B");
    }

    #[test]
    fn drop_points_are_flat_at_level_5() {
        // Soft/hard drop points arrive pre-computed (1/2 per cell) and are
        // never multiplied by level.
        for level in [1u32, 5] {
            let mut s = ScoreState::new();
            assert_eq!(s.on_lock(0, TSpinKind::None, 20, level, false).points, 20);
        }
        // They add onto a line clear unchanged.
        let mut s = ScoreState::new();
        assert_eq!(
            s.on_lock(3, TSpinKind::None, 40, 5, false).points,
            2500 + 40
        );
    }

    #[test]
    fn b2b_rounding_floors() {
        // floor(x * 1.5): exact for the even table bases, floors odd inputs.
        assert_eq!(b2b_scale(800), 1200);
        assert_eq!(b2b_scale(3), 4); // floor(4.5)
        assert_eq!(b2b_scale(1), 1); // floor(1.5)
        assert_eq!(b2b_scale(0), 0);
    }

    #[test]
    fn total_accumulates_and_reset_clears() {
        let mut s = ScoreState::new();
        s.on_lock(4, TSpinKind::None, 0, 1, false);
        s.on_lock(2, TSpinKind::Full, 12, 2, false);
        // lock 2 is a B2B TSD at level 2: 1200*2*1.5 + combo 50*1*2 + drop 12.
        assert_eq!(s.total, 800 + (3600 + 100 + 12));
        s.reset();
        assert_eq!(s.total, 0);
        assert!(!s.b2b);
        assert_eq!(s.combo, 0);
    }
}
