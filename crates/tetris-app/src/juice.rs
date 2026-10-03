//! Juice: line-clear flash, freeze frames, shake (T19).
//!
//! Presentation-only reaction to core events (PRD §8, plan T19). All effect
//! state lives in the pure [`JuiceState`] resource as data-driven timers that
//! tick in real render time (`Update`), so they run unaffected by
//! [`SimPaused`] while a freeze is active. Systems read the decayed values and
//! write them to a full-screen white overlay sprite and a [`Camera2d`]
//! transform; every channel decays to *exactly* zero, so the camera and
//! overlay always settle back to their resting state (no drift).
//!
//! Freeze semantics (plan T19: "sim freeze via the T10 `SimPaused` bridge
//! gate, ≤4 ticks ≤80 ms @ 60 Hz", PRD §8 "brief freeze ≤80 ms"): a freeze
//! toggles [`SimPaused`] for **exactly N frames** (N ≤ 4; each owned render
//! frame at 60 Hz covers one skipped fixed sim step) through the
//! [`JuiceFreeze`] guard. The guard only ever writes the [`SimPaused`] value
//! it set itself: if the sim was already paused when a freeze was requested
//! (T17 pause), juice never touches the flag and never burns its ticks until
//! the external pause lifts. On release it writes `false` exactly once, even
//! if external code toggled the flag meanwhile — juice owns ≤80 ms of the
//! flag and T17 re-asserts a genuine pause afterwards if needed (documented
//! choice, covered by `freeze_guard_owns_and_restores_exactly_once_...`).
//!
//! Quality gating (`Settings.effects`, plan: "Low disables shake/freeze"):
//! Low keeps only attenuated flashes for meaningful events (per T1's
//! `EffectsQuality` doc: "No shake, no freeze frames"), Medium is the
//! balanced default, High the full PRD budget. Concurrent triggers on the
//! same channel take the **max** (a stronger effect replaces a weaker one);
//! nothing ever sums.
//!
//! Deferred: a dedicated hard-drop *shake* — `GameEvent` has no hard-drop
//! variant, so PRD's hard-drop feel is approximated by the lock flash and the
//! clear shakes; if a future core revision emits a drop-strength event, add a
//! shake row to [`effect_plan`]. Move-action coalescing on freeze exit is an
//! input-layer concern (T12), not juice.

use bevy::prelude::*;
use bevy::render::view::window::screenshot::{save_to_disk, Screenshot};
use bevy::window::Window;

use tetris_core::event::GameEvent;

use crate::core_bridge::{CoreEvent, SimPaused, VersusMatch};
use crate::render;
use crate::state::{EffectsQuality, Settings};

/// One decaying effect channel: `amplitude` decays to exactly `0.0` over
/// `duration` seconds once triggered.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EffectSlot {
    /// Seconds elapsed since the active trigger.
    pub elapsed: f32,
    /// Total lifetime of the active trigger, seconds (0 = inactive).
    pub duration: f32,
    /// Peak amplitude at trigger time.
    pub amplitude: f32,
}

impl EffectSlot {
    /// `true` while the slot still has time left.
    pub fn is_active(&self) -> bool {
        self.duration > 0.0 && self.elapsed < self.duration
    }

    /// Current decayed value: quadratic ease-out, monotonic, exactly 0 once
    /// expired.
    pub fn value(&self) -> f32 {
        if !self.is_active() {
            return 0.0;
        }
        let p = (self.elapsed / self.duration).clamp(0.0, 1.0);
        self.amplitude * (1.0 - p) * (1.0 - p)
    }

    /// Trigger with `amplitude`/`duration` using **max-of** stacking: only a
    /// strictly stronger amplitude (re)starts the timer; concurrent triggers
    /// never add up. Returns `true` when the slot (re)triggered.
    pub fn trigger(&mut self, amplitude: f32, duration: f32) -> bool {
        if amplitude <= 0.0 || duration <= 0.0 {
            return false;
        }
        if !self.is_active() || amplitude > self.amplitude {
            *self = Self {
                elapsed: 0.0,
                duration,
                amplitude,
            };
            true
        } else {
            false
        }
    }

