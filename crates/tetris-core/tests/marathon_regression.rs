//! T1 golden regression: pin Marathon behavior before any mode work
//! (game-modes-plan.md).
//!
//! For three fixed seeds a deterministic scripted driver plays to top-out
//! through the public `Game` API only: each piece cycles hold (every 4th
//! piece), rotation, column moves, an optional soft drop (every 3rd piece),
//! a hard drop and fixed gravity ticks, with placements chosen by a
//! greedy snapshot solver — the same pattern as `core_bridge`'s bot,
//! reimplemented here so tetris-core stays dependency-light and the whole
//! action log is reproducible from the seed. Across the golden seeds the log
//! exercises every `Action` variant, hold swaps, line clears, scoring,
//! combos and the level curve.
//!
//! The final `GameSnapshot` is pinned by three layers:
//!
//! 1. exact scalar fields (score, level, lines, combo, b2b, game_over, hold
//!    slot + flag, next queue) plus an FNV-1a-64 digest over the 10×22 board
//!    cells (a readable stand-in for hand-transcribing the grid),
//! 2. the exact bincode byte length of the serialized snapshot,
//! 3. an FNV-1a-64 hash over those exact bytes.
//!
//! Layers 2–3 are the wire-shape canary: they mirror
//! `tetris_app::core_bridge::net::protocol` — the same
//! `bincode::DefaultOptions::new().with_fixint_encoding()
//! .reject_trailing_bytes()` codec and the same FNV-1a-64 loop as its
//! `snapshot_hash` — reimplemented locally because tetris-core must not
//! depend on the app. Any change to Marathon rules or to the serialized
//! shape of `GameSnapshot` flips one of these goldens; that is the R1 gate's
//! "the same seed and action log give an identical final snapshot before and
//! after R1" criterion. The values below were captured from the unmodified
//! core (v0.3.2): regenerate the `Golden` literals, do not bend the driver.

use std::collections::{HashSet, VecDeque};

use bincode::Options;

use tetris_core::actions::Action;
use tetris_core::board::{self, Board, COLS, ROWS};
use tetris_core::event::GameEvent;
use tetris_core::game::{Game, GameSnapshot};
use tetris_core::piece::{Piece, PieceState, Rotation};

// --- wire-shape helpers (mirrors of tetris-app's protocol codec) ----------

/// Same codec as `protocol::codec()`: fixed-integer encoding (byte-identical
/// to `bincode::serialize` defaults) with trailing bytes rejected.
fn wire_bytes(snapshot: &GameSnapshot) -> Vec<u8> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
        .serialize(snapshot)
        .expect("GameSnapshot serialization is infallible")
}

