//! Bot Ladder campaign (T16): Mode-select row → 8-rung solo ladder.
//!
//! One screen ([`LadderRoot`]) lists eight bot rungs of rising speed. A
//! rung press launches a **local versus match** (never netplay) through
//! [`start_versus_with_cooldown`] (T15): human on the left, greedy bot on
//! the right, bot idling [`RUNG_COOLDOWN_TICKS`][rung_cooldown] steps
//! after every lock — rung 1 the sluggish `[120]`, rung 8 the frantic
//! `[10]`. The human seat always gets cooldown `0` (it is not bot-driven).
//!
//! ## Rung tuning lives *only* here
//!
//! [`RUNG_COOLDOWNS`] is the single source of pace: rung 4 is the 60-tick
//! [`BOT_LOCK_COOLDOWN_STEPS`](crate::core_bridge::BOT_LOCK_COOLDOWN_STEPS)
//! default every other versus path uses, and the ladder ramps from
//! double-slow to six-times-fast around it. No other module reads the
//! array except the rematch handler and the row labels.
//!
//! ## Unlock / persistence
//!
//! The playable set derives from [`Records`]: highest
//! [`Record::HighestRung`](crate::records::Record::HighestRung) beaten (via
//! [`highest_beaten_rung`]) plus one rung. A crowning **human (Left) win**
//! on a ladder-origin match folds `HighestRung { rung }` into the records
//! (forced save, same disk policy as the T9 terminal recorder); a bot win
//! records nothing — the rung simply retries. Rungs above the frontier
//! render `LOCKED` and ignore presses.
//!
//! ## Ladder vs normal-versus isolation
//!
//! The flow marker is [`LadderOrigin`] on the existing
//! [`VersusFlow`](crate::screens_menu::VersusFlow) resource (no new
//! resources — house resource-count discipline): `Screen` while the list
//! shows, `Match { rung }` from rung press until the winner overlay's
//! Menu (or a pause quit) walks back to the list, `Closed` otherwise.
//! Only `Match { .. }` arms the record-on-crowning fold and the versus
//! HUD rung badge, so plain 1v1 / netplay crowns never touch
//! [`BOT_LADDER`](crate::records::BOT_LADDER) records or the badge.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use tetris_core::versus::{AttackRule, Side};

use crate::core_bridge::{
    start_versus_with_cooldown, Controller, SimPaused, VersusMatch, VersusWinner,
};
use crate::juice::JuiceFreeze;
use crate::records::{highest_beaten_rung, Record, Records, RecordsSaveQueue, BOT_LADDER};
use crate::screens_menu::{
    label_node, menu_button, release_sim, VersusFlow, BUTTON_BG, PANEL_BG, RECORD_COLOR,
};
use crate::state::{AppState, RebindingCapture};

// ---------------------------------------------------------------------------
// Ladder flow marker (lives on the existing VersusFlow resource)
// ---------------------------------------------------------------------------

/// Where the Bot Ladder campaign is in the flow, stored on
/// [`VersusFlow::ladder`] — the single marker that separates a ladder match
/// from a plain 1v1. Only [`Self::Match`] arms the record-on-crowning fold
/// and the versus HUD rung badge; `Closed` is every other flow (plain 1v1,
/// netplay, solo).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LadderOrigin {
    /// No ladder context anywhere (plain title / 1v1 / net / solo play).
    #[default]
    Closed,
    /// The ladder screen shows (inside `AppState::ModeSelect`); the mode
    /// list hides until Back or Escape closes it.
    Screen,
    /// A rung match is live or its winner overlay is showing; `rung` is
    /// 1-based. This is the *ladder-origin* flag the winner-overlay Menu /
    /// Rematch and the record fold key off, cleared back to
    /// [`Self::Screen`] when the match is left.
    Match {
        /// 1-based rung of the current match.
        rung: u32,
    },
}

// ---------------------------------------------------------------------------
// Rung tuning (the only place pace numbers live)
// ---------------------------------------------------------------------------