    /// Advance real time by `dt` and fully reset an expired slot.
    pub fn tick(&mut self, dt: f32) {
        if !self.is_active() {
            *self = Self::default();
            return;
        }
        self.elapsed += dt;
        if !self.is_active() {
            *self = Self::default();
        }
    }
}

/// Per-event effect recipe (amplitude, duration) per channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectPlan {
    /// Overlay flash (peak alpha in `0.0..=1.0`, seconds).
    pub flash: (f32, f32),
    /// Camera shake (peak amplitude in world px, seconds).
    pub shake: (f32, f32),
    /// Sim freeze length in frames (≤ 4 = ≤80 ms @ 60 Hz, plan T19 cap).
    pub freeze_frames: u32,
}

/// Data-driven effect matrix: event × quality → channels. Freeze is capped at
/// 4 frames (plan validation). Low disables shake/freeze entirely and keeps
/// attenuated flashes only (T1 `EffectsQuality` semantics).
pub fn effect_plan(event: &GameEvent, quality: EffectsQuality) -> EffectPlan {
    let off = EffectPlan {
        flash: (0.0, 0.0),
        shake: (0.0, 0.0),
        freeze_frames: 0,
    };
    use EffectsQuality::*;
    if let GameEvent::LineCleared { lines } = event {
        let n = *lines as f32;
        let lines = *lines as u32;
        let (flash, shake, freeze_frames) = match quality {
            Low => ((0.2 + 0.06 * n, 0.08 + 0.02 * n), (0.0, 0.0), 0),
            Medium => (
                (0.3 + 0.10 * n, 0.10 + 0.02 * n),
                (1.5 * n, 0.12 + 0.03 * n),
                lines.min(3),
            ),
            High => (
                (0.35 + 0.13 * n, 0.12 + 0.03 * n),
                (3.0 * n, 0.20 + 0.05 * n),
                (lines + 1).min(4),
            ),
        };
        return EffectPlan {
            flash,
            shake,
            freeze_frames,
        };
    }
    match (event, quality) {
        (GameEvent::TSpinDetected { .. }, Low) => EffectPlan {
            flash: (0.30, 0.12),
            ..off
        },
        (GameEvent::TSpinDetected { .. }, Medium) => EffectPlan {
            flash: (0.50, 0.18),
            shake: (5.0, 0.20),
            freeze_frames: 2,
        },
        (GameEvent::TSpinDetected { .. }, High) => EffectPlan {
            flash: (0.65, 0.22),
            shake: (8.0, 0.30),
            freeze_frames: 3,
        },
        (GameEvent::PerfectClear, Low) => EffectPlan {
            flash: (0.50, 0.20),
            ..off
        },
        (GameEvent::PerfectClear, Medium) => EffectPlan {
            flash: (0.80, 0.35),
            shake: (8.0, 0.35),
            freeze_frames: 4,
        },
        (GameEvent::PerfectClear, High) => EffectPlan {
            flash: (1.00, 0.45),
            shake: (12.0, 0.50),
            freeze_frames: 4,
        },
        (GameEvent::LevelUp { .. }, Low) => EffectPlan {
            flash: (0.20, 0.15),
            ..off
        },
        (GameEvent::LevelUp { .. }, Medium) => EffectPlan {
            flash: (0.40, 0.20),
            shake: (2.0, 0.15),
            ..off
        },
        (GameEvent::LevelUp { .. }, High) => EffectPlan {
            flash: (0.50, 0.25),
            shake: (3.0, 0.20),
            ..off
        },
        (GameEvent::GameOver, Low) => EffectPlan {
            flash: (0.40, 0.40),
            ..off
        },
        (GameEvent::GameOver, Medium) => EffectPlan {
            flash: (0.60, 0.50),
            shake: (4.0, 0.40),
            ..off
        },
        (GameEvent::GameOver, High) => EffectPlan {
            flash: (0.75, 0.60),
            shake: (6.0, 0.60),
            ..off
        },
        // PRD §8 lock-flash on every lock (no dedicated hard-drop event
        // exists; see module notes on the hard-drop shake deferral).
        (GameEvent::PieceLocked { .. }, Low) => off,
        (GameEvent::PieceLocked { .. }, Medium) => EffectPlan {
            flash: (0.12, 0.05),
            ..off
        },
        (GameEvent::PieceLocked { .. }, High) => EffectPlan {
            flash: (0.16, 0.06),
            shake: (1.0, 0.06),
            ..off
        },
        _ => off,
    }
}

