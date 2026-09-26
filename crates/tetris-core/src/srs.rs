//! SRS rotation states, JLSTZ/I wall-kick tables, 180° policy (T4).
//!
//! Coordinate convention: kick tables use the SRS spec's `(x, y)` system
//! with **+x right and +y UP**. The board's row axis grows DOWN, so
//! [`try_rotate`] converts each offset as `col += x`, `row -= y`. The
//! table rows for JLSTZ `0→R` and I `0→R` are pinned verbatim in tests
//! to catch sign errors in that conversion.

use serde::{Deserialize, Serialize};

use crate::board::Board;
use crate::piece::{Piece, PieceState, Rotation};

/// Direction of a rotation attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RotateDir {
    Clockwise,
    CounterClockwise,
    HalfTurn,
}

/// A successful rotation: the resulting piece state plus the 0-based kick
/// trial that made it legal. `kick_index == 0` means no offset was needed;
/// `kick_index == 4` is the far/final SRS kick that T7's T-spin Mini rule
/// needs to distinguish full from mini spins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationAttempt {
    pub state: PieceState,
    pub kick_index: u8,
}

/// No-offset trial, also the entire table for O (cells are identical in
/// every rotation, so O never needs a kick and always succeeds at 0).
const ZERO: [(i32, i32); 1] = [(0, 0)];

/// JLSTZ SRS kicks, indexed `[from][trial]` where the transition is
/// `from → from.clockwise()`. Values are `(x, y)` with +y UP, verbatim
/// per the SRS spec.
const JLSTZ_CW: [[(i32, i32); 5]; 4] = [
    [(0, 0), (-1, 0), (-1, 1), (0, -2), (-1, -2)], // 0 → R
    [(0, 0), (1, 0), (1, -1), (0, 2), (1, 2)],     // R → 2
    [(0, 0), (1, 0), (1, 1), (0, -2), (1, -2)],    // 2 → L
    [(0, 0), (-1, 0), (-1, -1), (0, 2), (-1, 2)],  // L → 0
];

/// JLSTZ SRS kicks counter-clockwise, `[from][trial]` for
/// `from → from.counter_clockwise()`. `(x, y)`, +y UP.
const JLSTZ_CCW: [[(i32, i32); 5]; 4] = [
    [(0, 0), (1, 0), (1, 1), (0, -2), (1, -2)],    // 0 → L
    [(0, 0), (1, 0), (1, -1), (0, 2), (1, 2)],     // R → 0
    [(0, 0), (-1, 0), (-1, 1), (0, -2), (-1, -2)], // 2 → R
    [(0, 0), (-1, 0), (-1, -1), (0, 2), (-1, 2)],  // L → 2
];

/// I-piece SRS kicks clockwise, `[from][trial]` for
/// `from → from.clockwise()`. `(x, y)`, +y UP.
const I_CW: [[(i32, i32); 5]; 4] = [
    [(0, 0), (-2, 0), (1, 0), (-2, -1), (1, 2)], // 0 → R
    [(0, 0), (-1, 0), (2, 0), (-1, 2), (2, -1)], // R → 2
    [(0, 0), (2, 0), (-1, 0), (2, 1), (-1, -2)], // 2 → L
    [(0, 0), (1, 0), (-2, 0), (1, -2), (-2, 1)], // L → 0
];

/// I-piece SRS kicks counter-clockwise, `[from][trial]` for
/// `from → from.counter_clockwise()`. `(x, y)`, +y UP.
const I_CCW: [[(i32, i32); 5]; 4] = [
    [(0, 0), (-1, 0), (2, 0), (-1, 2), (2, -1)], // 0 → L
    [(0, 0), (2, 0), (-1, 0), (2, 1), (-1, -2)], // R → 0
    [(0, 0), (-2, 0), (1, 0), (-2, -1), (1, 2)], // 2 → R
    [(0, 0), (1, 0), (-2, 0), (1, -2), (-2, 1)], // L → 2
];

/// 180° policy (no SRS spec exists for half turns). Guideline-style set
/// used by common implementations: in-place first, then small nudges —
/// one column left, one column right, one row down, and a combined
/// drop + right nudge. `(x, y)`, +y UP, applied to every transition.
const HALF_TURN: [(i32, i32); 5] = [(0, 0), (-1, 0), (1, 0), (0, -1), (1, -1)];

