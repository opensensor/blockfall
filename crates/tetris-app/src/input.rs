//! Input map, key bindings, and DAS/ARR state machine (T12).
//!
//! Reads raw keyboard state through [`ButtonInput<KeyCode>`] plus
//! [`MouseWheel`] messages, resolves them against the rebindable
//! [`KeyBindings`] resource (owned here, PRD §9 defaults), and pushes
//! `tetris_core::actions::Action` values into
//! [`PendingActions`](crate::core_bridge::PendingActions).
//!
//! T25 adds the versus half: [`VersusBindings`] (fixed two-player presets,
//! not rebindable in v0.2 — the settings screen keeps editing solo bindings
//! only) and [`VersusActions`], two per-side action queues produced by
//! [`versus_input_system`] while a versus match is active. The two halves
//! are mutually exclusive: `gameplay_input_system` skips while
//! [`VersusMatch`](crate::core_bridge::VersusMatch) is active, and
//! `versus_input_system` skips (and never emits for bot-controlled sides)
//! otherwise. Both repeat machines reuse [`ShiftRepeat`]/[`RepeatTimer`]
//! with the same [`Settings`] DAS/ARR/soft-drop values.
//!
//! Scheduling: [`gameplay_input_system`] runs on `FixedPreUpdate`, the
//! `FixedMain` sub-schedule that precedes the core bridge's `FixedUpdate`
//! drain (PRD §10.3 order `input → apply actions → core step`). Repeat
//! cadence therefore advances in **fixed-step ticks** (60 Hz), not wall
//! clock — framerate-independent by construction. Settings ms values are
//! converted to tick counts at use time via [`ticks_for`] (defaults:
//! DAS 150 ms → 9 ticks, ARR 33 ms → 2 ticks, ARR clamped to ≥ 1 tick).
//!
//! Emission gate: gameplay actions flow only while
//! [`AppState::Playing`](crate::state::AppState::Playing) and
//! [`RebindingCapture`](crate::state::RebindingCapture) is idle; edge
//! bookkeeping still advances while gated so no spurious press fires when
//! the gate reopens. The `Pause` binding slot lives in the same table but
//! is consumed by the menu-state handler (T17) and never emitted here.
//!
//! Note on PRD cross-reference: §6.4's "Q/E" rotate suggestion is
//! superseded by §9 (↑/Z, X alias) plus the wheel bindings required by
//! this task's plan entry; resolved in favor of §9 + wheel.

use std::collections::HashMap;

use bevy::ecs::system::SystemParam;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use tetris_core::actions::Action;

use crate::core_bridge::{Controller, PendingActions, VersusMatch, SIM_HZ};
use crate::state::{AppState, RebindingCapture, Settings};

/// Fixed-step rate as an integer, for [`ticks_for`] conversions.
const SIM_HZ_U32: u32 = SIM_HZ as u32;

/// Round milliseconds to fixed-step ticks at `hz`, half-up.
///
/// PRD §6.5 defaults at 60 Hz: 150 ms → 9, 33 ms → 2.
pub const fn ticks_for(ms: u32, hz: u32) -> u32 {
    ms.saturating_mul(hz).saturating_add(500) / 1000
}

/// Soft-drop repeat period in ticks for `multiplier × gravity` at `hz`.
///
/// Default multiplier 20 at 60 Hz → a `SoftDrop` action every 3rd tick.
pub fn soft_drop_period_ticks(multiplier: u8, hz: u32) -> u32 {
    let m = multiplier.max(1) as f64;
    ((hz as f64 / m).round_ties_even()).max(1.0) as u32
}

/// Pure DAS/ARR repeat state machine, driven one fixed-step tick at a
/// time by [`RepeatTimer::advance`] — unit-testable without any clock.
///
/// Semantics: [`press`](Self::press) fires immediately; after `das_ticks`
/// of hold, a repeat fires every `arr_ticks` ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepeatTimer {
    held: bool,
    elapsed: u32,
    das_ticks: u32,
    arr_ticks: u32,
}

impl Default for RepeatTimer {
    fn default() -> Self {
        Self::with_ticks(Self::DEFAULT_DAS_TICKS, Self::DEFAULT_ARR_TICKS)
    }
}

impl RepeatTimer {
    /// PRD §6.5 default DAS: 150 ms at 60 Hz → 9 ticks.
    pub const DEFAULT_DAS_TICKS: u32 = ticks_for(150, SIM_HZ_U32);
    /// PRD §6.5 default ARR: 33 ms at 60 Hz → 2 ticks.
    pub const DEFAULT_ARR_TICKS: u32 = {
        let ticks = ticks_for(33, SIM_HZ_U32);
        if ticks == 0 {
            1
        } else {
            ticks
        }
    };

    /// Build from millisecond settings (ARR clamped to ≥ 1 tick).
    pub fn new(das_ms: u32, arr_ms: u32) -> Self {
        Self::with_ticks(
            ticks_for(das_ms, SIM_HZ_U32),
            ticks_for(arr_ms, SIM_HZ_U32).max(1),
        )
    }

    /// Build directly from tick counts; `arr_ticks` is clamped to ≥ 1.
    pub const fn with_ticks(das_ticks: u32, arr_ticks: u32) -> Self {
        Self {
            held: false,
            elapsed: 0,
            das_ticks,
            arr_ticks: if arr_ticks == 0 { 1 } else { arr_ticks },
        }
    }

    /// Register a press; `true` = emit the immediate first action.
    /// Re-pressing while held restarts the DAS window.
    pub fn press(&mut self) -> bool {
        self.held = true;
        self.elapsed = 0;
        true
    }

    /// Advance one fixed-step tick; `true` = emit a repeat action.
    pub fn advance(&mut self) -> bool {
        if !self.held {
            return false;
        }
        self.elapsed += 1;
        self.elapsed >= self.das_ticks
            && (self.elapsed - self.das_ticks).is_multiple_of(self.arr_ticks)
    }

    /// Release: resets all repeat state.
    pub fn release(&mut self) {
        self.held = false;
        self.elapsed = 0;
    }

    /// Whether the key is currently registered as held.
    pub fn is_held(&self) -> bool {
        self.held
    }
}

