//! Mode select screen (T7): Title "Start" → [`AppState::ModeSelect`].
//!
//! One full-screen root ([`ModeSelectRoot`]) spawned once on `Startup` and
//! toggled by [`AppState`] exactly like the T17 menu roots: hidden UI never
//! receives `Interaction` (house picking discipline from
//! [`crate::screens_menu`]), and every click handler additionally gates on
//! the live state.
//!
//! ## Data-driven rows
//!
//! Rows are rendered from the T5 catalogue — [`ModeId::ALL`] filtered by
//! [`is_shipped`], Marathon first — never a hardcoded menu list. Each row is
//! a `Button` carrying [`ModeRowButton`] with the [`ModeId`] and shows
//! [`display_name`], [`description`] and the T6 record line
//! ([`record_line`], refreshed live from [`Records`]). Later releases flip
//! [`is_shipped`] only; this file never grows rows by hand.
//!
//! ## Row press
//!
//! Row press runs the shared T5 bridge path [`start_mode_run`] (seed
//! resolution via `TETRIS_SEED`, pre-roll re-arm, play-count bump) into
//! [`AppState::Playing`]; the overlay then hides with the state change.
//! The one exception is Bot Ladder (T16): its row opens the ladder screen
//! (a ModeSelect-internal flow on
//! [`VersusFlow`](crate::screens_menu::VersusFlow) — the list hides behind
//! it) instead of starting a solo run.
//!
//! ## Mutator toggles (T23)
//!
//! Above the row list, four [`MutatorToggleButton`]s (spawned from
//! [`Mutators::SELECTABLE`](crate::mutators::Mutators) in fixed order,
//! compact + wrap-friendly for portrait) flip bits of the session-scoped
//! pending selection on `GameCore::selected_mutators`. A row press snapshots
//! that selection into the run (`GameCore::start_mode` →
//! `ActiveMode::mutators`), so the toggles never affect a live run, and
//! the selection persists across screen navigation within the session
//! (never across app restarts — not persisted by design). Mutated runs get
//! no best records (`Records::record_run_mutated`) but still bump plays.
//! Back (and Escape) walk to [`AppState::Title`] via
//! [`goto_title`](crate::screens_menu::goto_title); the 1v1 / Online /
//! Settings / Quit entries stay on the Title screen (reachable by Back), so
//! they are deliberately *not* duplicated into the list.
//!
//! ## Scrolling (portrait-first)
//!
//! Rows live in a native bevy_ui scroll viewport: a flex column with
//! `overflow: Clip/Scroll` plus the toolkit's `ScrollPosition` component.
//! On phones the R2 list (Survival/Zen/Daily on top of R1's four) exceeds
//! the screen height, so [`mode_scroll_system`] drives `ScrollPosition` from
//! the mouse wheel and vertical touch drags while the screen is open,
//! clamped to the content (pure [`clamp_scroll`]). Bevy 0.19's
//! `ui_focus_system` honors overflow clipping, so rows scrolled out of the
//! viewport never swallow a tap. Portrait windows get larger row metrics
//! ([`render::portrait_layout`] decides them at build time); desktop uses
//! `max_width` so the column also shrinks on narrow windows.

use bevy::ecs::system::SystemParam;
use bevy::input::mouse::MouseWheel;
use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::prelude::*;

use crate::core_bridge::{start_mode_run, Countdown, GameCore, SimPaused};
use crate::daily::{self, DailyAttempt};
use crate::juice::JuiceFreeze;
use crate::modes::{description, display_name, format_time_ticks, is_shipped, mode_key, ModeId};
use crate::records::{Record, Records};
use crate::render;
use crate::screens_ladder::LadderOrigin;
use crate::screens_menu::{
    goto_title, label_node, menu_button, release_sim, VersusFlow, BUTTON_BG, PANEL_BG, RECORD_COLOR,
};
use crate::state::{AppState, RebindingCapture};

// ---------------------------------------------------------------------------
// Catalogue-driven data (pure)
// ---------------------------------------------------------------------------

/// The shipped mode list: [`ModeId::ALL`] filtered by [`is_shipped`], in
/// catalogue display order (Marathon first). The single source for the row
/// renderer and the tests — flipping `is_shipped` (T13/T14/T16/T17) is the
/// only way rows appear.
#[must_use]
pub fn shipped_modes() -> Vec<ModeId> {
    ModeId::ALL
        .iter()
        .copied()
        .filter(|id| is_shipped(*id))
        .collect()
}

/// Group a number into thousands separated by spaces (`123 456`) — the
/// bundled Fira Mono subset has no comma-glyph convention to lean on and
/// the PRD examples use spaces.
#[must_use]
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

/// One mode's record line for the list: `Best 2:43.91` for a
/// [`Record::BestTime`] (via [`format_time_ticks`]), `Best 123 456` for a
/// [`Record::BestScore`], `-` when the mode has no record yet. Every
/// variant renders something sensible so T14/T16/T17 need no changes here.
#[must_use]
pub fn record_line(record: Option<&Record>) -> String {
    match record {
        None => "-".to_string(),
        Some(Record::BestTime { ticks }) => format!("Best {}", format_time_ticks(*ticks)),
        Some(Record::BestScore { score, .. }) => format!("Best {}", group_thousands(*score)),
        Some(Record::LifetimeLines { total }) => format!("Lines {}", group_thousands(*total)),
        Some(Record::HighestRung { rung }) => format!("Rung {rung}"),
        Some(Record::Daily { date, result }) => format!("{date}: {result}"),
    }
}

/// Scroll clamping: `offset + delta` confined to `0..=content_h - viewport_h`
/// (never negative, no rubber-banding; a content shorter than the viewport
/// yields a max of `0`).
#[must_use]
pub fn clamp_scroll(offset: f32, delta: f32, viewport_h: f32, content_h: f32) -> f32 {
    let max = (content_h - viewport_h).max(0.0);
    (offset + delta).clamp(0.0, max)
}

// ---------------------------------------------------------------------------
// Pure handlers (systems are thin glue)
// ---------------------------------------------------------------------------

/// Open the mode list from the Title screen (and, from T9 on, from the
/// result screen's "Menu" button). Un-freezes the sim like `goto_title`.
pub fn open_mode_select(state: &mut AppState, sim: &mut SimPaused, freeze: &JuiceFreeze) {
    release_sim(sim, freeze);
    *state = AppState::ModeSelect;
}

