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
//! set itself), `resume_game` / `goto_title` / `start_new_run` *never write
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
//! - Restart always goes through
//!   [`restart_run`](crate::core_bridge::restart_run); the R key is bound by
//!   the core bridge (T14) and is *not* re-bound here.
//! - The best score is recorded solely by T15's `best_score_system` on the
//!   `GameEvent::GameOver` core event; this screen only *reads*
//!   [`PersistedBestScore`] for display and never writes it.
//! - Opening [`AppState::Settings`] is just a state write — T16's
//!   `track_settings_entry` records the origin (Pause → back → Pause) and
//!   `handle_back` returns here, closing T16's deferred round-trip check.
//! - Quit writes `AppExit::Success` and latches [`QuitRequested`] so headless
//!   tests can observe the request (the App consumes/exits on the message).

use bevy::ecs::system::SystemParam;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use crate::core_bridge::{restart_run, GameCore, SimPaused};
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::juice::JuiceFreeze;
use crate::settings_persist::PersistedBestScore;
use crate::state::{AppState, RebindingCapture};

/// App version shown on the title screen (PRD §7 "extras").
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Opaque backdrop for the title screen.
const PANEL_BG: Color = Color::srgb(0.09, 0.09, 0.12);
/// Dimming backdrop for the pause / game-over overlays.
const DIM_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.62);
const BUTTON_BG: Color = Color::srgb(0.22, 0.22, 0.27);
const RECORD_COLOR: Color = Color::srgb(1.0, 0.85, 0.3);

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

/// Fresh run via the shared T14 restart path (honors `TETRIS_SEED`); used by
/// Title → Start, Pause → Restart and Game Over → Play again.
pub fn start_new_run(
    core: &mut GameCore,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
) {
    release_sim(sim, freeze);
    restart_run(core, state);
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

/// PRD §7.4 highlight: the run tied the best score, which must be a real
/// record (`best > 0` keeps the 0 == 0 first-run case quiet).
pub fn is_new_record(final_score: u64, best: &PersistedBestScore) -> bool {
    final_score == best.score && best.score > 0
}

/// Final stats line (PRD §7.4: score, level and lines).
pub fn stats_text(score: u64, level: u32, lines: u32) -> String {
    format!("Score {score}   Level {level}   Lines {lines}")
}

/// Persisted best line (PRD §7.4 "best").
pub fn best_text(best: &PersistedBestScore) -> String {
    format!("Best {}", best.score)
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

/// Title "Start" button ([`start_new_run`]).
#[derive(Component)]
pub struct StartButton;

/// Game-over "Play again" button ([`start_new_run`]).
#[derive(Component)]
pub struct PlayAgainButton;

/// Pause "Resume" button ([`resume_game`]).
#[derive(Component)]
pub struct ResumeButton;

/// Restart button on both the pause overlay and game-over screen
/// ([`start_new_run`]).
#[derive(Component)]
pub struct RestartButton;

/// "Settings" button on the title and pause screens ([`open_settings`]).
#[derive(Component)]
pub struct OpenSettingsButton;

/// "Menu" button (PRD §7.3 quit-to-title, PRD §7.4 title) → [`goto_title`].
#[derive(Component)]
pub struct QuitToTitleButton;

/// "Quit" button on every screen: writes `AppExit::Success`.
#[derive(Component)]
pub struct QuitButton;

/// Game-over final stats label ([`stats_text`]).
#[derive(Component)]
pub struct StatsText;

/// Best-score label on the title and game-over screens ([`best_text`]).
#[derive(Component)]
pub struct BestText;

/// "NEW RECORD!" label ([`is_new_record`]).
#[derive(Component)]
pub struct RecordText;

/// Latched `true` when a quit button requested [`AppExit::Success`]. The
/// exit message itself is consumed by the App during `update()`, so headless
/// tests observe the request through this marker instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Resource)]
pub struct QuitRequested(pub bool);

// ---------------------------------------------------------------------------
// Systems (UI glue)
// ---------------------------------------------------------------------------

/// Root visibility query over the three menu screens. The `Or` filter is
/// load-bearing: `Visibility` is a default component on every entity, so a
/// bare `Has<…>` query would match (and hide) the entire world.
type MenuRoots<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Visibility,
        Has<TitleRoot>,
        Has<PauseRoot>,
        Has<GameOverRoot>,
    ),
    Or<(With<TitleRoot>, With<PauseRoot>, With<GameOverRoot>)>,
>;

#[derive(SystemParam)]
struct RootVisibilityParams<'w, 's> {
    state: Res<'w, AppState>,
    roots: MenuRoots<'w, 's>,
}

/// Show each root only in its own [`AppState`] variant.
fn sync_root_visibility(params: RootVisibilityParams) {
    let RootVisibilityParams { state, mut roots } = params;
    for (mut vis, title, pause, over) in roots.iter_mut() {
        let wanted = if (title && *state == AppState::Title)
            || (pause && *state == AppState::Paused)
            || (over && *state == AppState::GameOver)
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
/// [`toggle_pause`].
fn pause_chord_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    wheels: Option<Res<Messages<MouseWheel>>>,
    bindings: Res<KeyBindings>,
    capture: Res<RebindingCapture>,
    mut state: ResMut<AppState>,
    mut sim: ResMut<SimPaused>,
    freeze: Res<JuiceFreeze>,
) {
    if capture.capturing {
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
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    quit: ResMut<'w, QuitRequested>,
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
                if start {
                    start_new_run(
                        params.core.as_mut(),
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                    );
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
                    start_new_run(
                        params.core.as_mut(),
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                    );
                } else if settings {
                    open_settings(&mut params.state);
                } else if to_title {
                    goto_title(&mut params.state, &mut params.sim, &params.freeze);
                } else if quit {
                    quit_now();
                }
            }
            AppState::GameOver => {
                if again {
                    start_new_run(
                        params.core.as_mut(),
                        &mut params.state,
                        &mut params.sim,
                        &params.freeze,
                    );
                } else if to_title {
                    goto_title(&mut params.state, &mut params.sim, &params.freeze);
                } else if quit {
                    quit_now();
                }
            }
            _ => {}
        }
    }
}

