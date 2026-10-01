//! Title / pause / game-over screens (T17).
//!
//! Three retained UI roots ([`TitleRoot`], [`PauseRoot`], [`GameOverRoot`])
//! are spawned once on `Startup` and toggled by [`AppState`] exactly like
//! T16's [`SettingsRoot`](crate::screens_settings::SettingsRoot): hidden UI
//! nodes never receive `Interaction`, and every click handler additionally
//! gates on the live state, so a screen is inert outside its own state even
//! though its systems keep running.
//!
//! ## Startup state
//!
//! PRD §7.1 wants the app to open on the Title screen, but T1 froze both
//! [`AppState::default()`] (which is `Playing`) and `main.rs` — and T1's
//! permanent smoke tests in `state.rs` run this very plugin set and assert
//! the resource stays `Playing`. The Title flip is therefore registered as a
//! `Startup` system only for **non-test builds** (`#[cfg(not(test))]`): the
//! shipped binary opens on Title, while the test binary (which contains T1's
//! assertions) starts exactly as before and drives `AppState` explicitly.
//! The flip itself lives in the pure [`startup_goto_title`] handler.
//!
//! ## Pause vs `SimPaused` vs juice
//!
//! The user pause owns [`SimPaused`] while it is held: the toggle sets
//! `SimPaused(true)` on Playing → Paused and clears it on resume. Following
//! juice's ownership contract (`JuiceFreeze` only ever releases the flag it
//! set itself), `resume_game` / `goto_title` / `retry_run` *never write
//! the flag while juice owns it* — juice's freeze gate releases it after its
//! owned frames elapse. The core bridge already double-gates stepping on
//! `AppState != Playing`, so a juice release landing mid user-pause can
//! never un-freeze the simulation.
//!
//! ## Ownership boundaries
//!
//! - The pause **chord** is `KeyBindings::slot(BindSlot::Pause)` (default
//!   Esc/P), read directly here — it is never a core `Action` (T12's module
//!   docs), and is suppressed while [`RebindingCapture`] is active.
//! - Retry (Pause → Restart and Game Over → Play again) goes through
//!   [`retry_run`] → [`start_mode_run`], i.e. the same mode-aware path the
//!   R key took since T5 (fresh seed unless `TETRIS_SEED`, mode pre-roll
//!   re-armed, play counter bumped); the R key itself stays in the core
//!   bridge (T14) and is *not* re-bound here.
//! - Terminal records are written once per entry into `AppState::GameOver`
//!   by [`terminal_record_system`], gated by the PRD mode × terminal-reason
//!   matrix in [`terminal_record`] (Sprint/Dig top-out records *nothing*;
//!   Ultra's score stands either way). T9 retired T15's interim
//!   `settings_persist` auto-recorder, which folded EVERY `GameOver` into
//!   the Marathon best and would pollute records from Sprint/Dig top-outs.
//!   The result screen reads [`TerminalResult`] plus [`Records`] for display.
//! - Opening [`AppState::Settings`] is just a state write — T16's
//!   `track_settings_entry` records the origin (Pause → back → Pause) and
//!   `handle_back` returns here, closing T16's deferred round-trip check.
//! - Quit writes `AppExit::Success` and latches [`QuitRequested`] so headless
//!   tests can observe the request (the App consumes/exits on the message).
//!
//! ## 1 v 1 (T26)
//!
//! Title grows a "1 v 1" entry leading through a two-step submenu flow
//! (rule: Garbage / Race, then opponent: Human / Bot) that launches a match
//! via [`start_versus`]; the flow lives in the [`VersusFlow`] resource
//! ([`VersusStage`]) with Back buttons *and* Escape walking it back. While
//! [`VersusWinner`] is set the [`VersusOverRoot`] overlay shows the winner
//! ([`winner_text`]) plus Rematch / Menu; the pause chord is deliberately
//! blocked for a finished match so the overlay can never be paused away.
//! Root visibility for [`VersusHudRoot`](crate::hud::VersusHudRoot) (the
//! versus HUD) and this overlay joins [`sync_root_visibility`] — versus has
//! no dedicated [`AppState`] variant, so those roots toggle on match
//! activity instead of state, still strictly through marker-filtered queries
//! (the foreign-entity regression test covers the shared Or-filter).

use bevy::ecs::system::SystemParam;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

use crate::core_bridge::net::online_ui::{
    net_winner_text, OnlineButton, OnlineFlow, OnlineUiPlugin,
};
use crate::core_bridge::net::{NetRole, NetSession, NetStatus};
use crate::core_bridge::{
    end_versus, start_mode_run, start_versus, Controller, Countdown, GameCore, SimPaused,
    VersusMatch, VersusWinner,
};
use crate::hud::VersusHudRoot;
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::juice::JuiceFreeze;
use crate::modes::{format_time_ticks, mode_key, ModeId};
use crate::records::{Record, Records};
use crate::screens_modes::{open_mode_select, record_line};
use crate::settings_persist::PersistedBestScore;
use crate::state::{AppState, CaptureOrder, RebindingCapture};
use tetris_core::game::GameSnapshot;
use tetris_core::mode::FinishReason;

/// App version shown on the title screen (PRD §7 "extras").
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Opaque backdrop for the title screen.
pub(crate) const PANEL_BG: Color = Color::srgb(0.09, 0.09, 0.12);
/// Dimming backdrop for the pause / game-over overlays.
const DIM_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.62);
pub(crate) const BUTTON_BG: Color = Color::srgb(0.22, 0.22, 0.27);
pub(crate) const RECORD_COLOR: Color = Color::srgb(1.0, 0.85, 0.3);

// ---------------------------------------------------------------------------
// Pure handlers (drive the transitions; systems are thin glue)
// ---------------------------------------------------------------------------

/// PRD §7.1: the shipped app opens on the Title screen. See the module docs
/// for why the `Startup` registration of this handler is test-excluded.
pub fn startup_goto_title(state: &mut AppState) {
    *state = AppState::Title;
}

/// `true` when any bind of [`BindSlot::Pause`] fired this frame: a bound key
/// just pressed, or a wheel notch matching `WheelUp`/`WheelDown`.
pub fn pause_chord_pressed(
    keys: &ButtonInput<KeyCode>,
    wheels: Option<&Messages<MouseWheel>>,
    bindings: &KeyBindings,
) -> bool {
    bindings
        .slot(BindSlot::Pause)
        .iter()
        .any(|bind| match bind {
            Bind::Key(key) => keys.just_pressed(*key),
            Bind::WheelUp => {
                wheels.is_some_and(|w| w.iter_current_update_messages().any(|e| e.y > 0.0))
            }
            Bind::WheelDown => {
                wheels.is_some_and(|w| w.iter_current_update_messages().any(|e| e.y < 0.0))
            }
        })
}

/// Release [`SimPaused`] unless juice currently owns it (see module docs).
pub fn release_sim(sim: &mut SimPaused, freeze: &JuiceFreeze) {
    if !freeze.owns_pause {
        sim.0 = false;
    }
}

/// Enter the pause overlay: state flip plus simulation freeze (PRD §7.3).
pub fn pause_game(state: &mut AppState, sim: &mut SimPaused) {
    *state = AppState::Paused;
    sim.0 = true;
}

/// Leave the pause overlay; only clears the freeze flag when we (not juice)
/// own it. No-op outside [`AppState::Paused`].
pub fn resume_game(state: &mut AppState, sim: &mut SimPaused, freeze: &JuiceFreeze) {
    if *state == AppState::Paused {
        *state = AppState::Playing;
        release_sim(sim, freeze);
    }
}

/// The pause chord's whole behavior: toggles Playing ↔ Paused, ignores every
/// other screen (Settings' Escape is T16's capture-cancel, Title/GameOver
/// have no pause).
pub fn toggle_pause(state: &mut AppState, sim: &mut SimPaused, freeze: &JuiceFreeze) {
    match *state {
        AppState::Playing => pause_game(state, sim),
        AppState::Paused => resume_game(state, sim, freeze),
        _ => {}
    }
}

/// Fresh run of the **active mode** via the shared T5
/// [`start_mode_run`] path: honors `TETRIS_SEED`, re-arms the mode's
/// pre-roll and bumps its play counter — the exact contract of the R retry
/// since T5. Used by Pause → Restart and Game Over → Play again (T9: both
/// resume the *selected* mode, never a silent Marathon fallback). Title →
/// Start opens [`AppState::ModeSelect`](crate::screens_modes) instead.
pub fn retry_run(
    core: &mut GameCore,
    countdown: &mut Countdown,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
    records: Option<&mut Records>,
) -> u64 {
    release_sim(sim, freeze);
    let id = core.active_mode.id;
    start_mode_run(id, core, countdown, state, records)
}

/// Abandon the run back to the title (PRD §7.3 "quit to title"); un-freezes
/// the sim on the way out.
pub fn goto_title(state: &mut AppState, sim: &mut SimPaused, freeze: &JuiceFreeze) {
    release_sim(sim, freeze);
    *state = AppState::Title;
}

/// Open the settings screen; T16's `track_settings_entry` records the origin
/// and [`handle_back`](crate::screens_settings::handle_back) restores it.
/// SimPaused is deliberately untouched so a Pause → Settings → Back round
/// trip stays frozen end to end.
pub fn open_settings(state: &mut AppState) {
    *state = AppState::Settings;
}

/// PRD §"records" terminal matrix (T9): what a finished run is worth, keyed
/// by `(mode, terminal reason)`. Anything the table omits records **nothing**
/// — that is the whole Sprint/Dig top-out rule ("a top-out gives no
/// result"), and unshipped modes (Zen, Bot Ladder, …) simply have no row
/// yet. **T13 extends this table with exactly one row**:
/// `(ModeId::Survival, FinishReason::TopOut) => Some(Record::BestTime { ticks })`
/// ("the result is time survived").
#[must_use]
pub fn terminal_record(
    id: ModeId,
    reason: FinishReason,
    snapshot: &GameSnapshot,
    ticks: u64,
) -> Option<Record> {
    match (id, reason) {
        // Marathon: the top-out score/level/lines is the record (today's
        // behavior, preserved).
        (ModeId::Marathon, FinishReason::TopOut)
        // Ultra: "The score stands either way" (PRD) — TimeUp *and* TopOut.
        | (ModeId::Ultra, FinishReason::TimeUp | FinishReason::TopOut) => Some(Record::BestScore {
            score: snapshot.score,
            level: snapshot.level,
            lines: snapshot.lines,
        }),
        // Sprint/Dig: a time exists only when the goal was reached.
        (ModeId::Sprint | ModeId::Dig, FinishReason::GoalReached) => Some(Record::BestTime { ticks }),
        _ => None,
    }
}

/// Headline row of the mode-aware game-over screen (T9). Empty string = no
/// headline (Marathon keeps exactly today's layout, and unfinished /
/// foreign GameOver flips stay silent).
#[must_use]
pub fn result_text(id: ModeId, reason: Option<FinishReason>, ticks: u64) -> String {
    match (id, reason) {
        (ModeId::Sprint | ModeId::Dig, Some(FinishReason::GoalReached)) => {
            format!("Time {}", format_time_ticks(ticks))
        }
        (ModeId::Sprint | ModeId::Dig, Some(FinishReason::TopOut)) => "No result".to_string(),
        (ModeId::Ultra, Some(FinishReason::TimeUp)) => "Time up".to_string(),
        (ModeId::Ultra, Some(FinishReason::TopOut)) => "Top out".to_string(),
        // T13 (Survival) extends: `(ModeId::Survival, Some(TopOut))` shows
        // the survived time exactly like the Sprint/Dig goal row.
        _ => String::new(),
    }
}

/// Final stats line (PRD §7.4: score, level and lines).
pub fn stats_text(score: u64, level: u32, lines: u32) -> String {
    format!("Score {score}   Level {level}   Lines {lines}")
}

/// Persisted best line (PRD §7.4 "best").
pub fn best_text(best: &PersistedBestScore) -> String {
    format!("Best {}", best.score)
}

/// What the finished run meant, latched once per entry into
/// [`AppState::GameOver`] by [`terminal_record_system`] and consumed by the
/// result screen (T9). `reason` is `None` until a real terminal has been
/// observed (core missing or a foreign GameOver flip), which keeps the
/// legacy Marathon layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct TerminalResult {
    /// The [`ModeId`] of the run that ended.
    pub mode: ModeId,
    /// Why the game finished (`Game::finished_reason()` at terminal).
    pub reason: Option<FinishReason>,
    /// `Game::tick_count()` at terminal — the finish time for `GoalReached`.
    pub ticks: u64,
    /// `true` when the terminal fold improved the mode's stored record
    /// (`Records::record_run` returned `true`) — drives the NEW RECORD marker.
    pub new_record: bool,
}

impl Default for TerminalResult {
    fn default() -> Self {
        Self {
            mode: ModeId::Marathon,
            reason: None,
            ticks: 0,
            new_record: false,
        }
    }
}

/// Which step of the Title → 1v1 submenu flow is showing (T26). Versus has
/// no [`AppState`] variant (it plays inside `Playing`), so the flow state is
/// owned here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub enum VersusStage {
    /// Plain title menu.
    #[default]
    Title,
    /// Pick the attack rule (Garbage / Race).
    Rules,
    /// Pick the P2 opponent (Human / Bot); launches on choice.
    Opponent,
}

