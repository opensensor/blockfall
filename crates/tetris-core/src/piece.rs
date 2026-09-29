//! Tetromino geometry: `Piece`, `Rotation`, `PieceState`, SRS cell tables (T2).
//!
//! All piece geometry lives here. Cell tables are the pure SRS rotation
//! states (no kicks) — wall-kick offsets and `try_rotate` belong to `srs`
//! (T4), which consumes [`Piece::cells`] / [`Rotation::index`].
//!
//! Coordinate convention: cells are `(row, col)` offsets from the piece's
//! bounding-box top-left corner, rows grow downward. Boxes are 3x3 for
//! J/L/S/T/Z, 4x4 for I, 2x2 for O.

use serde::{Deserialize, Serialize};

/// The seven standard tetrominoes, plus the garbage filler used by the
/// versus `Match` (never dealt, never active, never rotated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Piece {
    I,
    J,
    L,
    O,
    S,
    T,
    Z,
    /// Versus garbage row filler (T24, `versus::push_garbage_rows`). Not in
    /// [`Piece::ALL`]: the 7-bag never deals it and no app path spawns it.
    Garbage,
}

impl Piece {
    /// All pieces, in canonical order.
    pub const ALL: [Piece; 7] = [
        Piece::I,
        Piece::J,
        Piece::L,
        Piece::O,
        Piece::S,
        Piece::T,
        Piece::Z,
    ];

    /// The four cells of this piece in rotation state `rot`, as
    /// `(row, col)` offsets within the piece's bounding box.
    ///
    /// Tables are the standard SRS states: spawn (0), CW (1), 180 (2),
    /// CCW (3). Rotation of `O` is a no-op in every state.
    pub fn cells(self, rot: Rotation) -> [(i32, i32); 4] {
        use Piece::*;
        use Rotation::*;
        match (self, rot) {
            (I, Spawn) => [(1, 0), (1, 1), (1, 2), (1, 3)],
            (I, Cw) => [(0, 2), (1, 2), (2, 2), (3, 2)],
            (I, R180) => [(2, 0), (2, 1), (2, 2), (2, 3)],
            (I, Ccw) => [(0, 1), (1, 1), (2, 1), (3, 1)],

            (J, Spawn) => [(0, 0), (1, 0), (1, 1), (1, 2)],
            (J, Cw) => [(0, 1), (0, 2), (1, 1), (2, 1)],
            (J, R180) => [(1, 0), (1, 1), (1, 2), (2, 2)],
            (J, Ccw) => [(0, 1), (1, 1), (2, 0), (2, 1)],

            (L, Spawn) => [(0, 2), (1, 0), (1, 1), (1, 2)],
            (L, Cw) => [(0, 1), (1, 1), (2, 1), (2, 2)],
            (L, R180) => [(1, 0), (1, 1), (1, 2), (2, 0)],
            (L, Ccw) => [(0, 0), (0, 1), (1, 1), (2, 1)],

            (O, _) => [(0, 0), (0, 1), (1, 0), (1, 1)],
            (Garbage, _) => [(0, 0), (0, 1), (1, 0), (1, 1)],

            (S, Spawn) => [(0, 1), (0, 2), (1, 0), (1, 1)],
            (S, Cw) => [(0, 1), (1, 1), (1, 2), (2, 2)],
            (S, R180) => [(1, 1), (1, 2), (2, 0), (2, 1)],
            (S, Ccw) => [(0, 0), (1, 0), (1, 1), (2, 1)],

            (T, Spawn) => [(0, 1), (1, 0), (1, 1), (1, 2)],
            (T, Cw) => [(0, 1), (1, 1), (1, 2), (2, 1)],
            (T, R180) => [(1, 0), (1, 1), (1, 2), (2, 1)],
            (T, Ccw) => [(0, 1), (1, 0), (1, 1), (2, 1)],

            (Z, Spawn) => [(0, 0), (0, 1), (1, 1), (1, 2)],
            (Z, Cw) => [(0, 2), (1, 1), (1, 2), (2, 1)],
            (Z, R180) => [(1, 0), (1, 1), (2, 1), (2, 2)],
            (Z, Ccw) => [(0, 1), (1, 0), (1, 1), (2, 0)],
        }
    }

    /// Side length of this piece's rotation bounding box.
    pub fn box_size(self) -> usize {
        match self {
            Piece::I => 4,
            Piece::O | Piece::Garbage => 2,
            _ => 3,
        }
    }
}

/// SRS rotation state: 0 = spawn, 1 = CW, 2 = 180, 3 = CCW.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    Spawn,
    Cw,
    R180,
    Ccw,
}

