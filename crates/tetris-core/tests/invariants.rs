//! T9 property tests: random action/tick drives must never break core
//! invariants. Evidence for "assertions bite": negative-control tests below
//! craft deliberately invalid snapshots and assert the checkers reject them.

use proptest::prelude::*;
use proptest::sample::select;

use tetris_core::actions::Action;
use tetris_core::board::{Board, COLS, ROWS};
use tetris_core::event::GameEvent;
use tetris_core::game::{Game, GameSnapshot};
use tetris_core::piece::{spawn_state, Piece};

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

/// One player action followed by 0..4 gravity frames.
type Step = (Action, u8);

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec((select(&ALL_ACTIONS), 0u8..5), 8..96)
}

fn seed_and_steps() -> impl Strategy<Value = (u64, Vec<Step>)> {
    (any::<u64>(), steps())
}

// --- invariant checkers (pure, reused by negative controls and the soak) ---

fn overlap_free(s: &GameSnapshot) -> bool {
    s.active.is_none_or(|ps| !s.board.collides(&ps))
}

fn no_full_rows(s: &GameSnapshot) -> bool {
    s.board.full_rows().is_empty()
}

fn dims_valid(s: &GameSnapshot) -> bool {
    ROWS == 22 && COLS == 10 && {
        let rows = s.board.full_rows();
        rows.len() <= ROWS && rows.iter().all(|&r| r < ROWS)
    }
}

fn queue_len_ok(s: &GameSnapshot) -> bool {
    (1..=6).contains(&s.next.len())
}

fn score_regressed(prev: u64, next: u64) -> bool {
    next < prev
}

/// All per-step structural violations of a snapshot, as tagged strings.
fn snapshot_violations(s: &GameSnapshot) -> Vec<&'static str> {
    let mut v = Vec::new();
    if !overlap_free(s) {
        v.push("active piece overlaps filled/out-of-bounds cells");
    }
    if !no_full_rows(s) {
        v.push("board left a full row uncleared");
    }
    if !dims_valid(s) {
        v.push("board dimensions invalid");
    }
    if !queue_len_ok(s) {
        v.push("next queue length outside 1..=6");
    }
    if s.combo > s.lines {
        v.push("combo exceeded total lines cleared");
    }
    if s.game_over && s.active.is_some() {
        v.push("game_over but active piece present");
    }
    v
}

