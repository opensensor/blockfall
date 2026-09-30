//! On-screen touch controls for the Android build.
//!
//! Two decks share one button pipeline. The **portrait-native deck** (the
//! Android build, portrait-locked) is a compact row of round buttons
//! (`CCW CW HOLD DROP`) under the field plus a pause disc up top; movement,
//! rotation and drops are driven by *gestures* on the playfield:
//! tap = rotate CW, drag left/right = shift (repeats with travel), swipe
//! down = soft drop, quick flick down = hard drop. The **landscape deck**
//! (desktop smoke tests via `TETRIS_TOUCH=1`, or a rotated debug window)
//! keeps the full button cluster: move `<> v` bottom-left, actions
//! `CCW CW DROP HOLD` bottom-right, pause top-right. Both decks are built
//! at `Startup`; [`sync_deck_visibility`] shows the one matching the window
//! shape, so a mid-run resize swaps layouts cleanly.
//!
//! Buttons are plain bevy `Button`s (touch and mouse pointer events both
//! reach them) and [`touch_action_system`] advances its own
//! [`ShiftRepeat`]/[`RepeatTimer`] from [`crate::input`] on
//! `FixedPreUpdate`, pushing into [`PendingActions`] (solo) or the local
//! seat's [`VersusActions`] queue — mirroring the local-seat rules
//! [`crate::input`] applies to versus matches. Gesture actions are
//! recognized by [`gesture_system`] (also `FixedPreUpdate`) from
//! [`TouchInput`] messages and join the same queue.
//!
//! Menu-layer rules apply (see the picking-incident docs in
//! [`crate::screens_menu`]): containers and labels carry
//! `Pickable::IGNORE`, and hidden widgets are made unpickable by
//! `sync_hidden_ui_unpickable`, so the overlay can never swallow menu
//! clicks (it is visible only while [`AppState::Playing`]). Gesture starts
//! are additionally restricted to the playfield rectangle, so a touch
//! intended for a deck button or the HUD never rotates the piece. Leaving
//! Paused goes back through the pause menu's own Resume button.
//!
//! On desktop the overlay is hidden unless `TETRIS_TOUCH=1`, which lets the
//! mouse drive the landscape buttons for smoke tests (gestures need real
//! touch, i.e. the Android device). Labels are ASCII because the bundled
//! default font is a Fira Mono subset.

use std::collections::HashMap;

use bevy::input::touch::{TouchInput, TouchPhase};
use bevy::prelude::*;
#[cfg(target_os = "android")]
use bevy::window::AppLifecycle;

use tetris_core::actions::Action;

use crate::core_bridge::net::{NetSession, NetStatus};
use crate::core_bridge::{
    Controller, PendingActions, SimPaused, VersusMatch, VersusWinner, SIM_HZ,
};
use crate::hud::hud_anchor;
use crate::input::{
    soft_drop_period_ticks, ticks_for, RepeatTimer, ShiftDir, ShiftRepeat, VersusActions,
};
use crate::juice::JuiceFreeze;
use crate::render;
use crate::screens_menu::toggle_pause;
use crate::state::{AppState, RebindingCapture, Settings};

/// Fixed-step rate as an integer, for the tick conversions shared with
/// [`crate::input`].
const SIM_HZ_U32: u32 = SIM_HZ as u32;

/// One on-screen button. `Rotate180` is intentionally absent: it is rare
/// enough not to earn screen space, and a connected keyboard still has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Component)]
enum TouchBtn {
    Left,
    Right,
    Soft,
    RotateCcw,
    RotateCw,
    HardDrop,
    Hold,
    Pause,
}

/// Marker for the full-screen overlay root (hidden outside `Playing`).
#[derive(Component)]
struct TouchOverlayRoot;

/// Which window shape a button deck is built for; only the matching deck is
/// visible/pickable at a time (see [`sync_deck_visibility`]).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
enum TouchDeck {
    Landscape,
    Portrait,
}

