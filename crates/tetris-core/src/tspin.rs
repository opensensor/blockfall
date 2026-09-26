//! T-spin detection: 3-corner rule plus last-kick exception (T7).
//!
//! Pure classifier over the locked-but-not-yet-merged T piece state, the
//! settled board, and two bits of per-piece history that T8 must track:
//!
//! - `last_action_was_rotation` — `true` iff the most recent *successful*
//!   player action for this piece was a rotation. Any successful move or
//!   downward movement sets it to `false`.
//! - `last_kick_index` — [`srs::RotationAttempt::kick_index`](crate::srs)
//!   of the most recent successful rotation; reset to `0` whenever a move
//!   or drop succeeds (only the latest rotation's kick matters). Index `4`
//!   is the far/final SRS trial that relaxes the front-corner requirement.
//!
//! Classification (guideline 3-corner rule): look at the four diagonal
//! corners of the T's 3×3 bounding box around its center. A corner counts
//! as filled when it is occupied or projects onto a wall or the floor;
//! rows above the top edge (the hidden buffer) are open. With at least
//! three filled corners: both *front* corners (the point side) filled →
//! [`TSpinKind::Full`]; exactly one front corner with both back corners →
//! [`TSpinKind::Mini`], except that a lock landed by the far kick
//! (`last_kick_index == 4`) upgrades it to [`TSpinKind::Full`]. Only the
//! T piece can ever spin; a lock not preceded by a rotation is never a
//! T-spin.

use serde::{Deserialize, Serialize};

use crate::board::{Board, COLS, ROWS};
use crate::piece::{Piece, PieceState, Rotation};

/// Result of classifying a T-piece lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TSpinKind {
    /// Not a T-spin (includes every non-T piece).
    #[default]
    None,
    /// One front corner + both back corners (or far-kick rescue below).
    Mini,
    /// Both front corners filled, or a 3-corner lock landed by kick index 4.
    Full,
}

/// Diagonal corner offset `(dr, dc)` from a bounding-box top-left.
type Corner = (i32, i32);

/// Corner offsets from the T bounding-box top-left, with the two *front*
/// corners (on the side the nub points) first.
fn corners(rot: Rotation) -> ([Corner; 2], [Corner; 2]) {
    const TL: Corner = (0, 0);
    const TR: Corner = (0, 2);
    const BL: Corner = (2, 0);
    const BR: Corner = (2, 2);
    match rot {
        Rotation::Spawn => ([TL, TR], [BL, BR]), // nub up
        Rotation::Cw => ([TR, BR], [TL, BL]),    // nub right
        Rotation::R180 => ([BL, BR], [TL, TR]),  // nub down
        Rotation::Ccw => ([TL, BL], [TR, BR]),   // nub left
    }
}

/// A corner counts as filled when occupied or projected onto a wall or the
/// floor; coordinates above the top edge (`row < 0`, the spawn buffer) are
/// open sky.
fn corner_filled(board: &Board, row: i32, col: i32) -> bool {
    if col < 0 || col >= COLS as i32 || row >= ROWS as i32 {
        return true;
    }
    if row < 0 {
        return false;
    }
    board.get(row as usize, col as usize).is_some()
}

/// Classify a T-piece lock as a T-spin using the 3-corner rule plus the
/// last-kick exception (kick index 4 always counts as full-spin-capable).
///
/// Inputs are the board *before* merging the piece (the T's own cells are
/// not corners, so pre/post-merge is equivalent for this classifier), and
/// the two history bits documented at the top of this module — see
/// [`TSpinKind`] and the module docs for what T8 must maintain.
pub fn detect_tspin(
    board: &Board,
    ps: &PieceState,
    last_action_was_rotation: bool,
    last_kick_index: u8,
) -> TSpinKind {
    if ps.piece != Piece::T || !last_action_was_rotation {
        return TSpinKind::None;
    }
    let (front, back) = corners(ps.rot);
    let filled = |c: (i32, i32)| corner_filled(board, ps.row + c.0, ps.col + c.1);
    let front_count = front.iter().filter(|&&c| filled(c)).count();
    let back_count = back.iter().filter(|&&c| filled(c)).count();
    if front_count + back_count < 3 {
        return TSpinKind::None;
    }
    if front_count == 2 || last_kick_index == 4 {
        TSpinKind::Full
    } else {
        TSpinKind::Mini
    }
}

#[cfg(test)]
mod tests {
    use super::detect_tspin;
    use super::TSpinKind;
    use crate::board::{Board, COLS, ROWS};
    use crate::piece::{Piece, PieceState, Rotation};

    fn ps(piece: Piece, rot: Rotation, row: i32, col: i32) -> PieceState {
        PieceState {
            piece,
            rot,
            row,
            col,
        }
    }

    /// Fill the four diagonal corners around a T at box `(15, 3)` except
    /// the ones listed in `open` (offsets `(dr, dc)` from box top-left).
    fn with_corners(open: &[(i32, i32)]) -> Board {
        let mut board = Board::new();
        for (dr, dc) in [(0, 0), (0, 2), (2, 0), (2, 2)] {
            if !open.contains(&(dr, dc)) {
                board.set((15 + dr) as usize, (3 + dc) as usize, Some(Piece::I));
            }
        }
        board
    }

    #[test]
    fn non_t_piece_is_never_a_tspin() {
        let board = with_corners(&[]);
        for &piece in &Piece::ALL {
            if piece == Piece::T {
                continue;
            }
            for &rot in &[Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
                for &kick in &[0u8, 4] {
                    assert_eq!(
                        detect_tspin(&board, &ps(piece, rot, 15, 3), true, kick),
                        TSpinKind::None,
                        "{piece:?} {rot:?} kick {kick} must not be a T-spin"
                    );
                }
            }
        }
    }