/// Flash tint per event kind (M5): a color-coded screen flash reads as
/// game juice instead of a white "display glitch", and each hue carries the
/// event's meaning (gold = perfect clear, violet = T-spin, red = game over).
pub fn flash_color_for(event: &GameEvent) -> Color {
    match event {
        GameEvent::LineCleared { lines } => {
            // Bigger clears trend from pale cyan toward bright ice.
            let t = (*lines as f32 / 4.0).min(1.0);
            Color::srgb(0.62 + 0.18 * t, 0.88 + 0.07 * t, 1.0)
        }
        GameEvent::TSpinDetected { .. } => Color::srgb(0.85, 0.55, 1.0),
        GameEvent::PerfectClear => Color::srgb(1.0, 0.9, 0.55),
        GameEvent::LevelUp { .. } => Color::srgb(0.7, 1.0, 0.78),
        GameEvent::GameOver => Color::srgb(1.0, 0.42, 0.38),
        _ => Color::WHITE,
    }
}

/// Pure juice state: decaying effect channels + the last camera offset applied
/// (delta application keeps the camera drift-free and composes with any
/// external transform writes).
#[derive(Debug, Clone, Copy, Resource, PartialEq)]
pub struct JuiceState {
    /// Full-screen flash channel (peak alpha).
    pub flash: EffectSlot,
    /// Camera shake channel (peak amplitude in world px).
    pub shake: EffectSlot,
    /// Tint of the active flash ([`flash_color_for`], set on trigger).
    pub flash_color: Color,
    /// Shake offset currently applied to the camera (rest = zero).
    pub shake_offset: Vec2,
}

impl JuiceState {
    /// Apply `event`'s plan for `quality`; returns the requested freeze
    /// length in frames (caller merges into [`JuiceFreeze`]).
    pub fn trigger(&mut self, event: &GameEvent, quality: EffectsQuality) -> u32 {
        let plan = effect_plan(event, quality);
        if self.flash.trigger(plan.flash.0, plan.flash.1) {
            self.flash_color = flash_color_for(event);
        }
        self.shake.trigger(plan.shake.0, plan.shake.1);
        plan.freeze_frames
    }

    /// Advance all channels by real frame time.
    pub fn tick(&mut self, dt: f32) {
        self.flash.tick(dt);
        self.shake.tick(dt);
    }

    /// Current shake translation, zero once settled. Deterministic time-based
    /// noise bounded by the decayed amplitude.
    pub fn shake_offset(&self) -> Vec2 {
        let a = self.shake.value();
        if a <= 0.0 {
            return Vec2::ZERO;
        }
        let t = self.shake.elapsed;
        Vec2::new((t * 1337.0).sin(), (t * 1571.0).cos()) * a
    }
}

impl Default for JuiceState {
    fn default() -> Self {
        Self {
            flash: EffectSlot::default(),
            shake: EffectSlot::default(),
            flash_color: Color::WHITE,
            shake_offset: Vec2::ZERO,
        }
    }
}