/// Which horizontal direction a [`ShiftRepeat`] focus/emit refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftDir {
    /// Emit `Action::MoveLeft`.
    Left,
    /// Emit `Action::MoveRight`.
    Right,
}

/// Paired left/right [`RepeatTimer`]s with a single repeat *focus*, so a
/// step can never emit both directions. A direction press (edge) fires
/// immediately and steals the focus — direction reversal therefore repeats
/// from the press tick. Releasing the focused direction falls back to the
/// other if still held (its own DAS/ARR progress is preserved); releasing
/// a key always resets that key's timer.
#[derive(Debug, Default, Clone, Copy, Resource)]
pub struct ShiftRepeat {
    left: RepeatTimer,
    right: RepeatTimer,
    focus: Option<ShiftDir>,
}

impl ShiftRepeat {
    /// Drive one fixed-step tick. `*_edge` = pressed this step but not
    /// the previous one. Returns the direction to emit, if any.
    pub fn step(
        &mut self,
        left: bool,
        right: bool,
        left_edge: bool,
        right_edge: bool,
        das_ticks: u32,
        arr_ticks: u32,
    ) -> Option<ShiftDir> {
        if !left {
            self.left.release();
        }
        if !right {
            self.right.release();
        }
        match self.focus {
            Some(ShiftDir::Left) if !left => {
                self.focus = if right { Some(ShiftDir::Right) } else { None };
            }
            Some(ShiftDir::Right) if !right => {
                self.focus = if left { Some(ShiftDir::Left) } else { None };
            }
            _ => {}
        }
        if left_edge {
            self.left = RepeatTimer::with_ticks(das_ticks, arr_ticks);
            self.left.press();
            self.focus = Some(ShiftDir::Left);
            return Some(ShiftDir::Left);
        }
        if right_edge {
            self.right = RepeatTimer::with_ticks(das_ticks, arr_ticks);
            self.right.press();
            self.focus = Some(ShiftDir::Right);
            return Some(ShiftDir::Right);
        }
        match self.focus {
            Some(ShiftDir::Left) if self.left.advance() => Some(ShiftDir::Left),
            Some(ShiftDir::Right) if self.right.advance() => Some(ShiftDir::Right),
            _ => None,
        }
    }

    /// Focused direction, if any key is held.
    pub fn focus(&self) -> Option<ShiftDir> {
        self.focus
    }
}

/// One rebindable input: a keyboard key or a mouse-wheel notch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bind {
    /// A physical keyboard key.
    Key(KeyCode),
    /// One mouse-wheel notch upward (bound to RotateCw).
    WheelUp,
    /// One mouse-wheel notch downward (bound to RotateCcw).
    WheelDown,
}

/// Every bindable action slot, including the menu-owned `Pause` chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindSlot {
    /// `Action::MoveLeft` (DAS/ARR repeat).
    MoveLeft,
    /// `Action::MoveRight` (DAS/ARR repeat).
    MoveRight,
    /// `Action::SoftDrop` (held cadence from the soft-drop multiplier).
    SoftDrop,
    /// `Action::HardDrop` (single trigger per press).
    HardDrop,
    /// `Action::RotateCw` (single trigger; also wheel-up).
    RotateCw,
    /// `Action::RotateCcw` (single trigger; also wheel-down).
    RotateCcw,
    /// `Action::Rotate180` (single trigger per press).
    Rotate180,
    /// `Action::Hold` (single trigger per press).
    Hold,
    /// Pause chord — consumed by the menu-state handler (T17), never
    /// emitted as a core `Action` by this plugin.
    Pause,
}

/// All bindable slots, in display order (useful for the T16 settings UI).
pub const ALL_BIND_SLOTS: [BindSlot; 9] = [
    BindSlot::MoveLeft,
    BindSlot::MoveRight,
    BindSlot::SoftDrop,
    BindSlot::HardDrop,
    BindSlot::RotateCw,
    BindSlot::RotateCcw,
    BindSlot::Rotate180,
    BindSlot::Hold,
    BindSlot::Pause,
];

/// Rebindable key bindings (PRD §9 defaults), owned by this module and
/// edited by the settings screen (T16 via [`BindSlot`] accessors).
#[derive(Debug, Clone, PartialEq, Eq, Resource)]
pub struct KeyBindings {
    move_left: Vec<Bind>,
    move_right: Vec<Bind>,
    soft_drop: Vec<Bind>,
    hard_drop: Vec<Bind>,
    rotate_cw: Vec<Bind>,
    rotate_ccw: Vec<Bind>,
    rotate_180: Vec<Bind>,
    hold: Vec<Bind>,
    pause: Vec<Bind>,
}

impl KeyBindings {
    /// PRD §9 defaults plus the plan-required wheel-up/down →
    /// RotateCw/Ccw, the X rotate alias, the Shift hold alias, and the
    /// Esc/P pause chord.
    pub fn default_slot(slot: BindSlot) -> Vec<Bind> {
        let k = |key| vec![Bind::Key(key)];
        match slot {
            BindSlot::MoveLeft => k(KeyCode::ArrowLeft),
            BindSlot::MoveRight => k(KeyCode::ArrowRight),
            BindSlot::SoftDrop => k(KeyCode::ArrowDown),
            BindSlot::HardDrop => k(KeyCode::Space),
            BindSlot::RotateCw => vec![
                Bind::Key(KeyCode::ArrowUp),
                Bind::Key(KeyCode::KeyX),
                Bind::WheelUp,
            ],
            BindSlot::RotateCcw => vec![Bind::Key(KeyCode::KeyZ), Bind::WheelDown],
            BindSlot::Rotate180 => k(KeyCode::KeyA),
            BindSlot::Hold => vec![
                Bind::Key(KeyCode::KeyC),
                Bind::Key(KeyCode::ShiftLeft),
                Bind::Key(KeyCode::ShiftRight),
            ],
            BindSlot::Pause => vec![Bind::Key(KeyCode::Escape), Bind::Key(KeyCode::KeyP)],
        }
    }