/// FNV-1a (64-bit), the exact loop of `protocol::snapshot_hash` (stable
/// across runs, builds and platforms for identical bytes).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stable digest over the settled 10×22 grid (row-major), encoding each cell
/// as one byte so the board is pinned without transcribing 220 cells into
/// the assertion.
fn board_digest(board: &Board) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for row in 0..ROWS {
        for col in 0..COLS {
            let code: u8 = match board.get(row, col) {
                None => 0,
                Some(Piece::I) => 1,
                Some(Piece::J) => 2,
                Some(Piece::L) => 3,
                Some(Piece::O) => 4,
                Some(Piece::S) => 5,
                Some(Piece::T) => 6,
                Some(Piece::Z) => 7,
                Some(Piece::Garbage) => 8,
            };
            hash ^= u64::from(code);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

// --- scripted drive ---------------------------------------------------------

/// Gravity frames run between piece cycles.
const GRAVITY_TICKS_PER_PIECE: usize = 2;

/// Hard bound on the scripted rush: reaching top-out must happen well
/// inside this (observed: 37–1248 cycles for the golden seeds).
const MAX_PIECE_CYCLES: usize = 3000;

/// Every `Action` variant; the golden driver must have issued all of them.
const ALL_ACTIONS: [Action; 8] = [
    Action::MoveLeft,
    Action::MoveRight,
    Action::SoftDrop,
    Action::HardDrop,
    Action::RotateCw,
    Action::RotateCcw,
    Action::Rotate180,
    Action::Hold,
];

/// Placement metrics for the greedy solver over a board of settled cells:
/// `(aggregate column height, holes, bumpiness)`. Same weights as
/// `core_bridge`'s bot so placement quality is comparable.
fn stack_metrics(board: &Board) -> (i32, i32, i32) {
    let mut agg = 0;
    let mut holes = 0;
    let mut heights = [0i32; COLS];
    for (col, height) in heights.iter_mut().enumerate() {
        let mut filled_seen = false;
        for row in 0..ROWS {
            if board.get(row, col).is_some() {
                if !filled_seen {
                    filled_seen = true;
                    *height = (ROWS - row) as i32;
                }
            } else if filled_seen {
                holes += 1;
            }
        }
        agg += *height;
    }
    let bump = heights.windows(2).map(|w| (w[0] - w[1]).abs()).sum();
    (agg, holes, bump)
}

/// All resting placements `(row, col)` of `base` (one fixed rotation) the
/// piece can reach by sliding and falling — BFS over non-colliding
/// `(row, col)` states, collecting resting states (`ghost_row == row`).
fn reachable_placements(board: &Board, base: PieceState) -> Vec<(i32, i32)> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    seen.insert((base.row, base.col));
    queue.push_back(base);
    let mut out = Vec::new();
    while let Some(s) = queue.pop_front() {
        if board::ghost_row(board, &s) == s.row {
            out.push((s.row, s.col));
        }
        for next in [
            PieceState {
                row: s.row + 1,
                ..s
            },
            PieceState {
                col: s.col - 1,
                ..s
            },
            PieceState {
                col: s.col + 1,
                ..s
            },
        ] {
            if !board.collides(&next) && seen.insert((next.row, next.col)) {
                queue.push_back(next);
            }
        }
    }
    out
}

/// Greedy snapshot solver: every rotation × every reachable resting slot,
/// scored by `(cleared, holes, aggregate height, bumpiness)` with the bot's
/// weights. `None` when no placement fits (block-out imminent).
fn best_move(snapshot: &GameSnapshot) -> Option<(Rotation, i32)> {
    let active = snapshot.active?;
    let mut best: Option<((i32, i32), (Rotation, i32))> = None;
    for rot in [Rotation::Spawn, Rotation::Cw, Rotation::R180, Rotation::Ccw] {
        let base = PieceState {
            piece: active.piece,
            rot,
            row: active.row,
            col: active.col,
        };
        if snapshot.board.collides(&base) {
            continue;
        }
        for (row, col) in reachable_placements(&snapshot.board, base) {
            let placed = PieceState { row, col, ..base };
            let mut sim = snapshot.board.clone();
            sim.merge(&placed);
            let cleared = sim.full_rows().len() as i32;
            sim.clear_full_rows();
            let (agg, holes, bump) = stack_metrics(&sim);
            let score = cleared * 4500 - holes * 500 - agg * 25 - bump * 12;
            let key = (score, -(col - active.col).abs());
            if best.is_none_or(|(best_key, _)| key > best_key) {
                best = Some((key, (rot, col)));
            }
        }
    }
    best.map(|(_, mv)| mv)
}

struct RunResult {
    snapshot: GameSnapshot,
    cycles: usize,
    holds: usize,
    lines_from_events: u32,
    actions_used: HashSet<Action>,
}

struct Driver<'g> {
    game: &'g mut Game,
    holds: usize,
    lines_from_events: u32,
    actions_used: HashSet<Action>,
}

impl Driver<'_> {
    fn act(&mut self, action: Action) {
        self.actions_used.insert(action);
        for event in self.game.apply(action) {
            match event {
                GameEvent::HoldPerformed { .. } => self.holds += 1,
                GameEvent::LineCleared { lines } => self.lines_from_events += lines as u32,
                _ => {}
            }
        }
    }
}

/// Drive `seed` with the scripted driver until `game_over`, recording the
/// action coverage and event-side counters used for cross-checks.
fn drive(seed: u64) -> RunResult {
    let mut game = Game::new(seed);
    let mut d = Driver {
        game: &mut game,
        holds: 0,
        lines_from_events: 0,
        actions_used: HashSet::new(),
    };
    let mut cycles = 0usize;
    loop {
        // Hold press every 4th piece (one press per piece; rejected second
        // presses are silent no-ops anyway).
        if cycles % 4 == 3 {
            d.act(Action::Hold);
        }
        let snapshot = d.game.snapshot();
        if let Some(active) = snapshot.active {
            if let Some((rot, target_col)) = best_move(&snapshot) {
                let rot_delta = (rot.index() + 4 - active.rot.index()) % 4;
                match rot_delta {
                    1 => d.act(Action::RotateCw),
                    2 => d.act(Action::Rotate180),
                    3 => d.act(Action::RotateCcw),
                    // Keep the log rotating even when spawn is already the
                    // best state: a CW/CCW round trip returns to spawn.
                    _ => {
                        if cycles % 5 == 2 {
                            d.act(Action::RotateCw);
                            d.act(Action::RotateCcw);
                        }
                    }
                }
                if let Some(now) = d.game.snapshot().active.map(|a| a.col) {
                    let step = if target_col > now {
                        Action::MoveRight
                    } else {
                        Action::MoveLeft
                    };
                    for _ in 0..(target_col - now).abs() {
                        d.act(step);
                    }
                }
                // Soft drop before the hard drop on every 3rd piece.
                if cycles % 3 == 1 {
                    d.act(Action::SoftDrop);
                }
                d.act(Action::HardDrop);
            } else {
                d.act(Action::HardDrop);
            }
        }
        for _ in 0..GRAVITY_TICKS_PER_PIECE {
            d.game.tick();
        }
        cycles += 1;
        let snapshot = d.game.snapshot();
        if snapshot.game_over {
            return RunResult {
                snapshot,
                cycles,
                holds: d.holds,
                lines_from_events: d.lines_from_events,
                actions_used: d.actions_used,
            };
        }
        assert!(
            cycles < MAX_PIECE_CYCLES,
            "scripted log must reach top-out within {MAX_PIECE_CYCLES} piece cycles (seed {seed})"
        );
    }
}