/// Freeze guard resource: counts pending freeze frames and remembers whether
/// juice currently owns the [`SimPaused`] flag, so it only ever restores the
/// value it set itself.
#[derive(Debug, Clone, Copy, Default, Resource, PartialEq)]
pub struct JuiceFreeze {
    /// Frames of sim freeze still owed (each owned frame covers one skipped
    /// fixed step; capped at 4 by [`effect_plan`]).
    pub frames_remaining: u32,
    /// `true` while juice has set `SimPaused(true)` and must release it.
    pub owns_pause: bool,
}

/// Marker for the full-screen flash overlay entity.
#[derive(Component)]
pub struct JuiceOverlay;

/// Drain [`CoreEvent`]s into [`JuiceState`] + [`JuiceFreeze`] per quality.
fn juice_from_events(
    mut events: MessageReader<CoreEvent>,
    settings: Res<Settings>,
    mut state: ResMut<JuiceState>,
    mut freeze: ResMut<JuiceFreeze>,
) {
    for event in events.read() {
        let frames = state.trigger(&event.0, settings.effects);
        freeze.frames_remaining = freeze.frames_remaining.max(frames);
    }
}

/// Core of [`freeze_gate`], pure over the flag for unit testing.
fn apply_freeze(paused: &mut bool, freeze: &mut JuiceFreeze) {
    if freeze.frames_remaining > 0 {
        if !*paused && !freeze.owns_pause {
            *paused = true;
            freeze.owns_pause = true;
        }
        if freeze.owns_pause {
            freeze.frames_remaining -= 1;
        }
    } else if freeze.owns_pause {
        *paused = false;
        freeze.owns_pause = false;
    }
}

/// Toggle `SimPaused` for exactly `frames_remaining` owned frames. Never
/// fights an external pause: it only takes ownership when the flag is free,
/// and only writes `false` (exactly once) when releasing what it set.
fn freeze_gate(mut sim_paused: ResMut<SimPaused>, mut freeze: ResMut<JuiceFreeze>) {
    apply_freeze(&mut sim_paused.0, &mut freeze);
}

/// Tick the timers in real time and paint the decayed values onto the overlay
/// and the camera. Runs in `Update`, so SimPaused never stops the decay.
fn juice_painter(
    time: Res<Time>,
    mut state: ResMut<JuiceState>,
    mut overlays: Query<&mut Sprite, With<JuiceOverlay>>,
    mut cameras: Query<&mut Transform, With<Camera2d>>,
) {
    state.tick(time.delta_secs());

    let alpha = state.flash.value();
    let tinted = state.flash_color.with_alpha(alpha);
    for mut sprite in overlays.iter_mut() {
        sprite.color = tinted;
    }

    let new_offset = state.shake_offset();
    let delta = new_offset - state.shake_offset;
    if delta != Vec2::ZERO {
        for mut transform in cameras.iter_mut() {
            transform.translation.x += delta.x;
            transform.translation.y += delta.y;
        }
        state.shake_offset = new_offset;
    }
}

/// M5: hug the flash to the playfield instead of the whole window — solo
/// covers the well plus its frame, versus the shared playfield view (both
/// halves). Keeps pillar-boxed desktop windows and the portrait HUD/touch
/// strips out of the flash so it reads as the field reacting. Headless (no
/// window) is a no-op; the overlay keeps its oversized resting rect there.
fn shape_overlay(
    windows: Query<&Window>,
    versus: Option<NonSend<VersusMatch>>,
    mut overlays: Query<(&mut Sprite, &mut Transform), With<JuiceOverlay>>,
) {
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let (view_w, view_h, center_y) = render::playfield_view(size.x, size.y);
    let active_versus = versus.is_some_and(|v| v.active);
    let (w, h) = if active_versus {
        (view_w, view_h)
    } else {
        let (cell, _, _) = render::letterbox(view_w, view_h);
        let pad = 0.7 * cell;
        (
            tetris_core::board::COLS as f32 * cell + 2.0 * pad,
            render::VISIBLE_ROWS as f32 * cell + 2.0 * pad,
        )
    };
    for (mut sprite, mut transform) in overlays.iter_mut() {
        if sprite.custom_size != Some(Vec2::new(w, h)) {
            sprite.custom_size = Some(Vec2::new(w, h));
        }
        if transform.translation.y != center_y {
            transform.translation.y = center_y;
        }
    }
}