/// Title 1v1 submenu flow state: current [`VersusStage`] plus the rule
/// picked on the way through (only meaningful from [`VersusStage::Opponent`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub struct VersusFlow {
    /// Which submenu (if any) is showing.
    pub stage: VersusStage,
    /// Rule selected in the rules step; defaults to
    /// [`AttackRule::default`] until the player picks one.
    pub rule: AttackRule,
}

/// Advance the flow one step back: Opponent → Rules → Title.
pub fn versus_flow_back(flow: &mut VersusFlow) {
    flow.stage = match flow.stage {
        VersusStage::Opponent => VersusStage::Rules,
        _ => VersusStage::Title,
    };
}

/// Launch a match from the finished flow (P1 is always a local human; the
/// submenu picked the rule and the P2 opponent), then close the flow.
pub fn launch_versus(
    versus: &mut VersusMatch,
    winner: &mut VersusWinner,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
    flow: &mut VersusFlow,
    p2: Controller,
) {
    release_sim(sim, freeze);
    let rule = flow.rule;
    *flow = VersusFlow::default();
    start_versus(versus, winner, state, rule, Controller::Human, p2);
}

/// Winner overlay "Rematch": restart with the same rule and controllers
/// (the T25 R-key semantics, exposed to the menu).
pub fn rematch_versus(
    versus: &mut VersusMatch,
    winner: &mut VersusWinner,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
) {
    release_sim(sim, freeze);
    let (rule, p1, p2) = (versus.rule, versus.p1, versus.p2);
    start_versus(versus, winner, state, rule, p1, p2);
}

/// Winner overlay "Menu": leave the match back to the title.
pub fn versus_to_title(
    versus: &mut VersusMatch,
    winner: &mut VersusWinner,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
) {
    release_sim(sim, freeze);
    end_versus(versus, winner, state);
}

/// Overlay headline for a crowned match: "BOT WINS" when the winning side
/// is bot-driven, else "PLAYER 1 WINS" / "PLAYER 2 WINS" (T26).
pub fn winner_text(winner: Side, p1: Controller, p2: Controller) -> String {
    let controller = if winner == Side::Left { p1 } else { p2 };
    match controller {
        Controller::Bot => "BOT WINS".to_string(),
        // N4 compile carve-out; N5 refines the netplay copy.
        Controller::Net => "OPPONENT WINS".to_string(),
        Controller::Human if winner == Side::Left => "PLAYER 1 WINS".to_string(),
        Controller::Human => "PLAYER 2 WINS".to_string(),
    }
}

// ---------------------------------------------------------------------------
// UI markers
// ---------------------------------------------------------------------------

/// Root of the title screen; visible only in [`AppState::Title`].
#[derive(Component)]
pub struct TitleRoot;

/// Root of the pause overlay; visible only in [`AppState::Paused`].
#[derive(Component)]
pub struct PauseRoot;

/// Root of the game-over screen; visible only in [`AppState::GameOver`].
#[derive(Component)]
pub struct GameOverRoot;

/// Title "Start" button → [`AppState::ModeSelect`] (T7 — the mode list in
/// [`crate::screens_modes`] decides what actually starts).
#[derive(Component)]
pub struct StartButton;

/// Game-over "Play again" button ([`retry_run`] — same mode, fresh seed).
#[derive(Component)]
pub struct PlayAgainButton;

/// Pause "Resume" button ([`resume_game`]).
#[derive(Component)]
pub struct ResumeButton;

/// Restart button on both the pause overlay and game-over screen
/// ([`retry_run`] — restarts the *selected* mode, T9).
#[derive(Component)]
pub struct RestartButton;

/// "Settings" button on the title and pause screens ([`open_settings`]).
#[derive(Component)]
pub struct OpenSettingsButton;

/// "Menu" button: on the pause overlay [`goto_title`] (PRD §7.3), on the
/// game-over screen [`open_mode_select`] (T9 — straight back to the mode
/// list).
#[derive(Component)]
pub struct QuitToTitleButton;

/// "Quit" button on every screen: writes `AppExit::Success`.
#[derive(Component)]
pub struct QuitButton;

/// Game-over final stats label ([`stats_text`]).
#[derive(Component)]
pub struct StatsText;

/// Best-score label on the title and game-over screens: the Marathon
/// [`best_text`] on the title, and the *per-mode* record line
/// ([`record_line`]) on the game-over screen (T9).
#[derive(Component)]
pub struct BestText;

/// "NEW RECORD!" label (latched [`TerminalResult::new_record`], T9).
#[derive(Component)]
pub struct RecordText;

/// Mode-aware headline row of the game-over screen ([`result_text`], T9);
/// display-hidden whenever the mode reports no headline.
#[derive(Component)]
pub struct ResultText;

/// Latched `true` when a quit button requested [`AppExit::Success`]. The
/// exit message itself is consumed by the App during `update()`, so headless
/// tests observe the request through this marker instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct QuitRequested(pub bool);

/// Title "1 v 1" entry → opens the rules submenu (T26).
#[derive(Component)]
pub struct OneVOneButton;

/// Root of the 1v1 rule submenu (Garbage / Race); visible in
/// [`AppState::Title`] while [`VersusStage::Rules`] is active.
#[derive(Component)]
pub struct VersusRulesRoot;

/// Root of the 1v1 opponent submenu (Human / Bot); visible in
/// [`AppState::Title`] while [`VersusStage::Opponent`] is active.
#[derive(Component)]
pub struct VersusOpponentRoot;

/// "Garbage" rule button in the rules submenu.
#[derive(Component)]
pub struct RuleGarbageButton;

/// "Race" rule button in the rules submenu.
#[derive(Component)]
pub struct RuleRaceButton;

/// "Human" opponent button: launches a local-human vs local-human match.
#[derive(Component)]
pub struct OpponentHumanButton;

/// "Bot" opponent button: launches a local-human vs bot match.
#[derive(Component)]
pub struct OpponentBotButton;

/// "Back" button on both versus submenus ([`versus_flow_back`]).
#[derive(Component)]
pub struct VersusBackButton;

/// Root of the versus winner overlay; visible while a match is active with
/// [`VersusWinner`] set (T26).
#[derive(Component)]
pub struct VersusOverRoot;

/// Winner headline label ([`winner_text`]).
#[derive(Component)]
pub struct VersusWinnerText;

/// Winner overlay "Rematch" button ([`rematch_versus`]).
#[derive(Component)]
pub struct VersusRematchButton;

/// Winner overlay "Menu" button ([`versus_to_title`]).
#[derive(Component)]
pub struct VersusMenuButton;

// ---------------------------------------------------------------------------
// Systems (UI glue)
// ---------------------------------------------------------------------------

/// Root visibility query over the menu screens and the versus roots (T26).
/// The `Or` filter is load-bearing: `Visibility` is a default component on
/// every entity, so a bare `Has<…>` query would match (and hide) the entire
/// world. The two versus roots toggle on *match activity* rather than an
/// [`AppState`] variant (versus has none — it plays inside `Playing`).
type MenuRoots<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Visibility,
        Has<TitleRoot>,
        Has<PauseRoot>,
        Has<GameOverRoot>,
        Has<VersusHudRoot>,
        Has<VersusOverRoot>,
    ),
    Or<(
        With<TitleRoot>,
        With<PauseRoot>,
        With<GameOverRoot>,
        With<VersusHudRoot>,
        With<VersusOverRoot>,
    )>,
>;

#[derive(SystemParam)]
struct RootVisibilityParams<'w, 's> {
    state: Res<'w, AppState>,
    flow: Res<'w, VersusFlow>,
    online_flow: Option<Res<'w, OnlineFlow>>,
    versus: Option<NonSend<'w, VersusMatch>>,
    winner: Option<Res<'w, VersusWinner>>,
    roots: MenuRoots<'w, 's>,
}