/// Post-lock idle (fixed steps) of the ladder bot per rung, rung 1 first.
/// Rung 4 == 60 == the default [`BOT_LOCK_COOLDOWN_STEPS`] of every other
/// versus path; the ladder spans double-slow to six-times-fast. Tuning the
/// campaign means editing this array and nothing else.
pub const RUNG_COOLDOWNS: [u32; 8] = [120, 104, 82, 60, 44, 30, 19, 10];

/// How many rungs the ladder has.
pub const TOTAL_RUNGS: usize = RUNG_COOLDOWNS.len();

/// Post-lock idle of the rung `rung` (1-based). Out-of-range rungs clamp to
/// the nearest end rather than panicking (defensive; callers only pass rungs
/// built from the list).
#[must_use]
pub fn rung_cooldown(rung: u32) -> u32 {
    RUNG_COOLDOWNS[(rung as usize).saturating_sub(1).min(TOTAL_RUNGS - 1)]
}

/// Which state rung `rung` shows in, derived from [`Records`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RungState {
    /// `rung <= highest` beaten — replayable (a win can never regress).
    Beaten,
    /// `rung == highest + 1` — the frontier; playable now.
    Playable,
    /// Above the frontier — inert until the rung below is beaten.
    Locked,
}

/// [`RungState`] of `rung` for the current ladder progress in `records`.
#[must_use]
pub fn rung_state(records: &Records, rung: u32) -> RungState {
    let highest = highest_beaten_rung(records);
    if rung <= highest {
        RungState::Beaten
    } else if rung == highest + 1 {
        RungState::Playable
    } else {
        RungState::Locked
    }
}

/// Row state label: `BEATEN` / `PLAY — <c> t bot` / `LOCKED`.
#[must_use]
pub fn rung_state_text(records: &Records, rung: u32) -> String {
    match rung_state(records, rung) {
        RungState::Beaten => "BEATEN".to_string(),
        RungState::Playable => format!("PLAY - {} t bot", rung_cooldown(rung)),
        RungState::Locked => "LOCKED".to_string(),
    }
}

/// Rung badge for the versus HUD Status slot while a ladder-origin match
/// is live, e.g. `RUNG 3/8`.
#[must_use]
pub fn ladder_badge_text(rung: u32) -> String {
    format!("RUNG {rung}/{TOTAL_RUNGS}")
}

// ---------------------------------------------------------------------------
// Pure handlers (systems are thin glue)
// ---------------------------------------------------------------------------

/// Open the ladder screen from the mode-select Bot Ladder row: mark the
/// flow and pin the state to `ModeSelect` (the mode list hides behind the
/// [`LadderRoot`] gate; no new [`AppState`] variant).
pub fn open_ladder(state: &mut AppState, flow: &mut VersusFlow) {
    flow.ladder = LadderOrigin::Screen;
    *state = AppState::ModeSelect;
}

/// Press rung `rung`: mark the flow as ladder-origin (this is what arms the
/// record-on-crowning fold and the HUD badge), then launch the match —
/// Garbage rule, human left / bot right, cooldowns `[0, rung]` (the human
/// seat is never bot-paced).
pub fn start_ladder_rung(
    rung: u32,
    versus: &mut VersusMatch,
    winner: &mut VersusWinner,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
    flow: &mut VersusFlow,
) {
    release_sim(sim, freeze);
    flow.ladder = LadderOrigin::Match { rung };
    start_versus_with_cooldown(
        versus,
        winner,
        state,
        AttackRule::Garbage,
        Controller::Human,
        Controller::Bot,
        [0, rung_cooldown(rung)],
    );
}

/// Ladder screen "Back" (and Escape): close the ladder, the mode list
/// shows again.
pub fn ladder_back(flow: &mut VersusFlow) {
    flow.ladder = LadderOrigin::Closed;
}

/// Human win on a ladder-origin match: fold `HighestRung { rung }` into the
/// records and force the debounced writer (unlocks the next rung). `true`
/// when the record improved. Bot wins / non-ladder crowns record nothing.
pub fn record_ladder_win(
    rung: u32,
    records: &mut Records,
    queue: Option<&mut RecordsSaveQueue>,
) -> bool {
    let improved = records.record_run(BOT_LADDER, Record::HighestRung { rung });
    if improved {
        if let Some(queue) = queue {
            queue.pending = true;
            queue.force = true;
        }
    }
    improved
}