    /// Binds currently assigned to `slot`.
    pub fn slot(&self, slot: BindSlot) -> &Vec<Bind> {
        match slot {
            BindSlot::MoveLeft => &self.move_left,
            BindSlot::MoveRight => &self.move_right,
            BindSlot::SoftDrop => &self.soft_drop,
            BindSlot::HardDrop => &self.hard_drop,
            BindSlot::RotateCw => &self.rotate_cw,
            BindSlot::RotateCcw => &self.rotate_ccw,
            BindSlot::Rotate180 => &self.rotate_180,
            BindSlot::Hold => &self.hold,
            BindSlot::Pause => &self.pause,
        }
    }

    /// Mutable view of `slot`'s binds (T16 rebinding UI).
    pub fn slot_mut(&mut self, slot: BindSlot) -> &mut Vec<Bind> {
        match slot {
            BindSlot::MoveLeft => &mut self.move_left,
            BindSlot::MoveRight => &mut self.move_right,
            BindSlot::SoftDrop => &mut self.soft_drop,
            BindSlot::HardDrop => &mut self.hard_drop,
            BindSlot::RotateCw => &mut self.rotate_cw,
            BindSlot::RotateCcw => &mut self.rotate_ccw,
            BindSlot::Rotate180 => &mut self.rotate_180,
            BindSlot::Hold => &mut self.hold,
            BindSlot::Pause => &mut self.pause,
        }
    }

    /// Replace `slot`'s binds wholesale.
    pub fn set_slot(&mut self, slot: BindSlot, binds: Vec<Bind>) {
        *self.slot_mut(slot) = binds;
    }

    /// Restore `slot` to its PRD §9 default.
    pub fn reset_slot(&mut self, slot: BindSlot) {
        self.set_slot(slot, Self::default_slot(slot));
    }
}

impl Default for KeyBindings {
    fn default() -> Self {
        let slot = KeyBindings::default_slot;
        Self {
            move_left: slot(BindSlot::MoveLeft),
            move_right: slot(BindSlot::MoveRight),
            soft_drop: slot(BindSlot::SoftDrop),
            hard_drop: slot(BindSlot::HardDrop),
            rotate_cw: slot(BindSlot::RotateCw),
            rotate_ccw: slot(BindSlot::RotateCcw),
            rotate_180: slot(BindSlot::Rotate180),
            hold: slot(BindSlot::Hold),
            pause: slot(BindSlot::Pause),
        }
    }
}

/// Capture helper for the T16 rebinding UI: the first key just pressed
/// this frame as a [`Bind`], if any (call from an `Update` system while
/// [`RebindingCapture`] is active).
pub fn pressed_key_to_bind(keys: &ButtonInput<KeyCode>) -> Option<Bind> {
    keys.get_just_pressed().next().copied().map(Bind::Key)
}

/// Per-step edge bookkeeping plus the two repeat machines. Kept in one
/// resource so the input system takes a single mutable parameter.
#[derive(Debug, Default, Resource)]
pub struct InputMachine {
    prev_pressed: HashMap<BindSlot, bool>,
    shift: ShiftRepeat,
    soft: RepeatTimer,
}

fn slot_pressed(bindings: &KeyBindings, keys: &ButtonInput<KeyCode>, slot: BindSlot) -> bool {
    bindings
        .slot(slot)
        .iter()
        .any(|b| matches!(b, Bind::Key(key) if keys.pressed(*key)))
}

fn edge(prev: &mut HashMap<BindSlot, bool>, slot: BindSlot, now: bool) -> bool {
    let was = prev.insert(slot, now).unwrap_or(false);
    now && !was
}

/// All fixed-step parameters for [`gameplay_input_system`] in one struct
/// (keeps the system under clippy's argument limits).
#[derive(SystemParam)]
struct InputParams<'w, 's> {
    keys: Res<'w, ButtonInput<KeyCode>>,
    wheel: MessageReader<'w, 's, MouseWheel>,
    bindings: Res<'w, KeyBindings>,
    settings: Res<'w, Settings>,
    app_state: Res<'w, AppState>,
    capture: Res<'w, RebindingCapture>,
    versus: NonSend<'w, VersusMatch>,
    machine: ResMut<'w, InputMachine>,
    pending: ResMut<'w, PendingActions>,
}

/// Samples bound inputs once per fixed step and pushes core `Action`s
/// into `PendingActions`. Registered on `FixedPreUpdate`, i.e. before the
/// core bridge drains the queue in `FixedUpdate` (PRD §10.3).
fn gameplay_input_system(mut params: InputParams) {
    // Edge bookkeeping runs even while gated, so reopening the gate
    // never replays a stale press as a fresh edge.
    let bindings = &params.bindings;
    let keys = &params.keys;
    let now = |slot| slot_pressed(bindings, keys, slot);
    let held_left = now(BindSlot::MoveLeft);
    let held_right = now(BindSlot::MoveRight);
    let held_soft = now(BindSlot::SoftDrop);
    let held_hard = now(BindSlot::HardDrop);
    let held_cw = now(BindSlot::RotateCw);
    let held_ccw = now(BindSlot::RotateCcw);
    let held_180 = now(BindSlot::Rotate180);
    let held_hold = now(BindSlot::Hold);
    let prev = &mut params.machine.prev_pressed;
    let move_left = edge(prev, BindSlot::MoveLeft, held_left);
    let move_right = edge(prev, BindSlot::MoveRight, held_right);
    let soft = edge(prev, BindSlot::SoftDrop, held_soft);
    let hard = edge(prev, BindSlot::HardDrop, held_hard);
    let rotate_cw = edge(prev, BindSlot::RotateCw, held_cw);
    let rotate_ccw = edge(prev, BindSlot::RotateCcw, held_ccw);
    let rotate_180 = edge(prev, BindSlot::Rotate180, held_180);
    let hold = edge(prev, BindSlot::Hold, held_hold);

    // Wheel notches: aggregate the step, net direction, ≤ 1 rotation per
    // kind per step. Consumed even while gated so nothing bursts later.
    let mut wheel_notch: i32 = 0;
    for event in params.wheel.read() {
        if event.y > 0.0 {
            wheel_notch += 1;
        } else if event.y < 0.0 {
            wheel_notch -= 1;
        }
    }

    let gated =
        *params.app_state != AppState::Playing || params.capture.capturing || params.versus.active;
    if gated {
        return;
    }

    let das_ticks = ticks_for(params.settings.das_ms, SIM_HZ_U32);
    let arr_ticks = ticks_for(params.settings.arr_ms, SIM_HZ_U32).max(1);
    let pending = &mut params.pending;
    let machine = &mut params.machine;

    if let Some(dir) = machine.shift.step(
        held_left, held_right, move_left, move_right, das_ticks, arr_ticks,
    ) {
        pending.push(match dir {
            ShiftDir::Left => Action::MoveLeft,
            ShiftDir::Right => Action::MoveRight,
        });
    }

    if soft {
        // Soft drop has no DAS: press fires, then a held cadence set by
        // the multiplier (PRD §6.5, default ×20 → every 3rd tick).
        let period = soft_drop_period_ticks(params.settings.soft_drop_multiplier, SIM_HZ_U32);
        machine.soft = RepeatTimer::with_ticks(0, period);
        machine.soft.press();
        pending.push(Action::SoftDrop);
    } else if held_soft {
        if machine.soft.advance() {
            pending.push(Action::SoftDrop);
        }
    } else {
        machine.soft.release();
    }

    if hard {
        pending.push(Action::HardDrop);
    }
    let wheel_cw = wheel_notch > 0
        && params
            .bindings
            .slot(BindSlot::RotateCw)
            .contains(&Bind::WheelUp);
    let wheel_ccw = wheel_notch < 0
        && params
            .bindings
            .slot(BindSlot::RotateCcw)
            .contains(&Bind::WheelDown);
    if rotate_cw || wheel_cw {
        pending.push(Action::RotateCw);
    }
    if rotate_ccw || wheel_ccw {
        pending.push(Action::RotateCcw);
    }
    if rotate_180 {
        pending.push(Action::Rotate180);
    }
    if hold {
        pending.push(Action::Hold);
    }
}

