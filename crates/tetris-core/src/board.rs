//! 10x22 cell grid with 2 hidden spawn rows: collision/merge/line-clear/ghost (T2).
//!
//! Row 0 is the top of the playfield; rows `0..HIDDEN_ROWS` are the hidden
//! spawn buffer and rows `HIDDEN_ROWS..ROWS` are visible (PRD 6.1). The
//! board is pure state — no RNG, no timing.

use serde::{Deserialize, Serialize};

use crate::piece::{Piece, PieceState};

/// Number of columns on the playfield.
pub const COLS: usize = 10;
/// Total rows including the hidden spawn buffer.
pub const ROWS: usize = 22;
/// Rows above the visible field (rows `0..HIDDEN_ROWS`) reserved for spawn.
pub const HIDDEN_ROWS: usize = 2;

type Row = [Option<Piece>; COLS];

/// Grid of settled cells; each cell is empty or holds the piece that
/// locked there (kept for rendering color).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    grid: [Row; ROWS],
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    /// Empty playfield.
    pub fn new() -> Self {
        Self {
            grid: [[None; COLS]; ROWS],
        }
    }

    /// True when no cell is occupied.
    pub fn is_empty(&self) -> bool {
        self.grid.iter().all(|row| row.iter().all(Option::is_none))
    }

    /// Collision test for `state`: true if any *alive* cell is out of
    /// bounds horizontally, at or below the floor, or overlaps an occupied
    /// cell. Cells with `row < 0` are NOT a collision — pieces may overhang
    /// the top edge, which is what makes block-out detection possible in T8.
    /// Dead cells (chipped by the future cannon mechanic) are ignored.
    pub fn collides(&self, state: &PieceState) -> bool {
        state.alive_cells().any(|(r, c)| {
            c < 0
                || c >= COLS as i32
                || r >= ROWS as i32
                || (r >= 0 && self.grid[r as usize][c as usize].is_some())
        })
    }

    /// Stamp `state`'s alive cells into the grid. Caller must ensure the
    /// state does not collide; out-of-range cells (e.g. overhanging rows `row<0`)
    /// are silently skipped rather than panicking. Dead cells leave holes.
    pub fn merge(&mut self, state: &PieceState) {
        for (r, c) in state.alive_cells() {
            if r >= 0 && r < ROWS as i32 && c >= 0 && c < COLS as i32 {
                self.grid[r as usize][c as usize] = Some(state.piece);
            }
        }
    }

    /// Indices of completely filled rows, ascending.
    pub fn full_rows(&self) -> Vec<usize> {
        self.grid
            .iter()
            .enumerate()
            .filter(|(_, row)| row.iter().all(Option::is_some))
            .map(|(r, _)| r)
            .collect()
    }

    /// Remove all full rows and shift surviving rows down to close the
    /// gaps. In-place; returns the number of rows cleared (0 = no-op).
    /// Rows above several cleared lines drop by the number of cleared
    /// lines below them, preserving their relative order.
    pub fn clear_full_rows(&mut self) -> usize {
        let full = self.full_rows();
        if full.is_empty() {
            return 0;
        }
        let mut shifted = [[None; COLS]; ROWS];
        // Fill `shifted` bottom-up from surviving source rows; rows above
        // the last survivor stay None, so the write cursor never underflows.
        let mut dst = ROWS - 1;
        for src in (0..ROWS).rev() {
            if full.contains(&src) {
                continue;
            }
            shifted[dst] = self.grid[src];
            dst -= 1;
        }
        self.grid = shifted;
        full.len()
    }

    /// Read a single cell; `None` for out-of-range coordinates. Exposed
    /// for tests, fixtures, and the T8 snapshot.
    pub fn get(&self, row: usize, col: usize) -> Option<Piece> {
        self.grid.get(row)?.get(col).copied().flatten()
    }

    /// Write a single cell; out-of-range coordinates are ignored. Fixture
    /// helper (tests, T8 restart-from-snapshot); prefer `merge` in the
    /// normal lock path.
    pub fn set(&mut self, row: usize, col: usize, cell: Option<Piece>) {
        if let Some(slot) = self.grid.get_mut(row).and_then(|r| r.get_mut(col)) {
            *slot = cell;
        }
    }
}