/// Row press: release any held pause, then start `id` through the shared
/// T5 mode-aware path (seed resolution, pre-roll re-arm, play bump). The
/// state flip to `Playing` hides this overlay.
pub fn start_mode_row(
    id: ModeId,
    core: &mut GameCore,
    countdown: &mut Countdown,
    state: &mut AppState,
    sim: &mut SimPaused,
    freeze: &JuiceFreeze,
    records: Option<&mut Records>,
) -> u64 {
    release_sim(sim, freeze);
    start_mode_run(id, core, countdown, state, records)
}

// ---------------------------------------------------------------------------
// UI markers
// ---------------------------------------------------------------------------

/// Root of the mode select screen; visible only in [`AppState::ModeSelect`].
#[derive(Component)]
pub struct ModeSelectRoot;

/// One mode row; the [`ModeId`] it starts.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeRowButton {
    /// Catalogue id started by pressing this row.
    pub id: ModeId,
}

/// "Back" button on the mode list → [`AppState::Title`].
#[derive(Component)]
pub struct ModeBackButton;

/// The Daily Challenge banner row (T17) — deliberately **not** a
/// [`ModeRowButton`]: the row list stays data-driven from the shipped
/// catalogue (Daily is not a play-mode row), while this one row shows
/// today's rotation + status and starts *today's* mode with *today's* seed.
#[derive(Component)]
pub struct DailyRowButton;

/// One mutator toggle on the mode list (T23). A press flips exactly one bit
/// of [`GameCore::selected_mutators`](crate::core_bridge::GameCore); the
/// next started run snapshots the selection
/// (`GameCore::start_mode` → `ActiveMode::mutators`), so toggling never
/// affects a live run. The buttons are spawned from
/// [`Mutators::SELECTABLE`](crate::mutators::Mutators) in fixed order;
/// [`sync_mutator_toggle_labels`] keeps their ON/OFF text on the selection
/// (which persists across screen navigation within the session, never
/// across app restarts).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutatorToggleButton {
    /// The single mutator bit this button toggles.
    pub bit: crate::mutators::Mutators,
}

/// Toggle button text: `NO HOLD OFF` / `20G ON`.
#[must_use]
pub fn mutator_toggle_text(
    bit: crate::mutators::Mutators,
    selected: crate::mutators::Mutators,
) -> String {
    let state = if selected.contains(bit) { "ON" } else { "OFF" };
    format!("{} {state}", bit.label())
}

/// The overflow-scrolling viewport around the row list
/// ([`mode_scroll_system`] drives its `ScrollPosition`).
#[derive(Component)]
pub struct ModeScrollViewport;

/// A row's record label, rewritten from [`Records`] on change.
#[derive(Component, Debug, Clone, Copy)]
pub struct ModeRecordLabel {
    /// The mode whose record this label shows.
    pub id: ModeId,
}

// ---------------------------------------------------------------------------
// UI construction
// ---------------------------------------------------------------------------

/// Landscape row metrics: (row min-height, name, description, record px).
const ROW_H: f32 = 92.0;
const ROW_NAME_PX: f32 = 22.0;
const ROW_DESC_PX: f32 = 13.0;
const ROW_REC_PX: f32 = 14.0;

/// Portrait-native row metrics (thumb-friendly, readable at arm's length).
const ROW_H_PORTRAIT: f32 = 104.0;
const ROW_NAME_PX_PORTRAIT: f32 = 24.0;
const ROW_DESC_PX_PORTRAIT: f32 = 15.0;
const ROW_REC_PX_PORTRAIT: f32 = 16.0;

/// Pixels one mouse-wheel notch scrolls.
const WHEEL_STEP: f32 = 90.0;

/// Mutator toggle metrics (T23): compact but thumb-friendly; portrait gets
/// the taller label targets.
const MUT_TOGGLE_H: f32 = 44.0;
const MUT_TOGGLE_H_PORTRAIT: f32 = 52.0;
const MUT_TOGGLE_PX: f32 = 14.0;
const MUT_TOGGLE_PX_PORTRAIT: f32 = 16.0;

/// Spawn one row per mode in `ids` under `parent` (landscape metrics — the
/// public entry the tests drive; `build_mode_select_ui` uses the sized
/// variant). Renderer over the catalogue, never a fixed menu.
pub fn spawn_mode_rows(parent: &mut ChildSpawnerCommands, ids: &[ModeId], records: &Records) {
    spawn_mode_rows_sized(
        parent,
        ids,
        records,
        ROW_H,
        ROW_NAME_PX,
        ROW_DESC_PX,
        ROW_REC_PX,
    );
}

#[allow(clippy::too_many_arguments)]
fn spawn_mode_rows_sized(
    parent: &mut ChildSpawnerCommands,
    ids: &[ModeId],
    records: &Records,
    row_h: f32,
    name_px: f32,
    desc_px: f32,
    rec_px: f32,
) {
    for id in ids {
        let record = record_line(records.record_for(mode_key(*id)));
        parent
            .spawn((
                Button,
                ModeRowButton { id: *id },
                BackgroundColor(BUTTON_BG),
                Node {
                    width: Val::Percent(100.0),
                    min_height: Val::Px(row_h),
                    flex_direction: FlexDirection::Column,
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Start,
                    row_gap: Val::Px(2.0),
                    padding: UiRect::axes(Val::Px(16.0), Val::Px(10.0)),
                    ..default()
                },
            ))
            .with_children(|row| {
                row.spawn(label_node(display_name(*id).to_string(), name_px));
                row.spawn(label_node(description(*id).to_string(), desc_px));
                row.spawn((
                    ModeRecordLabel { id: *id },
                    Text::new(record),
                    TextFont::from_font_size(rec_px),
                    TextColor(RECORD_COLOR),
                    Pickable::IGNORE,
                ));
            });
    }
}

