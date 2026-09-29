//! App-side 1v1 versus bridge (T25, PRD §15 1v1 Versus v0.2).
//!
//! Runs [`tetris_core::versus::Match`] (two independent `Game`s) inside the
//! app next to — never inside — the solo [`GameCore`](super::GameCore). While
//! [`VersusMatch::active`] is set:
//!
//! - the solo fixed step, solo bot and solo input freeze (they gate on
//!   `active`; no `CoreEvent` fires during versus),
//! - [`versus_bridge_system`] (on `FixedUpdate`) drains the per-side
//!   [`VersusActions`](crate::input::VersusActions) queues produced by the
//!   versus input system in `input.rs`, applies them to the match and ticks
//!   each side once per fixed step (60 Hz, `Match::tick`), writing the
//!   resulting [`MatchEvent`](tetris_core::versus::MatchEvent)s as
//!   [`VersusEvent`] messages for juice/audio (T26+ subscribes),
//! - [`versus_bot_system`] drives any [`Controller::Bot`] side with the T14
//!   greedy solver through the shared [`bot_side_drive`](super::bot_side_drive)
//!   executor, before the step drains,
//! - shared pause works for free: [`SimPaused`](super::SimPaused) and
//!   [`AppState::Paused`](crate::state::AppState::Paused) gate the versus
//!   step exactly like the solo one, freezing **both** cores; R restarts the
//!   match, [`start_versus`]/[`end_versus`] mirror the `restart_run` free
//!   function pattern for menu/UI code (T26 wires the buttons).
//!
//! `VersusMatch` is a **non-send** resource (it owns two `Game`s, whose
//! `RefCell` RNG makes them `!Sync`) — read it with
//! `world.non_send::<VersusMatch>()` / `NonSendMut`. Observables for T26:
//! `VersusMatch` (snapshots via `match_.left.snapshot()` /
//! `match_.right.snapshot()`, `match_.snapshot()` for both plus pending
//! garbage), `VersusWinner`, `Messages<VersusEvent>`.
//!
//! Headless CI harness: `TETRIS_1V1=garbage|race` starts bot-vs-bot matches
//! at startup (mirrors `TETRIS_BOT`) and logs `VERSUS winner=… fps_avg=…`
//! per match, exiting after [`HARNESS_MATCHES`] completed matches.

use bevy::prelude::*;

use tetris_core::actions::Action;
use tetris_core::versus::{AttackRule, Match, MatchEvent, Side, DEFAULT_RACE_LINES};

use super::{bot_side_drive, env_seed, wall_clock_seed, BotState, SimPaused};
use crate::input::VersusActions;
use crate::state::AppState;

/// Env var starting a bot-vs-bot match at startup: `"garbage"` or `"race"`
/// (mirrors the T14 `TETRIS_BOT` marathon switch).
pub const ONE_V_ONE_ENV: &str = "TETRIS_1V1";

/// Bot-vs-bot matches the [`ONE_V_ONE_ENV`] harness plays before exiting.
pub const HARNESS_MATCHES: u32 = 2;

/// Seconds the harness waits after a crowned winner before restarting.
const HARNESS_RESTART_DELAY_SECS: f32 = 0.5;

/// Who controls one side of a versus match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Controller {
    /// Keyboard player (bindings from `input.rs`'s [`VersusBindings`](crate::input::VersusBindings)).
    #[default]
    Human,
    /// The T14 greedy snapshot solver.
    Bot,
}

/// Match outcome, observable without touching the NonSend [`VersusMatch`]:
/// set once a [`MatchEvent::WinnerCrowned`] is stepped, cleared by
/// [`start_versus`]/[`end_versus`]. T26 renders the winner screen from this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct VersusWinner(pub Option<Side>);

/// App-side message wrapper over the core's `MatchEvent` (newtype required
/// for the same orphan-rule reason as [`CoreEvent`](super::CoreEvent)).
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct VersusEvent(pub MatchEvent);