/// Translates bound inputs into core `Action`s with DAS/ARR repeat handling.
pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits keep the system parameters satisfied in headless
        // `MinimalPlugins` tests that lack Bevy's own `InputPlugin`; both
        // are no-ops when it is present.
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        if !app.world().contains_resource::<Messages<MouseWheel>>() {
            app.add_message::<MouseWheel>();
        }
        app.init_resource::<KeyBindings>()
            .init_resource::<InputMachine>()
            // T25 versus half: fixed presets + the two per-side queues.
            .init_resource::<VersusBindings>()
            .init_resource::<VersusActions>()
            .init_resource::<VersusInputMachines>()
            .add_systems(FixedPreUpdate, gameplay_input_system)
            .add_systems(FixedPreUpdate, versus_input_system);
    }
}

// ---------------------------------------------------------------------------
// T25 versus input: fixed two-player presets and per-side action queues
// ---------------------------------------------------------------------------

/// One versus player's fixed preset. Not rebindable in v0.2 — the settings
/// screen keeps editing [`KeyBindings`] (solo) only. `Vec` slots exist for
/// alternate keys (P1 soft-drop alt, P2 hard-drop alt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerPreset {
    /// `Action::MoveLeft` (DAS/ARR repeat).
    pub move_left: Vec<KeyCode>,
    /// `Action::MoveRight` (DAS/ARR repeat).
    pub move_right: Vec<KeyCode>,
    /// `Action::RotateCw` (single trigger per press).
    pub rotate_cw: Vec<KeyCode>,
    /// `Action::RotateCcw` (single trigger per press).
    pub rotate_ccw: Vec<KeyCode>,
    /// `Action::SoftDrop` (held cadence from the soft-drop multiplier).
    pub soft_drop: Vec<KeyCode>,
    /// `Action::HardDrop` (single trigger per press).
    pub hard_drop: Vec<KeyCode>,
    /// `Action::Hold` (single trigger per press).
    pub hold: Vec<KeyCode>,
}

/// The versus binding table (both sides in one resource).
///
/// Note: [`versus_input_system`] hands the arrow preset to a *lone* human
/// side (vs the bot), so `p2` is what a solo keyboard player presses in a
/// Human-vs-Bot match; `p1` (WASD) only applies to the left seat of a
/// two-human match.
#[derive(Debug, Clone, PartialEq, Eq, Resource)]
pub struct VersusBindings {
    /// Left player (P1) preset.
    pub p1: PlayerPreset,
    /// Right player (P2) preset.
    pub p2: PlayerPreset,
}

impl Default for VersusBindings {
    fn default() -> Self {
        let k = |key| vec![key];
        Self {
            p1: PlayerPreset {
                move_left: k(KeyCode::KeyA),
                move_right: k(KeyCode::KeyD),
                rotate_cw: k(KeyCode::KeyW),
                rotate_ccw: k(KeyCode::KeyE),
                soft_drop: vec![KeyCode::KeyS, KeyCode::ShiftLeft],
                hard_drop: k(KeyCode::Space),
                hold: k(KeyCode::KeyQ),
            },
            p2: PlayerPreset {
                move_left: k(KeyCode::ArrowLeft),
                move_right: k(KeyCode::ArrowRight),
                rotate_cw: k(KeyCode::ArrowUp),
                rotate_ccw: k(KeyCode::Period),
                soft_drop: k(KeyCode::ArrowDown),
                hard_drop: vec![KeyCode::Slash, KeyCode::Numpad0],
                hold: k(KeyCode::Comma),
            },
        }
    }
}

/// Per-side action queues for a versus match: produced by
/// [`versus_input_system`] (human sides) and by
/// [`versus_bot_system`](crate::core_bridge), drained by the versus fixed
/// step. Held, never dropped, while stepping is gated (shared pause).
#[derive(Debug, Default, Resource)]
pub struct VersusActions {
    /// Left player's FIFO queue.
    pub left: Vec<Action>,
    /// Right player's FIFO queue.
    pub right: Vec<Action>,
}

/// Per-side edge bookkeeping plus the two repeat machines — the versus
/// analogue of [`InputMachine`] with plain bools instead of a
/// `HashMap<BindSlot, _>` (fixed seven slots, no rebinding).
#[derive(Debug, Default, Clone, Copy)]
struct SideMachines {
    prev_left: bool,
    prev_right: bool,
    prev_soft: bool,
    prev_hard: bool,
    prev_cw: bool,
    prev_ccw: bool,
    prev_hold: bool,
    shift: ShiftRepeat,
    soft: RepeatTimer,
}