fn build_mode_select_ui(
    mut commands: Commands,
    windows: Query<&Window>,
    records: Res<Records>,
    core: Option<NonSend<'_, GameCore>>,
) {
    let portrait = windows
        .iter()
        .next()
        .is_some_and(|w| render::portrait_layout(w.resolution.size().x, w.resolution.size().y));
    let (row_h, name_px, desc_px, rec_px) = if portrait {
        (
            ROW_H_PORTRAIT,
            ROW_NAME_PX_PORTRAIT,
            ROW_DESC_PX_PORTRAIT,
            ROW_REC_PX_PORTRAIT,
        )
    } else {
        (ROW_H, ROW_NAME_PX, ROW_DESC_PX, ROW_REC_PX)
    };
    // T23: seed the toggle labels from the session's pending mutator
    // selection; [`sync_mutator_toggle_labels`] keeps them current after.
    let selected_mutators = core.map(|core| core.selected_mutators).unwrap_or_default();

    commands
        .spawn((
            ModeSelectRoot,
            Visibility::Hidden,
            // Containers are inert (house picking discipline).
            Pickable::IGNORE,
            BackgroundColor(PANEL_BG),
            Node {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Start,
                align_items: AlignItems::Center,
                row_gap: Val::Px(10.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn(label_node("SELECT MODE".to_string(), 40.0));
            // Daily Challenge banner (T17): shows today's rotation slot and
            // status (`Daily · Dig — Not yet` / `— 1:42.35`); a press starts
            // TODAY'S mode with TODAY'S seed (daily::start_daily), so it is
            // its own marker component — never a catalogue ModeRowButton.
            root.spawn((
                Button,
                DailyRowButton,
                BackgroundColor(BUTTON_BG),
                Node {
                    width: Val::Px(560.0),
                    max_width: Val::Percent(94.0),
                    min_height: Val::Px(52.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                Text::new(daily::banner_text(
                    records.record_for(crate::records::DAILY),
                )),
                TextFont::from_font_size(18.0),
                TextColor(RECORD_COLOR),
            ));
            // Mutator toggles (T23): the session's selection on GameCore
            // (never persisted), snapshotted into the run by the shared
            // start path. Fixed order from `Mutators::SELECTABLE`, wrapping
            // on portrait widths; INVISIBLE stays reserved for T24.
            root.spawn((
                Node {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    column_gap: Val::Px(8.0),
                    row_gap: Val::Px(6.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                Pickable::IGNORE,
            ))
            .with_children(|row| {
                for bit in crate::mutators::Mutators::SELECTABLE {
                    row.spawn((
                        Button,
                        MutatorToggleButton { bit },
                        BackgroundColor(BUTTON_BG),
                        Node {
                            min_height: Val::Px(if portrait {
                                MUT_TOGGLE_H_PORTRAIT
                            } else {
                                MUT_TOGGLE_H
                            }),
                            padding: UiRect::axes(Val::Px(12.0), Val::Px(8.0)),
                            justify_content: JustifyContent::Center,
                            align_items: AlignItems::Center,
                            ..default()
                        },
                        Text::new(mutator_toggle_text(bit, selected_mutators)),
                        TextFont::from_font_size(if portrait {
                            MUT_TOGGLE_PX_PORTRAIT
                        } else {
                            MUT_TOGGLE_PX
                        }),
                    ));
                }
            });
            // Native scroll viewport: clips its overflow and takes
            // `ScrollPosition` input; rows outside the clip never catch a
            // tap (bevy_ui `ui_focus_system` honors overflow clipping).
            root.spawn((
                ModeScrollViewport,
                ScrollPosition::DEFAULT,
                Pickable::IGNORE,
                Node {
                    display: Display::Flex,
                    flex_direction: FlexDirection::Column,
                    flex_grow: 1.0,
                    width: Val::Percent(100.0),
                    justify_content: JustifyContent::Start,
                    align_items: AlignItems::Center,
                    overflow: Overflow {
                        x: OverflowAxis::Clip,
                        y: OverflowAxis::Scroll,
                    },
                    ..default()
                },
            ))
            .with_children(|viewport| {
                viewport
                    .spawn((
                        Node {
                            display: Display::Flex,
                            flex_direction: FlexDirection::Column,
                            width: Val::Px(560.0),
                            max_width: Val::Percent(94.0),
                            row_gap: Val::Px(10.0),
                            justify_content: JustifyContent::Start,
                            align_items: AlignItems::Stretch,
                            ..default()
                        },
                        Pickable::IGNORE,
                    ))
                    .with_children(|list| {
                        spawn_mode_rows_sized(
                            list,
                            &shipped_modes(),
                            &records,
                            row_h,
                            name_px,
                            desc_px,
                            rec_px,
                        );
                    });
            });
            menu_button(root, "Back", ModeBackButton);
        });
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

fn sync_mode_select_visibility(
    state: Res<AppState>,
    flow: Res<VersusFlow>,
    mut roots: Query<&mut Visibility, With<ModeSelectRoot>>,
) {
    // The ladder screen (T16) rides inside `AppState::ModeSelect`; when its
    // flow marker is set the list itself hides behind it (the ladder root's
    // own sync in `screens_ladder` shows on the inverse condition).
    let wanted = if *state == AppState::ModeSelect && flow.ladder == LadderOrigin::Closed {
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

/// Rewrite every row's record label whenever [`Records`] moves (a finished
/// run's record shows up on the next visit without rebuilding the list).
fn sync_mode_record_labels(
    records: Res<Records>,
    mut labels: Query<(&ModeRecordLabel, &mut Text)>,
) {
    if !records.is_changed() {
        return;
    }
    for (label, mut text) in &mut labels {
        let line = record_line(records.record_for(mode_key(label.id)));
        if text.0 != line {
            *text = Text::new(line);
        }
    }
}

/// Rewrite the Daily banner (T17) whenever [`Records`] moves (the finished
/// run's result replaces "Not yet") or [`AppState`] moves (a fresh visit
/// re-reads "today", so crossing UTC midnight needs no timer).
fn sync_daily_banner(
    records: Res<Records>,
    state: Res<AppState>,
    mut banners: Query<&mut Text, With<DailyRowButton>>,
) {
    if !records.is_changed() && !state.is_changed() {
        return;
    }
    let line = daily::banner_text(records.record_for(crate::records::DAILY));
    for mut text in &mut banners {
        if text.0 != line {
            *text = Text::new(line.clone());
        }
    }
}

/// Keep the four mutator toggle labels (T23) on the pending selection while
/// the list is open (also after a revisit, since the selection persists
/// across navigation within the session). Writes only on an actual text
/// change; runs after [`mode_select_clicks`] in the chain, so a press
/// updates its own label the same frame.
fn sync_mutator_toggle_labels(
    state: Res<AppState>,
    core: Option<NonSend<GameCore>>,
    mut toggles: Query<(&MutatorToggleButton, &mut Text)>,
) {
    if *state != AppState::ModeSelect {
        return;
    }
    let selected = core.map(|core| core.selected_mutators).unwrap_or_default();
    for (toggle, mut text) in &mut toggles {
        let want = mutator_toggle_text(toggle.bit, selected);
        if text.0 != want {
            *text = Text::new(want);
        }
    }
}

/// Row / Back / mutator-toggle clicks while the list is open.
type ModeSelectClickQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Interaction,
        Option<&'static ModeRowButton>,
        Has<ModeBackButton>,
        Has<DailyRowButton>,
        Option<&'static MutatorToggleButton>,
    ),
    (With<Button>, Changed<Interaction>),
>;

#[derive(SystemParam)]
struct ModeSelectClickParams<'w, 's> {
    buttons: ModeSelectClickQuery<'w, 's>,
    core: NonSendMut<'w, GameCore>,
    countdown: ResMut<'w, Countdown>,
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    records: Option<ResMut<'w, Records>>,
    flow: ResMut<'w, VersusFlow>,
}

fn mode_select_clicks(mut params: ModeSelectClickParams) {
    if *params.state != AppState::ModeSelect {
        return;
    }
    for (_entity, interaction, row, back, daily_row, toggle) in params.buttons.iter() {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Some(toggle) = toggle {
            // T23: flip this bit of the pending selection; the label sync
            // system rewrites the text in the same frame (later in the
            // chain). Never starts a run, never leaves the screen.
            params.core.selected_mutators.toggle(toggle.bit);
            return;
        }
        if let Some(row) = row {
            // Bot Ladder (T16) is not a solo bridge run: its row opens the
            // ladder screen (a ModeSelect-internal flow on `VersusFlow`)
            // instead of starting a `Game`.
            if row.id == ModeId::BotLadder {
                crate::screens_ladder::open_ladder(&mut params.state, &mut params.flow);
                return;
            }
            // T17: a normal row is never a daily attempt — clearing the
            // marker here is what keeps the three underlying modes fully
            // independent of the daily record.
            params.flow.daily = DailyAttempt::Idle;
            start_mode_row(
                row.id,
                params.core.as_mut(),
                params.countdown.as_mut(),
                &mut params.state,
                &mut params.sim,
                &params.freeze,
                params.records.as_deref_mut(),
            );
            return;
        } else if back {
            goto_title(&mut params.state, &mut params.sim, &params.freeze);
            return;
        } else if daily_row {
            // The Daily banner press: today's mode, today's seed (the
            // forced seed wins over TETRIS_SEED — everyone plays the same
            // board), and the run is flagged daily on the flow marker.
            release_sim(&mut params.sim, &params.freeze);
            let (date, _mode, _seed) = daily::start_daily(
                params.core.as_mut(),
                params.countdown.as_mut(),
                &mut params.state,
                params.records.as_deref_mut(),
            );
            params.flow.daily = DailyAttempt::Active { date };
            return;
        }
    }
}

/// Keyboard: Escape walks back to the Title (mirroring the versus submenus;
/// the pause chord is inert on this screen) and Enter activates the first
/// row. No focus system is invented — the list's only keyboard contract.
#[derive(SystemParam)]
struct ModeSelectKeyParams<'w> {
    keys: Option<Res<'w, ButtonInput<KeyCode>>>,
    capture: Res<'w, RebindingCapture>,
    state: ResMut<'w, AppState>,
    sim: ResMut<'w, SimPaused>,
    freeze: Res<'w, JuiceFreeze>,
    core: NonSendMut<'w, GameCore>,
    countdown: ResMut<'w, Countdown>,
    records: Option<ResMut<'w, Records>>,
    flow: ResMut<'w, VersusFlow>,
}

fn mode_select_key_system(mut params: ModeSelectKeyParams) {
    if params.capture.capturing || *params.state != AppState::ModeSelect {
        return;
    }
    let Some(keys) = params.keys else { return };
    if params.flow.ladder != LadderOrigin::Closed {
        // Ladder screen (T16) shows instead of the list: Escape walks it
        // back to the list; the row keys belong to the ladder's own click
        // system, so Enter is inert here.
        if keys.just_pressed(KeyCode::Escape) {
            params.flow.ladder = LadderOrigin::Closed;
        }
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        goto_title(&mut params.state, &mut params.sim, &params.freeze);
    } else if keys.just_pressed(KeyCode::Enter) {
        let Some(first) = shipped_modes().first().copied() else {
            return;
        };
        // T17: Enter starts the first row as a plain run — never a daily
        // attempt.
        params.flow.daily = DailyAttempt::Idle;
        start_mode_row(
            first,
            params.core.as_mut(),
            params.countdown.as_mut(),
            &mut params.state,
            &mut params.sim,
            &params.freeze,
            params.records.as_deref_mut(),
        );
    }
}

/// Wheel + touch-drag scrolling of [`ModeScrollViewport`] while the list is
/// open. Wheel: one notch = [`WHEEL_STEP`] px (wheel-up scrolls toward the
/// top). Touch: a vertical drag moves the content with the finger (screen
/// y grows downward, so dragging down reduces the offset). Clamped by
/// [`clamp_scroll`] against the viewport's laid-out `ComputedNode` sizes —
/// before layout, or while the content fits, nothing moves.
fn mode_scroll_system(
    state: Res<AppState>,
    mut wheels: MessageReader<MouseWheel>,
    mut touches: MessageReader<TouchInput>,
    mut track: Local<Option<(u64, f32)>>,
    mut viewports: Query<(&mut ScrollPosition, &ComputedNode), With<ModeScrollViewport>>,
) {
    if *state != AppState::ModeSelect {
        // Keep the cursors current so stale input never lands on a later
        // open of the screen.
        wheels.read().for_each(drop);
        touches.read().for_each(drop);
        *track = None;
        return;
    }
    let Ok((mut scroll, computed)) = viewports.single_mut() else {
        return;
    };
    let mut delta = 0.0f32;
    for m in wheels.read() {
        delta += -m.y * WHEEL_STEP;
    }
    for message in touches.read() {
        match message.phase {
            TouchPhase::Started => *track = Some((message.id, message.position.y)),
            TouchPhase::Moved => {
                if let Some((id, last)) = track.as_mut() {
                    if *id == message.id {
                        delta += *last - message.position.y;
                        *last = message.position.y;
                    }
                }
            }
            TouchPhase::Ended | TouchPhase::Canceled => {
                if track.is_some_and(|(id, _)| id == message.id) {
                    *track = None;
                }
            }
        }
    }
    if delta == 0.0 {
        return;
    }
    let next = clamp_scroll(scroll.0.y, delta, computed.size.y, computed.content_size.y);
    if next != scroll.0.y {
        scroll.0 = Vec2::new(scroll.0.x, next);
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// The T7 mode select screen. Mount alongside [`MenuScreensPlugin`](crate::
/// screens_menu::MenuScreensPlugin) (the shared hidden-UI picking discipline
/// and the Title screen live there).
pub struct ModeSelectPlugin;

impl Plugin for ModeSelectPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits (screens_menu precedent): no-ops when the owning
        // plugin already registered the resource, but every handler
        // parameter stays satisfied in headless `MinimalPlugins` tests.
        app.init_resource::<AppState>()
            .init_resource::<SimPaused>()
            .init_resource::<JuiceFreeze>()
            .init_resource::<RebindingCapture>()
            .init_resource::<Records>()
            // T16: the ladder flow marker lives on the title flow resource;
            // the row routing and the list-hiding gate read it (defensive
            // init — no-op when MenuScreensPlugin already registered it).
            .init_resource::<VersusFlow>();
        if !app.world().contains_resource::<Countdown>() {
            app.init_resource::<Countdown>();
        }
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        if !app.world().contains_resource::<Messages<MouseWheel>>() {
            app.add_message::<MouseWheel>();
        }
        if !app.world().contains_resource::<Messages<TouchInput>>() {
            app.add_message::<TouchInput>();
        }
        app.add_systems(Startup, build_mode_select_ui).add_systems(
            Update,
            // Input first (a press transitions the same frame), then the
            // scroll pass, then the visibility/label syncs observing it.
            (
                mode_select_key_system,
                mode_select_clicks,
                sync_mutator_toggle_labels,
                mode_scroll_system,
                sync_mode_select_visibility,
                sync_mode_record_labels,
                sync_daily_banner,
            )
                .chain(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::app::App;
    use bevy::ecs::relationship::Relationship;
    use bevy::input::mouse::MouseWheel;
    use bevy::input::touch::{TouchInput, TouchPhase};
    use bevy::window::{Window, WindowPlugin};

    use crate::core_bridge::{Countdown, GameCore, SimPaused};
    use crate::modes::{is_shipped, ModeId};
    use crate::records::{Record, Records, SPRINT};
    use crate::screens_menu::{MenuScreensPlugin, PauseRoot, StartButton, TitleRoot};
    use crate::state::AppState;

    fn mode_select_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t7 headless".into(),
                resolution: (1280, 720).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            crate::core_bridge::CoreBridgePlugin,
            crate::hud::HudPlugin,
            crate::input::InputPlugin,
            MenuScreensPlugin,
            ModeSelectPlugin,
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

    fn press_key(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(key);
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

    fn row_ids(app: &mut App) -> Vec<ModeId> {
        let world = app.world_mut();
        let mut rows = world.query_filtered::<&ModeRowButton, With<Button>>();
        rows.iter(world).map(|row| row.id).collect()
    }

    #[test]
    fn title_start_opens_mode_select_and_back_returns_to_title() {
        let mut app = mode_select_test_app();
        set_state(&mut app, AppState::Title);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
        assert_eq!(vis_of::<ModeSelectRoot>(&mut app), Visibility::Hidden);

        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        app.update(); // settle frame (cross-plugin Update order is free)
        assert_eq!(vis_of::<ModeSelectRoot>(&mut app), Visibility::Visible);
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Hidden);

        click_button_under(
            &mut app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| world.get::<ModeBackButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::Title);
        app.update();
        assert_eq!(vis_of::<TitleRoot>(&mut app), Visibility::Visible);
    }

    #[test]
    fn sprint_row_click_starts_sprint_with_preroll_and_bumps_plays() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::Title);
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        assert_eq!(app.world().resource::<Records>().plays(SPRINT), 0);

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
        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.active_mode.id, ModeId::Sprint);
        assert_eq!(app.world().resource::<Countdown>().0, 180, "pre-roll armed");
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        assert_eq!(app.world().resource::<Records>().plays(SPRINT), 1);
        assert_eq!(vis_of::<ModeSelectRoot>(&mut app), Visibility::Hidden);

        // The pre-roll actually gates stepping (T5 contract).
        for _ in 0..60 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        assert_eq!(app.world().non_send::<GameCore>().steps, 0);
        assert_eq!(app.world().resource::<Countdown>().0, 120);
    }

    #[test]
    fn marathon_row_click_starts_marathon_without_preroll() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);
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
        assert_eq!(core.active_mode.id, ModeId::Marathon);
        assert_eq!(app.world().resource::<Countdown>().0, 0);
    }

    #[test]
    fn rows_are_data_driven_from_the_shipped_catalogue_flag() {
        let mut app = mode_select_test_app();
        let expected: Vec<ModeId> = ModeId::ALL
            .iter()
            .copied()
            .filter(|id| is_shipped(*id))
            .collect();
        assert_eq!(expected[0], ModeId::Marathon, "Marathon first");
        assert_eq!(shipped_modes(), expected);
        assert_eq!(row_ids(&mut app), expected);
        for id in ModeId::ALL {
            if !is_shipped(id) {
                assert!(
                    !row_ids(&mut app).contains(&id),
                    "{id:?} must not be listed"
                );
            }
        }

        // The row renderer itself is catalogue-driven, not a fixed menu:
        // feeding it a hypothetical catalogue (a flipped `is_shipped`, e.g.
        // DigDuel in R3) renders exactly those rows. T17 note: the
        // hypothetical is no longer Daily — Daily ships as its own banner
        // row (never a catalogue ModeRowButton); BotLadder (T16) shipped
        // before it.
        let records = Records::default();
        {
            let mut cx = app.world_mut().commands();
            cx.spawn(Node::default()).with_children(|parent| {
                spawn_mode_rows(parent, &[ModeId::Marathon, ModeId::DigDuel], &records);
            });
        }
        app.update();
        let ids = row_ids(&mut app);
        assert_eq!(ids.iter().filter(|id| **id == ModeId::DigDuel).count(), 1);
        assert_eq!(
            ids.len(),
            expected.len() + 2,
            "renderer emitted exactly the two requested rows"
        );
        // T17: Daily is deliberately NOT shipped as a play-mode row — the
        // list above must contain no Daily row while the banner (separate
        // marker component) exists exactly once.
        assert!(
            !ids.contains(&ModeId::Daily),
            "Daily is a banner, not a row"
        );
        let daily_banners = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<Entity, With<DailyRowButton>>();
            q.iter(world).count()
        };
        assert_eq!(daily_banners, 1, "exactly one Daily banner row");
    }

    #[test]
    fn record_line_formats_from_records() {
        assert_eq!(record_line(None), "-");
        assert_eq!(
            record_line(Some(&Record::BestTime { ticks: 9835 })),
            "Best 2:43.91"
        );
        assert_eq!(
            record_line(Some(&Record::BestScore {
                score: 123456,
                level: 5,
                lines: 40
            })),
            "Best 123 456"
        );
        assert_eq!(
            record_line(Some(&Record::HighestRung { rung: 4 })),
            "Rung 4"
        );
        assert_eq!(
            record_line(Some(&Record::LifetimeLines { total: 5000 })),
            "Lines 5 000"
        );
        assert_eq!(
            record_line(Some(&Record::Daily {
                date: "2026-10-01".to_string(),
                result: "1:00.00".to_string(),
            })),
            "2026-10-01: 1:00.00"
        );
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(123), "123");
        assert_eq!(group_thousands(1234), "1 234");
        assert_eq!(group_thousands(1234567), "1 234 567");
    }

    #[test]
    fn row_record_labels_render_and_update_from_records() {
        let mut app = mode_select_test_app();
        let mut records = Records::default();
        records.record_run(SPRINT, Record::BestTime { ticks: 9835 });
        app.insert_resource(records);
        set_state(&mut app, AppState::ModeSelect);

        let sprint_record = |world: &World, e: Entity| {
            world
                .get::<ModeRecordLabel>(e)
                .is_some_and(|l| l.id == ModeId::Sprint)
        };
        assert_eq!(text_of(&mut app, sprint_record), "Best 2:43.91");
        // Modes without a record show the dash placeholder.
        let marathon_record = |world: &World, e: Entity| {
            world
                .get::<ModeRecordLabel>(e)
                .is_some_and(|l| l.id == ModeId::Marathon)
        };
        assert_eq!(text_of(&mut app, marathon_record), "-");

        // Live record improvement updates the row without reopening.
        app.world_mut().resource_mut::<Records>().record_run(
            crate::records::ULTRA,
            Record::BestScore {
                score: 123456,
                level: 2,
                lines: 10,
            },
        );
        app.update();
        let ultra_record = |world: &World, e: Entity| {
            world
                .get::<ModeRecordLabel>(e)
                .is_some_and(|l| l.id == ModeId::Ultra)
        };
        assert_eq!(text_of(&mut app, ultra_record), "Best 123 456");
    }

    #[test]
    fn keyboard_esc_and_enter_work_on_the_list() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());

        set_state(&mut app, AppState::ModeSelect);
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(app_state(&app), AppState::Title, "Esc walks back to Title");
        assert_ne!(app_state(&app), AppState::Paused, "no pause leak");

        set_state(&mut app, AppState::ModeSelect);
        press_key(&mut app, KeyCode::Enter);
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            *shipped_modes().first().expect("shipped list non-empty"),
            "Enter activates the first row"
        );
    }

    #[test]
    fn scroll_clamp_math_is_pure() {
        // viewport 200, content 500 => max scroll 300.
        assert_eq!(clamp_scroll(0.0, 1000.0, 200.0, 500.0), 300.0);
        assert_eq!(clamp_scroll(300.0, -1000.0, 200.0, 500.0), 0.0);
        assert_eq!(clamp_scroll(120.0, 60.0, 200.0, 500.0), 180.0);
        // Content shorter than the viewport never scrolls.
        assert_eq!(clamp_scroll(0.0, 120.0, 500.0, 200.0), 0.0);
    }

    #[test]
    fn mouse_wheel_scrolls_the_mode_select_viewport() {
        // Short window (landscape) so the four R1 rows actually overflow the
        // viewport — layout only exists with the real UI plugin stack.
        let mut app = mode_select_ui_test_app(800, 300);
        set_state(&mut app, AppState::ModeSelect);
        app.update();
        let before = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&ScrollPosition, With<ModeScrollViewport>>();
            q.single(world).expect("viewport exists").0.y
        };
        app.world_mut()
            .resource_mut::<Messages<MouseWheel>>()
            .write(MouseWheel {
                unit: bevy::input::mouse::MouseScrollUnit::Line,
                x: 0.0,
                y: -1.0,
                window: Entity::PLACEHOLDER,
                phase: TouchPhase::Moved,
            });
        app.update();
        let after = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&ScrollPosition, With<ModeScrollViewport>>();
            q.single(world).expect("viewport exists").0.y
        };
        // Down-wheel moves the scroll offset down by exactly one notch
        // (nothing at rest → 90 < max 188). `MessageReader` cursors make
        // each wheel message count exactly once regardless of the
        // per-frame message-buffer swap, so this asserts exact math.
        assert!(
            (after - before - 90.0).abs() < 0.01,
            "wheel-down scrolls one notch: {before} -> {after}"
        );

        // Wheel-up past the top parks at zero, never below.
        for _ in 0..50 {
            app.world_mut()
                .resource_mut::<Messages<MouseWheel>>()
                .write(MouseWheel {
                    unit: bevy::input::mouse::MouseScrollUnit::Line,
                    x: 0.0,
                    y: 1.0,
                    window: Entity::PLACEHOLDER,
                    phase: TouchPhase::Moved,
                });
            app.update();
        }
        let top = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&ScrollPosition, With<ModeScrollViewport>>();
            q.single(world).expect("viewport exists").0.y
        };
        assert_eq!(top, 0.0);
    }

    // ---- touch parity (real picking harness, portrait) ----

    use bevy::camera::visibility::InheritedVisibility;
    use bevy::ui::UiPlugin;
    use bevy::window::PrimaryWindow;

    #[derive(Resource)]
    struct UiWin(Entity);

    fn mode_select_ui_test_app(width: u32, height: u32) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris t7 touch".into(),
                resolution: (width, height).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            crate::core_bridge::CoreBridgePlugin,
            crate::hud::HudPlugin,
            crate::input::InputPlugin,
            MenuScreensPlugin,
            ModeSelectPlugin,
            crate::screens_settings::SettingsScreenPlugin,
        ));
        app.add_plugins(bevy::asset::AssetPlugin::default());
        app.init_asset::<Image>();
        app.init_asset::<bevy::image::TextureAtlasLayout>();
        app.add_plugins(bevy::input::InputPlugin);
        app.add_plugins(bevy::text::TextPlugin);
        app.add_plugins(UiPlugin);
        app.add_plugins(bevy::picking::DefaultPickingPlugins);
        app.world_mut().spawn((
            Camera2d,
            bevy::ui::IsDefaultUiCamera,
            Camera {
                viewport: Some(bevy::camera::Viewport {
                    physical_size: UVec2::new(width, height),
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

    /// Center of the first `ModeRowButton` (or the Back button) inside the
    /// mode-select root, in window-logical coords.
    fn tap_target(app: &mut App, want: Option<ModeId>) -> Vec2 {
        let world = app.world_mut();
        let mut q = world.query::<(
            &UiGlobalTransform,
            Option<&ModeRowButton>,
            Option<&ModeBackButton>,
        )>();
        let (t, ..) = q
            .iter(world)
            .find(|(_, row, back)| match want {
                Some(id) => row.is_some_and(|r| r.id == id),
                None => back.is_some(),
            })
            .expect("target button laid out");
        t.translation.xy()
    }

    /// One touch frame, exactly as the winit provider feeds the desktop
    /// stack: `Messages<TouchInput>` is what `bevy_input`'s
    /// `touch_screen_input_system` folds into `Touches`, and `ui_focus_system`
    /// presses buttons from `Touches::any_just_pressed` (the phone pipeline).
    fn touch(app: &mut App, phase: TouchPhase, at: Vec2) {
        let win = app.world().resource::<UiWin>().0;
        app.world_mut()
            .resource_mut::<Messages<TouchInput>>()
            .write(TouchInput {
                phase,
                position: at,
                window: win,
                force: None,
                id: 1,
            });
        app.update();
    }

    #[test]
    fn touch_tap_starts_a_mode_in_portrait() {
        crate::render::set_portrait_override(Some(true));
        let mut app = mode_select_ui_test_app(411, 731);
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);
        app.update();

        let dig = tap_target(&mut app, Some(ModeId::Dig));
        touch(&mut app, TouchPhase::Started, dig);
        assert_eq!(app_state(&app), AppState::Playing, "touch tap starts Dig");
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.id,
            ModeId::Dig
        );
        touch(&mut app, TouchPhase::Ended, dig);
        crate::render::set_portrait_override(None);
    }

    #[test]
    fn touch_tap_back_returns_to_title_in_portrait() {
        crate::render::set_portrait_override(Some(true));
        let mut app = mode_select_ui_test_app(411, 731);
        set_state(&mut app, AppState::ModeSelect);
        app.update();

        let back = tap_target(&mut app, None);
        touch(&mut app, TouchPhase::Started, back);
        assert_eq!(
            app_state(&app),
            AppState::Title,
            "touch tap on Back returns"
        );
        touch(&mut app, TouchPhase::Ended, back);
        crate::render::set_portrait_override(None);
    }

    #[test]
    fn hidden_mode_select_buttons_never_swallow_title_clicks() {
        // The mode-select root spawns a full-screen panel + buttons at app
        // boot; while Title is up they must be hidden AND inert.
        let mut app = mode_select_test_app();
        set_state(&mut app, AppState::Title);
        let world = app.world_mut();
        let mut q = world.query_filtered::<&Visibility, With<ModeSelectRoot>>();
        assert_eq!(*q.single(world).unwrap(), Visibility::Hidden);
        // Clicking Start (not a row) still lands on Title actions: state
        // moved to ModeSelect, not silently eaten by the hidden root.
        click_button_under(
            &mut app,
            |world, e| world.get::<TitleRoot>(e).is_some(),
            |world, e| world.get::<StartButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::ModeSelect);
        let _ = PauseRoot; // keep the import honest: pause state is untouched
        assert_eq!(app_state(&app), AppState::ModeSelect);
    }

    // ---- T17: Daily Challenge banner ----

    use crate::daily::{self, CivilDate, DailyAttempt};

    /// 2026-10-01 — a Thursday, so the rotation resolves to Sprint.
    fn daily_test_today() -> CivilDate {
        CivilDate::from_ymd(2026, 10, 1)
    }

    fn daily_banner_text(app: &mut App) -> String {
        text_of(app, |world, e| world.get::<DailyRowButton>(e).is_some())
    }

    #[test]
    fn daily_banner_shows_todays_mode_then_the_stored_result() {
        // NOTE: `set_today_override` is thread-local and the scheduled
        // `sync_daily_banner` may run on a scheduler worker thread, where
        // the override is invisible — so pin expectations to `daily::today()`
        // (exactly what the banner reads on this build), not to a fixed date.
        let today = daily::today();
        let mode = crate::modes::display_name(daily::daily_mode(today)).to_string();
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);

        assert_eq!(
            daily_banner_text(&mut app),
            format!("Daily \u{b7} {mode} \u{2014} Not yet"),
            "banner shows today's rotation slot, nothing recorded yet"
        );

        let mut records = app.world().resource::<Records>().clone();
        records.record_run(
            crate::records::DAILY,
            Record::Daily {
                date: today.to_string(),
                result: "1:42.35".to_string(),
            },
        );
        app.insert_resource(records);
        app.update();
        assert_eq!(
            daily_banner_text(&mut app),
            format!("Daily \u{b7} {mode} \u{2014} 1:42.35"),
            "today's stored result replaces the placeholder"
        );
    }

    #[test]
    fn daily_banner_press_starts_todays_mode_with_todays_seed() {
        daily::set_today_override(Some(daily_test_today()));
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);

        click_button_under(
            &mut app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| world.get::<DailyRowButton>(e).is_some(),
        );
        assert_eq!(app_state(&app), AppState::Playing);
        let core = app.world().non_send::<GameCore>();
        assert_eq!(
            core.active_mode.id,
            daily::daily_mode(daily_test_today()),
            "the banner starts TODAY'S rotated mode (Sprint on 2026-10-01)"
        );
        assert_eq!(
            core.seed,
            daily::daily_seed(daily_test_today()),
            "with TODAY'S seed (forced, TETRIS_SEED-proof)"
        );
        assert_eq!(
            app.world().resource::<Countdown>().0,
            crate::modes::pre_roll_ticks(ModeId::Sprint),
            "pre-roll re-armed through the shared start path"
        );
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        assert_eq!(
            app.world().resource::<VersusFlow>().daily,
            DailyAttempt::Active {
                date: daily_test_today()
            },
            "the run is flagged as a daily attempt on the flow marker"
        );
        daily::set_today_override(None);
    }

    #[test]
    fn normal_row_press_is_never_a_daily_attempt() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);
        // A live daily marker (stale from an abandoned run) is cleared by
        // starting a normal row.
        app.world_mut().resource_mut::<VersusFlow>().daily = DailyAttempt::Active {
            date: daily_test_today(),
        };

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
        assert_eq!(
            app.world().resource::<VersusFlow>().daily,
            DailyAttempt::Idle,
            "normal rows never inherit or keep the daily marker"
        );
    }

    // ---- T23: mutator toggles ----

    use crate::mutators::Mutators;

    fn toggle_bits(app: &mut App) -> Vec<Mutators> {
        let world = app.world_mut();
        let mut q = world.query_filtered::<&MutatorToggleButton, With<Button>>();
        q.iter(world).map(|t| t.bit).collect()
    }

    fn selected_mutators(app: &App) -> Mutators {
        app.world().non_send::<GameCore>().selected_mutators
    }

    fn click_mutator(app: &mut App, bit: Mutators) {
        click_button_under(
            app,
            |world, e| world.get::<ModeSelectRoot>(e).is_some(),
            |world, e| {
                world
                    .get::<MutatorToggleButton>(e)
                    .is_some_and(|t| t.bit == bit)
            },
        );
    }

    fn mutator_text(app: &mut App, bit: Mutators) -> String {
        text_of(app, |world, e| {
            world
                .get::<MutatorToggleButton>(e)
                .is_some_and(|t| t.bit == bit)
        })
    }

    #[test]
    fn mutator_toggles_render_in_fixed_order_off_by_default() {
        let mut app = mode_select_test_app();
        set_state(&mut app, AppState::ModeSelect);
        assert_eq!(
            toggle_bits(&mut app),
            Mutators::SELECTABLE.to_vec(),
            "toggles spawn from SELECTABLE in fixed left-to-right order"
        );
        for bit in Mutators::SELECTABLE {
            assert_eq!(
                mutator_text(&mut app, bit),
                mutator_toggle_text(bit, Mutators::empty()),
                "{bit:?} starts OFF"
            );
        }
        assert_eq!(mutator_text(&mut app, Mutators::NO_HOLD), "NO HOLD OFF");
    }

    #[test]
    fn clicking_a_toggle_flips_only_that_bit_and_stays_on_the_screen() {
        let mut app = mode_select_test_app();
        set_state(&mut app, AppState::ModeSelect);

        click_mutator(&mut app, Mutators::NO_HOLD);
        assert_eq!(app_state(&app), AppState::ModeSelect, "no run started");
        assert_eq!(selected_mutators(&app), Mutators::NO_HOLD);
        assert_eq!(mutator_text(&mut app, Mutators::NO_HOLD), "NO HOLD ON");
        assert_eq!(mutator_text(&mut app, Mutators::TWENTY_G), "20G OFF");

        click_mutator(&mut app, Mutators::TWENTY_G);
        assert_eq!(
            selected_mutators(&app),
            Mutators::NO_HOLD | Mutators::TWENTY_G
        );
        assert_eq!(mutator_text(&mut app, Mutators::TWENTY_G), "20G ON");

        click_mutator(&mut app, Mutators::NO_HOLD);
        assert_eq!(selected_mutators(&app), Mutators::TWENTY_G);
        assert_eq!(mutator_text(&mut app, Mutators::NO_HOLD), "NO HOLD OFF");
    }

    #[test]
    fn mutator_selection_persists_across_navigation_within_the_session() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);
        click_mutator(&mut app, Mutators::NO_GHOST);

        // Leave via a row press (Playing), then revisit the list (Back →
        // Title → Start ≈ a direct state flip here).
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
        assert_eq!(
            app.world().non_send::<GameCore>().active_mode.mutators,
            Mutators::NO_GHOST,
            "the started run snapshotted the selection"
        );

        set_state(&mut app, AppState::ModeSelect);
        assert_eq!(
            selected_mutators(&app),
            Mutators::NO_GHOST,
            "selection persists"
        );
        assert_eq!(mutator_text(&mut app, Mutators::NO_GHOST), "NO GHOST ON");
    }

    #[test]
    fn row_press_snapshots_selection_into_the_run() {
        let mut app = mode_select_test_app();
        app.insert_resource(Records::default());
        set_state(&mut app, AppState::ModeSelect);
        click_mutator(&mut app, Mutators::TWENTY_G);

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
        let core = app.world().non_send::<GameCore>();
        assert_eq!(core.active_mode.id, ModeId::Sprint);
        assert_eq!(core.active_mode.mutators, Mutators::TWENTY_G);
        assert_eq!(
            core.active_mode.config.start_level, 20,
            "20G rides into the mode config through the shared start path"
        );
    }
}