/// Spawn the always-present (alpha-0) flash overlay above every playfield
/// layer (board 0 / ghost 1 / active 2, T11). [`shape_overlay`] hugs it to
/// the active field(s) every frame; the oversized rect is only its resting
/// size for headless apps without a window.
fn spawn_overlay(mut commands: Commands) {
    commands.spawn((
        JuiceOverlay,
        Sprite {
            color: Color::WHITE.with_alpha(0.0),
            custom_size: Some(Vec2::splat(4096.0)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, 10.0),
    ));
}

/// Flash / freeze / shake reaction to core events (PRD §8, T19).
pub struct JuicePlugin;

impl Plugin for JuicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<JuiceState>()
            .init_resource::<JuiceFreeze>()
            // No-ops when the owning plugins already registered these.
            .init_resource::<Settings>()
            .init_resource::<SimPaused>()
            .add_message::<CoreEvent>()
            .add_systems(Startup, spawn_overlay)
            .add_systems(
                Update,
                (juice_from_events, freeze_gate, shape_overlay, juice_painter).chain(),
            )
            .add_systems(Update, apply_window_identity);
        if let Some(schedule) = parse_shot_schedule() {
            app.insert_resource(schedule)
                .add_systems(Update, schedule_shots);
        }
    }
}

/// T21 (PRD §14 #1): single source of truth for the user-visible app name
/// ("Blockfall"). `main.rs` is frozen with the placeholder title `tetris`,
/// so the display rename is applied from this registered plugin. The
/// compare-first guard keeps `Window` change detection quiet once applied.
fn apply_window_identity(mut windows: Query<&mut Window>) {
    const TITLE: &str = "Blockfall";
    for mut window in &mut windows {
        if window.title != TITLE {
            window.title = TITLE.into();
        }
    }
}

/// Env hook (T21 screenshot capture): `TETRIS_SHOT=/abs/a.png@90,/abs/b.png@1200`
/// captures the primary window at the given `Update` frame numbers using
/// Bevy 0.19's built-in [`Screenshot`] component + `save_to_disk` observer
/// (no external CLI needed on Wayland). Unset in normal runs: zero cost.
const SHOT_ENV: &str = "TETRIS_SHOT";

#[derive(Resource)]
struct ShotSchedule {
    frame: u64,
    remaining: Vec<(u64, String)>,
}

fn parse_shot_schedule() -> Option<ShotSchedule> {
    let raw = std::env::var_os(SHOT_ENV)?.to_str()?.to_owned();
    let mut remaining = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (path, frame) = item.split_once('@')?;
        remaining.push((frame.parse().ok()?, path.to_owned()));
    }
    if remaining.is_empty() {
        return None;
    }
    Some(ShotSchedule {
        frame: 0,
        remaining,
    })
}