/// Both sides' repeat machines (one resource keeps the system parameters
/// small, mirroring [`InputMachine`]).
#[derive(Debug, Default, Resource)]
pub struct VersusInputMachines {
    p1: SideMachines,
    p2: SideMachines,
}

/// Drive one versus side for a fixed-step tick: edge bookkeeping always
/// advances (so reopening a gate never replays stale presses — same rule as
/// the solo system), while actions only go to `out` (`None` = this side is
/// gated: versus inactive, paused, or bot-controlled).
fn drive_versus_side(
    preset: &PlayerPreset,
    keys: &ButtonInput<KeyCode>,
    das_ticks: u32,
    arr_ticks: u32,
    soft_period_ticks: u32,
    machine: &mut SideMachines,
    out: Option<&mut Vec<Action>>,
) {
    let any = |list: &[KeyCode]| list.iter().any(|key| keys.pressed(*key));
    let held_left = any(&preset.move_left);
    let held_right = any(&preset.move_right);
    let held_soft = any(&preset.soft_drop);
    let held_hard = any(&preset.hard_drop);
    let held_cw = any(&preset.rotate_cw);
    let held_ccw = any(&preset.rotate_ccw);
    let held_hold = any(&preset.hold);

    // NOTE: compute `was` unconditionally — `held && !replace(..)` would
    // short-circuit on release and latch the previous state to `true`,
    // killing every subsequent press edge for that slot.
    fn press_edge(prev: &mut bool, now: bool) -> bool {
        let was = std::mem::replace(prev, now);
        now && !was
    }
    let move_left = press_edge(&mut machine.prev_left, held_left);
    let move_right = press_edge(&mut machine.prev_right, held_right);
    let soft = press_edge(&mut machine.prev_soft, held_soft);
    let hard = press_edge(&mut machine.prev_hard, held_hard);
    let rotate_cw = press_edge(&mut machine.prev_cw, held_cw);
    let rotate_ccw = press_edge(&mut machine.prev_ccw, held_ccw);
    let hold = press_edge(&mut machine.prev_hold, held_hold);

    let Some(out) = out else { return };

    if let Some(dir) = machine.shift.step(
        held_left, held_right, move_left, move_right, das_ticks, arr_ticks,
    ) {
        out.push(match dir {
            ShiftDir::Left => Action::MoveLeft,
            ShiftDir::Right => Action::MoveRight,
        });
    }

    if soft {
        machine.soft = RepeatTimer::with_ticks(0, soft_period_ticks);
        machine.soft.press();
        out.push(Action::SoftDrop);
    } else if held_soft {
        if machine.soft.advance() {
            out.push(Action::SoftDrop);
        }
    } else {
        machine.soft.release();
    }

    if hard {
        out.push(Action::HardDrop);
    }

    if rotate_cw {
        out.push(Action::RotateCw);
    }
    if rotate_ccw {
        out.push(Action::RotateCcw);
    }
    if hold {
        out.push(Action::Hold);
    }
}

/// All fixed-step parameters for [`versus_input_system`] in one struct.
#[derive(SystemParam)]
struct VersusInputParams<'w> {
    keys: Res<'w, ButtonInput<KeyCode>>,
    bindings: Res<'w, VersusBindings>,
    settings: Res<'w, Settings>,
    app_state: Res<'w, AppState>,
    capture: Res<'w, RebindingCapture>,
    versus: NonSend<'w, VersusMatch>,
    machines: ResMut<'w, VersusInputMachines>,
    actions: ResMut<'w, VersusActions>,
}