// --- golden assertions ------------------------------------------------------

/// Everything pinned per seed. `Debug`/`PartialEq` so a mismatch prints both
/// full structs side by side, making regeneration mechanical.
#[derive(Debug, PartialEq)]
struct Golden {
    cycles: usize,
    score: u64,
    level: u32,
    lines: u32,
    combo: u32,
    b2b: bool,
    hold: Option<Piece>,
    hold_used: bool,
    next: Vec<Piece>,
    board_digest: u64,
    bincode_len: usize,
    fnv1a64: u64,
}

fn measure(run: &RunResult) -> Golden {
    let s = &run.snapshot;
    let bytes = wire_bytes(s);
    Golden {
        cycles: run.cycles,
        score: s.score,
        level: s.level,
        lines: s.lines,
        combo: s.combo,
        b2b: s.b2b,
        hold: s.hold,
        hold_used: s.hold_used,
        next: s.next.clone(),
        board_digest: board_digest(&s.board),
        bincode_len: bytes.len(),
        fnv1a64: fnv1a64(&bytes),
    }
}

fn check_marathon_golden(seed: u64, golden: &Golden) {
    let run = drive(seed);
    let s = &run.snapshot;

    // Top-out semantics the scripted log is supposed to produce.
    assert!(s.game_over, "seed {seed}: scripted log must top out");
    assert!(s.active.is_none(), "game_over clears the active piece");
    assert!(s.ghost_row.is_none(), "game_over clears the ghost row");
    // Snapshot/event cross-check: `lines` equals the LineCleared sum.
    assert_eq!(
        s.lines, run.lines_from_events,
        "seed {seed}: snapshot lines disagrees with LineCleared events"
    );
    assert!(
        run.holds >= 1,
        "seed {seed}: scripted log must perform an accepted hold"
    );
    // Action coverage: the log must have exercised every variant.
    for action in ALL_ACTIONS {
        assert!(
            run.actions_used.contains(&action),
            "seed {seed}: scripted log never used {action:?}"
        );
    }

    let computed = measure(&run);
    assert_eq!(
        &computed, golden,
        "seed {seed}: golden snapshot drifted — Marathon behavior or GameSnapshot wire shape changed (if intentional, regenerate this golden)"
    );
}

#[test]
fn marathon_golden_snapshot_seed_31337() {
    check_marathon_golden(
        31_337,
        &Golden {
            cycles: 139,
            score: 23_381,
            level: 5,
            lines: 49,
            combo: 0,
            b2b: false,
            hold: Some(Piece::L),
            hold_used: false,
            next: vec![Piece::O, Piece::Z, Piece::T, Piece::J, Piece::L],
            board_digest: 4727678028635499235,
            bincode_len: 542,
            fnv1a64: 15166114492097817985,
        },
    );
}

#[test]
fn marathon_golden_snapshot_seed_20261001() {
    check_marathon_golden(
        20_261_001,
        &Golden {
            cycles: 450,
            score: 200_502,
            level: 17,
            lines: 164,
            combo: 0,
            b2b: false,
            hold: Some(Piece::Z),
            hold_used: false,
            next: vec![Piece::L, Piece::T, Piece::I, Piece::I, Piece::O],
            board_digest: 7721337784784528652,
            bincode_len: 918,
            fnv1a64: 8282038781071262632,
        },
    );
}

#[test]
fn marathon_golden_snapshot_seed_42() {
    check_marathon_golden(
        42,
        &Golden {
            cycles: 1248,
            score: 1_633_828,
            level: 49,
            lines: 485,
            combo: 0,
            b2b: true,
            hold: Some(Piece::L),
            hold_used: false,
            next: vec![Piece::O, Piece::Z, Piece::T, Piece::L, Piece::J],
            board_digest: 1431982032902486057,
            bincode_len: 846,
            fnv1a64: 14000901299652375531,
        },
    );
}