fn schedule_shots(mut schedule: ResMut<ShotSchedule>, mut commands: Commands) {
    schedule.frame += 1;
    let frame = schedule.frame;
    schedule.remaining.retain(|(due, path)| {
        if *due != frame {
            return true;
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        false
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::CoreBridgePlugin;
    use crate::state::AppState;
    use tetris_core::tspin::TSpinKind;

    const TICK: f32 = 1.0 / 60.0;

    fn settle(state: &mut JuiceState, frames: u32) {
        for _ in 0..frames {
            state.tick(TICK);
        }
    }

    // ---------- pure JuiceState unit tests ----------

    #[test]
    fn line_clear_scales_with_line_count_and_quality() {
        for quality in [EffectsQuality::Medium, EffectsQuality::High] {
            let p1 = effect_plan(&GameEvent::LineCleared { lines: 1 }, quality);
            let p4 = effect_plan(&GameEvent::LineCleared { lines: 4 }, quality);
            assert!(
                p4.flash.0 > p1.flash.0 && p4.flash.1 >= p1.flash.1,
                "tetris flash must beat single flash at {quality:?}"
            );
            assert!(
                p4.shake.0 > p1.shake.0,
                "tetris shake bigger at {quality:?}"
            );
        }
        let p1 = effect_plan(&GameEvent::LineCleared { lines: 1 }, EffectsQuality::Low);
        let p4 = effect_plan(&GameEvent::LineCleared { lines: 4 }, EffectsQuality::Low);
        assert!(p4.flash.0 > p1.flash.0, "Low flash still scales");
        assert_eq!((p1.shake.0, p4.shake.0), (0.0, 0.0), "Low has zero shake");

        // Quality monotonicity: High >= Medium >= Low on every channel.
        for event in [
            GameEvent::LineCleared { lines: 4 },
            GameEvent::PerfectClear,
            GameEvent::GameOver,
        ] {
            let low = effect_plan(&event, EffectsQuality::Low);
            let med = effect_plan(&event, EffectsQuality::Medium);
            let high = effect_plan(&event, EffectsQuality::High);
            assert!(low.flash.0 <= med.flash.0 && med.flash.0 <= high.flash.0);
            assert!(low.shake.0 <= med.shake.0 && med.shake.0 <= high.shake.0);
            assert!(low.freeze_frames <= med.freeze_frames);
            assert!(med.freeze_frames <= high.freeze_frames);
        }
        // Low: zero shake/freeze, non-zero flash for meaningful events.
        let low = effect_plan(&GameEvent::PerfectClear, EffectsQuality::Low);
        assert_eq!((low.shake.0, low.freeze_frames), (0.0, 0));
        assert!(low.flash.0 > 0.0);
        // Medium default shakes + freezes where High does.
        let med = effect_plan(
            &GameEvent::TSpinDetected {
                kind: TSpinKind::Full,
            },
            EffectsQuality::Medium,
        );
        assert!(med.shake.0 > 0.0 && med.freeze_frames > 0);
    }

    #[test]
    fn freeze_never_exceeds_four_frames() {
        for quality in [
            EffectsQuality::Low,
            EffectsQuality::Medium,
            EffectsQuality::High,
        ] {
            for event in [
                GameEvent::LineCleared { lines: 4 },
                GameEvent::PerfectClear,
                GameEvent::TSpinDetected {
                    kind: TSpinKind::Mini,
                },
                GameEvent::LevelUp { level: 5 },
                GameEvent::GameOver,
            ] {
                assert!(
                    effect_plan(&event, quality).freeze_frames <= 4,
                    "plan T19 cap violated: {event:?} @ {quality:?}"
                );
            }
        }
    }

    #[test]
    fn decay_is_monotonic_and_exactly_zero_after_duration() {
        let mut slot = EffectSlot::default();
        slot.trigger(1.0, 0.3);
        let mut prev = f32::MAX;
        for _ in 0..18 {
            let v = slot.value();
            assert!(v <= prev + 1e-6, "non-monotonic decay: {v} > {prev}");
            prev = v;
            slot.tick(TICK);
        }
        assert_eq!(slot.value(), 0.0, "fully settled after duration");
        assert_eq!(slot, EffectSlot::default(), "slot reset, no drift");
    }

    #[test]
    fn concurrent_triggers_take_max_not_sum() {
        let mut state = JuiceState::default();
        state.trigger(&GameEvent::PerfectClear, EffectsQuality::High);
        let strong = (state.flash.amplitude, state.shake.amplitude);
        state.trigger(&GameEvent::LineCleared { lines: 1 }, EffectsQuality::High);
        assert_eq!(
            (state.flash.amplitude, state.shake.amplitude),
            strong,
            "weaker trigger must not add to a stronger one"
        );
        state.trigger(
            &GameEvent::TSpinDetected {
                kind: TSpinKind::Full,
            },
            EffectsQuality::Low,
        );
        assert_eq!(state.flash.amplitude, strong.0, "Low flash cannot replace");
        // A strictly stronger later trigger does replace.
        state.trigger(&GameEvent::GameOver, EffectsQuality::High);
        assert!(state.shake.amplitude <= strong.1);
    }

    #[test]
    fn state_settles_to_zero_offset() {
        let mut state = JuiceState {
            shake_offset: Vec2::new(3.0, -4.0),
            ..default()
        };
        state.trigger(&GameEvent::LineCleared { lines: 4 }, EffectsQuality::High);
        assert!(state.shake_offset().length() > 0.0);
        settle(&mut state, 60);
        assert_eq!(state.shake.value(), 0.0);
        assert_eq!(state.shake_offset(), Vec2::ZERO);
    }

    #[test]
    fn freeze_guard_owns_and_restores_exactly_once_through_external_toggles() {
        // Documented choice: juice takes the flag only while free, keeps it
        // for its frame budget, and writes `false` exactly once at release —
        // even if external code toggled the flag meanwhile (T17 is expected
        // to re-assert a genuine pause after juice releases).
        let mut paused = SimPaused(false);
        let mut freeze = JuiceFreeze {
            frames_remaining: 4,
            ..JuiceFreeze::default()
        };

        apply_freeze(&mut paused.0, &mut freeze);
        assert!(paused.0 && freeze.owns_pause, "guard took ownership");
        assert_eq!(freeze.frames_remaining, 3);

        // External code fights the flag during the freeze window.
        paused.0 = false;
        paused.0 = true;
        for _ in 0..3 {
            apply_freeze(&mut paused.0, &mut freeze);
        }
        assert_eq!(freeze.frames_remaining, 0);
        assert!(freeze.owns_pause, "still owed one release");

        apply_freeze(&mut paused.0, &mut freeze);
        assert!(!paused.0 && !freeze.owns_pause, "released once");

        // No further writes from an idle guard.
        paused.0 = true; // external pause re-asserted
        apply_freeze(&mut paused.0, &mut freeze);
        assert!(paused.0, "idle guard never touches an external pause");
    }

    #[test]
    fn freeze_guard_defers_to_external_pause() {
        let mut paused = SimPaused(true); // T17 pause already active
        let mut freeze = JuiceFreeze {
            frames_remaining: 4,
            ..JuiceFreeze::default()
        };
        apply_freeze(&mut paused.0, &mut freeze);
        assert!(paused.0 && !freeze.owns_pause);
        assert_eq!(freeze.frames_remaining, 4, "no ticks burned under a pause");
        paused.0 = false;
        apply_freeze(&mut paused.0, &mut freeze);
        assert!(paused.0 && freeze.owns_pause, "takes over once free");
    }

    // ---------- integration (headless) ----------

    fn juice_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, JuicePlugin));
        app.init_resource::<AppState>();
        app.insert_resource(Settings::default()); // Medium effects
                                                  // Keep the core from stepping (and emitting its own events).
        *app.world_mut().resource_mut::<AppState>() = AppState::Title;
        app
    }

    /// Run one deterministic frame: advance the virtual clock by exactly
    /// `TICK` and execute `Update` (TimePlugin's clock systems live in
    /// `First`, which `try_run_schedule` skips, so the clock stays under
    /// test control).
    fn step_update(app: &mut App) {
        app.world_mut()
            .resource_mut::<Time<Virtual>>()
            .advance_by(std::time::Duration::from_secs_f32(TICK));
        let generic = app.world().resource::<Time<Virtual>>().as_generic();
        *app.world_mut().resource_mut::<Time>() = generic;
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn overlay_alpha(app: &mut App) -> f32 {
        app.world_mut()
            .query::<(&JuiceOverlay, &Sprite)>()
            .iter(app.world())
            .map(|(_, s)| s.color.alpha())
            .next()
            .expect("overlay spawned")
    }

    fn camera_translation(app: &mut App) -> Vec3 {
        app.world_mut()
            .query::<(&Camera2d, &Transform)>()
            .iter(app.world())
            .map(|(_, t)| t.translation)
            .next()
            .expect("camera exists")
    }

    #[test]
    fn tetris_plus_perfect_clear_shakes_then_settles_camera_and_overlay() {
        let mut app = juice_app();
        app.update(); // Startup: overlay + camera
        assert_eq!(camera_translation(&mut app).truncate(), Vec2::ZERO);
        assert_eq!(overlay_alpha(&mut app), 0.0);

        app.world_mut()
            .resource_mut::<Messages<CoreEvent>>()
            .write_batch([
                CoreEvent(GameEvent::LineCleared { lines: 4 }),
                CoreEvent(GameEvent::PerfectClear),
            ]);

        step_update(&mut app);
        assert!(
            app.world().resource::<JuiceState>().shake.value() > 0.0,
            "shake amplitude >0 right after the events"
        );
        let peak_cam = camera_translation(&mut app).truncate().length();
        assert!(peak_cam > 0.0, "camera actually moved");
        assert!(overlay_alpha(&mut app) > 0.0, "overlay flashes");
        assert!(
            app.world().resource::<SimPaused>().0,
            "perfect clear freezes the sim"
        );

        // 4 owned freeze frames (plan T19 cap): count frames spent paused.
        let mut paused_frames = 1;
        for _ in 0..4 {
            step_update(&mut app);
            if app.world().resource::<SimPaused>().0 {
                paused_frames += 1;
            }
        }
        assert!(
            paused_frames <= 4,
            "freeze exceeded 4 ticks: {paused_frames}"
        );
        assert!(!app.world().resource::<SimPaused>().0, "freeze lifted");
        assert!(!app.world().resource::<JuiceFreeze>().owns_pause);

        // The longest Medium channel (PerfectClear 0.35 s) must fully decay:
        // ~40 more deterministic virtual frames.
        for _ in 0..40 {
            step_update(&mut app);
        }
        assert_eq!(app.world().resource::<JuiceState>().shake.value(), 0.0);
        assert_eq!(overlay_alpha(&mut app), 0.0, "overlay alpha back to zero");
        let rest = camera_translation(&mut app).truncate();
        assert!(
            rest.length() < 1e-3,
            "camera must return to origin, off by {rest:?}"
        );
    }

    #[test]
    fn low_quality_shakes_not_and_freezes_not() {
        let mut app = juice_app();
        app.world_mut().resource_mut::<Settings>().effects = EffectsQuality::Low;
        app.update();
        app.world_mut()
            .resource_mut::<Messages<CoreEvent>>()
            .write_batch([
                CoreEvent(GameEvent::LineCleared { lines: 4 }),
                CoreEvent(GameEvent::PerfectClear),
            ]);
        step_update(&mut app);
        assert!(!app.world().resource::<SimPaused>().0, "Low: no freeze");
        assert_eq!(
            app.world().resource::<JuiceState>().shake.value(),
            0.0,
            "Low: no shake"
        );
        for _ in 0..5 {
            step_update(&mut app);
        }
        assert_eq!(
            camera_translation(&mut app).truncate(),
            Vec2::ZERO,
            "Low: zero camera motion"
        );
        assert!(overlay_alpha(&mut app) > 0.0, "Low keeps attenuated flash");
    }
}