/// NonSend resource owning the live versus match. Inactive by default;
/// every versus system early-returns (or clears its queues) while
/// `active == false`, so solo play is untouched.
pub struct VersusMatch {
    /// `true` while a match occupies the game screen.
    pub active: bool,
    /// Attack rule of the current/last match.
    pub rule: AttackRule,
    /// Left player's controller.
    pub p1: Controller,
    /// Right player's controller.
    pub p2: Controller,
    /// The authoritative match; render/HUD read `match_.snapshot()`.
    pub match_: Match,
    /// Seed the current match was started from.
    pub seed: u64,
    /// Fixed steps actually applied to the match (diagnostics/tests).
    pub steps: u64,
    /// Internal: winner already surfaced to [`VersusWinner`].
    crowned: bool,
    /// Per-side greedy bot executors (index = `Side::index()`), reused from
    /// the solo marathon machinery.
    bots: [BotState; 2],
    /// Per-side pacing countdown: while `> 0` the side's bot idles (and the
    /// counter ticks down), see [`BOT_LOCK_COOLDOWN_STEPS`].
    bot_cooldown: [u32; 2],
}

impl VersusMatch {
    /// Fresh inactive slot: `Match` from `seed`, nobody in control yet.
    pub fn new(seed: u64, rule: AttackRule) -> Self {
        Self {
            active: false,
            rule,
            p1: Controller::Human,
            p2: Controller::Human,
            match_: Match::new(seed, rule),
            seed,
            steps: 0,
            crowned: false,
            bots: Default::default(),
            bot_cooldown: [0; 2],
        }
    }

    /// Current winner (`None` while the match is open), without needing the
    /// separate [`VersusWinner`] resource.
    pub fn winner(&self) -> Option<Side> {
        self.match_.winner()
    }
}

impl Default for VersusMatch {
    fn default() -> Self {
        Self::new(wall_clock_seed(), AttackRule::default())
    }
}

/// Fixed steps a versus bot idles after every lock before starting its next
/// placement (at 60 Hz: ~1 lock/sec — a competitive human pace). Without it
/// the greedy solver places a piece every handful of steps and buries the
/// human side under a wall of garbage before they can react.
pub const BOT_LOCK_COOLDOWN_STEPS: u32 = 60;

/// Start (or replace) a versus match: fresh [`Match`] seeded from
/// [`SEED_ENV`](super::SEED_ENV) when set (reproducible CI/headless runs) else wall clock,
/// mirroring [`restart_run`](super::restart_run). Takes over the game screen
/// (`AppState::Playing`) and clears any previous [`VersusWinner`].
pub fn start_versus(
    versus: &mut VersusMatch,
    winner: &mut VersusWinner,
    app_state: &mut AppState,
    rule: AttackRule,
    p1: Controller,
    p2: Controller,
) {
    let (seed, source) = match env_seed() {
        Some(seed) => (seed, "fixed"),
        None => (wall_clock_seed(), "wall clock"),
    };
    info!("versus start: seed {seed} ({source}) rule {rule:?} p1={p1:?} p2={p2:?}");
    versus.match_ = Match::new(seed, rule);
    versus.seed = seed;
    versus.rule = rule;
    versus.p1 = p1;
    versus.p2 = p2;
    versus.steps = 0;
    versus.crowned = false;
    versus.bots = Default::default();
    versus.bot_cooldown = [0; 2];
    versus.active = true;
    winner.0 = None;
    *app_state = AppState::Playing;
}

/// Leave the versus match: the solo path becomes live again and the app
/// returns to the title screen (T26's quit button calls this).
pub fn end_versus(versus: &mut VersusMatch, winner: &mut VersusWinner, app_state: &mut AppState) {
    info!("versus end: steps {}", versus.steps);
    versus.active = false;
    versus.crowned = false;
    versus.steps = 0;
    winner.0 = None;
    *app_state = AppState::Title;
}

// ---------------------------------------------------------------------------
// Fixed-step systems
// ---------------------------------------------------------------------------