impl Rotation {
    /// Rotate 90 degrees clockwise, wrapping mod 4.
    pub fn clockwise(self) -> Rotation {
        use Rotation::*;
        match self {
            Spawn => Cw,
            Cw => R180,
            R180 => Ccw,
            Ccw => Spawn,
        }
    }

    /// Rotate 90 degrees counter-clockwise, wrapping mod 4.
    pub fn counter_clockwise(self) -> Rotation {
        use Rotation::*;
        match self {
            Spawn => Ccw,
            Ccw => R180,
            R180 => Cw,
            Cw => Spawn,
        }
    }

    /// Flip 180 degrees, wrapping mod 4.
    pub fn half_turn(self) -> Rotation {
        use Rotation::*;
        match self {
            Spawn => R180,
            Cw => Ccw,
            R180 => Spawn,
            Ccw => Cw,
        }
    }

    /// Numeric state index (0..=4), for kick-table indexing in T4.
    pub fn index(self) -> usize {
        match self {
            Rotation::Spawn => 0,
            Rotation::Cw => 1,
            Rotation::R180 => 2,
            Rotation::Ccw => 3,
        }
    }

    /// Inverse of [`Rotation::index`]; `None` for out-of-range indices.
    pub fn from_index(index: usize) -> Option<Rotation> {
        match index {
            0 => Some(Rotation::Spawn),
            1 => Some(Rotation::Cw),
            2 => Some(Rotation::R180),
            3 => Some(Rotation::Ccw),
            _ => None,
        }
    }
}

/// A piece on the playfield: shape, rotation, and the board position of
/// its bounding box's top-left corner. `row`/`col` may be negative (a
/// piece may legally overhang the top edge; see `Board::collides`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PieceState {
    pub piece: Piece,
    pub rot: Rotation,
    pub row: i32,
    pub col: i32,
}

impl PieceState {
    /// Absolute board coordinates of the piece's four cells.
    pub fn cells(&self) -> [(i32, i32); 4] {
        let mut out = [(0, 0); 4];
        for (i, (dr, dc)) in self.piece.cells(self.rot).iter().enumerate() {
            out[i] = (self.row + dr, self.col + dc);
        }
        out
    }
}