/// Lowest valid `row` for `state` moving straight down from its current
/// position (the piece rests there; same rotation/col). Never panics: if
/// the input state already collides it is returned unchanged, and if the
/// piece is below the floor it also rests at its current row.
pub fn ghost_row(board: &Board, state: &PieceState) -> i32 {
    let mut row = state.row;
    loop {
        let next = PieceState {
            row: row + 1,
            ..*state
        };
        if board.collides(&next) {
            return row;
        }
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::{spawn_state, Rotation, FULL_CELLS};

    fn state(piece: Piece, rot: Rotation, row: i32, col: i32) -> PieceState {
        PieceState {
            piece,
            rot,
            row,
            col,
            alive: FULL_CELLS,
        }
    }

    /// Fill board row `r` completely (optionally leaving `gap` col empty).
    fn fill_row(board: &mut Board, r: usize, gap: Option<usize>) {
        for c in 0..COLS {
            if Some(c) != gap {
                board.set(r, c, Some(Piece::Z));
            }
        }
    }

    #[test]
    fn new_board_is_empty() {
        let board = Board::new();
        assert!(board.is_empty());
        assert_eq!(COLS, 10);
        assert_eq!(ROWS, 22);
        assert_eq!(HIDDEN_ROWS, 2);
    }

    #[test]
    fn merge_marks_cells_and_clears_empty_flag() {
        let mut board = Board::new();
        board.merge(&spawn_state(Piece::O));
        assert!(!board.is_empty());
        assert_eq!(board.get(0, 4), Some(Piece::O));
        assert_eq!(board.get(1, 5), Some(Piece::O));
        assert_eq!(board.get(1, 3), None);
    }

    #[test]
    fn spawn_states_do_not_collide_on_empty_board() {
        let board = Board::new();
        for &piece in &Piece::ALL {
            assert!(!board.collides(&spawn_state(piece)));
        }
    }

    #[test]
    fn collides_with_left_and_right_walls() {
        let board = Board::new();
        // Vertical I (Cw) has cells at dx=2; box col -3 puts a cell at col -1.
        assert!(board.collides(&state(Piece::I, Rotation::Cw, 0, -3)));
        // O at box col 9 spills to col 10.
        assert!(board.collides(&state(Piece::O, Rotation::Spawn, 5, 9)));
        // Just-inside variants are fine.
        assert!(!board.collides(&state(Piece::I, Rotation::Cw, 0, -2)));
        assert!(!board.collides(&state(Piece::O, Rotation::Spawn, 5, 8)));
    }

    #[test]
    fn collides_with_floor_but_not_ceiling() {
        let board = Board::new();
        // Vertical I cells occupy box rows 0..=3; box row ROWS-3 puts one cell on row ROWS.
        assert!(board.collides(&state(Piece::I, Rotation::Cw, ROWS as i32 - 3, 0)));
        // One row higher: lowest cell sits on the last board row -> valid.
        assert!(!board.collides(&state(Piece::I, Rotation::Cw, ROWS as i32 - 4, 0)));
        // row < 0 is explicitly NOT a collision (block-out detection).
        assert!(!board.collides(&state(Piece::T, Rotation::Spawn, -2, 3)));
        assert!(!board.collides(&state(Piece::I, Rotation::Spawn, -1, 3)));
    }

    #[test]
    fn collides_with_existing_stack() {
        let mut board = Board::new();
        board.merge(&state(Piece::O, Rotation::Spawn, 20, 4));
        assert!(board.collides(&state(Piece::O, Rotation::Spawn, 20, 4)));
        assert!(board.collides(&state(Piece::T, Rotation::Spawn, 19, 3)));
        assert!(!board.collides(&state(Piece::T, Rotation::Spawn, 18, 3)));
    }

    #[test]
    fn merge_ignores_out_of_range_cells_without_panicking() {
        let mut board = Board::new();
        // T straddling the top edge: box row -1 -> top cell on row -1 is skipped.
        board.merge(&state(Piece::T, Rotation::Spawn, -1, 3));
        assert_eq!(board.get(0, 4), Some(Piece::T));
        assert_eq!(board.get(0, 3), Some(Piece::T));
        let mut far = Board::new();
        far.merge(&state(Piece::O, Rotation::Spawn, 100, 100)); // fully OOB
        assert!(far.is_empty());
    }

    #[test]
    fn full_rows_detects_complete_rows_only() {
        let mut board = Board::new();
        assert!(board.full_rows().is_empty());
        fill_row(&mut board, 21, None);
        assert_eq!(board.full_rows(), vec![21]);
        fill_row(&mut board, 20, Some(5));
        assert_eq!(board.full_rows(), vec![21]);
        board.set(20, 5, Some(Piece::S));
        assert_eq!(board.full_rows(), vec![20, 21]);
    }

    #[test]
    fn clear_full_rows_removes_and_shifts_single_row() {
        let mut board = Board::new();
        // Marker above the full line drops one row; line disappears.
        fill_row(&mut board, 21, None);
        board.set(17, 0, Some(Piece::S));
        assert_eq!(board.clear_full_rows(), 1);
        assert!(board.full_rows().is_empty());
        assert_eq!(board.get(18, 0), Some(Piece::S));
        assert_eq!(board.get(17, 0), None);
        // Survivors were rows 0..=20 shifted down by 1, so row 21 holds
        // old row 20 (empty) and the cleared line is gone.
        assert!((0..COLS).all(|c| board.get(21, c).is_none()));
    }

    #[test]
    fn clear_full_rows_shift_ordering_with_two_non_adjacent_lines() {
        let mut board = Board::new();
        fill_row(&mut board, 19, None);
        fill_row(&mut board, 21, None);
        board.set(17, 2, Some(Piece::T)); // above both lines -> drops by 2
        board.set(20, 6, Some(Piece::L)); // between the lines -> drops by 1
        assert_eq!(board.clear_full_rows(), 2);
        assert_eq!(board.get(19, 2), Some(Piece::T));
        assert_eq!(board.get(17, 2), None);
        assert_eq!(board.get(21, 6), Some(Piece::L));
        assert_eq!(board.get(20, 6), None);
        assert!(board.full_rows().is_empty());
    }

    #[test]
    fn clear_full_rows_noop_returns_zero() {
        let mut board = Board::new();
        board.set(21, 0, Some(Piece::J));
        assert_eq!(board.clear_full_rows(), 0);
        assert_eq!(board.get(21, 0), Some(Piece::J));
    }

    #[test]
    fn clear_full_rows_leaves_other_stack_content_in_place() {
        let mut board = Board::new();
        fill_row(&mut board, 21, None);
        board.merge(&state(Piece::O, Rotation::Spawn, 20, 0)); // O on 20 cols 0-1, partial row
        assert_eq!(board.clear_full_rows(), 1);
        // Row 21 was full and removed; survivors (incl. the O on row 20) shift down.
        assert_eq!(board.get(21, 0), Some(Piece::O));
        assert_eq!(board.get(21, 1), Some(Piece::O));
        assert_eq!(board.get(20, 0), None);
    }

    #[test]
    fn ghost_row_rests_on_floor() {
        let board = Board::new();
        // Horizontal I: cells at box row+1; floor => box row ROWS-2.
        assert_eq!(ghost_row(&board, &spawn_state(Piece::I)), ROWS as i32 - 2);
        // T spawn cells span box rows 0..=1; floor => box row ROWS-2.
        assert_eq!(ghost_row(&board, &spawn_state(Piece::T)), ROWS as i32 - 2);
        // Vertical I: cells at box rows 0..=3; floor => box row ROWS-4.
        assert_eq!(
            ghost_row(&board, &state(Piece::I, Rotation::Cw, 0, 2)),
            ROWS as i32 - 4
        );
    }

    #[test]
    fn ghost_row_rests_on_stack_fixture() {
        let mut board = Board::new();
        board.set(21, 4, Some(Piece::O)); // bump at row 21 cols 4-5
        board.set(21, 5, Some(Piece::O));
        // T centered at spawn col 3: bottom row cols 3-5 overlaps the bump when
        // placed on rows 20-21, so the piece rests one row higher.
        assert_eq!(ghost_row(&board, &spawn_state(Piece::T)), 19);
    }

    #[test]
    fn ghost_row_falls_through_gap_fixture() {
        let mut board = Board::new();
        fill_row(&mut board, 21, Some(0)); // floor row with a gap at col 0
                                           // Vertical I over col 0 (box col -2 puts cells on col 0) drops to the floor.
        assert_eq!(
            ghost_row(&board, &state(Piece::I, Rotation::Cw, 0, -2)),
            ROWS as i32 - 4
        );
        // Vertical I over the filled part (box col -1 -> cells on col 1) rests one row above.
        assert_eq!(
            ghost_row(&board, &state(Piece::I, Rotation::Cw, 0, -1)),
            ROWS as i32 - 5
        );
    }

    #[test]
    fn ghost_row_on_already_colliding_state_returns_its_row() {
        let mut board = Board::new();
        board.merge(&state(Piece::O, Rotation::Spawn, 20, 4));
        let stuck = state(Piece::O, Rotation::Spawn, 20, 4);
        assert_eq!(ghost_row(&board, &stuck), 20);
        let below_floor = state(Piece::O, Rotation::Spawn, ROWS as i32, 4);
        assert_eq!(ghost_row(&board, &below_floor), ROWS as i32);
    }

    #[test]
    fn collides_ignores_dead_cells() {
        let mut board = Board::new();
        // T spawn cells: slot 0 is the nub at (row+0, col+1). Box (20, 3)
        // puts the nub on (20, 4); plant a blocker exactly there.
        board.set(20, 4, Some(Piece::Z));
        let full = state(Piece::T, Rotation::Spawn, 20, 3);
        assert!(board.collides(&full), "intact nub must collide");
        // Chip the nub (slot 0): the piece fits into the notch.
        let chipped = PieceState {
            alive: FULL_CELLS & !(1 << 0),
            ..full
        };
        assert!(!board.collides(&chipped), "dead nub must not collide");
        // Alive cells may still collide: with the nub dead, a blocker under
        // the still-alive bar cell (slot 2 at (21,5)) must be caught.
        let bar_hit = state(Piece::T, Rotation::Spawn, 20, 4); // cells (20,5) + bar 4..=6 row 21
        board.set(21, 5, Some(Piece::Z));
        let dead_wrong_slot = PieceState {
            alive: FULL_CELLS & !(1 << 0),
            ..bar_hit
        };
        assert!(
            board.collides(&dead_wrong_slot),
            "alive bar cell under a blocker must still collide"
        );
    }

    #[test]
    fn merge_skips_dead_cells_leaving_a_hole() {
        let mut board = Board::new();
        // T at box (20, 3) with the nub (slot 0, board cell (20,4)) dead.
        let chipped = PieceState {
            alive: FULL_CELLS & !(1 << 0),
            ..state(Piece::T, Rotation::Spawn, 20, 3)
        };
        board.merge(&chipped);
        assert_eq!(board.get(20, 4), None, "dead nub must leave a hole");
        assert_eq!(board.get(21, 3), Some(Piece::T));
        assert_eq!(board.get(21, 4), Some(Piece::T));
        assert_eq!(board.get(21, 5), Some(Piece::T));
        assert!(!board.full_rows().contains(&21), "row 21 is 3 cells only");
    }

    #[test]
    fn ghost_row_honours_the_alive_mask() {
        let mut board = Board::new();
        // T in Cw at box col 3: column cells (r,4),(r+1,4),(r+2,4) plus the
        // nub (slot 2) at (r+1,5). A blocker at (20,5) is touched only by
        // the nub: the intact piece rests one row higher than the chipped
        // one, which sinks its (dead) nub cell onto the blocker.
        board.set(20, 5, Some(Piece::Z));
        let intact = state(Piece::T, Rotation::Cw, 0, 3);
        assert_eq!(ghost_row(&board, &intact), 18);
        let chipped = PieceState {
            alive: FULL_CELLS & !(1 << 2),
            ..intact
        };
        assert_eq!(ghost_row(&board, &chipped), 19);
    }
}