/// Feed every [`Controller::Bot`] side's snapshot to the shared greedy bot
/// executor and push its chosen action into that side's queue. Sides that
/// already finished a Race target (frozen boards) are skipped. Each bot is
/// paced: after its hard drop it idles for [`BOT_LOCK_COOLDOWN_STEPS`]
/// fixed steps, keeping it at a human-plausible lock rate instead of
/// stacking 300 pieces a minute. Runs in `FixedUpdate` *before*
/// [`versus_bridge_system`], so actions apply same-tick (same placement as
/// the solo `bot_drive_system`).
fn versus_bot_system(
    versus: NonSendMut<VersusMatch>,
    mut actions: ResMut<VersusActions>,
    app_state: Res<AppState>,
    paused: Res<SimPaused>,
) {
    if !versus.active || paused.0 || *app_state != AppState::Playing {
        return;
    }
    let versus = versus.into_inner();
    for (index, side) in [(0usize, Side::Left), (1, Side::Right)] {
        let controller = if index == 0 { versus.p1 } else { versus.p2 };
        if controller != Controller::Bot || versus.match_.finished(side) {
            continue;
        }
        let snapshot = match side {
            Side::Left => versus.match_.left.snapshot(),
            Side::Right => versus.match_.right.snapshot(),
        };
        if versus.bot_cooldown[index] > 0 {
            versus.bot_cooldown[index] -= 1;
            continue;
        }
        let state = &mut versus.bots[index];
        let queue = if index == 0 {
            &mut actions.left
        } else {
            &mut actions.right
        };
        let mut dropped = false;
        bot_side_drive(&snapshot, state, &mut |a| {
            dropped |= matches!(a, Action::HardDrop);
            queue.push(a);
        });
        if dropped {
            versus.bot_cooldown[index] = BOT_LOCK_COOLDOWN_STEPS;
        }
    }
}