/// Guideline spawn state for `piece` on a standard-width playfield.
///
/// Convention (reused by T8): the bounding box is placed at `row = 0`, so
/// the piece's occupied cells land entirely inside the hidden buffer rows
/// `0..HIDDEN_ROWS` (J/L/S/T/Z and O fill both hidden rows; I fills row 1
/// only). Horizontally the box is centered: `col = (COLS - box_size) / 2`,
/// which yields guideline columns — I spans cols 3..=6, J/L/S/T/Z boxes
/// cols 3..=5, O cols 4..=5 (0-indexed).
pub fn spawn_state(piece: Piece) -> PieceState {
    let col = (crate::board::COLS as i32 - piece.box_size() as i32) / 2;
    PieceState {
        piece,
        rot: Rotation::Spawn,
        row: 0,
        col,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Board, COLS, HIDDEN_ROWS};

    fn assert_four_distinct(cells: &[(i32, i32); 4]) {
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(cells[i], cells[j], "duplicate cell {cells:?}");
            }
        }
    }

    fn box_size(piece: Piece) -> i32 {
        match piece {
            Piece::I => 4,
            Piece::O => 2,
            _ => 3,
        }
    }

    #[test]
    fn all_array_has_seven_unique_pieces() {
        assert_eq!(Piece::ALL.len(), 7);
        for (i, a) in Piece::ALL.iter().enumerate() {
            for b in &Piece::ALL[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn every_rotation_state_has_four_distinct_in_box_cells() {
        for &piece in &Piece::ALL {
            for &rot in &[Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
                let cells = piece.cells(rot);
                assert_four_distinct(&cells);
                let size = box_size(piece);
                for (dr, dc) in cells {
                    assert!(
                        dr >= 0 && dr < size,
                        "{piece:?} {rot:?} row {dr} out of {size}x{size} box"
                    );
                    assert!(
                        dc >= 0 && dc < size,
                        "{piece:?} {rot:?} col {dc} out of {size}x{size} box"
                    );
                }
            }
        }
    }

    #[test]
    fn cell_patterns_are_shape_preserving_across_rotations() {
        // Each rotation of a piece must cover the same cell-count footprint
        // (4) and JLSTZ/I tables must differ between 0 and 2 states.
        for &piece in &Piece::ALL {
            if piece == Piece::O {
                continue;
            }
            assert_ne!(piece.cells(Rotation::Spawn), piece.cells(Rotation::R180));
        }
    }

    #[test]
    fn o_rotation_is_noop() {
        let base = Piece::O.cells(Rotation::Spawn);
        assert_eq!(base, [(0, 0), (0, 1), (1, 0), (1, 1)]);
        for rot in [Rotation::Cw, Rotation::R180, Rotation::Ccw] {
            assert_eq!(Piece::O.cells(rot), base);
        }
    }

    #[test]
    fn srs_spawn_state_tables() {
        assert_eq!(
            Piece::T.cells(Rotation::Spawn),
            [(0, 1), (1, 0), (1, 1), (1, 2)]
        );
        assert_eq!(
            Piece::I.cells(Rotation::Spawn),
            [(1, 0), (1, 1), (1, 2), (1, 3)]
        );
        assert_eq!(
            Piece::J.cells(Rotation::Spawn),
            [(0, 0), (1, 0), (1, 1), (1, 2)]
        );
        assert_eq!(
            Piece::L.cells(Rotation::Spawn),
            [(0, 2), (1, 0), (1, 1), (1, 2)]
        );
        assert_eq!(
            Piece::S.cells(Rotation::Spawn),
            [(0, 1), (0, 2), (1, 0), (1, 1)]
        );
        assert_eq!(
            Piece::Z.cells(Rotation::Spawn),
            [(0, 0), (0, 1), (1, 1), (1, 2)]
        );
    }

    #[test]
    fn srs_side_state_tables_spot_check() {
        // Canonical SRS side states (subset; T4 relies on these exact tables).
        assert_eq!(
            Piece::J.cells(Rotation::Cw),
            [(0, 1), (0, 2), (1, 1), (2, 1)]
        );
        assert_eq!(
            Piece::J.cells(Rotation::R180),
            [(1, 0), (1, 1), (1, 2), (2, 2)]
        );
        assert_eq!(
            Piece::J.cells(Rotation::Ccw),
            [(0, 1), (1, 1), (2, 0), (2, 1)]
        );
        assert_eq!(
            Piece::I.cells(Rotation::Cw),
            [(0, 2), (1, 2), (2, 2), (3, 2)]
        );
        assert_eq!(
            Piece::I.cells(Rotation::Ccw),
            [(0, 1), (1, 1), (2, 1), (3, 1)]
        );
        assert_eq!(
            Piece::I.cells(Rotation::R180),
            [(2, 0), (2, 1), (2, 2), (2, 3)]
        );
        assert_eq!(
            Piece::T.cells(Rotation::Cw),
            [(0, 1), (1, 1), (1, 2), (2, 1)]
        );
        assert_eq!(
            Piece::S.cells(Rotation::Cw),
            [(0, 1), (1, 1), (1, 2), (2, 2)]
        );
        assert_eq!(
            Piece::Z.cells(Rotation::Ccw),
            [(0, 1), (1, 0), (1, 1), (2, 0)]
        );
    }

    #[test]
    fn rotation_default_is_spawn() {
        assert_eq!(Rotation::default(), Rotation::Spawn);
    }

    #[test]
    fn rotation_wrapping_arithmetic() {
        let mut r = Rotation::Spawn;
        for expected in [Rotation::Cw, Rotation::R180, Rotation::Ccw, Rotation::Spawn] {
            r = r.clockwise();
            assert_eq!(r, expected);
        }
        assert_eq!(Rotation::Spawn.counter_clockwise(), Rotation::Ccw);
        assert_eq!(Rotation::Ccw.counter_clockwise(), Rotation::R180);
        assert_eq!(Rotation::R180.counter_clockwise(), Rotation::Cw);
        assert_eq!(
            Rotation::Ccw
                .counter_clockwise()
                .counter_clockwise()
                .counter_clockwise(),
            Rotation::Spawn
        );
        for &start in &[Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
            assert_eq!(start.half_turn(), start.clockwise().clockwise());
            assert_eq!(start.half_turn().half_turn(), start);
            assert_eq!(start.clockwise().counter_clockwise(), start);
        }
    }

    #[test]
    fn rotation_index_roundtrip() {
        assert_eq!(Rotation::Spawn.index(), 0);
        assert_eq!(Rotation::Cw.index(), 1);
        assert_eq!(Rotation::R180.index(), 2);
        assert_eq!(Rotation::Ccw.index(), 3);
        for &r in &[Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
            assert_eq!(Rotation::from_index(r.index()), Some(r));
        }
        assert_eq!(Rotation::from_index(4), None);
        assert_eq!(Rotation::from_index(99), None);
    }

    #[test]
    fn piece_state_cells_offsets_bounding_box_origin() {
        let ps = PieceState {
            piece: Piece::I,
            rot: Rotation::Spawn,
            row: 2,
            col: 3,
        };
        assert_eq!(ps.cells(), [(3, 3), (3, 4), (3, 5), (3, 6)]);
        let ps = PieceState {
            piece: Piece::O,
            rot: Rotation::Spawn,
            row: -1,
            col: 0,
        };
        assert_eq!(ps.cells(), [(-1, 0), (-1, 1), (0, 0), (0, 1)]);
    }

    #[test]
    fn spawn_state_is_horizontally_centered() {
        // Guideline centers: I spans cols 3..=6, JLSTZ box cols 3..=5, O cols 4..=5.
        let i_cols: Vec<i32> = spawn_state(Piece::I)
            .cells()
            .iter()
            .map(|&(_, c)| c)
            .collect();
        assert_eq!(i_cols, [3, 4, 5, 6]);
        let o_cols: Vec<i32> = spawn_state(Piece::O)
            .cells()
            .iter()
            .map(|&(_, c)| c)
            .collect();
        assert_eq!(o_cols, [4, 5, 4, 5]);
        for &piece in &Piece::ALL {
            let ps = spawn_state(piece);
            for &(_, c) in &ps.cells() {
                assert!(c >= 0 && c < COLS as i32, "{piece:?} spawns out of bounds");
            }
        }
        let t = spawn_state(Piece::T);
        assert_eq!(t.col, 3);
        assert!(t.cells().iter().any(|&(_, c)| c == 3));
        assert!(t.cells().iter().any(|&(_, c)| c == 5));
    }

    #[test]
    fn spawn_state_lands_in_hidden_rows_on_empty_board() {
        let board = Board::new();
        for &piece in &Piece::ALL {
            let ps = spawn_state(piece);
            assert_eq!(ps.row, 0);
            for (r, _) in ps.cells() {
                assert!(
                    r >= 0 && r < HIDDEN_ROWS as i32,
                    "{piece:?} cell row {r} not in hidden band"
                );
            }
            assert!(
                !board.collides(&ps),
                "{piece:?} must not collide on empty board at spawn"
            );
        }
    }
    /// Regression (M3 playtest, shipped broken in v0.1.0): L's Cw/Ccw had
    /// their nub attached to the MIDDLE of the 3-cell bar — the shape of a
    /// T — so "rotating L turned it into a partial E". Nothing pinned the
    /// shape tables; now every rotation state must equal the pure matrix
    /// rotation of the spawn state within the piece's box.
    #[test]
    fn all_rotations_are_pure_rotations_of_spawn() {
        let n = 0; // silence unused warning path
        let _ = n;
        let rot_cw = |(r, c): (i32, i32), size: i32| (c, size - 1 - r);
        let rot_ccw = |(r, c): (i32, i32), size: i32| (size - 1 - c, r);
        let rot_180 = |(r, c): (i32, i32), size: i32| (size - 1 - r, size - 1 - c);
        let norm = |mut cells: [(i32, i32); 4]| {
            cells.sort_unstable();
            cells
        };
        for piece in Piece::ALL {
            let size = piece.box_size() as i32;
            let spawn = piece.cells(Rotation::Spawn);
            assert_eq!(
                norm(spawn.map(|p| rot_cw(p, size))),
                norm(piece.cells(Rotation::Cw)),
                "{piece:?} Cw is not a pure 90° CW rotation of spawn"
            );
            assert_eq!(
                norm(spawn.map(|p| rot_180(p, size))),
                norm(piece.cells(Rotation::R180)),
                "{piece:?} R180 is not a pure 180° rotation of spawn"
            );
            assert_eq!(
                norm(spawn.map(|p| rot_ccw(p, size))),
                norm(piece.cells(Rotation::Ccw)),
                "{piece:?} Ccw is not a pure 90° CCW rotation of spawn"
            );
        }
    }

    /// Explicit anchor for the M3 defect: rotated L stays an L (nub at the
    /// end of the bar), never a T (nub in the middle).
    #[test]
    fn l_piece_rotations_keep_nub_at_bar_end() {
        assert_eq!(
            Piece::L.cells(Rotation::Cw),
            [(0, 1), (1, 1), (2, 1), (2, 2)]
        );
        assert_eq!(
            Piece::L.cells(Rotation::Ccw),
            [(0, 0), (0, 1), (1, 1), (2, 1)]
        );
    }
}