    #[test]
    fn lock_without_preceding_rotation_is_none() {
        // All four corners filled, but the last action before lock was a
        // drop/move, not a rotation: never a T-spin.
        let board = with_corners(&[]);
        let state = ps(Piece::T, Rotation::R180, 15, 3);
        assert_eq!(detect_tspin(&board, &state, false, 0), TSpinKind::None);
        assert_eq!(detect_tspin(&board, &state, false, 4), TSpinKind::None);
    }

    #[test]
    fn fewer_than_three_corners_is_none() {
        // Only the two front corners filled (T pointing down, bottom corners).
        let board = with_corners(&[(0, 0), (0, 2)]);
        let state = ps(Piece::T, Rotation::R180, 15, 3);
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::None);
        // Even the far kick cannot rescue a 2-corner lock.
        assert_eq!(detect_tspin(&board, &state, true, 4), TSpinKind::None);
    }

    #[test]
    fn tsd_fixture_both_front_corners_is_full() {
        // Classic TSD: T rotated into the slot pointing down (R180). Both
        // front (bottom) corners + one back corner filled -> Full.
        let board = with_corners(&[(0, 2)]);
        let state = ps(Piece::T, Rotation::R180, 15, 3);
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Full);
    }

    #[test]
    fn tst_fixture_pointing_right_is_full() {
        // TST-style entry: T pointing right (Cw); front corners are the two
        // on the right side. Both front + one back filled -> Full.
        let board = with_corners(&[(2, 0)]);
        let state = ps(Piece::T, Rotation::Cw, 15, 3);
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Full);
        // All four corners -> also Full.
        let board = with_corners(&[]);
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Full);
    }

    #[test]
    fn front_one_back_two_is_mini_below_far_kick() {
        // 3-corner rule satisfied but only one front corner (back pair
        // filled): Mini for every kick trial except the far kick.
        let board = with_corners(&[(2, 0)]); // R180: bottom-left front open
        let state = ps(Piece::T, Rotation::R180, 15, 3);
        for &kick in &[0u8, 1, 2, 3] {
            assert_eq!(
                detect_tspin(&board, &state, true, kick),
                TSpinKind::Mini,
                "kick {kick} must stay Mini"
            );
        }
    }

    #[test]
    fn far_kick_index_4_upgrades_mini_to_full() {
        // Last-kick exception: a lock landed by the 5th SRS trial (index 4)
        // relaxes the front-corner requirement -> Full.
        let board = with_corners(&[(2, 0)]);
        let state = ps(Piece::T, Rotation::R180, 15, 3);
        assert_eq!(detect_tspin(&board, &state, true, 4), TSpinKind::Full);
    }

    #[test]
    fn wall_and_floor_corners_count_as_filled() {
        // T pointing right (Cw) with its bounding box at col -1: the two
        // BACK (left) corners project to col -1 (wall = filled) while the
        // piece cells themselves stay in bounds. One front corner filled
        // makes only a Mini; filling both front corners upgrades to Full.
        let mut board = Board::new();
        board.set(15, 1, Some(Piece::I)); // front-top corner
        let state = ps(Piece::T, Rotation::Cw, 15, -1);
        assert!(!board.collides(&state));
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Mini);
        board.set(17, 1, Some(Piece::I)); // front-bottom corner
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Full);

        // T pointing up (Spawn) resting on the floor at box row ROWS-2: the
        // two BACK (bottom) corners project below the floor (filled). One
        // front (top) corner -> Mini, both -> Full.
        let mut board = Board::new();
        board.set(ROWS as i32 as usize - 2, 3, Some(Piece::I));
        let state = ps(Piece::T, Rotation::Spawn, ROWS as i32 - 2, 3);
        assert!(!board.collides(&state));
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Mini);
        board.set(ROWS as i32 as usize - 2, 5, Some(Piece::I));
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::Full);
    }

    #[test]
    fn corners_above_the_top_edge_are_open() {
        // T box at row -1 in R180: both back corners are at row -1 (above
        // the field) and do NOT count. Even with both front (bottom) corners
        // filled that is only 2 corners -> None.
        let mut board = Board::new();
        board.set(1, 3, Some(Piece::I));
        board.set(1, 5, Some(Piece::I));
        let state = ps(Piece::T, Rotation::R180, -1, 3);
        assert_eq!(detect_tspin(&board, &state, true, 0), TSpinKind::None);
    }

    #[test]
    fn mini_requires_rotation_and_t_and_is_stable_across_rotations() {
        // Mini classification works in every rotation state given the right
        // corner set (one front + two back), and requires last rotation.
        let cases = [
            (Rotation::Spawn, [(2, 0), (2, 2), (0, 2)]), // front = top pair, one open
            (Rotation::Cw, [(0, 0), (2, 0), (2, 2)]),    // front = right pair
            (Rotation::R180, [(0, 0), (0, 2), (2, 2)]),  // front = bottom pair
            (Rotation::Ccw, [(0, 0), (0, 2), (2, 2)]),   // front = left pair
        ];
        for (rot, filled) in cases {
            let mut board = Board::new();
            for (dr, dc) in filled {
                board.set((15 + dr) as usize, (3 + dc) as usize, Some(Piece::I));
            }
            let state = ps(Piece::T, rot, 15, 3);
            assert_eq!(
                detect_tspin(&board, &state, true, 1),
                TSpinKind::Mini,
                "{rot:?}"
            );
            assert_eq!(detect_tspin(&board, &state, false, 1), TSpinKind::None);
        }
        assert_eq!(COLS, 10);
    }
}