// ---------------------------------------------------------------------------
// UI markers
// ---------------------------------------------------------------------------

/// Root of the ladder screen; visible while `AppState::ModeSelect` shows
/// and [`VersusFlow::ladder`] is [`LadderOrigin::Screen`] (the mode-select
/// list applies the matching hide — see
/// [`crate::screens_modes::sync_mode_select_visibility`]).
#[derive(Component)]
pub struct LadderRoot;

/// One ladder rung button; the 1-based `rung` it starts.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderRungButton {
    /// 1-based rung index into [`RUNG_COOLDOWNS`].
    pub rung: u32,
}

/// A rung row's state label ([`rung_state_text`]), rewritten from
/// [`Records`] on change.
#[derive(Component, Debug, Clone, Copy)]
pub struct LadderRungStateLabel {
    /// The rung this label describes.
    pub rung: u32,
}

/// "Back" button on the ladder screen → [`ladder_back`].
#[derive(Component)]
pub struct LadderBackButton;

// ---------------------------------------------------------------------------
// UI construction (rows render from RUNG_COOLDOWNS, never a fixed list)
// ---------------------------------------------------------------------------

fn build_ladder_ui(mut commands: Commands) {
    commands
        .spawn((
            LadderRoot,
            ZIndex(1),
            Visibility::Hidden,
            // Containers are inert (house picking discipline).
            Pickable::IGNORE,
            BackgroundColor(PANEL_BG),
            Node {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: Val::Px(8.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn(label_node("BOT LADDER".to_string(), 40.0));
            for (index, _cooldown) in RUNG_COOLDOWNS.iter().enumerate() {
                let rung = index as u32 + 1;
                root.spawn((
                    Button,
                    LadderRungButton { rung },
                    BackgroundColor(BUTTON_BG),
                    Node {
                        width: Val::Px(320.0),
                        max_width: Val::Percent(92.0),
                        min_height: Val::Px(48.0),
                        flex_direction: FlexDirection::Column,
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        row_gap: Val::Px(2.0),
                        ..default()
                    },
                ))
                .with_children(|row| {
                    row.spawn(label_node(format!("RUNG {rung}"), 20.0));
                    row.spawn((
                        LadderRungStateLabel { rung },
                        Text::new(String::new()),
                        TextFont::from_font_size(13.0),
                        TextColor(RECORD_COLOR),
                        Pickable::IGNORE,
                    ));
                });
            }
            menu_button(root, "Back", LadderBackButton);
        });
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

fn sync_ladder_visibility(
    state: Res<AppState>,
    flow: Res<VersusFlow>,
    mut roots: Query<&mut Visibility, With<LadderRoot>>,
) {
    let wanted = if *state == AppState::ModeSelect && flow.ladder == LadderOrigin::Screen {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    for mut vis in &mut roots {
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

/// Rewrite every rung row's state label when the records (progress) or the
/// flow (a fresh open of the screen) move.
fn sync_ladder_labels(
    records: Res<Records>,
    flow: Res<VersusFlow>,
    state: Res<AppState>,
    mut labels: Query<(&LadderRungStateLabel, &mut Text)>,
) {
    if !records.is_changed() && !flow.is_changed() && !state.is_changed() {
        return;
    }
    for (label, mut text) in &mut labels {
        let line = rung_state_text(&records, label.rung);
        if text.0 != line {
            *text = Text::new(line);
        }
    }
}

/// Rung / Back clicks while the ladder screen shows. Locked rungs above
/// the frontier are inert (no match, no state change).
type LadderClickQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Interaction,
        Option<&'static LadderRungButton>,
        Has<LadderBackButton>,
    ),
    (With<Button>, Changed<Interaction>),
>;

#[derive(SystemParam)]
struct LadderClickParams<'w, 's> {
    buttons: LadderClickQuery<'w, 's>,
    versus: Option<NonSendMut<'w, VersusMatch>>,
    winner: Option<ResMut<'w, VersusWinner>>,
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    flow: ResMut<'w, VersusFlow>,
    records: Option<Res<'w, Records>>,
}

fn ladder_clicks(mut params: LadderClickParams) {
    if *params.state != AppState::ModeSelect || params.flow.ladder != LadderOrigin::Screen {
        return;
    }
    for (_entity, interaction, rung, back) in params.buttons.iter() {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if back {
            ladder_back(&mut params.flow);
            return;
        }
        let Some(button) = rung else { continue };
        // Frontier check: only rungs at or below highest + 1 accept a press.
        let playable = params
            .records
            .as_deref()
            .is_none_or(|records| rung_state(records, button.rung) != RungState::Locked);
        if !playable {
            return;
        }
        if let (Some(versus), Some(winner)) =
            (params.versus.as_deref_mut(), params.winner.as_deref_mut())
        {
            start_ladder_rung(
                button.rung,
                versus,
                winner,
                &mut params.state,
                &mut params.sim,
                &params.freeze,
                &mut params.flow,
            );
        }
        return;
    }
}

/// Crown → record: a **Left** (human) win on a ladder-origin match folds
/// [`Record::HighestRung`] (unlocks the next rung, forced save). A Right
/// (bot) win records nothing — the rung retries. Non-ladder crowns
/// ([`LadderOrigin::Closed`], plain 1v1 / netplay) never reach the fold.
/// Re-crowning the same rung is idempotent (`record_run` keeps the higher).
fn ladder_record_system(
    winner: Res<VersusWinner>,
    flow: Res<VersusFlow>,
    mut records: Option<ResMut<Records>>,
    mut queue: Option<ResMut<RecordsSaveQueue>>,
) {
    if !winner.is_changed() || winner.0 != Some(Side::Left) {
        return;
    }
    let LadderOrigin::Match { rung } = flow.ladder else {
        return;
    };
    let Some(records) = records.as_deref_mut() else {
        return;
    };
    let improved = record_ladder_win(rung, records, queue.as_deref_mut());
    if improved {
        info!("bot ladder: rung {rung} beaten, next rung unlocked");
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// The T16 Bot Ladder screen. Mount alongside
/// [`MenuScreensPlugin`](crate::screens_menu::MenuScreensPlugin) and
/// [`ModeSelectPlugin`](crate::screens_modes::ModeSelectPlugin) (the flow
/// resource and the row that opens it live there).
pub struct BotLadderPlugin;

impl Plugin for BotLadderPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits (screens_menu precedent): no-ops when the owning
        // plugin already registered the resource, but every handler
        // parameter stays satisfied in headless `MinimalPlugins` tests.
        app.init_resource::<AppState>()
            .init_resource::<SimPaused>()
            .init_resource::<JuiceFreeze>()
            .init_resource::<RebindingCapture>()
            .init_resource::<Records>()
            .init_resource::<VersusFlow>();
        app.add_systems(Startup, build_ladder_ui).add_systems(
            Update,
            // Clicks first (a press transitions the same frame), then the
            // visibility/label syncs observing it; the record fold runs off
            // the winner resource whenever it moves.
            (
                ladder_clicks,
                ladder_record_system,
                sync_ladder_visibility,
                sync_ladder_labels,
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

    use bevy::app::App;
    use bevy::ecs::relationship::Relationship;
    use bevy::window::{Window, WindowPlugin};

    use crate::core_bridge::{CoreBridgePlugin, GameCore, BOT_LOCK_COOLDOWN_STEPS};
    use crate::hud::{VersusHudSlot, VersusHudText};
    use crate::modes::ModeId;
    use crate::records;
    use crate::screens_menu::{
        MenuScreensPlugin, VersusMenuButton, VersusOverRoot, VersusRematchButton,
    };
    use crate::screens_modes::{ModeBackButton, ModeRowButton, ModeSelectPlugin, ModeSelectRoot};
    use crate::settings_persist::{BEST_FILE, CONFIG_DIR_ENV, ENV_LOCK};

    // ---- pure handlers ----

    #[test]
    fn rung_cooldowns_match_the_plan_and_ramp_fast() {
        assert_eq!(RUNG_COOLDOWNS, [120, 104, 82, 60, 44, 30, 19, 10]);
        assert_eq!(TOTAL_RUNGS, 8);
        // Rung 4 is today's default versus pace; the ladder spans it.
        assert_eq!(RUNG_COOLDOWNS[3], BOT_LOCK_COOLDOWN_STEPS);
        for pair in RUNG_COOLDOWNS.windows(2) {
            assert!(pair[1] < pair[0], "rungs must rise in speed: {pair:?}");
        }
        assert_eq!(rung_cooldown(1), 120);
        assert_eq!(rung_cooldown(8), 10);
        assert_eq!(rung_cooldown(0), 120, "clamped low, never a panic");
        assert_eq!(rung_cooldown(99), 10, "clamped high, never a panic");
    }

    #[test]
    fn rung_states_derive_from_highest_beaten() {
        let mut records = Records::default();
        assert_eq!(rung_state(&records, 1), RungState::Playable);
        assert_eq!(rung_state(&records, 2), RungState::Locked);
        assert_eq!(
            rung_state_text(&records, 1),
            "PLAY - 120 t bot",
            "the frontier shows its bot pace"
        );
        assert_eq!(rung_state_text(&records, 2), "LOCKED");

        records.record_run(BOT_LADDER, Record::HighestRung { rung: 2 });
        assert_eq!(rung_state(&records, 1), RungState::Beaten);
        assert_eq!(rung_state(&records, 2), RungState::Beaten);
        assert_eq!(rung_state(&records, 3), RungState::Playable);
        assert_eq!(rung_state(&records, 4), RungState::Locked);
        assert_eq!(rung_state_text(&records, 1), "BEATEN");

        // A non-rung record under the key (foreign write) reads as no progress.
        let mut foreign = Records::default();
        foreign.record_run(BOT_LADDER, Record::BestTime { ticks: 1 });
        assert_eq!(highest_beaten_rung(&foreign), 0);
        assert_eq!(rung_state(&foreign, 1), RungState::Playable);
    }

    #[test]
    fn ladder_badge_text_shows_rung_and_total() {
        assert_eq!(ladder_badge_text(3), "RUNG 3/8");
        assert_eq!(ladder_badge_text(8), "RUNG 8/8");
    }

    // ---- headless integration ----

    fn ladder_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t16 headless".into(),
                resolution: (1280, 720).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            CoreBridgePlugin,
            crate::hud::HudPlugin,
            crate::input::InputPlugin,
            MenuScreensPlugin,
            ModeSelectPlugin,
            BotLadderPlugin,
        ));
        app.update();
        app
    }

    /// Production-equivalent persistence tree (records writer included) so
    /// the forced rung save can be verified on disk.
    fn persist_ladder_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t16 persist".into(),
                resolution: (1280, 720).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            CoreBridgePlugin,
            crate::settings_persist::SettingsPersistPlugin,
            MenuScreensPlugin,
            ModeSelectPlugin,
            BotLadderPlugin,
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

    fn flow(app: &App) -> VersusFlow {
        *app.world().resource::<VersusFlow>()
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

    fn versus_hud_text(app: &mut App, side: Side, slot: VersusHudSlot) -> String {
        let mut query = app.world_mut().query::<(&VersusHudText, &Text2d)>();
        query
            .iter(app.world())
            .find(|(meta, _)| meta.side == side && meta.slot == slot)
            .map(|(_, text)| text.0.clone())
            .expect("versus stat text exists")
    }

    fn open_ladder_via_mode_select(app: &mut App) {
        set_state(app, AppState::Title);
        click_button_under(
            app,
            |world, e| world.get::<crate::screens_menu::TitleRoot>(e).is_some(),
            |world, e| world.get::<crate::screens_menu::StartButton>(e).is_some(),
        );
        assert_eq!(app_state(app), AppState::ModeSelect);
        click_button_under(
            app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<ModeRowButton>(e)
                    .is_some_and(|row| row.id == ModeId::BotLadder)
            },
        );
    }

    /// Crown the match by writing the winner resource directly (the
    /// pattern the existing versus tests use) — the ladder record fold
    /// keys off the same observable the overlay does.
    fn crown(app: &mut App, side: Side) {
        *app.world_mut().resource_mut::<VersusWinner>() = VersusWinner(Some(side));
        app.update();
        app.update();
    }

    fn click_overlay(app: &mut App, marker: fn(&World, Entity) -> bool) {
        click_button_under(
            app,
            |world, e| world.get::<VersusOverRoot>(e).is_some(),
            marker,
        );
    }

    // ---- entry / screen ----

    #[test]
    fn mode_select_row_opens_ladder_without_starting_a_run() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        assert_eq!(
            app_state(&app),
            AppState::ModeSelect,
            "the ladder screen lives inside ModeSelect (additive flow state)"
        );
        assert_eq!(flow(&app).ladder, LadderOrigin::Screen);
        app.update();
        assert_eq!(vis_of::<LadderRoot>(&mut app), Visibility::Visible);
        assert_eq!(
            vis_of::<ModeSelectRoot>(&mut app),
            Visibility::Hidden,
            "the mode list hides behind the ladder"
        );
        assert!(
            !app.world().non_send::<VersusMatch>().active,
            "opening the ladder starts nothing"
        );
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
    }

    #[test]
    fn ladder_back_returns_to_the_mode_list() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| world.get::<LadderBackButton>(e).is_some(),
        );
        assert_eq!(flow(&app).ladder, LadderOrigin::Closed);
        assert_eq!(app_state(&app), AppState::ModeSelect);
        app.update();
        assert_eq!(vis_of::<LadderRoot>(&mut app), Visibility::Hidden);
        assert_eq!(vis_of::<ModeSelectRoot>(&mut app), Visibility::Visible);

        // And the mode list works again afterwards (Back -> Title).
        click_button_under(
            &mut app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| world.get::<ModeBackButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::Title);
    }

    #[test]
    fn ladder_rows_render_eight_rungs_with_states() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        let rungs: Vec<u32> = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&LadderRungButton, With<Button>>();
            let mut ids: Vec<u32> = q.iter(world).map(|b| b.rung).collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(rungs, (1..=8).collect::<Vec<u32>>());
        let label = |app: &mut App, rung: u32| {
            text_of(app, move |world, e| {
                world
                    .get::<LadderRungStateLabel>(e)
                    .is_some_and(|l| l.rung == rung)
            })
        };
        assert!(label(&mut app, 1).starts_with("PLAY"), "frontier playable");
        assert_eq!(label(&mut app, 2), "LOCKED");
        app.world_mut()
            .resource_mut::<Records>()
            .record_run(BOT_LADDER, Record::HighestRung { rung: 1 });
        app.update();
        assert_eq!(label(&mut app, 1), "BEATEN");
        assert!(label(&mut app, 2).starts_with("PLAY"));
    }

    // ---- rung press ----

    #[test]
    fn rung_press_starts_garbage_human_vs_bot_with_rung_cooldown() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 1)
            },
        );
        let versus = app.world().non_send::<VersusMatch>();
        assert!(versus.active, "rung press launches the match");
        assert_eq!(versus.rule, AttackRule::Garbage);
        assert_eq!(versus.p1, Controller::Human, "p1 = left = human");
        assert_eq!(versus.p2, Controller::Bot, "p2 = right = bot");
        assert_eq!(
            versus.bot_cooldown_ticks,
            [0, 120],
            "human seat never bot-paced; rung 1 bot idles 120"
        );
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(flow(&app).ladder, LadderOrigin::Match { rung: 1 });
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        app.update();
        assert_eq!(vis_of::<LadderRoot>(&mut app), Visibility::Hidden);
        // The HUD badge follows the flow.
        assert_eq!(
            versus_hud_text(&mut app, Side::Left, VersusHudSlot::Status),
            "RUNG 1/8"
        );
        assert_eq!(
            versus_hud_text(&mut app, Side::Right, VersusHudSlot::Status),
            "RUNG 1/8"
        );
    }

    #[test]
    fn locked_rung_press_is_inert() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 3)
            },
        );
        assert!(
            !app.world().non_send::<VersusMatch>().active,
            "rungs above highest + 1 must not start"
        );
        assert_eq!(flow(&app).ladder, LadderOrigin::Screen);
        assert_eq!(app_state(&app), AppState::ModeSelect);
    }

    // ---- win / loss ----

    #[test]
    fn human_win_records_rung_unlocks_next_and_menu_returns_to_ladder() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 1)
            },
        );
        crown(&mut app, Side::Left);
        assert_eq!(
            app.world().resource::<Records>().record_for(BOT_LADDER),
            Some(&Record::HighestRung { rung: 1 }),
            "a human win folds the rung into the records"
        );

        // Winner overlay is up; Menu walks back to the LADDER screen, not
        // the title, and the next rung is now playable.
        click_overlay(&mut app, |world, e| {
            world.get::<VersusMenuButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::ModeSelect);
        assert_eq!(flow(&app).ladder, LadderOrigin::Screen);
        assert!(!app.world().non_send::<VersusMatch>().active);
        app.update();
        assert_eq!(vis_of::<LadderRoot>(&mut app), Visibility::Visible);
        let rung2 = text_of(&mut app, |world, e| {
            world
                .get::<LadderRungStateLabel>(e)
                .is_some_and(|l| l.rung == 2)
        });
        assert!(
            rung2.starts_with("PLAY"),
            "beating rung 1 unlocks rung 2: {rung2}"
        );
        // And rung 2 really starts, with its own cooldown.
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 2)
            },
        );
        let versus = app.world().non_send::<VersusMatch>();
        assert!(versus.active);
        assert_eq!(versus.bot_cooldown_ticks, [0, 104]);
        assert_eq!(flow(&app).ladder, LadderOrigin::Match { rung: 2 });
    }

    #[test]
    fn bot_win_records_nothing_and_the_rung_retries() {
        let mut app = ladder_test_app();
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 1)
            },
        );
        crown(&mut app, Side::Right);
        assert!(
            app.world()
                .resource::<Records>()
                .record_for(BOT_LADDER)
                .is_none(),
            "a bot win records nothing"
        );
        click_overlay(&mut app, |world, e| {
            world.get::<VersusMenuButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::ModeSelect);
        assert_eq!(flow(&app).ladder, LadderOrigin::Screen);
        let rung1 = text_of(&mut app, |world, e| {
            world
                .get::<LadderRungStateLabel>(e)
                .is_some_and(|l| l.rung == 1)
        });
        assert!(rung1.starts_with("PLAY"), "the rung retries: {rung1}");
    }

    #[test]
    fn ladder_rematch_keeps_the_rung_and_its_cooldown() {
        let mut app = ladder_test_app();
        app.world_mut()
            .resource_mut::<Records>()
            .record_run(BOT_LADDER, Record::HighestRung { rung: 1 });
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 2)
            },
        );
        crown(&mut app, Side::Left);
        click_overlay(&mut app, |world, e| {
            world.get::<VersusRematchButton>(e).is_some()
        });
        let versus = app.world().non_send::<VersusMatch>();
        assert!(versus.active, "rematch keeps the ladder match up");
        assert_eq!(
            versus.bot_cooldown_ticks,
            [0, 104],
            "rematch re-arms rung 2's cooldown, not the 60 default"
        );
        assert_eq!(versus.steps, 0, "fresh match");
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));
        assert_eq!(flow(&app).ladder, LadderOrigin::Match { rung: 2 });
        assert_eq!(app_state(&app), AppState::Playing);
    }

    // ---- isolation from plain versus ----

    #[test]
    fn plain_versus_crowning_never_touches_the_ladder_record() {
        let mut app = ladder_test_app();
        set_state(&mut app, AppState::Playing);
        // A plain (non-ladder) match through the T26 start path.
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    crate::core_bridge::start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Bot,
                    );
                });
            });
        assert_eq!(flow(&app).ladder, LadderOrigin::Closed);
        crown(&mut app, Side::Left);
        assert!(
            app.world()
                .resource::<Records>()
                .record_for(BOT_LADDER)
                .is_none(),
            "flow isolation: a plain 1v1 win must not unlock ladder rungs"
        );
        // No badge on a plain match either.
        assert_eq!(
            versus_hud_text(&mut app, Side::Left, VersusHudSlot::Status),
            ""
        );
        // And its overlay Menu still walks to the Title, untouched.
        click_overlay(&mut app, |world, e| {
            world.get::<VersusMenuButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Title);
    }

    // ---- persistence round-trip ----

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("tetris-t16-{label}-{}-{id}", std::process::id()));
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

    #[test]
    fn rung_progress_persists_and_unlocks_the_next_rung_after_reload() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = TempDir::new("rung-progress");
        std::env::set_var(CONFIG_DIR_ENV, dir.path());

        // Beat rung 3 on a records-loaded boot.
        let mut app = persist_ladder_app();
        app.world_mut()
            .resource_mut::<Records>()
            .record_run(BOT_LADDER, Record::HighestRung { rung: 2 });
        open_ladder_via_mode_select(&mut app);
        click_button_under(
            &mut app,
            |world, e| world.get::<LadderRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<LadderRungButton>(e)
                    .is_some_and(|b| b.rung == 3)
            },
        );
        crown(&mut app, Side::Left);
        app.world_mut().write_message(AppExit::Success);
        app.update();
        let disk = records::load_from(dir.path());
        assert_eq!(
            disk.record_for(BOT_LADDER),
            Some(&Record::HighestRung { rung: 3 }),
            "the forced rung save lands in best.json ({})",
            dir.path().join(BEST_FILE).display()
        );

        // Reload: a fresh boot reads the ladder state and rung 4 is the
        // playable frontier (rung 5 stays locked).
        let mut app = persist_ladder_app();
        assert_eq!(highest_beaten_rung(app.world().resource::<Records>()), 3);
        open_ladder_via_mode_select(&mut app);
        let label = |app: &mut App, rung: u32| {
            text_of(app, move |world, e| {
                world
                    .get::<LadderRungStateLabel>(e)
                    .is_some_and(|l| l.rung == rung)
            })
        };
        assert!(label(&mut app, 4).starts_with("PLAY"), "rung 4 playable");
        assert_eq!(label(&mut app, 5), "LOCKED");
        assert_eq!(label(&mut app, 3), "BEATEN");
        std::env::remove_var(CONFIG_DIR_ENV);
    }

    // ---- ladder difficulty direction (T15 rate-test style) ----

    #[test]
    fn fast_bot_beats_slow_bot_at_the_ladder_poles() {
        // Rung difficulty direction: the 10-tick bot (rung 8 pace) must
        // bury the 120-tick bot (rung 1 pace). Both seats Bot so the
        // comparison is pure pacing — the T15 cooldown param, broadcast
        // per-side as [120, 10].
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, crate::input::InputPlugin));
        app.init_resource::<AppState>();
        {
            let world = app.world_mut();
            world.resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus_with_cooldown(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Bot,
                        Controller::Bot,
                        [RUNG_COOLDOWNS[0], RUNG_COOLDOWNS[7]],
                    );
                });
            });
            // Pin the match RNG (start seeds from the wall clock) — the
            // cooldown params under test are untouched by the re-seed.
            let mut versus = world.non_send_mut::<VersusMatch>();
            versus.match_ = tetris_core::versus::Match::new(0x5EED_0016, AttackRule::Garbage);
        }
        let mut winner = None;
        for _ in 0..30_000 {
            app.world_mut().run_schedule(FixedUpdate);
            if let Some(side) = app.world().resource::<VersusWinner>().0 {
                winner = Some(side);
                break;
            }
        }
        assert_eq!(
            winner,
            Some(Side::Right),
            "the 10-tick bot must out-pace the 120-tick bot"
        );
    }
}