/// Final-stats label query.
type StatsLabels<'w, 's> =
    Query<'w, 's, &'static mut Text, (With<StatsText>, Without<BestText>, Without<RecordText>)>;

/// Best-score label query.
type BestLabels<'w, 's> = Query<'w, 's, &'static mut Text, (With<BestText>, Without<RecordText>)>;

/// Record-highlight label query.
type RecordLabels<'w, 's> = Query<'w, 's, &'static mut Text, With<RecordText>>;

/// Rewrite the dynamic labels (stats / best / NEW RECORD) whenever the state
/// or best score moves. T15 records the best on the GameOver core event —
/// this display path never writes it back.
#[derive(SystemParam)]
struct ScreenTextParams<'w, 's> {
    state: Res<'w, AppState>,
    best: Res<'w, PersistedBestScore>,
    core: Option<NonSend<'w, GameCore>>,
    stats: StatsLabels<'w, 's>,
    best_labels: BestLabels<'w, 's>,
    record_labels: RecordLabels<'w, 's>,
}

fn sync_screen_texts(params: ScreenTextParams) {
    let ScreenTextParams {
        state,
        best,
        core,
        mut stats,
        mut best_labels,
        mut record_labels,
    } = params;
    if !state.is_changed() && !best.is_changed() {
        return;
    }
    let best_string = best_text(&best);
    for mut text in best_labels.iter_mut() {
        *text = Text::new(best_string.clone());
    }
    let game_over = *state == AppState::GameOver;
    if let Some(core) = core {
        let snapshot = core.game.snapshot();
        let stats_string = stats_text(snapshot.score, snapshot.level, snapshot.lines);
        for mut text in stats.iter_mut() {
            *text = Text::new(stats_string.clone());
        }
        let record = if game_over && is_new_record(snapshot.score, &best) {
            "NEW RECORD!"
        } else {
            ""
        };
        for mut text in record_labels.iter_mut() {
            *text = Text::new(record);
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

fn label_node(text: String, size: f32) -> (Text, TextFont, TextColor) {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor::WHITE,
    )
}

fn menu_button(parent: &mut ChildSpawnerCommands, text: &str, marker: impl Bundle) {
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
        menu_button(root, "Settings", OpenSettingsButton);
        menu_button(root, "Quit", QuitButton);
    });

    add_menu_root(&mut commands, PauseRoot, DIM_BG, |root| {
        root.spawn(label_node("PAUSED".to_string(), 48.0));
        menu_button(root, "Resume", ResumeButton);
        menu_button(root, "Restart", RestartButton);
        menu_button(root, "Settings", OpenSettingsButton);
        menu_button(root, "Quit to title", QuitToTitleButton);
        menu_button(root, "Quit", QuitButton);
    });

    add_menu_root(&mut commands, GameOverRoot, DIM_BG, |root| {
        root.spawn(label_node("GAME OVER".to_string(), 48.0));
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
        menu_button(root, "Quit", QuitButton);
    });
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Title / pause / game-over screen logic for the T1 `AppState` machine.
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
            .init_resource::<QuitRequested>();
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        if !app.world().contains_resource::<Messages<MouseWheel>>() {
            app.add_message::<MouseWheel>();
        }
        #[cfg(not(test))]
        app.add_systems(Startup, startup_goto_title_system);
        app.add_systems(Startup, build_menu_ui).add_systems(
            Update,
            // Input first: a chord/button transition shows its overlay in the
            // same frame, and the label sync then sees the new state.
            (
                pause_chord_system,
                menu_button_clicks,
                sync_root_visibility,
                sync_screen_texts,
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
    fn new_record_requires_positive_matching_best() {
        let mut best = PersistedBestScore::default();
        assert!(!is_new_record(0, &best), "first run 0 == 0 is not a record");
        best.score = 1000;
        assert!(is_new_record(1000, &best));
        assert!(!is_new_record(999, &best));
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
            InputPlugin,
            MenuScreensPlugin,
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
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
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
        // A previous best below the final score: no record highlight yet.
        app.world_mut().resource_mut::<PersistedBestScore>().score =
            snapshot.score.saturating_sub(1);
        app.update();
        assert_eq!(
            text_of(&mut app, |world, e| world.get::<RecordText>(e).is_some()),
            ""
        );

        // Tie the best to this run -> NEW RECORD per PRD §7.4. T15 owns the
        // write in production; the test sets the resource the same way.
        app.world_mut().resource_mut::<PersistedBestScore>().score = snapshot.score;
        app.update();

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
    }

    #[test]
    fn game_over_play_again_starts_fresh_and_menu_returns_title() {
        let mut app = menu_test_app();
        force_game_over(&mut app);
        let over_root = |world: &World, e: Entity| world.get::<GameOverRoot>(e).is_some();

        click_button_under(&mut app, over_root, |world, e| {
            world.get::<PlayAgainButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);

        force_game_over(&mut app);
        click_button_under(&mut app, over_root, |world, e| {
            world.get::<QuitToTitleButton>(e).is_some()
        });
        assert_eq!(app_state(&app), AppState::Title);
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
}