/// Wall-kick offsets for rotating `piece` from rotation state `from` in
/// direction `dir`, as a slice of `(x, y)` trials in the **SRS spec
/// coordinate system: +x right, +y UP** (board conversion `col += x`,
/// `row -= y` happens in [`try_rotate`]). The first entry is always
/// `(0, 0)`. Quarter turns return the 5 SRS trials (JLSTZ and I tables
/// per spec); O returns just the no-offset trial; [`RotateDir::HalfTurn`]
/// returns the guideline set above, independent of `piece`/`from`.
pub fn kick_offsets(dir: RotateDir, piece: Piece, from: Rotation) -> &'static [(i32, i32)] {
    let f = from.index();
    match (dir, piece) {
        (RotateDir::HalfTurn, _) => &HALF_TURN,
        (_, Piece::O) => &ZERO,
        (RotateDir::Clockwise, Piece::I) => &I_CW[f],
        (RotateDir::CounterClockwise, Piece::I) => &I_CCW[f],
        (RotateDir::Clockwise, _) => &JLSTZ_CW[f],
        (RotateDir::CounterClockwise, _) => &JLSTZ_CCW[f],
    }
}

/// Try to rotate `ps` in direction `dir`, returning the first kick trial
/// whose candidate does not collide with `board` (negative rows never do,
/// per the T2 convention). `kick_index` is the 0-based trial index
/// (0 = no offset; 4 = the far SRS kick T7's T-spin rule keys on).
/// Returns `None` if every trial collides (the piece is wedged).
pub fn try_rotate(board: &Board, ps: &PieceState, dir: RotateDir) -> Option<RotationAttempt> {
    let rot = match dir {
        RotateDir::Clockwise => ps.rot.clockwise(),
        RotateDir::CounterClockwise => ps.rot.counter_clockwise(),
        RotateDir::HalfTurn => ps.rot.half_turn(),
    };
    for (i, &(x, y)) in kick_offsets(dir, ps.piece, ps.rot).iter().enumerate() {
        let candidate = PieceState {
            piece: ps.piece,
            rot,
            row: ps.row - y,
            col: ps.col + x,
        };
        if !board.collides(&candidate) {
            return Some(RotationAttempt {
                state: candidate,
                kick_index: i as u8,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{kick_offsets, try_rotate, RotateDir, RotationAttempt};
    use crate::board::{Board, COLS, ROWS};
    use crate::piece::{Piece, PieceState, Rotation};

    const QUARTER_DIRS: [RotateDir; 2] = [RotateDir::Clockwise, RotateDir::CounterClockwise];
    const ALL_DIRS: [RotateDir; 3] = [
        RotateDir::Clockwise,
        RotateDir::CounterClockwise,
        RotateDir::HalfTurn,
    ];
    const ALL_ROTS: [Rotation; 4] = [Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw];

    fn ps(piece: Piece, rot: Rotation, row: i32, col: i32) -> PieceState {
        PieceState {
            piece,
            rot,
            row,
            col,
        }
    }

    fn filled_rows(board: &mut Board, rows: &[usize], empty_cols: &[usize]) {
        for &r in rows {
            for c in 0..COLS {
                if !empty_cols.contains(&c) {
                    board.set(r, c, Some(Piece::I));
                }
            }
        }
    }

    fn target_rot(dir: RotateDir, from: Rotation) -> Rotation {
        match dir {
            RotateDir::Clockwise => from.clockwise(),
            RotateDir::CounterClockwise => from.counter_clockwise(),
            RotateDir::HalfTurn => from.half_turn(),
        }
    }

    #[test]
    fn every_quarter_rotation_succeeds_at_index_zero_in_open_space() {
        let board = Board::new();
        for &piece in &Piece::ALL {
            for &dir in &QUARTER_DIRS {
                for &from in &ALL_ROTS {
                    let start = ps(piece, from, 4, 3);
                    assert!(!board.collides(&start), "{piece:?} {from:?} start collides");
                    let got = try_rotate(&board, &start, dir);
                    assert!(
                        got.is_some(),
                        "{piece:?} {from:?} {dir:?} must rotate in open space"
                    );
                    let a: RotationAttempt = got.unwrap();
                    assert_eq!(a.kick_index, 0, "{piece:?} {from:?} {dir:?}");
                    assert_eq!(a.state.rot, target_rot(dir, from));
                    assert_eq!(a.state.row, 4);
                    assert_eq!(a.state.col, 3);
                }
            }
        }
    }

    #[test]
    fn jlstz_spawn_to_cw_table_is_pinned_y_up() {
        // SRS spec (x, y) with y UP: a +1 y offset must move the piece UP,
        // i.e. decrease the board row (row grows down). If the sign were
        // flipped, this pin and the I pin below would both fail.
        assert_eq!(
            kick_offsets(RotateDir::Clockwise, Piece::J, Rotation::Spawn),
            &[(0, 0), (-1, 0), (-1, 1), (0, -2), (-1, -2)]
        );
        assert_eq!(
            kick_offsets(RotateDir::CounterClockwise, Piece::T, Rotation::R180),
            &[(0, 0), (-1, 0), (-1, 1), (0, -2), (-1, -2)]
        );
    }

    #[test]
    fn i_spawn_to_cw_table_is_pinned_y_up() {
        assert_eq!(
            kick_offsets(RotateDir::Clockwise, Piece::I, Rotation::Spawn),
            &[(0, 0), (-2, 0), (1, 0), (-2, -1), (1, 2)]
        );
        assert_eq!(
            kick_offsets(RotateDir::CounterClockwise, Piece::I, Rotation::Spawn),
            &[(0, 0), (-1, 0), (2, 0), (-1, 2), (2, -1)]
        );
    }

    #[test]
    fn kick_table_shapes_are_srs_conform() {
        for &piece in &Piece::ALL {
            for &dir in &ALL_DIRS {
                for &from in &ALL_ROTS {
                    let offs = kick_offsets(dir, piece, from);
                    assert_eq!(offs[0], (0, 0), "{piece:?} {from:?} {dir:?} first kick");
                    let expected = if piece == Piece::O && dir != RotateDir::HalfTurn {
                        1
                    } else {
                        5
                    };
                    assert_eq!(offs.len(), expected, "{piece:?} {from:?} {dir:?}");
                }
            }
        }
        // O always succeeds on the first (only) trial.
        assert_eq!(
            kick_offsets(RotateDir::Clockwise, Piece::O, Rotation::Cw),
            &[(0, 0)]
        );
    }

    #[test]
    fn i_wall_kick_against_left_wall_at_floor() {
        // I lying on the floor flush with the left wall (cells row 21, cols
        // 0..=3). CW rotation to vertical needs the 5th SRS trial (+1, +2):
        // one right, two UP — row must DECREASE by 2 (y-up conversion).
        let mut board = Board::new();
        filled_rows(&mut board, &[ROWS - 1], &[0, 1, 2, 3]);
        let start = ps(Piece::I, Rotation::Spawn, ROWS as i32 - 2, 0);
        assert!(!board.collides(&start));
        let a = try_rotate(&board, &start, RotateDir::Clockwise).expect("I must kick off floor");
        assert_eq!(a.kick_index, 4);
        assert_eq!(a.state.rot, Rotation::Cw);
        assert_eq!(a.state.row, ROWS as i32 - 4);
        assert_eq!(a.state.col, 1);
        assert!(!board.collides(&a.state));
    }

    #[test]
    fn i_wall_kick_against_right_wall_at_floor() {
        // Mirror image: flush right wall (cols 6..=9), CCW rotation needs
        // the 4th SRS trial (-1, +2) — one left, two up.
        let mut board = Board::new();
        filled_rows(&mut board, &[ROWS - 1], &[6, 7, 8, 9]);
        let start = ps(Piece::I, Rotation::Spawn, ROWS as i32 - 2, 6);
        assert!(!board.collides(&start));
        let a =
            try_rotate(&board, &start, RotateDir::CounterClockwise).expect("I must kick off floor");
        assert_eq!(a.kick_index, 3);
        assert_eq!(a.state.rot, Rotation::Ccw);
        assert_eq!(a.state.row, ROWS as i32 - 4);
        assert_eq!(a.state.col, 5);
        assert!(!board.collides(&a.state));
    }

    #[test]
    fn jlstz_near_wall_kick_left_wall() {
        // T hugging the left wall in Cw (bounding box col -1, cells in
        // cols 0..=1). CW to R180 needs the 2nd JLSTZ trial (+1, 0).
        let board = Board::new();
        let start = ps(Piece::T, Rotation::Cw, 10, -1);
        assert!(!board.collides(&start));
        let a = try_rotate(&board, &start, RotateDir::Clockwise).expect("T must kick off wall");
        assert_eq!(a.kick_index, 1);
        assert_eq!(a.state.rot, Rotation::R180);
        assert_eq!(a.state.row, 10);
        assert_eq!(a.state.col, 0);
        assert!(!board.collides(&a.state));
    }

    #[test]
    fn o_rotation_is_noop_success_in_every_direction() {
        let board = Board::new();
        for &dir in &ALL_DIRS {
            for &from in &ALL_ROTS {
                let start = ps(Piece::O, from, 5, 4);
                let a = try_rotate(&board, &start, dir).expect("O always rotates");
                assert_eq!(a.kick_index, 0);
                assert_eq!(a.state.rot, target_rot(dir, from));
                assert_eq!(a.state.row, 5);
                assert_eq!(a.state.col, 4);
                assert_eq!(a.state.cells(), start.cells());
            }
        }
    }

    #[test]
    fn wedged_i_in_one_high_slit_returns_none_every_quarter_dir() {
        // Horizontal one-cell-high slit: I fits flat but no vertical
        // placement (needs 4 empty rows in a column) exists anywhere, and
        // no |x| <= 2 / |y| <= 2 kick rescues any rotation.
        let mut board = Board::new();
        filled_rows(&mut board, &[ROWS - 3], &[]);
        filled_rows(&mut board, &[ROWS - 2], &[0, 1, 2, 3]);
        filled_rows(&mut board, &[ROWS - 1], &[]);
        let start = ps(Piece::I, Rotation::Spawn, ROWS as i32 - 3, 0);
        assert!(!board.collides(&start));
        for &dir in &QUARTER_DIRS {
            for &from in &ALL_ROTS {
                let s = ps(Piece::I, from, start.row, start.col);
                if board.collides(&s) {
                    continue;
                }
                assert!(
                    try_rotate(&board, &s, dir).is_none(),
                    "I in 1-high slit must be wedged for {from:?} {dir:?}"
                );
            }
        }
    }

    #[test]
    fn half_turn_succeeds_at_index_zero_in_open_space() {
        let board = Board::new();
        for &piece in &Piece::ALL {
            for &from in &ALL_ROTS {
                let start = ps(piece, from, 4, 3);
                let a = try_rotate(&board, &start, RotateDir::HalfTurn).expect("open 180");
                assert_eq!(a.kick_index, 0, "{piece:?} {from:?}");
                assert_eq!(a.state.rot, from.half_turn());
                assert_eq!(a.state.row, 4);
                assert_eq!(a.state.col, 3);
            }
        }
    }

    #[test]
    fn half_turn_notched_slot_uses_guideline_kick() {
        // T pointing up over a pocket: 180 to point-down is blocked at the
        // notch (row 21, col 4 filled) but fits after the (-1, 0) nudge.
        let mut board = Board::new();
        filled_rows(&mut board, &[ROWS - 3], &[4]); // row 19: only col 4 open
        filled_rows(&mut board, &[ROWS - 2], &[2, 3, 4, 5]); // row 20: cols 2..=5 open
        filled_rows(&mut board, &[ROWS - 1], &[3]); // row 21: only col 3 open
        let start = ps(Piece::T, Rotation::Spawn, ROWS as i32 - 3, 3);
        assert!(!board.collides(&start));
        let a = try_rotate(&board, &start, RotateDir::HalfTurn).expect("180 must kick left");
        assert_eq!(a.kick_index, 1);
        assert_eq!(a.state.rot, Rotation::R180);
        assert_eq!(a.state.row, ROWS as i32 - 3);
        assert_eq!(a.state.col, 2);
        assert!(!board.collides(&a.state));
    }

    #[test]
    fn half_turn_wedged_in_chamber_returns_none() {
        // T filling a 2x3 chamber: the point-down target always pokes into
        // the floor or the filled shoulders; no guideline offset helps.
        let mut board = Board::new();
        filled_rows(&mut board, &[ROWS - 3], &[]);
        filled_rows(&mut board, &[ROWS - 2], &[4]);
        filled_rows(&mut board, &[ROWS - 1], &[3, 4, 5]);
        let start = ps(Piece::T, Rotation::Spawn, ROWS as i32 - 2, 3);
        assert!(!board.collides(&start));
        assert!(try_rotate(&board, &start, RotateDir::HalfTurn).is_none());
    }

    #[test]
    fn negative_rows_never_collide() {
        // T2 convention: cells with row < 0 are legal (top overhang), so a
        // rotation whose candidate pokes above the ceiling still succeeds.
        let board = Board::new();
        let overhang = ps(Piece::I, Rotation::Cw, -1, 3); // cells rows -1..=2
        assert!(!board.collides(&overhang));
        let a = try_rotate(&board, &overhang, RotateDir::Clockwise).expect("overhang rotates");
        assert_eq!(a.kick_index, 0);
        assert_eq!(a.state.row, -1);
        assert_eq!(a.state.cells(), [(1, 3), (1, 4), (1, 5), (1, 6)]);
    }
}