/// Show each root only in its own [`AppState`] variant; the versus HUD root
/// shows for the whole duration of an active match and the versus winner
/// overlay while that match has a crowned winner (T26).
fn sync_root_visibility(params: RootVisibilityParams) {
    let RootVisibilityParams {
        state,
        flow,
        online_flow,
        versus,
        winner,
        mut roots,
    } = params;
    let versus_active = versus.is_some_and(|versus| versus.active);
    let match_over = versus_active && winner.is_some_and(|winner| winner.0.is_some());
    // N5: an open online flow hides the title root (same discipline as the
    // 1v1 submenus) so its buttons can never be clicked through a panel.
    let online_open = online_flow.is_some_and(|online_flow| online_flow.open());
    for (mut vis, title, pause, over, versus_hud, versus_over) in roots.iter_mut() {
        let wanted = if (title
            && *state == AppState::Title
            && flow.stage == VersusStage::Title
            && !online_open)
            || (pause && *state == AppState::Paused)
            || (over && *state == AppState::GameOver)
            || (versus_hud && versus_active)
            || (versus_over && match_over)
        {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

/// Show the 1v1 submenus over the title while their [`VersusStage`] is
/// active (T26); leaving Title hides both and resets a half-finished flow,
/// so the next visit opens on the plain title. The `Or` filter keeps
/// foreign entities out of the query (same discipline as [`MenuRoots`]).
#[allow(clippy::type_complexity)]
type VersusSubmenuRoots<'w, 's> = Query<
    'w,
    's,
    (&'static mut Visibility, Has<VersusRulesRoot>),
    Or<(With<VersusRulesRoot>, With<VersusOpponentRoot>)>,
>;

fn sync_versus_menu_visibility(
    state: Res<AppState>,
    mut flow: ResMut<VersusFlow>,
    mut roots: VersusSubmenuRoots,
) {
    if flow.stage != VersusStage::Title && *state != AppState::Title {
        flow.stage = VersusStage::Title;
        flow.rule = AttackRule::default();
    }
    for (mut vis, rules) in roots.iter_mut() {
        let wanted = if (rules && flow.stage == VersusStage::Rules)
            || (!rules && flow.stage == VersusStage::Opponent && *state == AppState::Title)
        {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

/// Consume the pause chord. Suppressed entirely while rebinding is capturing
/// (the chord's "when NOT rebinding" contract) and outside Playing/Paused via
/// [`toggle_pause`]. While a versus match has crowned a winner the chord is
/// blocked entering a *fresh* pause (T26): the match is frozen anyway, and a
/// pause overlay must never cover the winner overlay.
#[allow(clippy::too_many_arguments)]
fn pause_chord_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    wheels: Option<Res<Messages<MouseWheel>>>,
    bindings: Res<KeyBindings>,
    capture: Res<RebindingCapture>,
    mut state: ResMut<AppState>,
    mut sim: ResMut<SimPaused>,
    freeze: Res<JuiceFreeze>,
    versus: Option<NonSend<VersusMatch>>,
    winner: Option<Res<VersusWinner>>,
    net: Option<Res<NetSession>>,
) {
    if capture.capturing {
        return;
    }
    // N5: net matches have no pause — lockstep is authoritative on both ends,
    // so Escape belongs to the online flow's leave-confirm instead (its own
    // system consumes the chord while [`NetStatus::InMatch`]).
    if net.is_some_and(|net| net.status == NetStatus::InMatch) {
        return;
    }
    let finished_match = versus.is_some_and(|versus| versus.active)
        && winner.is_some_and(|winner| winner.0.is_some());
    if finished_match && *state == AppState::Playing {
        return;
    }
    let Some(keys) = keys else {
        return;
    };
    if pause_chord_pressed(&keys, wheels.as_deref(), &bindings) {
        toggle_pause(&mut state, &mut sim, &freeze);
    }
}

/// One query over every menu button in the tree (T16 `ClickQuery` pattern).
type MenuClickQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Interaction,
        Has<StartButton>,
        Has<PlayAgainButton>,
        Has<ResumeButton>,
        Has<RestartButton>,
        Has<OpenSettingsButton>,
        Has<QuitToTitleButton>,
        Has<QuitButton>,
    ),
    (With<Button>, Changed<Interaction>),
>;

#[derive(SystemParam)]
struct MenuClickParams<'w, 's> {
    buttons: MenuClickQuery<'w, 's>,
    core: NonSendMut<'w, GameCore>,
    countdown: ResMut<'w, Countdown>,
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    quit: ResMut<'w, QuitRequested>,
    flow: Res<'w, VersusFlow>,
    versus: Option<NonSendMut<'w, VersusMatch>>,
    winner: Option<ResMut<'w, VersusWinner>>,
    records: Option<ResMut<'w, Records>>,
    exits: MessageWriter<'w, AppExit>,
}

fn menu_button_clicks(mut params: MenuClickParams) {
    if !matches!(
        *params.state,
        AppState::Title | AppState::Paused | AppState::GameOver
    ) {
        return;
    }
    for (_entity, interaction, start, again, resume, restart, settings, to_title, quit) in
        params.buttons.iter()
    {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let mut quit_now = || {
            params.quit.0 = true;
            params.exits.write(AppExit::Success);
        };
        match *params.state {
            AppState::Title => {
                // A 1v1 submenu covers the title while it is open; its own
                // click system handles those buttons (defence in depth).
                if params.flow.stage != VersusStage::Title {
                    continue;
                }
                if start {
                    open_mode_select(&mut params.state, &mut params.sim, &params.freeze);
                } else if settings {
                    open_settings(&mut params.state);
                } else if quit {
                    quit_now();
                }
            }
            AppState::Paused => {
                if resume {
                    resume_game(&mut params.state, &mut params.sim, &params.freeze);
                } else if restart {
                    retry_run(
                        params.core.as_mut(),
                        params.countdown.as_mut(),
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                        params.records.as_deref_mut(),
                    );
                } else if settings {
                    open_settings(&mut params.state);
                } else if to_title {
                    // Pause → "Quit to Title" on an active 1v1 must tear the
                    // match down like the winner overlay's Menu button: the
                    // versus HUD root shows for the whole duration of an
                    // active match, so a live match left running keeps its
                    // full-screen root visible over the title and swallows
                    // every subsequent menu click.
                    if let (Some(versus), Some(winner)) =
                        (params.versus.as_deref_mut(), params.winner.as_deref_mut())
                    {
                        if versus.active {
                            end_versus(versus, winner, &mut params.state);
                        }
                    }
                    goto_title(&mut params.state, &mut params.sim, &params.freeze);
                } else if quit {
                    quit_now();
                }
            }
            AppState::GameOver => {
                if again {
                    retry_run(
                        params.core.as_mut(),
                        params.countdown.as_mut(),
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                        params.records.as_deref_mut(),
                    );
                } else if to_title {
                    // T9: the result screen's "Menu" walks back to the mode
                    // list (the mode you just played is one row away), not
                    // past it to the title.
                    open_mode_select(&mut params.state, &mut params.sim, &params.freeze);
                } else if quit {
                    quit_now();
                }
            }
            _ => {}
        }
    }
}

/// Buttons of the 1v1 flow and the winner overlay (T26).
type VersusClickQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Interaction,
        Has<OneVOneButton>,
        Has<RuleGarbageButton>,
        Has<RuleRaceButton>,
        Has<OpponentHumanButton>,
        Has<OpponentBotButton>,
        Has<VersusBackButton>,
        Has<VersusRematchButton>,
        Has<VersusMenuButton>,
    ),
    (With<Button>, Changed<Interaction>),
>;

#[derive(SystemParam)]
struct VersusClickParams<'w, 's> {
    buttons: VersusClickQuery<'w, 's>,
    versus: Option<NonSendMut<'w, VersusMatch>>,
    winner: Option<ResMut<'w, VersusWinner>>,
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    flow: ResMut<'w, VersusFlow>,
    net: Option<Res<'w, NetSession>>,
}

fn versus_button_clicks(mut params: VersusClickParams) {
    let versus_active = params.versus.as_deref().is_some_and(|versus| versus.active);
    let match_over = versus_active
        && params
            .winner
            .as_deref()
            .is_some_and(|winner| winner.0.is_some());
    let on_title = *params.state == AppState::Title && !versus_active;
    // N5: while a net match is live the winner overlay's Rematch/Menu belong
    // to the online flow (host re-arms via `start_net_match`, Menu runs the
    // full `net_leave_to_title` teardown). The local `start_versus` reseed
    // here would fork the guest mirror — never a fallback on this path.
    let net_in_match = params
        .net
        .is_some_and(|net| net.status == NetStatus::InMatch);

    for (_entity, interaction, one_v_one, garbage, race, human, bot, back, rematch, menu) in
        params.buttons.iter()
    {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if on_title {
            match params.flow.stage {
                VersusStage::Title => {
                    if one_v_one {
                        params.flow.stage = VersusStage::Rules;
                    }
                }
                VersusStage::Rules => {
                    if garbage {
                        params.flow.rule = AttackRule::Garbage;
                        params.flow.stage = VersusStage::Opponent;
                    } else if race {
                        params.flow.rule = AttackRule::Race {
                            target_lines: DEFAULT_RACE_LINES,
                        };
                        params.flow.stage = VersusStage::Opponent;
                    } else if back {
                        versus_flow_back(&mut params.flow);
                    }
                }
                VersusStage::Opponent => {
                    if back {
                        versus_flow_back(&mut params.flow);
                    } else if human || bot {
                        let p2 = if bot {
                            Controller::Bot
                        } else {
                            Controller::Human
                        };
                        if let (Some(versus), Some(winner)) =
                            (params.versus.as_deref_mut(), params.winner.as_deref_mut())
                        {
                            launch_versus(
                                versus,
                                winner,
                                &mut params.state,
                                &mut params.sim,
                                &params.freeze,
                                &mut params.flow,
                                p2,
                            );
                        }
                    }
                }
            }
            continue;
        }

        if match_over && !net_in_match {
            if rematch {
                if let (Some(versus), Some(winner)) =
                    (params.versus.as_deref_mut(), params.winner.as_deref_mut())
                {
                    rematch_versus(
                        versus,
                        winner,
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                    );
                }
            } else if menu {
                if let (Some(versus), Some(winner)) =
                    (params.versus.as_deref_mut(), params.winner.as_deref_mut())
                {
                    versus_to_title(
                        versus,
                        winner,
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                    );
                }
            }
        }
    }
}

/// Escape walks the 1v1 submenu back one step (T26). Only while the Title
/// screen shows a submenu — elsewhere Escape keeps its pause-chord meaning
/// ([`toggle_pause`] is inert on Title anyway, so the two never collide).
fn versus_flow_esc_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    capture: Res<RebindingCapture>,
    state: Res<AppState>,
    mut flow: ResMut<VersusFlow>,
) {
    if capture.capturing {
        return;
    }
    let Some(keys) = keys else {
        return;
    };
    if *state != AppState::Title || flow.stage == VersusStage::Title {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        versus_flow_back(&mut flow);
    }
}

/// The central terminal recorder (T9): fires exactly once per entry into
/// [`AppState::GameOver`] (the state write the bridge performs on the
/// terminal event; `is_changed` clears on the next frame, so repeated frames
/// inside GameOver never re-record — and equal re-entries are idempotent
/// anyway because `record_run` keeps the first on ties). Reads
/// `GameCore::active_mode.id` + `Game::finished_reason()` + snapshot, folds
/// them through the [`terminal_record`] matrix via
/// [`Records::record_run`], force-flushes an improved record (same disk
/// policy as the retired interim writer) and latches the
/// [`TerminalResult`] the result screen renders. Play counters are *not*
/// bumped here — `start_mode_run` owns that (T5/T6). No-ops without the
/// core bridge or for foreign GameOver flips (`finished_reason() == None`).
fn terminal_record_system(
    state: Res<AppState>,
    core: Option<NonSend<GameCore>>,
    mut records: Option<ResMut<Records>>,
    mut queue: Option<ResMut<crate::records::RecordsSaveQueue>>,
    mut result: ResMut<TerminalResult>,
) {
    if !state.is_changed() || *state != AppState::GameOver {
        return;
    }
    let Some(core) = core else { return };
    let id = core.active_mode.id;
    let snapshot = core.game.snapshot();
    let ticks = core.game.tick_count();
    let reason = core.game.finished_reason();
    let mut new_record = false;
    if let (Some(reason), Some(records)) = (reason, records.as_deref_mut()) {
        if let Some(record) = terminal_record(id, reason, &snapshot, ticks) {
            new_record = records.record_run(mode_key(id), record);
            if new_record {
                if let Some(queue) = queue.as_deref_mut() {
                    queue.pending = true;
                    queue.force = true;
                }
            }
        }
    }
    *result = TerminalResult {
        mode: id,
        reason,
        ticks,
        new_record,
    };
}

/// Winner-headline label query.
type VersusWinnerLabels<'w, 's> = Query<'w, 's, &'static mut Text, With<VersusWinnerText>>;

/// Write "PLAYER 1 WINS" / "PLAYER 2 WINS" / "BOT WINS" into the winner
/// overlay whenever [`VersusWinner`] moves (T26).
fn sync_versus_winner_text(
    versus: Option<NonSend<VersusMatch>>,
    winner: Option<Res<VersusWinner>>,
    net: Option<Res<NetSession>>,
    mut labels: VersusWinnerLabels,
) {
    let Some(winner_side) = winner.and_then(|winner| winner.0) else {
        return;
    };
    let Some(versus) = versus else {
        return;
    };
    // N5: once a seat is Net the generic "OPPONENT WINS" carve-out gives way
    // to role-aware copy — the local seat winning reads "YOU WIN".
    let text = if versus.p1 == Controller::Net || versus.p2 == Controller::Net {
        let role = net.as_deref().map(|net| net.role).unwrap_or(NetRole::Host);
        net_winner_text(winner_side, role)
    } else {
        winner_text(winner_side, versus.p1, versus.p2)
    };
    for mut label in labels.iter_mut() {
        if label.0 != text {
            *label = Text::new(text.clone());
        }
    }
}

/// Final-stats label query.
type StatsLabels<'w, 's> = Query<
    'w,
    's,
    &'static mut Text,
    (
        With<StatsText>,
        Without<BestText>,
        Without<RecordText>,
        Without<ResultText>,
    ),
>;

/// Best-score label query.
type BestLabels<'w, 's> =
    Query<'w, 's, &'static mut Text, (With<BestText>, Without<RecordText>, Without<ResultText>)>;

/// Record-highlight label query.
type RecordLabels<'w, 's> =
    Query<'w, 's, &'static mut Text, (With<RecordText>, Without<ResultText>)>;

/// Mode-headline label query (text + layout slot, T9 — `Display::None`
/// keeps Marathon's screen pixel-identical to today's when empty).
type ResultLabels<'w, 's> = Query<
    'w,
    's,
    (&'static mut Text, &'static mut Node),
    (
        With<ResultText>,
        Without<StatsText>,
        Without<BestText>,
        Without<RecordText>,
    ),
>;

/// Rewrite the dynamic labels (headline / stats / best / NEW RECORD)
/// whenever the state, the best view or the terminal result moves. The
/// recording itself is T9's exclusive job (`terminal_record_system`); this
/// display path never writes records. Per mode (T9): Marathon keeps today's
/// exact layout, Sprint/Dig/Ultra read their own record line from
/// [`Records`] and their headline from [`TerminalResult`].
#[derive(SystemParam)]
struct ScreenTextParams<'w, 's> {
    state: Res<'w, AppState>,
    best: Res<'w, PersistedBestScore>,
    records: Option<Res<'w, Records>>,
    result: Res<'w, TerminalResult>,
    core: Option<NonSend<'w, GameCore>>,
    stats: StatsLabels<'w, 's>,
    best_labels: BestLabels<'w, 's>,
    record_labels: RecordLabels<'w, 's>,
    result_labels: ResultLabels<'w, 's>,
}

fn sync_screen_texts(params: ScreenTextParams) {
    let ScreenTextParams {
        state,
        best,
        records,
        result,
        core,
        mut stats,
        mut best_labels,
        mut record_labels,
        mut result_labels,
    } = params;
    if !state.is_changed() && !best.is_changed() && !result.is_changed() {
        return;
    }
    let game_over = *state == AppState::GameOver;
    // Per-mode best line (T9): the title and Marathon keep the persisted
    // Marathon view verbatim; a finished Sprint/Dig/Ultra run shows its own
    // record row (`record_line` from T7).
    let mode = if game_over {
        core.as_deref().map(|core| core.active_mode.id)
    } else {
        None
    };
    let best_string = match mode {
        Some(mode) if mode != ModeId::Marathon => records.map_or_else(
            || "-".to_string(),
            |records| record_line(records.record_for(mode_key(mode))),
        ),
        _ => best_text(&best),
    };
    for mut text in best_labels.iter_mut() {
        *text = Text::new(best_string.clone());
    }
    let record = if game_over && result.new_record {
        "NEW RECORD!"
    } else {
        ""
    };
    for mut text in record_labels.iter_mut() {
        *text = Text::new(record);
    }
    let headline = if game_over {
        result_text(result.mode, result.reason, result.ticks)
    } else {
        String::new()
    };
    let headline_shown = !headline.is_empty();
    for (mut text, mut node) in result_labels.iter_mut() {
        *text = Text::new(headline.clone());
        let wanted = if headline_shown {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != wanted {
            node.display = wanted;
        }
    }
    if let Some(core) = core {
        let snapshot = core.game.snapshot();
        let stats_string = stats_text(snapshot.score, snapshot.level, snapshot.lines);
        for mut text in stats.iter_mut() {
            *text = Text::new(stats_string.clone());
        }
    }
}

/// Startup glue for [`startup_goto_title`]; registered only in non-test
/// builds so T1's permanent `state.rs` smoke assertions (which run this very
/// plugin tree and expect the default `Playing`) stay green.
fn startup_goto_title_system(mut state: ResMut<AppState>) {
    startup_goto_title(&mut state);
}

// ---------------------------------------------------------------------------
// Startup UI construction
// ---------------------------------------------------------------------------

pub(crate) fn label_node(text: String, size: f32) -> (Text, TextFont, TextColor, Pickable) {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor::WHITE,
        // Labels never own a click — the button underneath them does.
        Pickable::IGNORE,
    )
}

/// Marks `Pickable::IGNORE` auto-applied by [`sync_hidden_ui_unpickable`] so
/// it can be lifted when the node becomes visible again; user-authored
/// `IGNORE` (labels, menu roots) survives untouched.
#[derive(Component)]
struct AutoUnpickable;

/// Third picking incident (after the T26 `ZIndex(1)` submenu-root fix in
/// `build_menu_ui` and the click-through regression): Bevy 0.19 UI picking
/// derives hit depth from **query-iteration order** and ignores `ZIndex`
/// entirely (`bevy_ui-0.19.1/src/picking_backend.rs:251-269`), and
/// resources *are* entities in 0.19 ECS — inserting ANY new resource shifts
/// entity indices and re-randomises which (possibly hidden) widget wins an
/// equal-depth tie (a unit `insert_resource` in an empty plugin was enough
/// to flip the 1v1 submenu click test). Root fix: **hidden UI never
/// participates in picking.** Every hidden `Node` gets `Pickable::IGNORE`
/// (hover, press and click all honor it) and it is lifted when the node
/// becomes visible again; `add_menu_root` roots and `label_node` labels
/// carry `Pickable::IGNORE` permanently so containers and text can never
/// swallow or steal a click. A click resolves to the topmost *visible*
/// button by construction — never to entity-iteration order.
///
/// **G3:** no wiring needed — join-by-code buttons and panels inherit this
/// automatically through `Visibility`; still spawn buttons with
/// [`menu_button`], labels with [`label_node`] and full-screen panels via
/// `add_menu_root` to keep the container rule uniform. Runs in `PreUpdate`:
/// picking's hover pass runs later the same frame, and
/// `InheritedVisibility` was computed in the previous frame's `PostUpdate`.
#[allow(clippy::type_complexity)]
fn sync_hidden_ui_unpickable(
    mut commands: Commands,
    ui: Query<
        (
            Entity,
            &InheritedVisibility,
            Option<&AutoUnpickable>,
            Option<&Pickable>,
        ),
        With<Node>,
    >,
) {
    for (entity, inherited, auto, pickable) in &ui {
        if !inherited.get() {
            if !pickable.is_some_and(|p| *p == Pickable::IGNORE) {
                commands
                    .entity(entity)
                    .insert((AutoUnpickable, Pickable::IGNORE));
            }
        } else if auto.is_some() {
            commands
                .entity(entity)
                .remove::<(AutoUnpickable, Pickable)>();
        }
    }
}

pub(crate) fn menu_button(parent: &mut ChildSpawnerCommands, text: &str, marker: impl Bundle) {
    parent
        .spawn((
            Button,
            marker,
            BackgroundColor(BUTTON_BG),
            Node {
                width: Val::Px(220.0),
                height: Val::Px(36.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|button| {
            button.spawn(label_node(text.to_string(), 18.0));
        });
}

fn add_menu_root(
    commands: &mut Commands,
    marker: impl Bundle,
    background: Color,
    build: impl FnOnce(&mut ChildSpawnerCommands),
) {
    commands
        .spawn((
            marker,
            Visibility::Hidden,
            // Containers are inert: a click must resolve to a visible
            // button only (see `MenuPickTarget` docs).
            Pickable::IGNORE,
            BackgroundColor(background),
            Node {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: Val::Px(10.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .with_children(build);
}

fn build_menu_ui(mut commands: Commands, bindings: Res<KeyBindings>) {
    let pause_hint = format!(
        "Pause: {}   (start with Enter-free mouse clicks)",
        crate::screens_settings::slot_display(bindings.slot(BindSlot::Pause))
    );

    add_menu_root(&mut commands, TitleRoot, PANEL_BG, |root| {
        root.spawn(label_node("BLOCKFALL".to_string(), 64.0));
        root.spawn(label_node(format!("v{VERSION}"), 14.0));
        root.spawn((BestText, label_node(String::new(), 20.0)));
        root.spawn(label_node(pause_hint, 14.0));
        menu_button(root, "Start", StartButton);
        menu_button(root, "1 v 1", OneVOneButton);
        menu_button(root, "Online", OnlineButton);
        menu_button(root, "Settings", OpenSettingsButton);
        // Android: "Quit" would AppExit with the process still alive behind
        // a destroyed NativeActivity (a headless zombie winit loop); phone
        // exits go through the system back gesture.
        #[cfg(not(target_os = "android"))]
        menu_button(root, "Quit", QuitButton);
    });

    // The submenu roots render *over* the title (their `ZIndex(1)` controls
    // draw order; it is NOT load-bearing for picking — 0.19 picking ignores
    // ZIndex and sorts by iteration order, see `MenuPickTarget` docs). The
    // title hides while a submenu is open (see `sync_root_visibility`) and
    // `sync_menu_button_pickability` makes the hidden title buttons
    // un-pick-able, so real pointer input can only ever reach the visible
    // panel.
    add_menu_root(
        &mut commands,
        (VersusRulesRoot, ZIndex(1)),
        PANEL_BG,
        |root| {
            root.spawn(label_node("1 v 1 — RULE".to_string(), 40.0));
            menu_button(root, "Garbage", RuleGarbageButton);
            menu_button(root, "Race", RuleRaceButton);
            menu_button(root, "Back", VersusBackButton);
        },
    );

    add_menu_root(
        &mut commands,
        (VersusOpponentRoot, ZIndex(1)),
        PANEL_BG,
        |root| {
            root.spawn(label_node("1 v 1 — OPPONENT".to_string(), 40.0));
            menu_button(root, "Human", OpponentHumanButton);
            menu_button(root, "Bot", OpponentBotButton);
            menu_button(root, "Back", VersusBackButton);
        },
    );

    add_menu_root(&mut commands, VersusOverRoot, DIM_BG, |root| {
        root.spawn((VersusWinnerText, label_node(String::new(), 48.0)));
        menu_button(root, "Rematch", VersusRematchButton);
        menu_button(root, "Menu", VersusMenuButton);
    });

    add_menu_root(&mut commands, PauseRoot, DIM_BG, |root| {
        root.spawn(label_node("PAUSED".to_string(), 48.0));
        menu_button(root, "Resume", ResumeButton);
        menu_button(root, "Restart", RestartButton);
        menu_button(root, "Settings", OpenSettingsButton);
        menu_button(root, "Quit to title", QuitToTitleButton);
        #[cfg(not(target_os = "android"))]
        menu_button(root, "Quit", QuitButton);
    });

    add_menu_root(&mut commands, GameOverRoot, DIM_BG, |root| {
        root.spawn(label_node("GAME OVER".to_string(), 48.0));
        // T9 mode-aware headline (final time / "No result" / Ultra ending);
        // layout-hidden unless the mode reports one, keeping the Marathon
        // screen pixel-identical to the legacy layout.
        root.spawn((
            ResultText,
            Node {
                display: Display::None,
                ..default()
            },
            label_node(String::new(), 26.0),
        ));
        root.spawn((StatsText, label_node(String::new(), 24.0)));
        root.spawn((BestText, label_node(String::new(), 20.0)));
        root.spawn((
            RecordText,
            Text::new(String::new()),
            TextFont::from_font_size(26.0),
            TextColor(RECORD_COLOR),
        ));
        menu_button(root, "Play again", PlayAgainButton);
        menu_button(root, "Menu", QuitToTitleButton);
        #[cfg(not(target_os = "android"))]
        menu_button(root, "Quit", QuitButton);
    });
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Title / pause / game-over screens (T17) plus the 1v1 entry flow, winner
/// overlay and versus root visibility (T26).
pub struct MenuScreensPlugin;

impl Plugin for MenuScreensPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits (T16 precedent): all no-ops when the owning plugin
        // already registered the resource, but every handler parameter stays
        // satisfied in headless `MinimalPlugins` tests.
        app.init_resource::<AppState>()
            .init_resource::<KeyBindings>()
            .init_resource::<RebindingCapture>()
            .init_resource::<SimPaused>()
            .init_resource::<JuiceFreeze>()
            .init_resource::<PersistedBestScore>()
            .init_resource::<QuitRequested>()
            .init_resource::<VersusFlow>()
            // T9 terminal recorder resources: the screen-side latch plus the
            // two resources its systems touch (no-op when the owning plugins
            // registered them, but the menu screen stays functional in bare
            // headless trees).
            .init_resource::<TerminalResult>();
        if !app.world().contains_resource::<Countdown>() {
            app.init_resource::<Countdown>();
        }
        if !app.world().contains_resource::<Records>() {
            app.init_resource::<Records>();
        }
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        if !app.world().contains_resource::<Messages<MouseWheel>>() {
            app.add_message::<MouseWheel>();
        }
        // N5: the online flow shares the title screen (its "Online" button is
        // spawned in `build_menu_ui`); the plugin is mount-guarded so adding
        // it here is the single canonical mount point.
        app.add_plugins(OnlineUiPlugin);
        app.configure_sets(Update, CaptureOrder::Chord);
        app.add_systems(PreUpdate, sync_hidden_ui_unpickable);
        #[cfg(not(test))]
        app.add_systems(Startup, startup_goto_title_system);
        app.add_systems(Startup, build_menu_ui).add_systems(
            Update,
            // Input first: a chord/button transition shows its overlay in the
            // same frame, and the label/visibility sync then sees the change.
            // The chord pins to [`CaptureOrder::Chord`] so it reads the
            // capture flag before the settings screen's cleanup runs. The
            // terminal recorder runs ahead of the click handlers so a
            // same-frame Retry click can never skip the record write.
            (
                pause_chord_system.in_set(CaptureOrder::Chord),
                versus_flow_esc_system,
                terminal_record_system,
                menu_button_clicks,
                versus_button_clicks,
                sync_versus_menu_visibility,
                sync_root_visibility,
                sync_screen_texts,
                sync_versus_winner_text,
            )
                .chain(),
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::ecs::relationship::Relationship;
    use bevy::window::{Window, WindowPlugin};

    use crate::core_bridge::CoreBridgePlugin;
    use crate::input::InputPlugin;
    use crate::modes::ModeId;
    use crate::screens_modes::{ModeRowButton, ModeSelectPlugin, ModeSelectRoot};
    use crate::screens_settings::SettingsScreenPlugin;
    use bevy::app::App;

    // ---- pure handler tests ----

    #[test]
    fn startup_handler_selects_title() {
        let mut state = AppState::default();
        assert_eq!(state, AppState::Playing, "T1 default stays frozen");
        startup_goto_title(&mut state);
        assert_eq!(state, AppState::Title);
    }

    #[test]
    fn pause_chord_matches_bound_keys_and_wheels() {
        let bindings = KeyBindings::default();
        let mut keys = ButtonInput::<KeyCode>::default();
        assert!(!pause_chord_pressed(&keys, None, &bindings));
        keys.press(KeyCode::Escape);
        assert!(pause_chord_pressed(&keys, None, &bindings));
        keys.clear();
        keys.press(KeyCode::KeyP);
        assert!(pause_chord_pressed(&keys, None, &bindings));
        keys.clear();
        keys.press(KeyCode::KeyQ);
        assert!(!pause_chord_pressed(&keys, None, &bindings), "unbound key");

        let mut rebound = KeyBindings::default();
        rebound.set_slot(BindSlot::Pause, vec![Bind::WheelUp]);
        assert!(!pause_chord_pressed(&keys, None, &rebound));
        let mut wheels = Messages::<MouseWheel>::default();
        wheels.write(MouseWheel {
            unit: bevy::input::mouse::MouseScrollUnit::Line,
            x: 0.0,
            y: 1.0,
            window: Entity::PLACEHOLDER,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        assert!(pause_chord_pressed(&keys, Some(&wheels), &rebound));
        assert!(!pause_chord_pressed(&keys, Some(&wheels), &bindings));
    }

    #[test]
    fn toggle_pause_freezes_and_restores_sim() {
        let freeze = JuiceFreeze::default();
        let mut state = AppState::Playing;
        let mut sim = SimPaused(false);
        toggle_pause(&mut state, &mut sim, &freeze);
        assert_eq!(state, AppState::Paused);
        assert_eq!(sim, SimPaused(true), "user pause freezes the sim");
        toggle_pause(&mut state, &mut sim, &freeze);
        assert_eq!(state, AppState::Playing);
        assert_eq!(sim, SimPaused(false), "resume unfreezes");

        let mut settings = AppState::Settings;
        let mut sim = SimPaused(true);
        toggle_pause(&mut settings, &mut sim, &freeze);
        assert_eq!(settings, AppState::Settings, "chord is inert in Settings");
        assert_eq!(sim, SimPaused(true), "and never touches the flag there");
    }

    #[test]
    fn resume_defers_to_juice_freeze_ownership() {
        let owned = JuiceFreeze {
            frames_remaining: 2,
            owns_pause: true,
        };
        let mut state = AppState::Paused;
        let mut sim = SimPaused(true);
        resume_game(&mut state, &mut sim, &owned);
        assert_eq!(state, AppState::Playing);
        assert_eq!(sim, SimPaused(true), "juice releases the flag it set");

        let mut state = AppState::Paused;
        let mut sim = SimPaused(true);
        goto_title(&mut state, &mut sim, &owned);
        assert_eq!(state, AppState::Title);
        assert_eq!(sim, SimPaused(true));
    }

    #[test]
    fn terminal_record_matrix_follows_the_prd() {
        let snapshot = GameSnapshot {
            board: tetris_core::board::Board::new(),
            active: None,
            ghost_row: None,
            hold: None,
            hold_used: false,
            next: Vec::new(),
            score: 500,
            level: 3,
            lines: 40,
            combo: 0,
            b2b: false,
            game_over: true,
        };
        let best_score = Record::BestScore {
            score: 500,
            level: 3,
            lines: 40,
        };
        // Marathon: top-out score (today's behavior, preserved).
        assert_eq!(
            terminal_record(ModeId::Marathon, FinishReason::TopOut, &snapshot, 999),
            Some(best_score.clone())
        );
        // Sprint/Dig: only a goal finish is timed; top-outs record nothing.
        assert_eq!(
            terminal_record(ModeId::Sprint, FinishReason::GoalReached, &snapshot, 9835),
            Some(Record::BestTime { ticks: 9835 })
        );
        assert_eq!(
            terminal_record(ModeId::Dig, FinishReason::GoalReached, &snapshot, 42),
            Some(Record::BestTime { ticks: 42 })
        );
        assert_eq!(
            terminal_record(ModeId::Sprint, FinishReason::TopOut, &snapshot, 10),
            None,
            "PRD: a top-out gives no result"
        );
        assert_eq!(
            terminal_record(ModeId::Dig, FinishReason::TopOut, &snapshot, 10),
            None
        );
        // Ultra: score stands either way.
        assert_eq!(
            terminal_record(ModeId::Ultra, FinishReason::TimeUp, &snapshot, 7200),
            Some(best_score.clone())
        );
        assert_eq!(
            terminal_record(ModeId::Ultra, FinishReason::TopOut, &snapshot, 10),
            Some(best_score)
        );
        // T13's extension point: Survival (and every other unshipped mode)
        // records nothing until its one row lands in the table.
        assert_eq!(
            terminal_record(ModeId::Survival, FinishReason::TopOut, &snapshot, 10),
            None
        );
        assert_eq!(
            terminal_record(ModeId::Zen, FinishReason::TimeUp, &snapshot, 10),
            None
        );
    }

    #[test]
    fn result_text_per_mode_and_reason() {
        assert_eq!(
            result_text(ModeId::Sprint, Some(FinishReason::GoalReached), 9835),
            format!("Time {}", crate::modes::format_time_ticks(9835))
        );
        assert_eq!(
            result_text(ModeId::Dig, Some(FinishReason::GoalReached), 9835),
            format!("Time {}", crate::modes::format_time_ticks(9835))
        );
        assert_eq!(
            result_text(ModeId::Sprint, Some(FinishReason::TopOut), 10),
            "No result"
        );
        assert_eq!(
            result_text(ModeId::Dig, Some(FinishReason::TopOut), 10),
            "No result"
        );
        assert_eq!(
            result_text(ModeId::Ultra, Some(FinishReason::TimeUp), 7200),
            "Time up"
        );
        assert_eq!(
            result_text(ModeId::Ultra, Some(FinishReason::TopOut), 100),
            "Top out"
        );
        // Marathon keeps today's exact layout: no headline row.
        assert_eq!(
            result_text(ModeId::Marathon, Some(FinishReason::TopOut), 10),
            ""
        );
        assert_eq!(result_text(ModeId::Marathon, None, 0), "");
    }

    #[test]
    fn text_formatters_show_score_level_lines_and_best() {
        assert_eq!(stats_text(1234, 5, 42), "Score 1234   Level 5   Lines 42");
        assert_eq!(
            best_text(&PersistedBestScore {
                score: 9999,
                level: 3,
                lines: 60,
            }),
            "Best 9999"
        );
    }

    // ---- headless integration ----

    fn menu_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t17 headless".into(),
                resolution: (1280, 720).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            CoreBridgePlugin,
            crate::hud::HudPlugin,
            InputPlugin,
            MenuScreensPlugin,
            // T7 production parity: the mode-select roots (extra resource +
            // UI entities) must not perturb any menu behavior.
            ModeSelectPlugin,
            SettingsScreenPlugin,
        ));
        app.update();
        app
    }

    fn set_state(app: &mut App, state: AppState) {
        *app.world_mut().resource_mut::<AppState>() = state;
        app.update();
    }

    fn app_state(app: &App) -> AppState {
        *app.world().resource::<AppState>()
    }

    fn vis_of<R: Component>(app: &mut App) -> Visibility {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Visibility, With<R>>();
        *query.single(world).expect("root entity exists")
    }

    fn under(world: &World, entity: Entity, root_pred: &impl Fn(&World, Entity) -> bool) -> bool {
        let mut node = entity;
        loop {
            if root_pred(world, node) {
                return true;
            }
            let Some(child_of) = world.get::<ChildOf>(node) else {
                return false;
            };
            node = child_of.get();
        }
    }

    /// Click the first `Button` matching `btn_pred` that descends from a root
    /// matching `root_pred` (real pointer input can only hit visible roots;
    /// tests must not accidentally press a twin button under a hidden one).
    fn click_button_under(
        app: &mut App,
        root_pred: impl Fn(&World, Entity) -> bool,
        btn_pred: impl Fn(&World, Entity) -> bool,
    ) {
        let entity = {
            let world = app.world_mut();
            let mut buttons = world.query_filtered::<Entity, With<Button>>();
            buttons
                .iter(world)
                .find(|e| btn_pred(world, *e) && under(world, *e, &root_pred))
                .expect("button entity exists")
        };
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
    }

    fn text_of(app: &mut App, predicate: impl Fn(&World, Entity) -> bool) -> String {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &Text)>();
        query
            .iter(world)
            .find(|(e, _)| predicate(world, *e))
            .expect("label entity exists")
            .1
            .to_string()
    }

    /// Simulates one real key press: the edge frame plus the full per-frame
    /// `ButtonInput::reset` a real release performs, so the next press is a
    /// fresh `just_pressed` edge again.
    fn press_key(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(key);
    }

    fn force_game_over(app: &mut App) {
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        for _ in 0..2000 {
            app.world_mut()
                .resource_mut::<crate::core_bridge::PendingActions>()
                .push(tetris_core::actions::Action::HardDrop);
            app.world_mut().run_schedule(FixedUpdate);
            if app_state(app) == AppState::GameOver {
                break;
            }
        }
        assert_eq!(app_state(app), AppState::GameOver, "pile-out reached");
        app.update(); // let the label sync observe the state change
    }

    #[test]
    fn title_screen_starts_and_hides_until_title_state() {
        let mut app = menu_test_app();
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Hidden);
        set_state(&mut app, AppState::Title);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
        assert_eq!(vis_of::<PauseRoot>(&mut app), Visibility::Hidden);
        assert_eq!(vis_of::<GameOverRoot>(&mut app), Visibility::Hidden);
    }

    #[test]
    fn root_visibility_sync_never_touches_foreign_entities() {
        // Regression: the unfiltered root query (bare `Has<..>`, no Or-filter)
        // matched every entity's default `Visibility` and hid the camera,
        // sprites, HUD and every menu child — real window showed a flat panel
        // with no text, buttons or playfield.
        let mut app = menu_test_app();
        let foreign = app.world_mut().spawn(Visibility::default()).id();
        let child = app
            .world_mut()
            .spawn(Visibility::default())
            .insert(TitleRoot)
            .id();
        set_state(&mut app, AppState::Playing);
        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(foreign).unwrap(),
            Visibility::Inherited,
            "foreign entity visibility was hijacked"
        );
        assert_eq!(
            *app.world().get::<Visibility>(child).unwrap(),
            Visibility::Hidden,
            "root itself must still sync"
        );
        set_state(&mut app, AppState::Title);
        app.update();
        assert_eq!(
            *app.world().get::<Visibility>(foreign).unwrap(),
            Visibility::Inherited
        );
        assert_eq!(
            *app.world().get::<Visibility>(child).unwrap(),
            Visibility::Visible
        );
    }

    #[test]
    fn start_button_launches_fresh_playing_run() {
        // T7: Title "Start" opens the mode list; the picked row starts the
        // fresh run (Sprint here, so the T5 pre-roll contract is covered too).
        let mut app = menu_test_app();
        startup_goto_title(app.world_mut().resource_mut::<AppState>().as_mut());
        app.update();
        assert_eq!(app_state(&app), AppState::Title);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);

        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        click_button_under(
            &mut app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<ModeRowButton>(e)
                    .is_some_and(|row| row.id == ModeId::Sprint)
            },
        );
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Sprint
        );
        assert_eq!(
            *app.world().resource::<crate::core_bridge::Countdown>(),
            crate::core_bridge::Countdown(crate::modes::PRE_ROLL_TICKS),
            "Sprint starts behind its pre-roll"
        );
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        assert!(snapshot.board.is_empty() && snapshot.score == 0);
    }

    #[test]
    fn pause_chord_freezes_core_and_resumes() {
        let mut app = menu_test_app();
        for _ in 0..5 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        let steps_before = app.world().non_send::<GameCore>().steps;
        assert!(steps_before > 0);

        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(true));

        for _ in 0..5 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        assert_eq!(
            app.world().non_send::<GameCore>().steps,
            steps_before,
            "core frozen while paused (zero core ticks)"
        );

        press_key(&mut app, KeyCode::KeyP);
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        app.world_mut().run_schedule(FixedUpdate);
        assert!(app.world().non_send::<GameCore>().steps > steps_before);
    }

    #[test]
    fn pause_chord_suppressed_while_capturing() {
        let mut app = menu_test_app();
        app.world_mut().resource_mut::<RebindingCapture>().capturing = true;
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Playing);
    }

    #[test]
    fn pause_overlay_buttons_round_trip() {
        let mut app = menu_test_app();
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        assert_eq!(vis_of::<PauseRoot>(&mut app), Visibility::Visible);
        let pause_root = |world: &World, e: Entity| world.get::<PauseRoot>(e).is_some();

        // Settings from Pause keeps the sim frozen; T16 Back returns to Pause.
        click_button_under(&mut app, pause_root, |world, e| {
            world.get::<OpenSettingsButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Settings);
        assert_eq!(
            *app.world().resource::<SimPaused>(),
            SimPaused(true),
            "Pause -> Settings keeps the sim frozen"
        );
        click_button_under(
            &mut app,
            |world, e| {
                world
                    .get::<crate::screens_settings::SettingsRoot>(e)
                    .is_some()
            },
            |world, e| {
                world
                    .get::<crate::screens_settings::BackButton>(e)
                    .is_some()
            },
        );
        assert_eq!(app_state(&app), AppState::Paused);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(true));

        // Resume restores play.
        click_button_under(&mut app, pause_root, |world, e| {
            world.get::<ResumeButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));

        // Restart via the shared restart_run path.
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        click_button_under(&mut app, pause_root, |world, e| {
            world.get::<RestartButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);

        // Quit to title un-freezes on the way out.
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        click_button_under(&mut app, pause_root, |world, e| {
            world.get::<QuitToTitleButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Title);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
    }

    #[test]
    fn game_over_screen_shows_final_stats_and_best() {
        let mut app = menu_test_app();
        force_game_over(&mut app);
        assert_eq!(vis_of::<GameOverRoot>(&mut app), Visibility::Visible);

        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        // T9: the terminal recorder already folded the (first) Marathon run
        // into Records — the fresh improvement marks the run.
        assert_eq!(
            app.world().resource::<Records>().record_for(MARATHON),
            Some(&Record::BestScore {
                score: snapshot.score,
                level: snapshot.level,
                lines: snapshot.lines,
            })
        );
        // The title-screen Marathon view follows the record (production does
        // this via the settings view system; set it here to the same value).
        app.world_mut().resource_mut::<PersistedBestScore>().score = snapshot.score;
        app.update();

        // Marathon layout is exactly today's: stats + best, no headline row.
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<StatsText>(e).is_some()),
            stats_text(snapshot.score, snapshot.level, snapshot.lines)
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<BestText>(e).is_some()),
            format!("Best {}", snapshot.score)
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "NEW RECORD!"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            ""
        );
    }

    #[test]
    fn new_record_marker_shows_only_on_improvement() {
        let mut app = menu_test_app();
        // First run improves the (empty) record -> marker shows.
        force_game_over(&mut app);
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "NEW RECORD!"
        );

        // A second (hard-drop) run cannot beat the pile-up it inherits the
        // board-independent best from... to make "worse" explicit: seed a
        // huge best, top out again, expect no marker.
        app.world_mut().resource_mut::<Records>().record_run(
            MARATHON,
            Record::BestScore {
                score: u32::MAX as u64,
                level: 9,
                lines: 999,
            },
        );
        let over_root = |world: &World, e: Entity| world.get::<GameOverRoot>(e).is_some();
        click_button_under(&mut app, over_root, |world, e| {
            world.get::<PlayAgainButton>(e).is_some()
        });
        force_game_over(&mut app);
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "",
            "a worse run must not re-show NEW RECORD!"
        );
    }

    #[test]
    fn game_over_play_again_starts_fresh_and_menu_returns_mode_select() {
        let mut app = menu_test_app();
        force_game_over(&mut app);
        let over_root = |world: &World, e: Entity| world.get::<GameOverRoot>(e).is_some();

        click_button_under(&mut app, over_root, |world, e| {
            world.get::<PlayAgainButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);

        force_game_over(&mut app);
        // T9: "Menu" walks back to the mode list, not past it to the title.
        click_button_under(&mut app, over_root, |world, e| {
            world.get::<QuitToTitleButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::ModeSelect);
    }

    #[test]
    fn quit_button_requests_app_exit() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<QuitButton>(e).is_some(),
        );
        assert!(app.world().resource::<QuitRequested>().0);
    }

    #[test]
    fn settings_round_trip_from_title_returns_to_title() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OpenSettingsButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::Settings);
        click_button_under(
            &mut app,
            |world, e| {
                world
                    .get::<crate::screens_settings::SettingsRoot>(e)
                    .is_some()
            },
            |world, e| {
                world
                    .get::<crate::screens_settings::BackButton>(e)
                    .is_some()
            },
        );
        assert_eq!(app_state(&app), AppState::Title);
    }

    // ---- T26: 1v1 flow, winner overlay, pause interaction ----

    use crate::core_bridge::{start_versus, Controller, VersusMatch, VersusWinner};
    use crate::input::VersusActions;
    use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

    fn flow(app: &App) -> VersusFlow {
        *app.world().resource::<VersusFlow>()
    }

    fn versus_state(app: &App) -> (bool, AttackRule, Controller, Controller) {
        let versus = app.world().non_send::<VersusMatch>();
        (versus.active, versus.rule, versus.p1, versus.p2)
    }

    /// Drive the Title → 1v1 → rule → opponent flow to a started match.
    fn click_1v1_path(app: &mut App, rule: &str, opponent: &str) {
        set_state(app, AppState::Title);
        click_button_under(
            app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OneVOneButton>(e).is_some(),
        );
        assert_eq!(
            flow(app).stage,
            VersusStage::Rules,
            "1v1 opens the rules step"
        );
        assert_eq!(vis_of::<VersusRulesRoot>(app), Visibility::Visible);
        assert_eq!(vis_of::<VersusOpponentRoot>(app), Visibility::Hidden);
        let rules_root = |world: &World, e: Entity| world.get::<VersusRulesRoot>(e).is_some();
        if rule == "garbage" {
            click_button_under(app, rules_root, |world, e| {
                world.get::<RuleGarbageButton>(e).is_some()
            });
        } else {
            click_button_under(app, rules_root, |world, e| {
                world.get::<RuleRaceButton>(e).is_some()
            });
        }
        assert_eq!(flow(app).stage, VersusStage::Opponent);
        assert_eq!(vis_of::<VersusOpponentRoot>(app), Visibility::Visible);
        let opponent_root = |world: &World, e: Entity| world.get::<VersusOpponentRoot>(e).is_some();
        if opponent == "human" {
            click_button_under(app, opponent_root, |world, e| {
                world.get::<OpponentHumanButton>(e).is_some()
            });
        } else {
            click_button_under(app, opponent_root, |world, e| {
                world.get::<OpponentBotButton>(e).is_some()
            });
        }
    }

    #[test]
    fn one_v_one_garbage_human_flow_starts_a_match() {
        let mut app = menu_test_app();
        click_1v1_path(&mut app, "garbage", "human");
        let (active, rule, p1, p2) = versus_state(&app);
        assert!(active, "match active after the flow");
        assert_eq!(rule, AttackRule::Garbage);
        assert_eq!(p1, Controller::Human, "P1 is always the local human");
        assert_eq!(p2, Controller::Human);
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(
            flow(&app).stage,
            VersusStage::Title,
            "flow closed on launch"
        );
        assert_eq!(vis_of::<VersusRulesRoot>(&mut app), Visibility::Hidden);
        assert_eq!(vis_of::<VersusOpponentRoot>(&mut app), Visibility::Hidden);
        assert_eq!(vis_of::<VersusOverRoot>(&mut app), Visibility::Hidden);
    }

    #[test]
    fn one_v_one_race_bot_flow_selects_race_and_bot() {
        let mut app = menu_test_app();
        click_1v1_path(&mut app, "race", "bot");
        let (active, rule, p1, p2) = versus_state(&app);
        assert!(active);
        assert_eq!(
            rule,
            AttackRule::Race {
                target_lines: DEFAULT_RACE_LINES
            }
        );
        assert_eq!(p1, Controller::Human);
        assert_eq!(p2, Controller::Bot);
    }

    #[test]
    fn quit_to_title_from_paused_versus_ends_the_match() {
        fn hud_root_vis(app: &mut App) -> (usize, usize) {
            let world = app.world_mut();
            let mut query = world.query_filtered::<&Visibility, With<VersusHudRoot>>();
            let vis: Vec<_> = query.iter(world).copied().collect();
            let shown = vis.iter().filter(|v| **v == Visibility::Visible).count();
            (vis.len(), shown)
        }

        let mut app = menu_test_app();
        click_1v1_path(&mut app, "race", "bot");
        assert!(app.world().non_send::<VersusMatch>().active);
        assert_eq!(
            hud_root_vis(&mut app),
            (2, 2),
            "versus HUDs show for the active match"
        );

        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        click_button_under(
            &mut app,
            |world, e| world.get::<PauseRoot>(e).is_some(),
            |world, e| world.get::<QuitToTitleButton>(e).is_some(),
        );

        assert_eq!(app_state(&app), AppState::Title);
        assert!(
            !app.world().non_send::<VersusMatch>().active,
            "Quit to Title must end the match: a live versus keeps its \
             full-screen HUD root visible over the title and swallows every \
             menu click"
        );
        assert_eq!(
            hud_root_vis(&mut app),
            (2, 0),
            "HUD roots must release the title once the match ends"
        );

        // The title menu must be selectable again (T7: Start opens the
        // mode list — the important assertion is that the click lands on a
        // live title button at all, not on the dead versus HUD).
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        assert!(!app.world().non_send::<VersusMatch>().active);
    }

    #[test]
    fn versus_flow_back_button_and_escape_walk_the_submenus() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OneVOneButton>(e).is_some(),
        );
        assert_eq!(flow(&app).stage, VersusStage::Rules);

        press_key(&mut app, KeyCode::Escape);
        assert_eq!(
            flow(&app).stage,
            VersusStage::Title,
            "Esc closes the rules step"
        );
        assert_eq!(vis_of::<VersusRulesRoot>(&mut app), Visibility::Hidden);
        assert_eq!(
            app_state(&app),
            AppState::Title,
            "no pause leak from Title Esc"
        );

        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OneVOneButton>(e).is_some(),
        );
        click_button_under(
            &mut app,
            |world, e| world.get::<VersusRulesRoot>(e).is_some(),
            |world, e| world.get::<RuleGarbageButton>(e).is_some(),
        );
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(
            flow(&app).stage,
            VersusStage::Rules,
            "Esc walks back one step"
        );

        let rules_root = |world: &World, e: Entity| world.get::<VersusRulesRoot>(e).is_some();
        click_button_under(&mut app, rules_root, |world, e| {
            world.get::<VersusBackButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, VersusStage::Title);
        assert_eq!(
            flow(&app).rule,
            AttackRule::Garbage,
            "rule choice survives Back"
        );
    }

    /// Start a race-to-zero match and crown Left: both sides lock their
    /// first piece in the same step (under Race, reaching the target only
    /// finishes a side), and the perfect tie goes to the first finisher.
    fn crown_winner(app: &mut App, p2: Controller) {
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Race { target_lines: 0 },
                        Controller::Human,
                        p2,
                    );
                });
            });
        app.world_mut()
            .resource_mut::<VersusActions>()
            .left
            .push(tetris_core::actions::Action::HardDrop);
        app.world_mut()
            .resource_mut::<VersusActions>()
            .right
            .push(tetris_core::actions::Action::HardDrop);
        app.world_mut().run_schedule(FixedUpdate);
        app.update();
    }

    #[test]
    fn winner_overlay_shows_text_and_pause_is_blocked() {
        let mut app = menu_test_app();
        crown_winner(&mut app, Controller::Human);
        assert_eq!(app.world().resource::<VersusWinner>().0, Some(Side::Left));
        assert_eq!(vis_of::<VersusOverRoot>(&mut app), Visibility::Visible);
        let text = text_of(&mut app, |world, e| {
            world.get::<VersusWinnerText>(e).is_some()
        });
        assert_eq!(text, "PLAYER 1 WINS");

        // Pausing a finished match is blocked: the overlay is never covered.
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
    }

    #[test]
    fn winner_overlay_rematch_restarts_and_menu_returns_to_title() {
        let mut app = menu_test_app();
        crown_winner(&mut app, Controller::Bot);
        let text = text_of(&mut app, |world, e| {
            world.get::<VersusWinnerText>(e).is_some()
        });
        assert_eq!(text, "PLAYER 1 WINS", "human left beats bot right");

        let over_root = |world: &World, e: Entity| world.get::<VersusOverRoot>(e).is_some();
        click_button_under(&mut app, over_root, |world, e| {
            world.get::<VersusRematchButton>(e).is_some()
        });
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));
        let (active, rule, _, p2) = versus_state(&app);
        assert!(active, "rematch keeps the match up");
        assert_eq!(rule, AttackRule::Race { target_lines: 0 });
        assert_eq!(p2, Controller::Bot, "rematch keeps the controllers");
        assert_eq!(
            app.world().non_send::<VersusMatch>().steps,
            0,
            "fresh match"
        );
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(vis_of::<VersusOverRoot>(&mut app), Visibility::Hidden);

        crown_winner(&mut app, Controller::Bot);
        click_button_under(&mut app, over_root, |world, e| {
            world.get::<VersusMenuButton>(e).is_some()
        });
        assert_eq!(
            app_state(&app),
            AppState::Title,
            "Menu ends the match to Title"
        );
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));
        assert_eq!(vis_of::<VersusOverRoot>(&mut app), Visibility::Hidden);

        // Solo path intact: Title → Start opens the list, and a row press
        // launches a fresh solo run (T7).
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        click_button_under(
            &mut app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<ModeRowButton>(e)
                    .is_some_and(|row| row.id == ModeId::Marathon)
            },
        );
        assert_eq!(app_state(&app), AppState::Playing);
        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.steps, 0);
        assert_eq!(core.active_mode.id, ModeId::Marathon);
        let snapshot = core.game.snapshot();
        assert!(snapshot.board.is_empty() && snapshot.score == 0);
    }

    // ---- Real-pointer regression (UiPlugin + bevy_ui's ui_focus_system) ----

    use bevy::camera::visibility::InheritedVisibility;
    use bevy::ui::UiPlugin;
    use bevy::window::PrimaryWindow;

    #[derive(Resource)]
    struct UiWin(Entity);

    /// App wiring for real pointer hit-testing. Bevy 0.19 writes
    /// `Interaction` in bevy_ui's `ui_focus_system`, which reads
    /// `Window::physical_cursor_position` and `ButtonInput<MouseButton>`
    /// directly, so the harness drives exactly those two inputs through
    /// winit's own pipeline shape: cursor writes to the `Window` and
    /// `MouseButtonInput` *messages* replayed by the real
    /// `mouse_button_input_system` (clear + reapply per frame).
    ///
    /// `emulate_inherited_visibility` stands in for the render world's
    /// extract pass: without one, `InheritedVisibility` never exists and
    /// `ui_focus_system` treats every node as un-interactable.
    fn menu_ui_test_app() -> App {
        let mut app = menu_test_app();
        app.add_plugins(bevy::asset::AssetPlugin::default());
        app.init_asset::<Image>();
        app.init_asset::<bevy::image::TextureAtlasLayout>();
        app.add_plugins(bevy::input::InputPlugin);
        app.add_plugins(bevy::text::TextPlugin);
        app.add_plugins(UiPlugin);
        // `UiPlugin`'s viewport widgets require the picking core resources.
        if !app
            .world()
            .contains_resource::<Messages<bevy::input::touch::TouchInput>>()
        {
            app.add_message::<bevy::input::touch::TouchInput>();
        }
        app.add_plugins(bevy::picking::DefaultPickingPlugins);
        app.world_mut().spawn((
            Camera2d,
            bevy::ui::IsDefaultUiCamera,
            Camera {
                viewport: Some(bevy::camera::Viewport {
                    physical_size: UVec2::new(1280, 720),
                    ..default()
                }),
                ..default()
            },
        ));
        let primary = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<Entity, With<PrimaryWindow>>();
            q.single(world).expect("primary window")
        };
        app.insert_resource(UiWin(primary));
        app.add_systems(PostUpdate, emulate_inherited_visibility);
        app.update();
        app
    }

    /// Headless stand-in for the render world's `sync_visible_systems`:
    /// mirror `Visibility` down the tree into `InheritedVisibility`.
    fn emulate_inherited_visibility(world: &mut World) {
        fn rec(world: &mut World, entity: Entity, parent_visible: bool) {
            let visible = parent_visible
                && world
                    .get::<Visibility>(entity)
                    .is_none_or(|v| *v != Visibility::Hidden);
            let flag = if visible {
                InheritedVisibility::VISIBLE
            } else {
                InheritedVisibility::HIDDEN
            };
            match world.get_mut::<InheritedVisibility>(entity) {
                Some(mut current) => *current = flag,
                None => {
                    world.entity_mut(entity).insert(flag);
                }
            }
            let children: Vec<Entity> = world
                .get::<Children>(entity)
                .map(|children| children.iter().collect())
                .unwrap_or_default();
            for child in children {
                rec(world, child, visible);
            }
        }
        let roots: Vec<Entity> = world
            .iter_entities()
            .filter(|e| e.contains::<Visibility>() && !e.contains::<ChildOf>())
            .map(|e| e.id())
            .collect();
        for root in roots {
            rec(world, root, true);
        }
    }

    /// Name of the known menu root a button descends from ("" when none).
    /// Root scoping (T7): every menu root reuses the same button markers at
    /// different positions ("Settings" on Title *and* Pause, "Back" on both
    /// versus submenus), and the old first-match lookup resolved them by
    /// entity iteration order — which Bevy 0.19 reshuffles whenever ANY
    /// resource or entity is added (resources are entities!), so a new
    /// `init_resource` silently rerouted taps (measured at T5: red from +2
    /// resource entities on). Buttons are now addressed by (root, label).
    fn root_name_of(world: &World, entity: Entity) -> &'static str {
        let mut node = entity;
        loop {
            if world.get::<TitleRoot>(node).is_some() {
                return "title";
            }
            if world.get::<PauseRoot>(node).is_some() {
                return "pause";
            }
            if world.get::<GameOverRoot>(node).is_some() {
                return "over";
            }
            if world.get::<VersusRulesRoot>(node).is_some() {
                return "rules";
            }
            if world.get::<VersusOpponentRoot>(node).is_some() {
                return "opponent";
            }
            if world.get::<VersusOverRoot>(node).is_some() {
                return "versus";
            }
            let Some(child_of) = world.get::<ChildOf>(node) else {
                return "";
            };
            node = child_of.get();
        }
    }

    /// `(root, label, center in window-logical coords)` of every menu
    /// button, sorted deterministically by `(root, label)` — never by
    /// entity iteration order (see [`root_name_of`]).
    fn button_rects(app: &mut App) -> Vec<(String, &'static str, Vec2)> {
        let world = app.world_mut();
        let mut q = world.query::<(
            Entity,
            &UiGlobalTransform,
            Option<&StartButton>,
            Option<&OneVOneButton>,
            Option<&OpenSettingsButton>,
            Option<&QuitButton>,
            Option<&RuleGarbageButton>,
            Option<&RuleRaceButton>,
            Option<&OpponentHumanButton>,
            Option<&OpponentBotButton>,
            Option<&VersusBackButton>,
            Option<&VersusRematchButton>,
            Option<&VersusMenuButton>,
        )>();
        let mut out = Vec::new();
        for (e, t, start, one, settings, quit, g, r, h, b, back, rematch, menu) in q.iter(world) {
            let label = if start.is_some() {
                "start"
            } else if one.is_some() {
                "1v1"
            } else if settings.is_some() {
                "settings"
            } else if quit.is_some() {
                "quit"
            } else if g.is_some() {
                "garbage"
            } else if r.is_some() {
                "race"
            } else if h.is_some() {
                "human"
            } else if b.is_some() {
                "bot"
            } else if back.is_some() {
                "back"
            } else if rematch.is_some() {
                "rematch"
            } else if menu.is_some() {
                "menu"
            } else {
                continue;
            };
            out.push((
                root_name_of(world, e).to_string(),
                label,
                t.translation.xy(),
            ));
        }
        out.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(b.1))
                .then(a.2.x.total_cmp(&b.2.x))
                .then(a.2.y.total_cmp(&b.2.y))
        });
        out
    }

    fn rect_of_under(app: &mut App, root: &str, label: &str) -> Vec2 {
        button_rects(app)
            .into_iter()
            .find(|(r, l, _)| r == root && *l == label)
            .unwrap_or_else(|| panic!("{root}/{label} button laid out"))
            .2
    }

    fn set_cursor(app: &mut App, at: Vec2) {
        let win = app.world().resource::<UiWin>().0;
        let mut w = app
            .world_mut()
            .entity_mut(win)
            .get::<Window>()
            .unwrap()
            .clone();
        w.set_cursor_position(Some(at));
        app.world_mut().entity_mut(win).insert(w);
        app.update();
    }

    fn send_button(app: &mut App, state: bevy::input::ButtonState) {
        let win = app.world().resource::<UiWin>().0;
        app.world_mut()
            .resource_mut::<Messages<bevy::input::mouse::MouseButtonInput>>()
            .write(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state,
                window: win,
            });
        app.update();
    }

    /// Press frame, idle hold frames, release — a normal human click.
    fn tap(app: &mut App, at: Vec2) {
        set_cursor(app, at);
        send_button(app, bevy::input::ButtonState::Pressed);
        app.update();
        send_button(app, bevy::input::ButtonState::Released);
        app.update();
    }

    /// Press and release delivered inside a single frame (event batching).
    fn tap_fast(app: &mut App, at: Vec2) {
        set_cursor(app, at);
        let win = app.world().resource::<UiWin>().0;
        {
            let world = app.world_mut();
            let mut msgs = world.resource_mut::<Messages<bevy::input::mouse::MouseButtonInput>>();
            msgs.write(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state: bevy::input::ButtonState::Pressed,
                window: win,
            });
            msgs.write(bevy::input::mouse::MouseButtonInput {
                button: MouseButton::Left,
                state: bevy::input::ButtonState::Released,
                window: win,
            });
        }
        app.update();
        app.update();
    }

    /// Regression (real window): top-level menu roots share one UI camera.
    /// Clicking "1 v 1" then "opened" a submenu under the (opaque) title,
    /// so the title kept swallowing every click and a 1v1 could never be
    /// configured. The submenu roots carry `ZIndex(1)` for draw order, the
    /// title hides while a submenu is open, and hidden widgets are excluded
    /// from picking entirely (`sync_hidden_ui_unpickable` — 0.19 picking
    /// ranks by iteration order, not ZIndex), so real pointer input can
    /// only ever reach the visible panel.

    #[test]
    fn one_v_one_submenu_clicks_reach_the_submenu_not_the_title() {
        let mut app = menu_ui_test_app();
        set_state(&mut app, AppState::Title);
        app.update();
        let one = rect_of_under(&mut app, "title", "1v1");
        // Title "Settings" y-range sits under the rules panel but inside no
        // submenu button — the pre-fix build opened Settings through the
        // (invisible-under-title) submenu here.
        let title_settings = rect_of_under(&mut app, "title", "settings");
        let garbage = rect_of_under(&mut app, "rules", "garbage");
        let bot = rect_of_under(&mut app, "opponent", "bot");

        tap_fast(&mut app, one);
        assert_eq!(flow(&app).stage, VersusStage::Rules, "1v1 opens rules");
        assert_eq!(vis_of::<VersusRulesRoot>(&mut app), Visibility::Visible);
        assert_eq!(
            vis_of::<TitleRoot>(&mut app),
            Visibility::Hidden,
            "title must not stay interactive under the submenu"
        );

        // The title's Settings button sits under the open submenu: a real
        // click at its center must be inert (this used to open Settings
        // through the panel, stranding the player with no way to configure
        // the match).
        tap(&mut app, title_settings);
        assert_eq!(app_state(&app), AppState::Title, "no click-through");
        assert_eq!(flow(&app).stage, VersusStage::Rules);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);

        tap_fast(&mut app, garbage);
        assert_eq!(flow(&app).stage, VersusStage::Opponent);
        assert_eq!(vis_of::<VersusOpponentRoot>(&mut app), Visibility::Visible);

        tap_fast(&mut app, bot);
        let (active, rule, p1, p2) = versus_state(&app);
        assert!(active, "match launches through real input");
        assert_eq!(rule, AttackRule::Garbage);
        assert_eq!((p1, p2), (Controller::Human, Controller::Bot));
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(flow(&app).stage, VersusStage::Title);
    }

    /// Regression (G2 resource-shift incident): in Bevy 0.19 every
    /// resource is an entity and UI picking ranks equal-depth hits by
    /// iteration order, so adding *any* new resource type to the app used
    /// to silently reroute the submenu click tests above. Inserting dummy
    /// resources must now change nothing: the same click sequence still
    /// configures and launches the 1v1 exactly once, through the submenu.
    #[test]
    fn dummy_resources_cannot_reroute_submenu_clicks() {
        #[derive(Resource)]
        struct DummyA;
        #[derive(Resource)]
        struct DummyB(u32);

        let mut app = menu_ui_test_app();
        app.insert_resource(DummyA);
        app.insert_resource(DummyB(7));
        set_state(&mut app, AppState::Title);
        app.update();

        let one = rect_of_under(&mut app, "title", "1v1");
        let title_settings = rect_of_under(&mut app, "title", "settings");
        let garbage = rect_of_under(&mut app, "rules", "garbage");
        let bot = rect_of_under(&mut app, "opponent", "bot");

        tap_fast(&mut app, one);
        assert_eq!(flow(&app).stage, VersusStage::Rules, "1v1 opens rules");
        tap(&mut app, title_settings);
        assert_eq!(app_state(&app), AppState::Title, "no click-through");
        assert_eq!(flow(&app).stage, VersusStage::Rules);
        tap_fast(&mut app, garbage);
        assert_eq!(flow(&app).stage, VersusStage::Opponent);
        tap_fast(&mut app, bot);
        let (active, rule, p1, p2) = versus_state(&app);
        assert!(active, "match launches through real input");
        assert_eq!(rule, AttackRule::Garbage);
        assert_eq!((p1, p2), (Controller::Human, Controller::Bot));
        assert_eq!(app_state(&app), AppState::Playing);
    }

    /// The T7 deterministic-helper fix: rect lookups resolve the *intended*
    /// button by (owning root, label) and are invariant under resource AND
    /// entity churn. The old helper returned the first label match in
    /// entity-iteration order — it conflated the Title and Pause
    /// "Settings" buttons, and Bevy 0.19 reshuffles that order whenever any
    /// resource (which are entities) or UI entity appears, which is exactly
    /// how the T5 `Countdown` resource and the T7 mode-select screen would
    /// have silently rerouted these taps.
    #[test]
    fn root_scoped_rect_resolution_survives_resource_and_entity_churn() {
        #[derive(Resource)]
        struct DummyA;
        #[derive(Resource)]
        struct DummyB(u32);
        #[derive(Resource)]
        struct DummyC;

        // `menu_test_app` already mounts `ModeSelectPlugin` (rows add
        // Button entities + a resource — the exact T5 red condition).
        let mut app = menu_ui_test_app();
        let start = rect_of_under(&mut app, "title", "start");
        let title_settings = rect_of_under(&mut app, "title", "settings");
        let pause_settings = rect_of_under(&mut app, "pause", "settings");
        // The two "settings" buttons are distinct widgets: the old
        // first-match helper silently picked one (the pause twin at a
        // different y) depending on spawn order.
        assert_ne!(title_settings, pause_settings);
        assert_ne!(rect_of_under(&mut app, "title", "1v1"), start);

        let dummy_entity = app
            .world_mut()
            .spawn((Visibility::default(), Node::default()))
            .id();
        app.insert_resource(DummyA);
        app.insert_resource(DummyB(7));
        app.insert_resource(DummyC);
        app.update();
        assert_eq!(rect_of_under(&mut app, "title", "start"), start);
        assert_eq!(rect_of_under(&mut app, "title", "settings"), title_settings);
        assert_eq!(rect_of_under(&mut app, "pause", "settings"), pause_settings);

        app.world_mut().despawn(dummy_entity);
        app.update();
        assert_eq!(rect_of_under(&mut app, "title", "start"), start);
        assert_eq!(rect_of_under(&mut app, "title", "settings"), title_settings);
    }

    /// Walking the flow with the submenu's Back button (which overlaps the
    /// title's 1v1 button) returns to the title and restores it.
    #[test]
    fn one_v_one_back_button_walks_out_and_restores_the_title() {
        let mut app = menu_ui_test_app();
        set_state(&mut app, AppState::Title);
        app.update();
        let one = rect_of_under(&mut app, "title", "1v1");
        let back = rect_of_under(&mut app, "rules", "back");
        tap_fast(&mut app, one);
        assert_eq!(flow(&app).stage, VersusStage::Rules);

        tap_fast(&mut app, back);
        assert_eq!(flow(&app).stage, VersusStage::Title);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
        assert_eq!(app_state(&app), AppState::Title);

        // …and the title buttons work again afterwards (T7: "Start" opens
        // the mode list, it no longer jumps straight into a run).
        let start = rect_of_under(&mut app, "title", "start");
        tap_fast(&mut app, start);
        assert_eq!(app_state(&app), AppState::ModeSelect);
    }

    #[test]
    fn winner_text_names_the_winning_player_or_bot() {
        assert_eq!(
            winner_text(Side::Left, Controller::Human, Controller::Human),
            "PLAYER 1 WINS"
        );
        assert_eq!(
            winner_text(Side::Right, Controller::Human, Controller::Human),
            "PLAYER 2 WINS"
        );
        assert_eq!(
            winner_text(Side::Left, Controller::Bot, Controller::Human),
            "BOT WINS"
        );
        assert_eq!(
            winner_text(Side::Right, Controller::Human, Controller::Bot),
            "BOT WINS"
        );
    }

    // ---- N5: online flow wiring ----

    use crate::core_bridge::net::online_ui::OnlineStage;

    #[test]
    fn online_button_opens_the_flow_and_hides_the_title_behind_it() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OnlineButton>(e).is_some(),
        );
        assert_eq!(
            app.world().resource::<OnlineFlow>().stage,
            OnlineStage::Mode
        );
        // The click system is exclusive; the visibility sync settles in the
        // same frame (parallel schedule — one settle frame to be robust).
        app.update();
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Hidden);
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(
            app.world().resource::<OnlineFlow>().stage,
            OnlineStage::Closed
        );
        app.update();
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
    }

    #[test]
    fn clicking_online_leaves_solo_and_versus_flows_untouched() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<OnlineButton>(e).is_some(),
        );
        // No run started, no 1v1 submenu opened — the Online marker simply
        // is not one of `menu_button_clicks`' cases.
        assert_eq!(app_state(&app), AppState::Title);
        let flow = *app.world().resource::<VersusFlow>();
        assert_eq!(flow.stage, VersusStage::Title);
    }

    #[test]
    fn winner_headline_is_role_aware_once_a_seat_is_net() {
        for (role, winner, expected) in [
            (NetRole::Host, Side::Left, "YOU WIN"),
            (NetRole::Guest, Side::Right, "YOU WIN"),
            (NetRole::Host, Side::Right, "OPPONENT WINS"),
            (NetRole::Guest, Side::Left, "OPPONENT WINS"),
        ] {
            let mut app = menu_test_app();
            set_state(&mut app, AppState::Playing);
            app.world_mut()
                .resource_scope::<AppState, ()>(|world, state| {
                    world.resource_scope::<VersusWinner, ()>(|world, winner_res| {
                        let versus = world.non_send_mut::<VersusMatch>();
                        start_versus(
                            versus.into_inner(),
                            winner_res.into_inner(),
                            state.into_inner(),
                            AttackRule::Garbage,
                            Controller::Human,
                            Controller::Net,
                        );
                    });
                });
            app.world_mut().resource_mut::<VersusWinner>().0 = Some(winner);
            let mut net = app.world_mut().resource_mut::<NetSession>();
            net.role = role;
            net.status = NetStatus::InMatch;
            app.update();
            app.update();
            assert_eq!(vis_of::<VersusOverRoot>(&mut app), Visibility::Visible);
            let label = text_of(&mut app, |world, e| {
                world.get::<VersusWinnerText>(e).is_some()
            });
            assert_eq!(label, expected, "{role:?} winner {winner:?}");
        }
    }

    // ---- T9: terminal recording matrix + interim recorder retirement ----

    use crate::core_bridge::{start_mode_run, Countdown};
    use crate::records::{self, Record, Records, DIG, MARATHON, SPRINT, ULTRA};
    use crate::settings_persist::{CONFIG_DIR_ENV, ENV_LOCK};
    use tetris_core::game::Game;
    use tetris_core::mode::{BlockOutBehavior, FinishReason, Goal, ModeConfig};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("tetris-t9-{label}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir created");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Production-equivalent persistence tree (settings + records + the T9
    /// terminal recorder) against an isolated `TETRIS_CONFIG_DIR`.
    /// (the caller points `CONFIG_DIR_ENV` at `dir` *before* calling — the
    /// persistence plugins resolve the directory at build time).
    fn persist_app(_dir: &TempDir, seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((
            CoreBridgePlugin,
            crate::settings_persist::SettingsPersistPlugin,
            MenuScreensPlugin,
        ));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app.update();
        app
    }

    /// Start `id` through the shared T5 path, burn its pre-roll, then pile
    /// hard drops until the core tops out (the bridge flips `GameOver`).
    fn start_and_top_out(app: &mut App, id: ModeId) {
        let world = app.world_mut();
        let mut state = world.remove_resource::<AppState>().unwrap();
        let mut countdown = world.remove_resource::<Countdown>().unwrap();
        let mut records = world.remove_resource::<Records>();
        {
            let mut core = world.non_send_mut::<GameCore>();
            start_mode_run(
                id,
                core.as_mut(),
                &mut countdown,
                &mut state,
                records.as_mut(),
            );
        }
        world.insert_resource(state);
        world.insert_resource(countdown);
        if let Some(records) = records {
            world.insert_resource(records);
        }
        while app.world().resource::<Countdown>().0 > 0 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        for _ in 0..2000 {
            app.world_mut()
                .resource_mut::<crate::core_bridge::PendingActions>()
                .push(tetris_core::actions::Action::HardDrop);
            app.world_mut().run_schedule(FixedUpdate);
            if app_state(app) == AppState::GameOver {
                break;
            }
        }
        assert_eq!(app_state(app), AppState::GameOver, "pile-out reached");
    }

    /// Reach `GoalReached` quickly: a goal config satisfied on the first
    /// lock (`GarbageCleared` over a board without garbage — T2 contract),
    /// claimed as `id` via `active_mode` exactly like a catalogue start.
    fn set_terminal_game(app: &mut App, id: ModeId, config: ModeConfig, seed: u64) {
        let mut core = app.world_mut().non_send_mut::<GameCore>();
        core.game = Game::with_config(seed, &config);
        core.seed = seed;
        core.steps = 0;
        core.pending_events.clear();
        core.active_mode = crate::core_bridge::ActiveMode { id, config };
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
    }

    fn goal_on_first_lock() -> ModeConfig {
        ModeConfig {
            start_level: 1,
            levels_advance: false,
            goal: Some(Goal::GarbageCleared),
            clock_ticks: None,
            start_board: None,
            on_block_out: BlockOutBehavior::End,
        }
    }

    /// Terminal-recorder retirement (T9): a Sprint top-out must write NO
    /// record — neither `Records` nor `best.json` — even on the full
    /// production persistence tree. Pre-T9 the interim
    /// `settings_persist::best_score_system` folded EVERY `GameOver`
    /// (Sprint top-outs included) into the Marathon `BestScore`; this test
    /// is the gate against that pollution ever coming back.
    #[test]
    fn sprint_top_out_records_nothing_anywhere() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("sprint-pollution");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = persist_app(&dir, 0x7E90);
        start_and_top_out(&mut app, ModeId::Sprint);
        app.update();
        app.update();

        let records = app.world().resource::<Records>();
        assert!(
            records.record_for(MARATHON).is_none(),
            "a Sprint top-out must not touch the Marathon record (PRD: no result)"
        );
        assert!(records.record_for(SPRINT).is_none());

        app.world_mut().write_message(AppExit::Success);
        app.update();
        let disk = records::load_from(dir.path());
        assert!(
            disk.record_for(MARATHON).is_none(),
            "best.json must gain no Marathon record from a Sprint top-out"
        );
        assert!(disk.record_for(SPRINT).is_none());
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    /// Regression through the persistence tree (T9 owns the recorder now):
    /// a Marathon top-out folds into the Marathon `BestScore`, the forced
    /// flush lands it in `best.json`, and the title-screen view follows.
    #[test]
    fn marathon_top_out_persists_best_score_to_disk() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("marathon-disk");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        let mut app = persist_app(&dir, 0xD05E);
        force_game_over(&mut app);
        app.update();

        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        let disk = records::load_from(dir.path());
        assert_eq!(
            disk.record_for(MARATHON),
            Some(&Record::BestScore {
                score: snapshot.score,
                level: snapshot.level,
                lines: snapshot.lines,
            })
        );
        assert_eq!(
            app.world().resource::<PersistedBestScore>().score,
            snapshot.score,
            "the Marathon view keeps following the record"
        );
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    /// Sprint goal finish: BestTime lands in `Records`, the headline shows
    /// the final `m:ss.hh` time, and a *slower* second finish neither
    /// replaces it nor re-shows the marker.
    #[test]
    fn sprint_goal_finish_records_best_time_and_shows_time() {
        let mut app = menu_test_app();
        set_terminal_game(&mut app, ModeId::Sprint, goal_on_first_lock(), 0xC1EA);
        app.world_mut()
            .resource_mut::<crate::core_bridge::PendingActions>()
            .push(tetris_core::actions::Action::HardDrop);
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app_state(&app), AppState::GameOver);
        app.update();

        let ticks = app.world().non_send::<GameCore>().game.tick_count();
        assert_eq!(
            app.world().resource::<Records>().record_for(SPRINT),
            Some(&Record::BestTime { ticks }),
            "goal finish writes the finish-time record"
        );
        assert!(app
            .world()
            .resource::<Records>()
            .record_for(MARATHON)
            .is_none());
        let time = crate::modes::format_time_ticks(ticks);
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            format!("Time {time}"),
            "the headline shows the final time"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<BestText>(e).is_some()),
            format!("Best {time}"),
            "best line follows the Sprint record"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "NEW RECORD!"
        );

        // A slower finish (one gravity step before the drop): the stored
        // BestTime stands and no marker shows.
        set_terminal_game(&mut app, ModeId::Sprint, goal_on_first_lock(), 0xC1EB);
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut()
            .resource_mut::<crate::core_bridge::PendingActions>()
            .push(tetris_core::actions::Action::HardDrop);
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app_state(&app), AppState::GameOver);
        let slower = app.world().non_send::<GameCore>().game.tick_count();
        assert!(
            slower > ticks,
            "second finish is slower: {slower} vs {ticks}"
        );
        app.update();
        assert_eq!(
            app.world().resource::<Records>().record_for(SPRINT),
            Some(&Record::BestTime { ticks }),
            "a slower time keeps the first record"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "",
            "no marker on a non-improving run"
        );
    }

    /// Sprint top-out on screen: explicit `No result` instead of a time,
    /// score/lines context kept, and `Records` untouched.
    #[test]
    fn sprint_top_out_screen_says_no_result_without_recording() {
        let mut app = menu_test_app();
        start_and_top_out(&mut app, ModeId::Sprint);
        app.update();

        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            "No result"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            ""
        );
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        assert!(snapshot.score > 0, "precondition: the run scored");
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<StatsText>(e).is_some()),
            stats_text(snapshot.score, snapshot.level, snapshot.lines),
            "score/lines context stays visible"
        );
        let records = app.world().resource::<Records>();
        assert!(records.record_for(SPRINT).is_none());
        assert!(records.record_for(MARATHON).is_none());
    }

    /// Dig top-out (real buried-garbage board): same no-result rule.
    #[test]
    fn dig_top_out_records_nothing_and_says_no_result() {
        let mut app = menu_test_app();
        start_and_top_out(&mut app, ModeId::Dig);
        app.update();

        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            "No result"
        );
        let records = app.world().resource::<Records>();
        assert!(records.record_for(DIG).is_none());
        assert!(records.record_for(MARATHON).is_none());
    }

    /// Ultra clock expiry: BestScore recorded, `Time up` headline, marker.
    #[test]
    fn ultra_time_up_records_best_score_either_way() {
        let mut app = menu_test_app();
        set_terminal_game(
            &mut app,
            ModeId::Ultra,
            ModeConfig {
                clock_ticks: Some(120),
                ..ModeConfig::default()
            },
            0x7174,
        );
        for _ in 0..130 {
            app.world_mut().run_schedule(FixedUpdate);
            if app_state(&app) == AppState::GameOver {
                break;
            }
        }
        assert_eq!(app_state(&app), AppState::GameOver);
        app.update();

        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.game.finished_reason(), Some(FinishReason::TimeUp));
        let snapshot = core.game.snapshot();
        let records = app.world().resource::<Records>();
        assert_eq!(
            records.record_for(ULTRA),
            Some(&Record::BestScore {
                score: snapshot.score,
                level: snapshot.level,
                lines: snapshot.lines,
            }),
            "Ultra's clock ending records the score"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            "Time up"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            "NEW RECORD!"
        );
    }

    /// Ultra top-out before the clock: the score still stands (PRD).
    #[test]
    fn ultra_top_out_still_records_best_score() {
        let mut app = menu_test_app();
        start_and_top_out(&mut app, ModeId::Ultra);
        app.update();

        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.game.finished_reason(), Some(FinishReason::TopOut));
        let snapshot = core.game.snapshot();
        let records = app.world().resource::<Records>();
        assert_eq!(
            records.record_for(ULTRA),
            Some(&Record::BestScore {
                score: snapshot.score,
                level: snapshot.level,
                lines: snapshot.lines,
            }),
            "an Ultra top-out keeps the score it earned"
        );
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<ResultText>(e).is_some()),
            "Top out"
        );
    }

    /// T9 retry semantics: "Play again" re-runs the mode that just ended
    /// (not always-Marathon like the old `restart_run` path), fresh board,
    /// pre-roll re-armed.
    #[test]
    fn game_over_retry_reruns_the_selected_mode() {
        let mut app = menu_test_app();
        start_and_top_out(&mut app, ModeId::Sprint);
        app.update();
        let over_root = |world: &World, e: Entity| world.get::<GameOverRoot>(e).is_some();

        click_button_under(&mut app, over_root, |world, e| {
            world.get::<PlayAgainButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Sprint,
            "retry must resume the selected mode, not Marathon"
        );
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(
            app.world().resource::<Countdown>().0,
            crate::modes::PRE_ROLL_TICKS,
            "the pre-roll re-arms for the retry"
        );
    }

    /// The pause panel's Restart uses the same mode-aware retry (T7 board
    /// note: it used to go through `restart_run` → always Marathon).
    #[test]
    fn pause_restart_resumes_the_selected_mode() {
        let mut app = menu_test_app();
        set_state(&mut app, AppState::Title);
        let title_root = |world: &World, e: Entity| world.get::<TitleRoot>(e).is_some();
        click_button_under(&mut app, title_root, |world, e| {
            world.get::<StartButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::ModeSelect);
        let modes_root = |world: &World, e: Entity| world.get::<ModeSelectRoot>(e).is_some();
        click_button_under(&mut app, modes_root, |world, e| {
            world
                .get::<ModeRowButton>(e)
                .is_some_and(|row| row.id == ModeId::Sprint)
        });
        assert_eq!(app_state(&app), AppState::Playing);

        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Paused);
        let pause_root = |world: &World, e: Entity| world.get::<PauseRoot>(e).is_some();
        click_button_under(&mut app, pause_root, |world, e| {
            world.get::<RestartButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Sprint,
            "pause-Restart resumes the selected mode"
        );
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(
            app.world().resource::<Countdown>().0,
            crate::modes::PRE_ROLL_TICKS
        );
    }

    #[test]
    fn local_winner_headline_never_says_you_win_in_plain_versus() {
        // Controller::Human/Human with a crowned winner keeps the T26 copy —
        // the net role-aware path must not leak into local versus.
        let mut app = menu_test_app();
        crown_winner(&mut app, Controller::Human);
        app.update();
        app.update();
        let label = text_of(&mut app, |world, e| {
            world.get::<VersusWinnerText>(e).is_some()
        });
        assert_eq!(label, "PLAYER 1 WINS");
    }
}