/// Drive one game, asserting every per-step invariant; on `game_over`,
/// additionally prove the frozen state survives further input.
fn drive(seed: u64, seq: &[Step]) {
    let mut game = Game::new(seed);
    let mut prev_score = 0u64;
    let mut lines_seen: u32 = 0;
    let mut expect_hold_used = false;

    for (action, ticks) in seq {
        let mut events = game.apply(*action);
        for _ in 0..*ticks {
            events.extend(game.tick());
        }
        let s = game.snapshot();

        assert!(
            !score_regressed(prev_score, s.score),
            "score decreased {prev_score} -> {} (seed {seed})",
            s.score
        );
        prev_score = s.score;

        let mut saw_line_clear = false;
        for e in &events {
            match e {
                GameEvent::HoldPerformed { .. } => expect_hold_used = true,
                GameEvent::PieceLocked { .. } => expect_hold_used = false,
                GameEvent::LineCleared { lines } => {
                    lines_seen += *lines as u32;
                    saw_line_clear = true;
                }
                _ => {}
            }
        }
        assert_eq!(
            s.hold_used, expect_hold_used,
            "hold_used flag out of sync with events (seed {seed})"
        );
        assert_eq!(
            s.lines, lines_seen,
            "lines counter disagrees with LineCleared events (seed {seed})"
        );
        if saw_line_clear {
            assert!(
                no_full_rows(&s),
                "LineCleared emitted but board still has a full row (seed {seed})"
            );
        }

        let violations = snapshot_violations(&s);
        assert!(violations.is_empty(), "{violations:?} (seed {seed})");

        if s.game_over {
            let frozen = s.clone();
            for extra in ALL_ACTIONS {
                assert!(game.apply(extra).is_empty());
                assert!(game.tick().is_empty());
            }
            assert_eq!(game.snapshot(), frozen, "game_over must freeze all state");
            return;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn structural_invariants_hold_under_random_play((seed, seq) in seed_and_steps()) {
        drive(seed, &seq);
    }

    #[test]
    fn snapshot_always_renderable_under_random_play((seed, seq) in seed_and_steps()) {
        let mut game = Game::new(seed);
        for (action, ticks) in &seq {
            game.apply(*action);
            for _ in 0..*ticks {
                game.tick();
            }
            let s = game.snapshot();
            prop_assert!(queue_len_ok(&s));
            prop_assert!(overlap_free(&s));
            if let Some(ps) = s.active {
                prop_assert!(s.ghost_row.is_some(), "active piece must expose ghost row: {ps:?}");
                prop_assert!(s.ghost_row.unwrap() >= ps.row);
            }
        }
    }

    #[test]
    fn hold_flag_and_combo_never_go_invalid((seed, seq) in seed_and_steps()) {
        // Combo/b2b sanity: combo is u32 (never < 0) and can never exceed
        // the number of lines ever cleared; b2b is only ever true/false.
        let mut game = Game::new(seed);
        for (action, ticks) in &seq {
            game.apply(*action);
            for _ in 0..*ticks {
                game.tick();
            }
            let s = game.snapshot();
            prop_assert!(s.combo <= s.lines);
        }
    }
}

#[test]
fn game_over_freezes_state_directed() {
    let mut game = Game::new(20260926);
    let mut guard = 0;
    let frozen = loop {
        game.apply(Action::HardDrop);
        for _ in 0..3 {
            game.tick();
        }
        let s = game.snapshot();
        if s.game_over {
            break s;
        }
        guard += 1;
        assert!(guard < 500, "directed hard-drop rush must end the game");
    };
    for action in ALL_ACTIONS.iter().cycle().take(64) {
        assert!(game.apply(*action).is_empty());
        assert!(game.tick().is_empty());
    }
    assert_eq!(game.snapshot(), frozen);
}

// --- negative controls: prove the invariant expressions actually bite ---

fn crafted_snapshot() -> GameSnapshot {
    GameSnapshot {
        board: Board::new(),
        active: None,
        ghost_row: None,
        hold: None,
        hold_used: false,
        next: vec![Piece::I],
        score: 0,
        level: 1,
        lines: 0,
        combo: 0,
        b2b: false,
        game_over: false,
    }
}

#[test]
fn negative_control_overlap_checker_catches_planted_bug() {
    let mut s = crafted_snapshot();
    let spawn = spawn_state(Piece::O);
    s.active = Some(spawn);
    s.ghost_row = Some(spawn.row);
    for (r, c) in spawn.cells() {
        let r = usize::try_from(r).expect("spawn cells are on-board");
        let c = usize::try_from(c).expect("spawn cells are on-board");
        s.board.set(r, c, Some(Piece::Z));
    }
    let v = snapshot_violations(&s);
    assert!(
        v.contains(&"active piece overlaps filled/out-of-bounds cells"),
        "overlap checker failed to catch planted overlap: {v:?}"
    );
    assert!(overlap_free(&crafted_snapshot()));
}

#[test]
fn negative_control_full_row_and_score_checkers_catch_planted_bugs() {
    let mut s = crafted_snapshot();
    for c in 0..COLS {
        s.board.set(ROWS - 1, c, Some(Piece::T));
    }
    let v = snapshot_violations(&s);
    assert!(
        v.contains(&"board left a full row uncleared"),
        "full-row checker failed to catch planted full row: {v:?}"
    );

    assert!(score_regressed(10, 5), "score checker missed a regression");
    assert!(!score_regressed(10, 10));
    assert!(!score_regressed(10, 4337));

    let mut q = crafted_snapshot();
    q.next = vec![];
    assert!(
        snapshot_violations(&q).contains(&"next queue length outside 1..=6"),
        "queue checker missed empty queue"
    );
    q.next = vec![Piece::O; 7];
    assert!(
        snapshot_violations(&q).contains(&"next queue length outside 1..=6"),
        "queue checker missed oversized queue"
    );

    let mut c = crafted_snapshot();
    c.lines = 1;
    c.combo = 2;
    assert!(
        snapshot_violations(&c).contains(&"combo exceeded total lines cleared"),
        "combo checker missed impossible combo"
    );
}