/// Samples the two versus presets once per fixed step and pushes each human
/// side's `Action`s into its [`VersusActions`] queue. Runs on
/// `FixedPreUpdate`, ahead of the versus bridge's `FixedUpdate` drain —
/// exactly like the solo system, and mutually exclusive with it.
fn versus_input_system(mut params: VersusInputParams) {
    let playing = *params.app_state == AppState::Playing;
    let active = params.versus.active && playing && !params.capture.capturing;
    let das_ticks = ticks_for(params.settings.das_ms, SIM_HZ_U32);
    let arr_ticks = ticks_for(params.settings.arr_ms, SIM_HZ_U32).max(1);
    let soft_period = soft_drop_period_ticks(params.settings.soft_drop_multiplier, SIM_HZ_U32);

    let bindings = &params.bindings;
    let keys = &params.keys;
    let machines = &mut params.machines;
    let actions = &mut params.actions;

    let p1_human = matches!(params.versus.p1, Controller::Human);
    let p2_human = matches!(params.versus.p2, Controller::Human);
    // A `Net` seat counts as occupied (netplay-plan.md N4): without this the
    // host's `Human+Net` pair would trip `lone_human_p1` and the host would be
    // handed the arrow/solo-alternate preset instead of its WASD `bindings.p1`.
    let p2_net = matches!(params.versus.p2, Controller::Net);

    // A lone human (vs the bot) drives with the arrow preset — the same
    // keys as solo play. A two-human match keeps the classic shared-keyboard
    // split: P1 WASD on the left, P2 arrows on the right. The lone human's
    // copy also claims the solo alternate keys (Space hard drop, X/Z
    // rotations, C/Shift hold) — all free because no P2 seat competes.
    let lone_human_p1 = p1_human && !(p2_human || p2_net);

    let solo_arrow_preset;
    let (p1_preset, p2_preset) = if lone_human_p1 {
        let mut preset = bindings.p2.clone();
        preset.hard_drop.push(KeyCode::Space);
        preset.rotate_cw.push(KeyCode::KeyX);
        preset.rotate_ccw.push(KeyCode::KeyZ);
        preset.hold.push(KeyCode::KeyC);
        preset.hold.push(KeyCode::ShiftLeft);
        solo_arrow_preset = preset;
        (&solo_arrow_preset, &bindings.p1)
    } else {
        (&bindings.p1, &bindings.p2)
    };

    drive_versus_side(
        p1_preset,
        keys,
        das_ticks,
        arr_ticks,
        soft_period,
        &mut machines.p1,
        if active && p1_human {
            Some(&mut actions.left)
        } else {
            None
        },
    );
    drive_versus_side(
        p2_preset,
        keys,
        das_ticks,
        arr_ticks,
        soft_period,
        &mut machines.p2,
        if active && p2_human {
            Some(&mut actions.right)
        } else {
            None
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::app::App;
    use bevy::input::mouse::MouseScrollUnit;
    use bevy::input::touch::TouchPhase;

    use crate::core_bridge::{CoreBridgePlugin, GameCore};
    use crate::state::{AppState, RebindingCapture, Settings};

    // ---- pure repeat state machine (fake tick clock) ----

    #[test]
    fn ticks_for_matches_prd_defaults() {
        assert_eq!(ticks_for(150, 60), 9);
        assert_eq!(ticks_for(33, 60), 2);
        assert_eq!(ticks_for(0, 60), 0);
        assert_eq!(RepeatTimer::DEFAULT_DAS_TICKS, 9);
        assert_eq!(RepeatTimer::DEFAULT_ARR_TICKS, 2);
        assert_eq!(RepeatTimer::new(150, 33).arr_ticks, 2);
        assert_eq!(RepeatTimer::new(150, 0).arr_ticks, 1, "ARR ≥ 1 tick");
        assert_eq!(soft_drop_period_ticks(20, 60), 3);
        assert_eq!(soft_drop_period_ticks(255, 60), 1);
        assert_eq!(soft_drop_period_ticks(0, 60), 60);
    }

    #[test]
    fn press_is_immediate_then_das_then_arr_cadence() {
        let mut t = RepeatTimer::new(150, 33);
        assert!(t.press(), "immediate first action");
        for tick in 1..9 {
            assert!(!t.advance(), "silent during the DAS window (tick {tick})");
        }
        assert!(t.advance(), "first repeat lands exactly at DAS ticks");
        assert!(!t.advance(), "odd tick between repeats");
        assert!(t.advance(), "repeat every ARR ticks");
        assert!(!t.advance());
        assert!(t.advance());
    }

    #[test]
    fn release_resets_repeat_state() {
        let mut t = RepeatTimer::new(150, 33);
        t.press();
        for _ in 0..11 {
            t.advance();
        }
        t.release();
        assert!(!t.advance(), "released timer never fires");
        assert!(!t.is_held());
        assert!(t.press(), "re-press is immediate again");
        for tick in 1..9 {
            assert!(!t.advance(), "DAS restarted (tick {tick})");
        }
        assert!(t.advance());
    }

    #[test]
    fn direction_reversal_fires_immediately_and_steals_focus() {
        let mut s = ShiftRepeat::default();
        // Hold Right through its DAS and one repeat.
        assert_eq!(
            s.step(false, true, false, true, 9, 2),
            Some(ShiftDir::Right)
        );
        for tick in 1..9 {
            assert_eq!(s.step(false, true, false, false, 9, 2), None, "tick {tick}");
        }
        assert_eq!(
            s.step(false, true, false, false, 9, 2),
            Some(ShiftDir::Right)
        );
        // Reverse: Left press emits on the spot and takes repeat focus.
        assert_eq!(s.step(true, true, true, false, 9, 2), Some(ShiftDir::Left));
        assert_eq!(s.focus(), Some(ShiftDir::Left));
        // While both are held only the focus repeats — and Left restarted
        // its own DAS, so nothing before its tick 9.
        for tick in 1..9 {
            assert_eq!(s.step(true, true, false, false, 9, 2), None, "tick {tick}");
        }
        assert_eq!(s.step(true, true, false, false, 9, 2), Some(ShiftDir::Left));
    }

    #[test]
    fn shift_release_resets_and_falls_back_to_other_held_key() {
        let mut s = ShiftRepeat::default();
        assert_eq!(s.step(true, false, true, false, 9, 2), Some(ShiftDir::Left));
        // Right press steals focus immediately (reversal).
        assert_eq!(s.step(true, true, false, true, 9, 2), Some(ShiftDir::Right));
        // Releasing Left (not the focus) changes nothing; Right keeps
        // its own DAS progress.
        for tick in 1..9 {
            assert_eq!(s.step(false, true, false, false, 9, 2), None, "tick {tick}");
        }
        assert_eq!(
            s.step(false, true, false, false, 9, 2),
            Some(ShiftDir::Right)
        );
        // Releasing the focus stops everything and clears the focus.
        assert_eq!(s.step(false, false, false, false, 9, 2), None);
        assert_eq!(s.focus(), None);
    }

    #[test]
    fn soft_drop_is_das_free_multiplier_cadence() {
        let mut soft = RepeatTimer::with_ticks(0, soft_drop_period_ticks(20, 60));
        assert!(soft.press());
        assert!(!soft.advance());
        assert!(!soft.advance());
        assert!(soft.advance(), "fires every 3rd tick (×20 at 60 Hz)");
        assert!(!soft.advance());
        assert!(!soft.advance());
        assert!(soft.advance());
        soft.release();
        assert!(!soft.advance());
    }

    // ---- integration: InputPlugin + CoreBridgePlugin, headless ----

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, InputPlugin));
        app.insert_non_send(GameCore::new(0xBEEF));
        app.init_resource::<AppState>()
            .init_resource::<Settings>()
            .init_resource::<RebindingCapture>();
        app
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
    }

    fn release(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(key);
    }

    fn set_state(app: &mut App, state: AppState) {
        *app.world_mut().resource_mut::<AppState>() = state;
    }

    /// One fixed step, mirroring the real order: `FixedPreUpdate` (input)
    /// then `FixedUpdate` (core bridge drain). Returns the actions the
    /// input system queued for this step.
    fn fixed_step(app: &mut App) -> Vec<Action> {
        let _ = app.world_mut().try_run_schedule(FixedPreUpdate);
        let queued: Vec<Action> = app
            .world()
            .resource::<PendingActions>()
            .queue
            .iter()
            .copied()
            .collect();
        let _ = app.world_mut().try_run_schedule(FixedUpdate);
        queued
    }

    fn scroll(app: &mut App, y: f32) {
        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Line,
            x: 0.0,
            y,
            window: Entity::PLACEHOLDER,
            phase: TouchPhase::Moved,
        });
    }

    #[test]
    fn left_tap_emits_exactly_one_moveleft() {
        let mut app = test_app();
        press(&mut app, KeyCode::ArrowLeft);
        assert_eq!(fixed_step(&mut app), vec![Action::MoveLeft]);
        // Still held (ButtonInput keeps the press), but inside the DAS
        // window: no repeat before tick 9.
        for step in 2..8 {
            assert_eq!(fixed_step(&mut app), Vec::new(), "DAS tick {step}");
        }
        release(&mut app, KeyCode::ArrowLeft);
        assert_eq!(fixed_step(&mut app), Vec::new());
    }

    #[test]
    fn held_left_matches_das_arr_math() {
        // Settings defaults 150/33 → DAS 9 ticks, ARR 2 ticks. The press
        // consumes step 1; the first repeat lands 9 ticks later (exactly
        // 150 ms after the press action): steps 1, 10, 12, 14, 16.
        let mut app = test_app();
        press(&mut app, KeyCode::ArrowLeft);
        let mut emitted = Vec::new();
        for step in 1..=16 {
            if !fixed_step(&mut app).is_empty() {
                emitted.push(step);
            }
        }
        assert_eq!(emitted, vec![1, 10, 12, 14, 16]);
    }

    #[test]
    fn soft_drop_release_stops_repeat_forever() {
        // Regression: the soft RepeatTimer was never released on key-up,
        // so one tap kept emitting SoftDrop at the held cadence forever
        // (permanent speed-up). Press → cadence → release → total silence.
        let mut app = test_app();
        press(&mut app, KeyCode::ArrowDown);
        let mut emitted = Vec::new();
        for step in 1..=10 {
            if fixed_step(&mut app).contains(&Action::SoftDrop) {
                emitted.push(step);
            }
        }
        assert_eq!(
            emitted,
            vec![1, 4, 7, 10],
            "held soft drop repeats every 3rd tick (×20 at 60 Hz)"
        );
        release(&mut app, KeyCode::ArrowDown);
        for step in 11..=40 {
            assert!(
                !fixed_step(&mut app).contains(&Action::SoftDrop),
                "soft drop emitted at step {step} after release"
            );
        }
        // A fresh tap starts a fresh cadence (not a resumed one).
        press(&mut app, KeyCode::ArrowDown);
        assert!(fixed_step(&mut app).contains(&Action::SoftDrop));
    }

    #[test]
    fn one_shot_actions_trigger_once_per_press() {
        let mut app = test_app();
        press(&mut app, KeyCode::Space);
        press(&mut app, KeyCode::ArrowUp);
        press(&mut app, KeyCode::KeyC);
        let first = fixed_step(&mut app);
        assert_eq!(
            first,
            vec![Action::HardDrop, Action::RotateCw, Action::Hold],
            "each one-shot action fires once, in system order"
        );
        for step in 2..6 {
            assert_eq!(fixed_step(&mut app), Vec::new(), "held step {step}");
        }
    }

    #[test]
    fn wheel_notches_map_to_rotations() {
        let mut app = test_app();
        scroll(&mut app, 1.0);
        assert_eq!(fixed_step(&mut app), vec![Action::RotateCw]);
        scroll(&mut app, -1.0);
        assert_eq!(fixed_step(&mut app), vec![Action::RotateCcw]);
    }

    #[test]
    fn nothing_emits_outside_playing_state() {
        let mut app = test_app();
        set_state(&mut app, AppState::Paused);
        press(&mut app, KeyCode::ArrowLeft);
        for step in 1..12 {
            assert_eq!(fixed_step(&mut app), Vec::new(), "step {step}");
        }
        assert!(
            app.world().resource::<PendingActions>().queue.is_empty(),
            "gated inputs never leak into the core queue"
        );
    }

    #[test]
    fn nothing_emits_while_rebinding_capture_is_active() {
        let mut app = test_app();
        app.world_mut().resource_mut::<RebindingCapture>().capturing = true;
        press(&mut app, KeyCode::ArrowLeft);
        press(&mut app, KeyCode::Space);
        scroll(&mut app, 1.0);
        for step in 1..12 {
            assert_eq!(fixed_step(&mut app), Vec::new(), "step {step}");
        }
        // Gate reopens while the key is still held: no stale press edge,
        // so the next emission is the DAS repeat at tick 9 of the hold.
        app.world_mut().resource_mut::<RebindingCapture>().capturing = false;
        assert_eq!(fixed_step(&mut app), Vec::new(), "no stale edge");
    }

    #[test]
    fn queued_action_is_drained_by_the_same_core_step() {
        let mut app = test_app();
        let col_before = app
            .world()
            .non_send::<GameCore>()
            .game
            .snapshot()
            .active
            .expect("piece spawns at tick 0")
            .col;
        press(&mut app, KeyCode::ArrowLeft);
        assert_eq!(fixed_step(&mut app), vec![Action::MoveLeft]);
        assert!(
            app.world().resource::<PendingActions>().queue.is_empty(),
            "the very next fixed step drains the queue"
        );
        let col_after = app
            .world()
            .non_send::<GameCore>()
            .game
            .snapshot()
            .active
            .expect("piece still active")
            .col;
        assert_eq!(
            col_after,
            col_before - 1,
            "the MoveLeft action reached the core"
        );
    }

    // ---- versus preset selection ----

    /// Versus-active app: `VersusMatch` live with the given controllers,
    /// `AppState::Playing`. Only `FixedPreUpdate` is ever run by these
    /// tests, so the queues keep what the input systems pushed.
    fn versus_test_app(p1: Controller, p2: Controller) -> App {
        // `CoreBridgePlugin` already hosts `VersusMatch` (inactive by
        // default); these tests just arm it and run `FixedPreUpdate`, so
        // the queues keep whatever the input systems pushed.
        let mut app = test_app();
        {
            let mut versus = app.world_mut().non_send_mut::<VersusMatch>();
            versus.active = true;
            versus.p1 = p1;
            versus.p2 = p2;
        }
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        app
    }

    fn versus_queues(app: &App) -> (Vec<Action>, Vec<Action>) {
        let actions = app.world().resource::<VersusActions>();
        (actions.left.clone(), actions.right.clone())
    }

    fn step_pre(app: &mut App) {
        let _ = app.world_mut().try_run_schedule(FixedPreUpdate);
    }

    #[test]
    fn lone_human_vs_bot_drives_with_arrows_not_wasd() {
        // Regression: the menu starts Human-vs-Bot with the human on P1,
        // whose fixed preset is WASD — the solo muscle-memory arrow keys
        // did nothing. A lone human must get the arrow preset instead.
        let mut app = versus_test_app(Controller::Human, Controller::Bot);
        press(&mut app, KeyCode::ArrowLeft);
        press(&mut app, KeyCode::Space);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            left,
            vec![Action::MoveLeft, Action::HardDrop],
            "arrows and space drive the lone human seat"
        );
        assert_eq!(right, Vec::new(), "the bot side never takes keyboard");

        // WASD is not bound to the lone human seat.
        release(&mut app, KeyCode::ArrowLeft);
        release(&mut app, KeyCode::Space);
        press(&mut app, KeyCode::KeyA);
        press(&mut app, KeyCode::KeyW);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            (left, right),
            (vec![Action::MoveLeft, Action::HardDrop], Vec::new()),
            "WASD stays silent while the human plays alone"
        );
    }

    #[test]
    fn two_humans_keep_the_classic_wasd_and_arrow_split() {
        let mut app = versus_test_app(Controller::Human, Controller::Human);
        press(&mut app, KeyCode::KeyA);
        press(&mut app, KeyCode::ArrowLeft);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(left, vec![Action::MoveLeft], "P1 keeps WASD");
        assert_eq!(right, vec![Action::MoveLeft], "P2 keeps arrows");
    }

    #[test]
    fn versus_edges_refire_after_release() {
        // Regression: `held && !mem::replace(prev, held)` short-circuits on
        // release, so `prev` latched to `true` and only the FIRST press of
        // every versus key ever queued an action (one hard drop per match,
        // rotation dead after one use). Every press after a release must
        // fire again.
        let mut app = versus_test_app(Controller::Human, Controller::Bot);
        press(&mut app, KeyCode::Space);
        step_pre(&mut app);
        release(&mut app, KeyCode::Space);
        step_pre(&mut app);
        press(&mut app, KeyCode::KeyX);
        step_pre(&mut app);
        release(&mut app, KeyCode::KeyX);
        step_pre(&mut app);
        press(&mut app, KeyCode::KeyC);
        step_pre(&mut app);
        release(&mut app, KeyCode::KeyC);
        step_pre(&mut app);
        press(&mut app, KeyCode::ArrowLeft);
        step_pre(&mut app);
        release(&mut app, KeyCode::ArrowLeft);
        step_pre(&mut app);
        press(&mut app, KeyCode::ArrowLeft);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            left,
            vec![
                Action::HardDrop,
                Action::RotateCw,
                Action::Hold,
                Action::MoveLeft,
                Action::MoveLeft,
            ],
            "second presses of hard drop, rotate, hold and move all re-fire"
        );
        assert_eq!(right, Vec::new(), "the bot side never takes keyboard");
    }

    // ---- N4 netplay seat-pair preset routing (netplay-plan.md) ----

    /// Host seat pair `Human+Net`: the local (left) seat drives with the P1
    /// (WASD) preset — *not* the lone-human arrow preset — and the remote
    /// (Net) queue is never fed by local keys.
    #[test]
    fn net_host_seat_pair_uses_p1_wasd_and_never_the_remote_queue() {
        let mut app = versus_test_app(Controller::Human, Controller::Net);
        press(&mut app, KeyCode::KeyA);
        press(&mut app, KeyCode::KeyQ);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            left,
            vec![Action::MoveLeft, Action::Hold],
            "host left seat takes the P1 WASD preset"
        );
        assert!(right.is_empty(), "the remote (Net) queue is never touched");

        // Arrows belong to the absent P2 seat: they must not drive the host's
        // left side — the assertion that fails *before* the lone-human fix
        // (which would hand the host the arrow/solo preset).
        let mut app = versus_test_app(Controller::Human, Controller::Net);
        press(&mut app, KeyCode::ArrowLeft);
        press(&mut app, KeyCode::Comma);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert!(left.is_empty(), "arrows are not the host's WASD preset");
        assert!(right.is_empty());
    }

    /// Guest seat pair `Net+Human`: the local (right) seat drives with the P2
    /// (arrow) preset and the remote (Net) queue is never fed by local keys.
    #[test]
    fn net_guest_seat_pair_uses_p2_arrows_and_never_the_remote_queue() {
        let mut app = versus_test_app(Controller::Net, Controller::Human);
        press(&mut app, KeyCode::ArrowLeft);
        press(&mut app, KeyCode::Comma);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            right,
            vec![Action::MoveLeft, Action::Hold],
            "guest right seat takes the P2 arrow preset"
        );
        assert!(left.is_empty(), "the remote (Net) queue is never touched");

        // WASD belongs to the absent P1 seat: must not drive the guest's right.
        let mut app = versus_test_app(Controller::Net, Controller::Human);
        press(&mut app, KeyCode::KeyA);
        press(&mut app, KeyCode::KeyQ);
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert!(right.is_empty(), "WASD is not the guest's P2 preset");
        assert!(left.is_empty());
    }

    /// Regression guard for the `Net`-seat-occupied fix: a `Bot` seat must NOT
    /// count as occupying P2, so a lone human vs the Bot still gets the
    /// arrow + solo-alternate preset (Space hard drop, X/Z rotates, C/Shift
    /// hold) on the left seat.
    #[test]
    fn lone_human_vs_bot_still_gets_arrow_and_solo_alternate_preset() {
        let mut app = versus_test_app(Controller::Human, Controller::Bot);
        press(&mut app, KeyCode::Space); // solo-alternate hard drop
        press(&mut app, KeyCode::KeyX); // solo-alternate rotate cw
        press(&mut app, KeyCode::KeyC); // solo-alternate hold
        step_pre(&mut app);
        let (left, right) = versus_queues(&app);
        assert_eq!(
            left,
            vec![Action::HardDrop, Action::RotateCw, Action::Hold],
            "vs Bot the lone human keeps the arrow+solo-alternate preset on the left seat"
        );
        assert!(right.is_empty(), "the bot side never takes keyboard");
    }
}