/// The versus fixed step: drain both side queues, apply every action to the
/// match, then tick each side once. Queued actions are *held* (not dropped)
/// while paused, matching the solo bridge. Crowns [`VersusWinner`] exactly
/// once when the core sets a winner.
fn versus_bridge_system(
    versus: NonSendMut<VersusMatch>,
    mut actions: ResMut<VersusActions>,
    mut messages: MessageWriter<VersusEvent>,
    mut winner: ResMut<VersusWinner>,
    app_state: Res<AppState>,
    paused: Res<SimPaused>,
) {
    if !versus.active {
        actions.left.clear();
        actions.right.clear();
        return;
    }
    if paused.0 || *app_state != AppState::Playing {
        return;
    }
    let versus = versus.into_inner();

    let mut events = Vec::new();
    for (side, queue) in [
        (Side::Left, std::mem::take(&mut actions.left)),
        (Side::Right, std::mem::take(&mut actions.right)),
    ] {
        for action in queue {
            events.extend(versus.match_.apply(side, action));
        }
        events.extend(versus.match_.tick(side));
    }
    versus.steps += 1;
    for event in events {
        messages.write(VersusEvent(event));
    }
    if !versus.crowned {
        if let Some(side) = versus.match_.winner() {
            versus.crowned = true;
            winner.0 = Some(side);
            info!("versus: winner {side:?} after {} steps", versus.steps);
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle systems (Update)
// ---------------------------------------------------------------------------

/// R restarts the current match (versus analogue of `restart_on_r_system`),
/// honoring [`SEED_ENV`](super::SEED_ENV) through [`start_versus`].
fn versus_restart_on_r_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    versus: NonSendMut<VersusMatch>,
    winner: ResMut<VersusWinner>,
    app_state: ResMut<AppState>,
) {
    let Some(keys) = keys else { return };
    if !versus.active || !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    let (rule, p1, p2) = (versus.rule, versus.p1, versus.p2);
    start_versus(
        versus.into_inner(),
        winner.into_inner(),
        app_state.into_inner(),
        rule,
        p1,
        p2,
    );
}

/// Parse a [`ONE_V_ONE_ENV`] value (`"garbage"` / `"race"`, case-insensitive).
fn parse_1v1(value: &str) -> Option<AttackRule> {
    match value.trim().to_ascii_lowercase().as_str() {
        "garbage" => Some(AttackRule::Garbage),
        "race" => Some(AttackRule::Race {
            target_lines: DEFAULT_RACE_LINES,
        }),
        _ => None,
    }
}

/// [`ONE_V_ONE_ENV`] as an [`AttackRule`], if set and recognized.
fn one_v_one_env() -> Option<AttackRule> {
    std::env::var(ONE_V_ONE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_1v1)
}

/// Harness bookkeeping for the `TETRIS_1V1` bot-vs-bot run: rule (`None` =
/// disabled), completed matches, the post-winner restart countdown, and the
/// frame-time accumulator feeding `fps_avg=` on the per-match log.
#[derive(Debug, Resource)]
pub struct VersusHarness {
    /// Parsed [`ONE_V_ONE_ENV`] rule; `None` disables the harness.
    pub rule: Option<AttackRule>,
    /// Matches with a crowned winner so far.
    pub matches_done: u32,
    /// Post-winner restart countdown active.
    pub awaiting_restart: bool,
    /// Seconds left on the countdown.
    pub restart_in: f32,
    /// Frames counted since the last per-match fps log.
    pub frames: u64,
    /// Frame seconds accumulated since the last per-match fps log.
    pub frame_secs: f64,
}

impl Default for VersusHarness {
    fn default() -> Self {
        Self {
            rule: one_v_one_env(),
            matches_done: 0,
            awaiting_restart: false,
            restart_in: 0.0,
            frames: 0,
            frame_secs: 0.0,
        }
    }
}

/// Startup: with `TETRIS_1V1` set, kick off the first bot-vs-bot match.
fn versus_harness_startup(
    versus: NonSendMut<VersusMatch>,
    winner: ResMut<VersusWinner>,
    app_state: ResMut<AppState>,
    harness: Res<VersusHarness>,
) {
    let Some(rule) = harness.rule else { return };
    info!("versus harness: bot-vs-bot {rule:?} (from {ONE_V_ONE_ENV})");
    start_versus(
        versus.into_inner(),
        winner.into_inner(),
        app_state.into_inner(),
        rule,
        Controller::Bot,
        Controller::Bot,
    );
}

/// Real-frame harness loop (mirrors `bot_marathon_system`): fixes the Title
/// state the menu startup may impose, logs `VERSUS winner=… fps_avg=…` per
/// completed match, restarts after a short delay, exits `AppExit::Success`
/// once [`HARNESS_MATCHES`] matches are done.
fn versus_harness_update(
    mut harness: ResMut<VersusHarness>,
    versus: NonSendMut<VersusMatch>,
    winner: ResMut<VersusWinner>,
    mut app_state: ResMut<AppState>,
    time: Res<Time>,
    mut exits: MessageWriter<AppExit>,
) {
    let Some(rule) = harness.rule else { return };
    harness.frames += 1;
    harness.frame_secs += time.delta_secs() as f64;

    // T17-style Title at startup: press "Start" the same way the bot
    // marathon does (only while our match is the one on screen).
    if versus.active && *app_state == AppState::Title && !harness.awaiting_restart {
        *app_state = AppState::Playing;
    }

    if versus.active && versus.crowned && !harness.awaiting_restart {
        harness.matches_done += 1;
        let fps = if harness.frame_secs > 0.0 {
            harness.frames as f64 / harness.frame_secs
        } else {
            0.0
        };
        let (left, right) = (
            versus.match_.left.snapshot(),
            versus.match_.right.snapshot(),
        );
        info!(
            "VERSUS match_done seed={} winner_side_left={} left_score={} left_lines={} right_score={} right_lines={}",
            versus.seed,
            winner.0 == Some(Side::Left),
            left.score,
            left.lines,
            right.score,
            right.lines
        );
        info!(
            "VERSUS winner={winner:?} fps_avg={fps:.1}",
            winner = winner.0
        );
        harness.frames = 0;
        harness.frame_secs = 0.0;
        if harness.matches_done >= HARNESS_MATCHES {
            exits.write(AppExit::Success);
            return;
        }
        harness.awaiting_restart = true;
        harness.restart_in = HARNESS_RESTART_DELAY_SECS;
    }

    if harness.awaiting_restart {
        harness.restart_in -= time.delta_secs();
        if harness.restart_in <= 0.0 {
            info!("versus harness: restart (R-equivalent)");
            let winner_inner = winner.into_inner();
            let state_inner = app_state.into_inner();
            start_versus(
                versus.into_inner(),
                winner_inner,
                state_inner,
                rule,
                Controller::Bot,
                Controller::Bot,
            );
            harness.awaiting_restart = false;
        }
    }
}

/// Mounts the versus bridge: resources, messages and the fixed-step/update
/// systems above. Added by `CoreBridgePlugin` (main.rs is frozen); fully
/// inert while [`VersusMatch`] is inactive.
pub struct VersusBridgePlugin;

impl Plugin for VersusBridgePlugin {
    fn build(&self, app: &mut App) {
        // `VersusActions` is hosted here (not only in `InputPlugin`) so the
        // versus systems validate in apps that build `CoreBridgePlugin`
        // without the input plugin; `init_resource` is a no-op when
        // `InputPlugin` already inserted it.
        app.insert_non_send(VersusMatch::default())
            .init_resource::<VersusActions>()
            .init_resource::<VersusWinner>()
            .init_resource::<VersusHarness>()
            .add_message::<VersusEvent>()
            .add_systems(Startup, versus_harness_startup)
            .add_systems(Update, (versus_restart_on_r_system, versus_harness_update))
            .add_systems(FixedUpdate, versus_bot_system)
            .add_systems(FixedUpdate, versus_bridge_system.after(versus_bot_system));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, CoreEvent, GameCore, PendingActions};
    use crate::input::{InputPlugin, VersusBindings};
    use crate::state::{RebindingCapture, Settings};
    use tetris_core::actions::Action;
    use tetris_core::event::GameEvent;
    use tetris_core::game::GameSnapshot;

    fn test_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, InputPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.insert_non_send(VersusMatch::new(seed, AttackRule::Garbage));
        app.init_resource::<AppState>()
            .init_resource::<Settings>()
            .init_resource::<RebindingCapture>();
        app
    }

    fn fixed_step(app: &mut App) {
        let _ = app.world_mut().try_run_schedule(FixedPreUpdate);
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn solo(app: &App) -> tetris_core::game::GameSnapshot {
        app.world().non_send::<GameCore>().game.snapshot()
    }

    fn versus(app: &App) -> tetris_core::versus::MatchSnapshot {
        app.world().non_send::<VersusMatch>().match_.snapshot()
    }

    fn versus_steps(app: &App) -> u64 {
        app.world().non_send::<VersusMatch>().steps
    }

    /// `start_versus` through the world: nested `resource_scope`s are how
    /// Bevy 0.19 hands out two resources at once from `&mut World`.
    fn start_in_app(app: &mut App, rule: AttackRule, p1: Controller, p2: Controller) {
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        rule,
                        p1,
                        p2,
                    );
                });
            });
    }

    fn activate(app: &mut App, p1: Controller, p2: Controller) {
        start_in_app(app, AttackRule::Garbage, p1, p2);
    }

    fn drained_versus(app: &mut App) -> Vec<VersusEvent> {
        app.world_mut()
            .resource_mut::<Messages<VersusEvent>>()
            .drain()
            .collect()
    }

    fn drained_core(app: &mut App) -> Vec<CoreEvent> {
        app.world_mut()
            .resource_mut::<Messages<CoreEvent>>()
            .drain()
            .collect()
    }

    fn push_side(app: &mut App, side: Side, actions: &[Action]) {
        let mut queue_res = app.world_mut().resource_mut::<VersusActions>();
        let queue = match side {
            Side::Left => &mut queue_res.left,
            Side::Right => &mut queue_res.right,
        };
        queue.extend(actions.iter().copied());
    }

    #[test]
    fn harness_parses_only_garbage_and_race() {
        assert_eq!(parse_1v1("garbage"), Some(AttackRule::Garbage));
        assert_eq!(parse_1v1(" GARBAGE "), Some(AttackRule::Garbage));
        assert_eq!(
            parse_1v1("Race"),
            Some(AttackRule::Race {
                target_lines: DEFAULT_RACE_LINES
            })
        );
        assert_eq!(parse_1v1(""), None);
        assert_eq!(parse_1v1("survival"), None);
    }

    #[test]
    fn inactive_versus_leaves_the_solo_path_running() {
        let mut app = test_app(0xC0FFEE);
        for _ in 0..3 {
            app.world_mut()
                .resource_mut::<PendingActions>()
                .push(Action::HardDrop);
            fixed_step(&mut app);
        }
        assert_eq!(app.world().non_send::<GameCore>().steps, 3);
        assert_eq!(versus_steps(&app), 0);
        assert!(drained_versus(&mut app).is_empty());
        let core_events = drained_core(&mut app);
        assert!(
            core_events
                .iter()
                .any(|e| matches!(e.0, GameEvent::PieceLocked { .. })),
            "solo events still flow: {core_events:?}"
        );
    }

    #[test]
    fn active_versus_freezes_solo_core_events_and_solo_input() {
        let mut app = test_app(1);
        activate(&mut app, Controller::Human, Controller::Human);

        // Solo hotkeys must not reach the solo core while versus is up.
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Space);
        let solo_before = solo(&app);
        for _ in 0..3 {
            fixed_step(&mut app);
        }
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(solo(&app), solo_before, "solo core frozen");
        assert!(
            app.world().resource::<PendingActions>().queue.is_empty(),
            "solo input system is gated while versus is active"
        );
        assert!(
            drained_core(&mut app).is_empty(),
            "no CoreEvent during versus"
        );
        assert_eq!(versus_steps(&app), 3, "versus stepped instead");
    }

    #[test]
    fn versus_step_advances_both_cores_and_emits_events() {
        let mut app = test_app(2);
        activate(&mut app, Controller::Human, Controller::Human);
        push_side(&mut app, Side::Left, &[Action::HardDrop]);
        push_side(&mut app, Side::Right, &[Action::HardDrop]);

        fixed_step(&mut app);

        let snap = versus(&app);
        assert!(!snap.left.board.is_empty() && !snap.right.board.is_empty());
        assert!(
            snap.left.score > 0 && snap.right.score > 0,
            "both cores scored their hard drops"
        );
        assert_eq!(versus_steps(&app), 1);
        let locks = drained_versus(&mut app)
            .into_iter()
            .filter(|e| matches!(e.0, MatchEvent::PieceLocked { .. }))
            .count();
        assert_eq!(locks, 2, "one lock per side this step");
        assert!(app.world().resource::<VersusActions>().left.is_empty());
    }

    #[test]
    fn human_input_is_routed_per_side() {
        fn active_col(snapshot: &GameSnapshot) -> Option<i32> {
            snapshot.active.map(|p| p.col)
        }
        let mut app = test_app(3);
        activate(&mut app, Controller::Human, Controller::Human);
        let before = versus(&app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyA); // P1 move left
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::ArrowLeft); // P2 move left
        fixed_step(&mut app);

        let after = versus(&app);
        assert_eq!(
            active_col(&after.left),
            active_col(&before.left).map(|c| c - 1),
            "P1 A slid the left piece one column"
        );
        assert_eq!(
            active_col(&after.right),
            active_col(&before.right).map(|c| c - 1),
            "P2 ArrowLeft slid the right piece one column"
        );

        // A bot-controlled side is driven by the solver, never by human keys:
        // P2's hold key must not reach the right side (the solver never
        // plays holds), while P1's hold key still takes on the left side.
        let mut app2 = test_app(4);
        activate(&mut app2, Controller::Human, Controller::Bot);
        let before2 = versus(&app2);
        app2.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Comma); // P2 hold — must be ignored (bot side)
        app2.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyQ); // P1 hold — must take (human side)
        fixed_step(&mut app2);
        let after2 = versus(&app2);
        assert_eq!(
            after2.left.hold,
            before2.left.active.map(|p| p.piece),
            "P1 Q held on the left side"
        );
        assert!(
            after2.right.hold.is_none() && !after2.right.hold_used,
            "P2 keys must not reach the bot-driven right side"
        );
        assert_eq!(
            after2.right.active.map(|p| p.piece),
            before2.right.active.map(|p| p.piece),
            "the solver only places, it never holds"
        );
    }

    #[test]
    fn versus_bindings_match_the_preset() {
        let app = test_app(5);
        let b = app.world().resource::<VersusBindings>();
        assert_eq!(b.p1.move_left, vec![KeyCode::KeyA]);
        assert_eq!(b.p1.move_right, vec![KeyCode::KeyD]);
        assert_eq!(b.p1.rotate_cw, vec![KeyCode::KeyW]);
        assert_eq!(b.p1.rotate_ccw, vec![KeyCode::KeyE]);
        assert_eq!(b.p1.soft_drop, vec![KeyCode::KeyS, KeyCode::ShiftLeft]);
        assert_eq!(b.p1.hard_drop, vec![KeyCode::Space]);
        assert_eq!(b.p1.hold, vec![KeyCode::KeyQ]);
        assert_eq!(b.p2.move_left, vec![KeyCode::ArrowLeft]);
        assert_eq!(b.p2.move_right, vec![KeyCode::ArrowRight]);
        assert_eq!(b.p2.rotate_cw, vec![KeyCode::ArrowUp]);
        assert_eq!(b.p2.rotate_ccw, vec![KeyCode::Period]);
        assert_eq!(b.p2.soft_drop, vec![KeyCode::ArrowDown]);
        assert_eq!(b.p2.hard_drop, vec![KeyCode::Slash, KeyCode::Numpad0]);
        assert_eq!(b.p2.hold, vec![KeyCode::Comma]);
    }

    #[test]
    fn pause_freezes_both_cores_and_holds_queues() {
        let mut app = test_app(6);
        activate(&mut app, Controller::Human, Controller::Human);
        push_side(&mut app, Side::Left, &[Action::HardDrop]);
        push_side(&mut app, Side::Right, &[Action::HardDrop]);
        app.world_mut().resource_mut::<SimPaused>().0 = true;

        let before = versus(&app);
        for _ in 0..5 {
            fixed_step(&mut app);
        }

        assert_eq!(versus(&app), before, "both cores frozen");
        assert_eq!(versus_steps(&app), 0);
        let held = &app.world().resource::<VersusActions>();
        assert_eq!(held.left, vec![Action::HardDrop]);
        assert_eq!(held.right, vec![Action::HardDrop]);

        // Resume: the held actions land on the next step.
        app.world_mut().resource_mut::<SimPaused>().0 = false;
        fixed_step(&mut app);
        assert_ne!(versus(&app), before);
    }

    #[test]
    fn winner_is_crowned_once_and_the_match_stays_frozen() {
        let mut app = test_app(7);
        // Race to 0 lines: each side's first lock *finishes* it; the
        // crowning waits for the second finisher, then the perfect tie
        // (all zeros) goes to the side that finished first.
        start_in_app(
            &mut app,
            AttackRule::Race { target_lines: 0 },
            Controller::Human,
            Controller::Human,
        );

        push_side(&mut app, Side::Left, &[Action::HardDrop]);
        fixed_step(&mut app);
        assert_eq!(
            *app.world().resource::<VersusWinner>(),
            VersusWinner(None),
            "finishing alone never crowns"
        );
        let after_left = drained_versus(&mut app);
        assert!(
            after_left
                .iter()
                .any(|e| matches!(e.0, MatchEvent::RaceTargetReached { side: Side::Left })),
            "left finishes while the match stays open: {after_left:?}"
        );

        push_side(&mut app, Side::Right, &[Action::HardDrop]);
        fixed_step(&mut app);
        assert_eq!(
            *app.world().resource::<VersusWinner>(),
            VersusWinner(Some(Side::Left)),
            "first finisher takes the perfect tie"
        );
        assert_eq!(*app.world().resource::<AppState>(), AppState::Playing);
        let crowning = drained_versus(&mut app);
        assert!(
            crowning
                .iter()
                .any(|e| matches!(e.0, MatchEvent::WinnerCrowned { side: Side::Left })),
            "crowning event reaches the message stream: {crowning:?}"
        );

        let frozen = versus(&app);
        push_side(&mut app, Side::Right, &[Action::HardDrop]);
        for _ in 0..3 {
            fixed_step(&mut app);
        }
        assert_eq!(versus(&app), frozen, "match frozen after crowning");
        assert_eq!(
            *app.world().resource::<VersusWinner>(),
            VersusWinner(Some(Side::Left))
        );
        let events = drained_versus(&mut app);
        assert!(
            events.is_empty(),
            "no events and no re-crowning on a frozen match: {events:?}"
        );
    }

    #[test]
    fn r_restarts_the_match() {
        let mut app = test_app(8);
        activate(&mut app, Controller::Human, Controller::Human);
        push_side(&mut app, Side::Left, &[Action::HardDrop]);
        fixed_step(&mut app);
        let progressed = versus(&app);

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyR);
        app.world_mut().run_schedule(Update);

        let after = versus(&app);
        assert_ne!(after, progressed, "R reset the match");
        assert_eq!(
            after,
            Match::new(app_world_seed(&app), AttackRule::Garbage).snapshot()
        );
        assert_eq!(versus_steps(&app), 0);
        assert!(app.world().non_send::<VersusMatch>().active);
    }

    fn app_world_seed(app: &App) -> u64 {
        app.world().non_send::<VersusMatch>().seed
    }

    #[test]
    fn end_versus_returns_to_title_and_reopens_solo() {
        let mut app = test_app(9);
        activate(&mut app, Controller::Human, Controller::Human);
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    end_versus(versus.into_inner(), winner.into_inner(), state.into_inner());
                });
            });
        assert_eq!(*app.world().resource::<AppState>(), AppState::Title);
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));

        fixed_step(&mut app);
        assert_eq!(versus_steps(&app), 0);
        assert!(drained_versus(&mut app).is_empty());
        // Solo is live again (still Title, so the solo core waits as always).
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
    }

    #[test]
    fn versus_bots_lock_at_most_once_per_cooldown_window() {
        // Regression: unpaced, the greedy solver drops a piece every few
        // fixed steps and buries the opponent in garbage within seconds.
        // Each bot must idle for BOT_LOCK_COOLDOWN_STEPS after its own lock.
        let mut app = test_app(42);
        start_in_app(
            &mut app,
            AttackRule::Garbage,
            Controller::Bot,
            Controller::Bot,
        );
        let mut locks = [0u32; 2];
        let window = 10 * BOT_LOCK_COOLDOWN_STEPS as usize; // ~10 s
        for _ in 0..window {
            fixed_step(&mut app);
            let drained: Vec<VersusEvent> = app
                .world_mut()
                .resource_mut::<Messages<VersusEvent>>()
                .drain()
                .collect();
            for e in drained {
                if let MatchEvent::PieceLocked { side, .. } = e.0 {
                    locks[if matches!(side, Side::Left) { 0 } else { 1 }] += 1;
                }
            }
        }
        for (index, n) in locks.iter().enumerate() {
            assert!(
                (*n as usize) <= window / BOT_LOCK_COOLDOWN_STEPS as usize + 2,
                "side {index} locked {n} times in {window} fixed steps — \
                 versus bot pacing broken"
            );
        }
        assert!(
            locks[0] + locks[1] > 0,
            "the bots still play, they are only throttled"
        );
    }

    #[test]
    fn bot_vs_bot_match_completes_and_crowns_a_winner() {
        let mut app = test_app(42);
        start_in_app(
            &mut app,
            AttackRule::Garbage,
            Controller::Bot,
            Controller::Bot,
        );

        for _ in 0..30_000 {
            fixed_step(&mut app);
            if app.world().resource::<VersusWinner>().0.is_some() {
                let winner = *app.world().resource::<VersusWinner>();
                assert!(matches!(winner.0, Some(Side::Left) | Some(Side::Right)));
                let events: Vec<VersusEvent> = app
                    .world_mut()
                    .resource_mut::<Messages<VersusEvent>>()
                    .drain()
                    .collect();
                assert!(
                    events
                        .iter()
                        .any(|e| matches!(e.0, MatchEvent::WinnerCrowned { .. })),
                    "crowning event reached the message stream"
                );
                let dead = match winner.0 {
                    Some(Side::Left) => versus(&app).right.game_over,
                    _ => versus(&app).left.game_over,
                };
                assert!(dead, "the loser's core is topped out");
                return;
            }
        }
        panic!("bot-vs-bot match never produced a winner within 30k fixed steps");
    }

    #[test]
    fn harness_restarts_after_a_match_and_exits_after_two() {
        // Drive the harness logic directly (no env var): first crowned match
        // logs + schedules a restart, the second exits with AppExit::Success.
        let mut app = test_app(11);
        // Consume `Startup` while the rule is still disabled, so
        // `versus_harness_startup` cannot clobber the matches started below.
        app.update();
        app.world_mut().resource_mut::<VersusHarness>().rule =
            Some(AttackRule::Race { target_lines: 0 });
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>() // keep Update R-system quiet
            .release(KeyCode::KeyR);

        let start_rule = AttackRule::Race { target_lines: 0 };
        for round in 0..2 {
            start_in_app(&mut app, start_rule, Controller::Bot, Controller::Bot);
            // Crown a winner via the bridge (bot drops).
            loop {
                fixed_step(&mut app);
                if app.world().non_send::<VersusMatch>().crowned {
                    break;
                }
            }
            app.update(); // harness sees the crowning in Update
            let harness = app.world().resource::<VersusHarness>();
            assert_eq!(harness.matches_done, round + 1);
            if round == 0 {
                assert!(harness.awaiting_restart, "restart scheduled");
                // Count the restart down and let it fire in a later frame.
                app.world_mut().resource_mut::<VersusHarness>().restart_in = 0.0;
                app.update();
                assert!(!app.world().resource::<VersusHarness>().awaiting_restart);
                assert_eq!(
                    *app.world().resource::<AppState>(),
                    AppState::Playing,
                    "harness restarted the match"
                );
            }
        }
        let exits: Vec<AppExit> = app
            .world_mut()
            .resource_mut::<Messages<AppExit>>()
            .drain()
            .collect();
        assert!(
            matches!(exits.first(), Some(AppExit::Success)),
            "harness exits after {HARNESS_MATCHES} matches: {exits:?}"
        );
    }
}