/// `TETRIS_UI_DEBUG=1` (or any Android build): log the overlay's computed
/// layout once plus every button press / gesture emission — cheap enough to
/// leave on the mobile debug APK, device triage via logcat (`TUI`).
fn ui_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(target_os = "android") || std::env::var_os("TETRIS_UI_DEBUG").is_some_and(|v| v == "1")
    })
}

/// Touch-side edge bookkeeping and repeat machines — the touch analogue of
/// [`crate::input::InputMachine`] for the single local hand touch drives
/// (solo, or the local seat of a versus/net match).
#[derive(Debug, Default, Resource)]
struct TouchMachines {
    prev_pressed: HashMap<TouchBtn, bool>,
    shift: ShiftRepeat,
    soft: RepeatTimer,
}

/// Live gesture tracks keyed by touch id.
#[derive(Debug, Default, Resource)]
struct GestureState {
    tracks: HashMap<u64, GestureTrack>,
    /// Core actions recognized since the last fixed step; drained by
    /// [`touch_action_system`] (registered after [`gesture_system`]).
    emitted: Vec<Action>,
}

/// One finger's recognizer state, positions in **physical** pixels.
#[derive(Debug, Clone, Copy)]
struct GestureTrack {
    start: Vec2,
    prev: Vec2,
    /// Unspent horizontal / vertical travel in physical px.
    acc_x: f32,
    acc_y: f32,
    /// `Time::elapsed_secs()` at Started — flick/tap windows are measured in
    /// real seconds, not Moved-event counts, because touch sampling rate
    /// varies per device (a 240 Hz touch would blow any event budget).
    start_secs: f32,
    mode: GestureMode,
    /// Flick already fired for this touch (one hard drop per swipe).
    flicked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureMode {
    Undecided,
    Horiz,
    Vert,
}

/// Overlay visible on this platform? Desktop stays keyboard-first;
/// `TETRIS_TOUCH=1` opts in for mouse-driven smoke tests.
fn touch_overlay_enabled() -> bool {
    cfg!(target_os = "android") || std::env::var_os("TETRIS_TOUCH").is_some_and(|v| v == "1")
}

/// Turns on-screen button presses into core `Action`s with DAS/ARR handling.
pub struct TouchPlugin;

impl Plugin for TouchPlugin {
    fn build(&self, app: &mut App) {
        if !touch_overlay_enabled() {
            return;
        }
        app.init_resource::<TouchMachines>()
            .init_resource::<GestureState>()
            .add_systems(Startup, build_touch_overlay)
            .add_systems(PreUpdate, (sync_overlay_visibility, sync_deck_visibility))
            .add_systems(Update, (button_feedback, ui_debug_dump))
            // Runs on the same fixed step as `gameplay_input_system` /
            // `versus_input_system`; order among the producers is
            // irrelevant (never a shared machine) and the bridge drains in
            // `FixedUpdate`. Gestures feed the same queue, so recognize
            // first and let the button system drain the list.
            .add_systems(
                FixedPreUpdate,
                (gesture_system, touch_action_system).chain(),
            );
        #[cfg(target_os = "android")]
        app.add_systems(Update, lifecycle_pause_system);
    }
}

/// Pressed alpha over the idle alpha (white, translucent discs).
const BTN_IDLE_ALPHA: f32 = 0.14;
const BTN_PRESSED_ALPHA: f32 = 0.34;

/// Round translucent button at an absolute spot inside the overlay root.
/// `pos` fills the position fields only (`left`/`right` + `top`/`bottom`);
/// the label is `IGNORE` so only the disc is pickable (house discipline).
fn touch_button(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    btn: TouchBtn,
    size: f32,
    pos: Node,
    font_size: f32,
) {
    parent
        .spawn((
            Button,
            btn,
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, BTN_IDLE_ALPHA)),
            Node {
                position_type: PositionType::Absolute,
                width: Val::Px(size),
                height: Val::Px(size),
                left: pos.left,
                right: pos.right,
                top: pos.top,
                bottom: pos.bottom,
                border_radius: BorderRadius::all(Val::Px(size / 2.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|disc| {
            disc.spawn((
                Text::new(label.to_string()),
                TextFont::from_font_size(font_size),
                TextColor::WHITE,
                Pickable::IGNORE,
            ));
        });
}

/// Bottom-right `x` offsets from the screen edge; bottom-left use `left`.
fn right_pos(offset_from_right: f32, bottom: f32) -> Node {
    Node {
        right: Val::Px(offset_from_right),
        bottom: Val::Px(bottom),
        ..default()
    }
}

/// Round translucent disc participating in a parent's flex flow (portrait
/// deck row) instead of being absolutely positioned.
fn touch_button_flow(
    parent: &mut ChildSpawnerCommands,
    label: &str,
    btn: TouchBtn,
    size: f32,
    font_size: f32,
) {
    parent
        .spawn((
            Button,
            btn,
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, BTN_IDLE_ALPHA)),
            Node {
                width: Val::Px(size),
                height: Val::Px(size),
                border_radius: BorderRadius::all(Val::Px(size / 2.0)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|disc| {
            disc.spawn((
                Text::new(label.to_string()),
                TextFont::from_font_size(font_size),
                TextColor::WHITE,
                Pickable::IGNORE,
            ));
        });
}

fn build_touch_overlay(mut commands: Commands) {
    // Both decks are built up front; `sync_deck_visibility` shows only the
    // one matching the window shape. Sizes are logical px, so they stay
    // thumb-sized on high-DPI phones.
    const PAD: f32 = 14.0;
    commands
        .spawn((
            TouchOverlayRoot,
            Visibility::Hidden,
            Pickable::IGNORE,
            Node {
                // Absolute: the overlay must never participate in the window
                // root's flex row (100%-wide relative siblings would shrink
                // it to a fraction of the screen).
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .with_children(|root| {
            // Portrait-native deck: one compact action row centered at the
            // bottom — the playfield gestures cover shift, rotate CW, soft
            // and hard drop, so only the rare actions need discs.
            root.spawn((
                TouchDeck::Portrait,
                Visibility::Hidden,
                Pickable::IGNORE,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
            ))
            .with_children(|deck| {
                deck.spawn(Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(0.0),
                    right: Val::Px(0.0),
                    bottom: Val::Px(PAD),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::Center,
                    column_gap: Val::Px(8.0),
                    ..default()
                })
                .with_children(|row| {
                    touch_button_flow(row, "CCW", TouchBtn::RotateCcw, 64.0, 12.0);
                    touch_button_flow(row, "CW", TouchBtn::RotateCw, 64.0, 12.0);
                    touch_button_flow(row, "HOLD", TouchBtn::Hold, 64.0, 11.0);
                    touch_button_flow(row, "DROP", TouchBtn::HardDrop, 64.0, 11.0);
                });
                deck.spawn((
                    Button,
                    TouchBtn::Pause,
                    BackgroundColor(Color::srgba(1.0, 1.0, 1.0, BTN_IDLE_ALPHA)),
                    Node {
                        position_type: PositionType::Absolute,
                        right: Val::Px(PAD),
                        top: Val::Px(PAD),
                        width: Val::Px(76.0),
                        height: Val::Px(76.0),
                        border_radius: BorderRadius::all(Val::Px(38.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                ))
                .with_children(|disc| {
                    disc.spawn((
                        Text::new("II"),
                        TextFont::from_font_size(20.0),
                        TextColor::WHITE,
                        Pickable::IGNORE,
                    ));
                });
            });

            // Landscape deck (desktop smoke tests / rotated debug windows):
            // move cluster bottom-left, action cluster bottom-right, pause
            // top-right.
            root.spawn((
                TouchDeck::Landscape,
                Pickable::IGNORE,
                Node {
                    position_type: PositionType::Absolute,
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
            ))
            .with_children(|deck| {
                // Move cluster: < > and the soft-drop v.
                touch_button(
                    deck,
                    "<",
                    TouchBtn::Left,
                    118.0,
                    Node {
                        left: Val::Px(PAD),
                        bottom: Val::Px(PAD),
                        ..default()
                    },
                    34.0,
                );
                touch_button(
                    deck,
                    ">",
                    TouchBtn::Right,
                    118.0,
                    Node {
                        left: Val::Px(PAD + 130.0),
                        bottom: Val::Px(PAD),
                        ..default()
                    },
                    34.0,
                );
                touch_button(
                    deck,
                    "v",
                    TouchBtn::Soft,
                    94.0,
                    Node {
                        left: Val::Px(PAD + 262.0),
                        bottom: Val::Px(PAD),
                        ..default()
                    },
                    26.0,
                );
                // Action cluster: CW / CCW row with the big hard drop, hold
                // above the rotation pair.
                touch_button(
                    deck,
                    "DROP",
                    TouchBtn::HardDrop,
                    128.0,
                    right_pos(PAD, PAD),
                    20.0,
                );
                touch_button(
                    deck,
                    "CW",
                    TouchBtn::RotateCw,
                    102.0,
                    right_pos(PAD + 132.0, PAD),
                    20.0,
                );
                touch_button(
                    deck,
                    "CCW",
                    TouchBtn::RotateCcw,
                    102.0,
                    right_pos(PAD + 242.0, PAD),
                    18.0,
                );
                touch_button(
                    deck,
                    "HOLD",
                    TouchBtn::Hold,
                    112.0,
                    right_pos(PAD, PAD + 132.0),
                    18.0,
                );
                // Pause top-right, clear of the HUD.
                touch_button(
                    deck,
                    "II",
                    TouchBtn::Pause,
                    84.0,
                    Node {
                        right: Val::Px(PAD),
                        top: Val::Px(PAD),
                        ..default()
                    },
                    22.0,
                );
            });
        });
}

/// Show only the deck matching the current window shape. The deck being
/// hidden additionally gets its buttons forced to `Pickable::IGNORE`
/// immediately (house picking discipline — never rely on the visibility
/// pass catching up first), and restored to `Pickable::default()` when shown.
fn sync_deck_visibility(
    windows: Query<&Window>,
    mut decks: Query<(&TouchDeck, &mut Visibility, &Children)>,
    all_children: Query<&Children>,
    mut buttons: Query<&mut Pickable, With<TouchBtn>>,
) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let portrait = render::portrait_layout(size.x, size.y);
    for (deck, mut visibility, children) in &mut decks {
        let visible = matches!(*deck, TouchDeck::Portrait) == portrait;
        let wanted = if visible {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *visibility == wanted {
            continue;
        }
        *visibility = wanted;
        let pickable = if visible {
            Pickable::default()
        } else {
            Pickable::IGNORE
        };
        let mut stack: Vec<Entity> = children.iter().collect();
        while let Some(entity) = stack.pop() {
            if let Ok(mut component) = buttons.get_mut(entity) {
                *component = pickable;
            }
            if let Ok(kids) = all_children.get(entity) {
                stack.extend(kids.iter());
            }
        }
    }
}

/// Portrait gesture recognizer: taps rotate CW, horizontal drags shift the
/// piece once per cell of travel, downward swipes soft-drop per cell and a
/// quick flick hard-drops (once per touch). Only active while playing on a
/// portrait-native window; touch starts outside the playfield rectangle
/// (deck, HUD strip, pause disc) never produce gestures.
const TAP_MAX_SECS: f32 = 0.25;
const TAP_MAX_DIST: f32 = 28.0;
const COMMIT_PX: f32 = 18.0;
/// A hard-drop flick must cover 3 cells within this window — i.e. a mean
/// speed of ~37 cells/s. Device calibration: a deliberate soft-scroll
/// (~1 cell/100 ms) and even a brisk constant-velocity 300 ms swipe stay
/// under it; a real flick (~90 px in <60 ms) is comfortably over.
const FLICK_MAX_SECS: f32 = 0.08;

fn gesture_system(
    mut messages: MessageReader<TouchInput>,
    windows: Query<&Window>,
    time: Res<Time>,
    state: Res<AppState>,
    capture: Res<RebindingCapture>,
    mut gestures: ResMut<GestureState>,
) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.resolution.size();
    let active = render::portrait_layout(size.x, size.y) && *state == AppState::Playing;
    let batch: Vec<TouchInput> = messages.read().cloned().collect();
    if !active || capture.capturing {
        gestures.tracks.clear();
        gestures.emitted.clear();
        if ui_debug() && !batch.is_empty() {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            if N.fetch_add(1, Ordering::Relaxed) < 20 {
                info!(
                    "TUI gesture blocked msgs {} playing {:?}",
                    batch.len(),
                    *state
                );
            }
        }
        return;
    }
    // Both touch positions and `resolution.size()` are logical pixels in
    // bevy 0.19 (`WindowResolution::size()` is the logical size) — no scale
    // factor anywhere in this comparison.
    let anchor = hud_anchor(size.x, size.y);
    let cell = anchor.cell;
    // Playfield rectangle in screen coords (y down, physical px).
    let pad = 0.6 * cell;
    let x0 = size.x * 0.5 + anchor.field_left - pad;
    let x1 = size.x * 0.5 + anchor.field_right + pad;
    let y0 = size.y * 0.5 - anchor.field_top - pad;
    let y1 = size.y * 0.5 - anchor.field_bottom + pad;

    let mut out: Vec<Action> = Vec::new();
    if ui_debug() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ONCE: AtomicU32 = AtomicU32::new(0);
        if ONCE.swap(1, Ordering::Relaxed) == 0 {
            info!(
                "TUI field rect [{x0:.0} {y0:.0}]-[{x1:.0} {y1:.0}] cell {cell:.1} win {:?}",
                size
            );
        }
    }
    for message in batch.iter() {
        let position = message.position;
        match message.phase {
            TouchPhase::Started => {
                let in_field =
                    position.x >= x0 && position.x <= x1 && position.y >= y0 && position.y <= y1;
                if ui_debug() {
                    info!(
                        "TUI raw start {:?} scaled {position:?} in_field {in_field}",
                        message.position
                    );
                }
                if in_field {
                    gestures.tracks.insert(
                        message.id,
                        GestureTrack {
                            start: position,
                            prev: position,
                            acc_x: 0.0,
                            acc_y: 0.0,
                            start_secs: time.elapsed_secs(),
                            mode: GestureMode::Undecided,
                            flicked: false,
                        },
                    );
                } else {
                    gestures.tracks.remove(&message.id);
                }
            }
            TouchPhase::Moved => {
                let Some(track) = gestures.tracks.get_mut(&message.id) else {
                    continue;
                };
                let delta = position - track.prev;
                track.prev = position;
                let total = position - track.start;
                if track.flicked {
                    continue;
                }
                if track.mode != GestureMode::Horiz
                    && total.y >= 3.0 * cell
                    && time.elapsed_secs() - track.start_secs <= FLICK_MAX_SECS
                {
                    out.push(Action::HardDrop);
                    track.flicked = true;
                    continue;
                }
                match track.mode {
                    GestureMode::Undecided => {
                        if total.x.abs() >= COMMIT_PX && total.x.abs() > total.y.abs() {
                            track.mode = GestureMode::Horiz;
                            track.acc_x = total.x;
                            track.acc_y = 0.0;
                        } else if total.y.abs() >= COMMIT_PX {
                            track.mode = GestureMode::Vert;
                            track.acc_y = total.y.max(0.0);
                            track.acc_x = 0.0;
                        }
                    }
                    GestureMode::Horiz => {
                        track.acc_x += delta.x;
                        let step = (0.9 * cell).max(1.0);
                        while track.acc_x.abs() >= step {
                            let dir = track.acc_x.signum();
                            track.acc_x -= step * dir;
                            out.push(if dir < 0.0 {
                                Action::MoveLeft
                            } else {
                                Action::MoveRight
                            });
                        }
                    }
                    GestureMode::Vert => {
                        track.acc_y += delta.y;
                        let step = (0.7 * cell).max(1.0);
                        while track.acc_y >= step {
                            track.acc_y -= step;
                            out.push(Action::SoftDrop);
                        }
                        if track.acc_y < 0.0 {
                            track.acc_y = 0.0;
                        }
                    }
                }
            }
            TouchPhase::Ended => {
                if let Some(track) = gestures.tracks.remove(&message.id) {
                    if track.mode == GestureMode::Undecided
                        && !track.flicked
                        && time.elapsed_secs() - track.start_secs <= TAP_MAX_SECS
                        && (position - track.start).length() <= TAP_MAX_DIST
                    {
                        out.push(Action::RotateCw);
                    }
                }
            }
            TouchPhase::Canceled => {
                gestures.tracks.remove(&message.id);
            }
        }
    }
    if ui_debug() && !out.is_empty() {
        info!("TUI gesture emit {out:?}");
    }
    gestures.emitted.append(&mut out);
}

/// Show the overlay only while playing: hidden means invisible and
/// unpickable for the menu layer.
fn sync_overlay_visibility(
    state: Res<AppState>,
    mut roots: Query<&mut Visibility, With<TouchOverlayRoot>>,
) {
    let wanted = if *state == AppState::Playing {
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

/// Press feedback: brighten the disc under a live press.
#[allow(clippy::type_complexity)]
fn button_feedback(
    mut buttons: Query<
        (&TouchBtn, &Interaction, &mut BackgroundColor),
        (Changed<Interaction>, With<TouchBtn>),
    >,
) {
    for (btn, interaction, mut bg) in &mut buttons {
        if ui_debug() && *interaction == Interaction::Pressed {
            info!("TUI press {btn:?}");
        }
        let alpha = if *interaction == Interaction::Pressed {
            BTN_PRESSED_ALPHA
        } else {
            BTN_IDLE_ALPHA
        };
        *bg = BackgroundColor(Color::srgba(1.0, 1.0, 1.0, alpha));
    }
}

/// `TETRIS_UI_DEBUG=1` layout dump ~4 s in: proves where taffy actually put
/// the overlay, decks and discs on the device (world = UI coords, top-left
/// origin, physical px).
#[allow(clippy::type_complexity)]
fn ui_debug_dump(
    mut frame: Local<u32>,
    overlays: Query<(&ComputedNode, &UiGlobalTransform), With<TouchOverlayRoot>>,
    decks: Query<(&TouchDeck, &ComputedNode, &UiGlobalTransform)>,
    buttons: Query<(
        &TouchBtn,
        &ComputedNode,
        &UiGlobalTransform,
        Option<&Pickable>,
    )>,
) {
    if !ui_debug() {
        return;
    }
    *frame += 1;
    if *frame != 240 {
        return;
    }
    for (node, transform) in &overlays {
        info!(
            "TUI root center {:?} size {:?}",
            transform.translation, node.size
        );
    }
    for (deck, node, transform) in &decks {
        info!(
            "TUI deck {deck:?} center {:?} size {:?}",
            transform.translation, node.size
        );
    }
    for (btn, node, transform, pickable) in &buttons {
        info!(
            "TUI btn {btn:?} center {:?} size {:?} pickable {pickable:?}",
            transform.translation, node.size,
        );
    }
}

/// Press-edge bookkeeping: `true` exactly on the step a button goes from
/// released to held (same rule as the keyboard layer — bookkeeping always
/// advances, so reopening a gate never replays a stale press).
fn edge(prev: &mut HashMap<TouchBtn, bool>, btn: TouchBtn, held: bool) -> bool {
    let pressed_before = prev.insert(btn, held).unwrap_or(false);
    held && !pressed_before
}

#[allow(clippy::too_many_arguments)]
fn touch_action_system(
    buttons: Query<(&TouchBtn, &Interaction)>,
    mut state: ResMut<AppState>,
    capture: Res<RebindingCapture>,
    settings: Res<Settings>,
    freeze: Res<JuiceFreeze>,
    mut sim: ResMut<SimPaused>,
    mut pending: ResMut<PendingActions>,
    mut versus_actions: ResMut<VersusActions>,
    versus: Option<NonSend<VersusMatch>>,
    winner: Option<Res<VersusWinner>>,
    net: Option<Res<NetSession>>,
    mut machines: ResMut<TouchMachines>,
    mut gestures: ResMut<GestureState>,
) {
    // Both decks carry the same `TouchBtn` markers; only the visible deck is
    // ever pickable, so a logical action is held when *any* of its copies is
    // pressed. (`find()` here picked the hidden deck's copy — first in
    // iteration order — and silently swallowed every disc press.)
    let is_held = |btn| {
        buttons
            .iter()
            .any(|(b, i)| *b == btn && *i == Interaction::Pressed)
    };
    let held_left = is_held(TouchBtn::Left);
    let held_right = is_held(TouchBtn::Right);
    let held_soft = is_held(TouchBtn::Soft);
    let held_hard = is_held(TouchBtn::HardDrop);
    let held_cw = is_held(TouchBtn::RotateCw);
    let held_ccw = is_held(TouchBtn::RotateCcw);
    let held_hold = is_held(TouchBtn::Hold);
    let held_pause = is_held(TouchBtn::Pause);

    let prev = &mut machines.prev_pressed;
    let move_left = edge(prev, TouchBtn::Left, held_left);
    let move_right = edge(prev, TouchBtn::Right, held_right);
    let soft = edge(prev, TouchBtn::Soft, held_soft);
    let hard = edge(prev, TouchBtn::HardDrop, held_hard);
    let rotate_cw = edge(prev, TouchBtn::RotateCw, held_cw);
    let rotate_ccw = edge(prev, TouchBtn::RotateCcw, held_ccw);
    let hold = edge(prev, TouchBtn::Hold, held_hold);
    let pause_edge = edge(prev, TouchBtn::Pause, held_pause);

    // Pause mirrors the keyboard chord's gates (net matches are never
    // paused; a crowned match blocks a *fresh* pause under the winner
    // overlay).
    let net_in_match = net.is_some_and(|net| net.status == NetStatus::InMatch);
    let finished_match = versus.as_ref().is_some_and(|versus| versus.active)
        && winner.is_some_and(|winner| winner.0.is_some());
    let blocked_fresh_pause = finished_match && *state == AppState::Playing;
    if pause_edge && !capture.capturing && !net_in_match && !blocked_fresh_pause {
        toggle_pause(&mut state, &mut sim, &freeze);
    }

    // Gameplay emission mirrors the two keyboard systems exactly. Gesture
    // actions recognized this step join the same queue; if a gameplay gate
    // is closed they are drained and dropped rather than queued.
    let gesture_actions = std::mem::take(&mut gestures.emitted);
    let playing = *state == AppState::Playing;
    if !playing || capture.capturing {
        return;
    }
    let das_ticks = ticks_for(settings.das_ms, SIM_HZ_U32);
    let arr_ticks = ticks_for(settings.arr_ms, SIM_HZ_U32).max(1);

    let mut local: Vec<Action> = Vec::new();
    let target: Option<&mut Vec<Action>> = if versus.as_ref().is_some_and(|versus| versus.active) {
        // Versus: same active gate as `versus_input_system`; touch feeds
        // the one *local* seat (a `Net` seat never takes local input; a
        // two-human local match hands touch the left seat).
        let versus = versus.expect("active versus exists");
        let p1_human = matches!(versus.p1, Controller::Human);
        let p2_human = matches!(versus.p2, Controller::Human);
        let p1_net = matches!(versus.p1, Controller::Net);
        let p2_net = matches!(versus.p2, Controller::Net);
        // Seat priority: a `Net` opposite pins the local side; otherwise
        // the first `Human` seat (left wins a two-human match).
        if p2_net || p1_human {
            Some(&mut versus_actions.left)
        } else if p1_net || p2_human {
            Some(&mut versus_actions.right)
        } else {
            None
        }
    } else {
        Some(&mut local)
    };
    if let Some(queue) = target {
        push_gameplay(
            queue,
            &mut machines,
            held_left,
            held_right,
            move_left,
            move_right,
            soft,
            held_soft,
            hard,
            rotate_cw,
            rotate_ccw,
            hold,
            &settings,
            das_ticks,
            arr_ticks,
        );
        queue.extend(gesture_actions);
    }
    for action in local {
        pending.push(action);
    }
}

/// The shared emission body: shift repeat with DAS/ARR, soft-drop cadence,
/// and the one-shot actions — identical order and semantics to
/// `gameplay_input_system` (minus wheel/180, which have no button).
#[allow(clippy::too_many_arguments)]
fn push_gameplay(
    queue: &mut Vec<Action>,
    machines: &mut TouchMachines,
    held_left: bool,
    held_right: bool,
    move_left: bool,
    move_right: bool,
    soft: bool,
    held_soft: bool,
    hard: bool,
    rotate_cw: bool,
    rotate_ccw: bool,
    hold: bool,
    settings: &Settings,
    das_ticks: u32,
    arr_ticks: u32,
) {
    if let Some(dir) = machines.shift.step(
        held_left, held_right, move_left, move_right, das_ticks, arr_ticks,
    ) {
        queue.push(match dir {
            ShiftDir::Left => Action::MoveLeft,
            ShiftDir::Right => Action::MoveRight,
        });
    }

    if soft {
        let period = soft_drop_period_ticks(settings.soft_drop_multiplier, SIM_HZ_U32);
        machines.soft = RepeatTimer::with_ticks(0, period);
        machines.soft.press();
        queue.push(Action::SoftDrop);
    } else if held_soft {
        if machines.soft.advance() {
            queue.push(Action::SoftDrop);
        }
    } else {
        machines.soft.release();
    }

    if hard {
        queue.push(Action::HardDrop);
    }
    if rotate_cw {
        queue.push(Action::RotateCw);
    }
    if rotate_ccw {
        queue.push(Action::RotateCcw);
    }
    if hold {
        queue.push(Action::Hold);
    }
}

/// Android lifecycle: backgrounding a live match pauses it (Playing →
/// Paused) so the sim never burns gravity while the activity is stopped.
/// Net matches can't pause (lockstep is authoritative on both ends), and a
/// crowned versus match stays under its winner overlay — same gates as the
/// pause button.
#[cfg(target_os = "android")]
#[allow(clippy::too_many_arguments)]
fn lifecycle_pause_system(
    mut messages: MessageReader<AppLifecycle>,
    mut state: ResMut<AppState>,
    mut sim: ResMut<SimPaused>,
    freeze: Res<JuiceFreeze>,
    net: Option<Res<NetSession>>,
    versus: Option<NonSend<VersusMatch>>,
    winner: Option<Res<VersusWinner>>,
) {
    for message in messages.read() {
        if !matches!(message, AppLifecycle::WillSuspend | AppLifecycle::Suspended) {
            continue;
        }
        let net_in_match = net
            .as_ref()
            .is_some_and(|net| net.status == NetStatus::InMatch);
        let finished_match = versus.as_ref().is_some_and(|versus| versus.active)
            && winner.as_ref().is_some_and(|winner| winner.0.is_some());
        if *state == AppState::Playing && !net_in_match && !finished_match {
            toggle_pause(&mut state, &mut sim, &freeze);
        }
    }
}
