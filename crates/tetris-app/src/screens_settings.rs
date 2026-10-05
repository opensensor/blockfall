//! Settings screen: rebinding capture, sliders, effects quality (T16).
//!
//! Bevy 0.19 immediate-mode-style retained UI: the whole screen is spawned
//! once on `Startup` under a [`SettingsRoot`] node and toggled between
//! `Visibility::Visible`/`Hidden` by the [`AppState::Settings`] variant
//! (T1's plain-resource state machine — no `States` registration). Hidden UI
//! nodes never receive `Interaction` (Bevy guarantees `Interaction::None`
//! while a node's inherited visibility is off), so the screen is inert
//! outside the Settings state even though its handlers keep running.
//!
//! Architecture: every piece of logic lives in a `pub` handler function that
//! takes plain resource references ([`begin_rebind`], [`cancel_rebind`],
//! [`apply_capture`], [`slider_value_from_pos`], [`apply_slider`],
//! [`reset_all_bindings`], [`handle_back`], cycle helpers, label formatters).
//! The systems below are thin UI glue that tests drive either by simulating
//! `Interaction::Pressed` on the spawned widgets or by calling the handlers
//! directly. Visual pointer-drag fidelity (picking coordinates through the
//! real `Pointer<Press/Drag/Release>` messages) is validated on device in
//! T14; here the drag path is exercised through the same
//! [`slider_value_from_pos`] handler the glue uses.
//!
//! ## Rebinding capture flow
//!
//! 1. Clicking a binding row ([`BindRow`]) calls [`begin_rebind`], which
//!    sets [`RebindingCapture::capturing`] — T12's emission gate reads this
//!    and mutes all gameplay `Action`s until the capture ends (this plugin
//!    never adds a second gate).
//! 2. The next frame, [`apply_capture`] consumes the first key reported by
//!    [`pressed_key_to_bind`](crate::input::pressed_key_to_bind) and writes
//!    it into the slot via [`KeyBindings::set_slot`].
//! 3. `Escape` cancels the capture without binding anything (it is never
//!    fed to `set_slot` even though `Escape` is itself bindable).
//!
//! ## Persistence
//!
//! Only [`Settings`] and [`KeyBindings`] are mutated — T15's
//! `SettingsPersistPlugin` fingerprints both resources and persists them
//! debounced, so this screen contains no save logic at all.
//!
//! ## Ranges (PRD §6.5/§9; the PRD fixes defaults, not slider bounds)
//!
//! Volumes `0.0..=1.0`; DAS `0..=500 ms`, ARR `0..=200 ms`, both snapped to
//! 10 ms; next-queue size cycles `1..=6` (PRD §6.3 cap); effects quality
//! cycles Low → Medium → High (PRD §8 "all juice skippable").

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::input::{pressed_key_to_bind, Bind, BindSlot, KeyBindings};
use crate::state::{AppState, CaptureOrder, EffectsQuality, RebindingCapture, Settings};

/// DAS slider bounds, milliseconds (PRD §6.5 default 150).
pub const DAS_RANGE: (f32, f32) = (0.0, 500.0);
/// ARR slider bounds, milliseconds (PRD §6.5 default 33).
pub const ARR_RANGE: (f32, f32) = (0.0, 200.0);
/// DAS/ARR snap increment in milliseconds.
pub const MS_SNAP: f32 = 10.0;
/// Volume slider bounds (linear gain).
pub const VOLUME_RANGE: (f32, f32) = (0.0, 1.0);
/// Text shown in a binding row while capturing.
pub const CAPTURE_HINT: &str = "press key…";

// ---------------------------------------------------------------------------
// Handler data types
// ---------------------------------------------------------------------------

/// Which value a slider/button edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SliderKey {
    /// [`Settings::master_volume`].
    MasterVolume,
    /// [`Settings::sfx_volume`].
    SfxVolume,
    /// [`Settings::music_volume`],
    MusicVolume,
    /// [`Settings::das_ms`] (10 ms snap).
    Das,
    /// [`Settings::arr_ms`] (10 ms snap).
    Arr,
}

impl SliderKey {
    /// `min..=max` bounds this key writes into.
    pub fn range(self) -> (f32, f32) {
        match self {
            Self::MasterVolume | Self::SfxVolume | Self::MusicVolume => VOLUME_RANGE,
            Self::Das => DAS_RANGE,
            Self::Arr => ARR_RANGE,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure slider math (unit-testable)
// ---------------------------------------------------------------------------

/// Normalized `0..=1` position of `pos_x` inside `left..left+width`.
/// A degenerate width yields `0.0`; results clamp at both ends.
pub fn norm_from_pos(pos_x: f32, left: f32, width: f32) -> f32 {
    if !pos_x.is_finite() || !width.is_finite() || width <= f32::EPSILON {
        return 0.0;
    }
    ((pos_x - left) / width).clamp(0.0, 1.0)
}

/// Slider value for a pointer x-position over the track rect, mapped to
/// `min..=max` and clamped.
pub fn slider_value_from_pos(pos_x: f32, left: f32, width: f32, min: f32, max: f32) -> f32 {
    let norm = norm_from_pos(pos_x, left, width);
    min + norm * (max - min)
}

/// Round `ms` to the nearest [`MS_SNAP`] multiple (half away from zero).
pub fn snap_ms(ms: f32) -> u32 {
    if !ms.is_finite() {
        return 0;
    }
    ((ms / MS_SNAP).round() * MS_SNAP).max(0.0) as u32
}

/// Write a raw slider `value` (already position-mapped) into `settings`,
/// clamping to the key's range and snapping ms values to [`MS_SNAP`].
pub fn apply_slider(key: SliderKey, value: f32, settings: &mut Settings) {
    let value = if value.is_finite() { value } else { 0.0 };
    let (min, max) = key.range();
    let clamped = value.clamp(min, max);
    match key {
        SliderKey::MasterVolume => settings.master_volume = clamped,
        SliderKey::SfxVolume => settings.sfx_volume = clamped,
        SliderKey::MusicVolume => settings.music_volume = clamped,
        SliderKey::Das => {
            settings.das_ms = snap_ms(clamped).clamp(DAS_RANGE.0 as u32, DAS_RANGE.1 as u32)
        }
        SliderKey::Arr => {
            settings.arr_ms = snap_ms(clamped).clamp(ARR_RANGE.0 as u32, ARR_RANGE.1 as u32)
        }
    }
}

/// Current fill fraction `0..=1` of a slider for the persisted setting
/// (clamped, so out-of-range files load sensibly).
pub fn slider_norm(key: SliderKey, settings: &Settings) -> f32 {
    let (min, max) = key.range();
    let value = match key {
        SliderKey::MasterVolume => settings.master_volume,
        SliderKey::SfxVolume => settings.sfx_volume,
        SliderKey::MusicVolume => settings.music_volume,
        SliderKey::Das => settings.das_ms as f32,
        SliderKey::Arr => settings.arr_ms as f32,
    };
    ((value - min) / (max - min)).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Rebinding capture handlers
// ---------------------------------------------------------------------------

/// Which binding row currently owns the capture (T16-owned companion to the
/// shared [`RebindingCapture`] flag; T12 only reads the flag).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Resource)]
pub struct CaptureTarget {
    /// Slot whose row began the capture, if any.
    pub slot: Option<BindSlot>,
}

/// Begin capturing for `slot`: marks the shared flag (muting T12 emission)
/// and remembers the slot.
pub fn begin_rebind(slot: BindSlot, capture: &mut RebindingCapture, target: &mut CaptureTarget) {
    capture.capturing = true;
    target.slot = Some(slot);
}

/// Abandon the capture without touching any binding.
pub fn cancel_rebind(capture: &mut RebindingCapture, target: &mut CaptureTarget) {
    capture.capturing = false;
    target.slot = None;
}

/// Consume one capture frame: `Escape` cancels *without* binding; any other
/// freshly pressed key (via [`pressed_key_to_bind`]) replaces the slot's
/// binds. No-ops when idle or when no slot owns the capture.
pub fn apply_capture(
    keys: &ButtonInput<KeyCode>,
    capture: &mut RebindingCapture,
    target: &mut CaptureTarget,
    bindings: &mut KeyBindings,
) {
    if !capture.capturing {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        cancel_rebind(capture, target);
        return;
    }
    let Some(slot) = target.slot else {
        cancel_rebind(capture, target);
        return;
    };
    if let Some(bind) = pressed_key_to_bind(keys) {
        bindings.set_slot(slot, vec![bind]);
        cancel_rebind(capture, target);
    }
}

/// Restore every [`ALL_BIND_SLOTS`](crate::input::ALL_BIND_SLOTS) slot to
/// its PRD §9 default — the settings screen's global reset.
pub fn reset_all_bindings(bindings: &mut KeyBindings) {
    for slot in crate::input::ALL_BIND_SLOTS {
        bindings.reset_slot(slot);
    }
}

// ---------------------------------------------------------------------------
// Cycle / navigation handlers
// ---------------------------------------------------------------------------

/// Low → Medium → High → Low.
pub fn cycle_effects(quality: &mut EffectsQuality) {
    *quality = match quality {
        EffectsQuality::Low => EffectsQuality::Medium,
        EffectsQuality::Medium => EffectsQuality::High,
        EffectsQuality::High => EffectsQuality::Low,
    };
}

/// Next-queue preview size cycles `1..=6` (PRD §6.3).
pub fn cycle_next_queue_size(size: &mut u8) {
    *size = if *size >= 6 || *size < 1 {
        1
    } else {
        *size + 1
    };
}

/// Soundtrack cycles through [`Soundtrack::ALL`] (Classic → Auto → Pulse →
/// Drift → Classic). `audio::start_bgm` restarts the loop the same frame.
pub fn cycle_soundtrack(track: &mut crate::state::Soundtrack) {
    use crate::state::Soundtrack;
    *track = match track {
        Soundtrack::Classic => Soundtrack::Auto,
        Soundtrack::Auto => Soundtrack::Pulse,
        Soundtrack::Pulse => Soundtrack::Drift,
        Soundtrack::Drift => Soundtrack::Classic,
    };
}

/// Color scheme toggles Classic ↔ Colorblind (Okabe–Ito palette).
pub fn cycle_color_scheme(scheme: &mut crate::state::ColorScheme) {
    use crate::state::ColorScheme;
    *scheme = match scheme {
        ColorScheme::Classic => ColorScheme::Colorblind,
        ColorScheme::Colorblind => ColorScheme::Classic,
    };
}

/// Flip the reduced-flash accessibility flag.
pub fn toggle_reduce_flash(on: &mut bool) {
    *on = !*on;
}

/// Screen-local resource: which state [`handle_back`] restores. Defaults to
/// [`AppState::Title`] (PRD §7.1); [`track_settings_entry`] records the
/// screen the user arrived from (Pause keeps returning to Pause).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource)]
pub struct SettingsReturn {
    /// State restored by the back button.
    pub state: AppState,
}

impl Default for SettingsReturn {
    fn default() -> Self {
        Self {
            state: AppState::Title,
        }
    }
}

/// Back button: clear any capture, return to the recorded screen and reset
/// the memory to Title.
pub fn handle_back(
    app_state: &mut AppState,
    ret: &mut SettingsReturn,
    capture: &mut RebindingCapture,
    target: &mut CaptureTarget,
) {
    cancel_rebind(capture, target);
    *app_state = ret.state;
    ret.state = AppState::Title;
}

// ---------------------------------------------------------------------------
// Label formatters
// ---------------------------------------------------------------------------

/// Human-readable slot name for the binding list.
pub fn slot_name(slot: BindSlot) -> &'static str {
    match slot {
        BindSlot::MoveLeft => "Move left",
        BindSlot::MoveRight => "Move right",
        BindSlot::SoftDrop => "Soft drop",
        BindSlot::HardDrop => "Hard drop",
        BindSlot::RotateCw => "Rotate CW",
        BindSlot::RotateCcw => "Rotate CCW",
        BindSlot::Rotate180 => "Rotate 180",
        BindSlot::Hold => "Hold",
        BindSlot::Pause => "Pause",
    }
}

/// Short readable name of one key (arrows become glyphs, `KeyX` → `X`).
pub fn bind_display(bind: Bind) -> String {
    match bind {
        Bind::WheelUp => "Wheel Up".into(),
        Bind::WheelDown => "Wheel Down".into(),
        Bind::Key(key) => key_display(key),
    }
}

fn key_display(key: KeyCode) -> String {
    let name = format!("{key:?}");
    match key {
        KeyCode::ArrowLeft => "←".into(),
        KeyCode::ArrowRight => "→".into(),
        KeyCode::ArrowUp => "↑".into(),
        KeyCode::ArrowDown => "↓".into(),
        KeyCode::Escape => "Esc".into(),
        KeyCode::ShiftLeft => "LShift".into(),
        KeyCode::ShiftRight => "RShift".into(),
        KeyCode::ControlLeft => "LCtrl".into(),
        KeyCode::ControlRight => "RCtrl".into(),
        KeyCode::AltLeft => "LAlt".into(),
        KeyCode::AltRight => "RAlt".into(),
        KeyCode::Space => "Space".into(),
        _ => name
            .strip_prefix("Key")
            .or_else(|| name.strip_prefix("Digit"))
            .map_or(name.clone(), str::to_string),
    }
}

/// All binds of a slot joined by `" / "`, or `"(none)"` when empty.
pub fn slot_display(binds: &[Bind]) -> String {
    if binds.is_empty() {
        return "(none)".into();
    }
    binds
        .iter()
        .map(|b| bind_display(*b))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Display string for a slider's current value ("68%", "150 ms").
pub fn value_label(key: SliderKey, settings: &Settings) -> String {
    match key {
        SliderKey::MasterVolume => format!("{:.0}%", settings.master_volume * 100.0),
        SliderKey::SfxVolume => format!("{:.0}%", settings.sfx_volume * 100.0),
        SliderKey::MusicVolume => format!("{:.0}%", settings.music_volume * 100.0),
        SliderKey::Das => format!("{} ms", settings.das_ms),
        SliderKey::Arr => format!("{} ms", settings.arr_ms),
    }
}

fn effects_name(quality: EffectsQuality) -> &'static str {
    match quality {
        EffectsQuality::Low => "Low",
        EffectsQuality::Medium => "Medium",
        EffectsQuality::High => "High",
    }
}

// ---------------------------------------------------------------------------
// UI components / resources
// ---------------------------------------------------------------------------

/// Root node of the settings screen; shown only in [`AppState::Settings`].
#[derive(Component)]
pub struct SettingsRoot;

/// Button entity for one rebindable slot; clicking it begins capture.
#[derive(Component)]
pub struct BindRow {
    /// Slot this row edits.
    pub slot: BindSlot,
}

/// [`BindRow`] key label showing the current binds or the capture hint.
#[derive(Component)]
struct BindValue {
    slot: BindSlot,
}

/// Button entity resetting one slot to its PRD default.
#[derive(Component)]
pub struct ResetSlot {
    /// Slot reset on click.
    pub slot: BindSlot,
}

/// Back button ([`handle_back`]).
#[derive(Component)]
pub struct BackButton;

/// Global reset button ([`reset_all_bindings`]).
#[derive(Component)]
pub struct ResetAllButton;

/// Draggable slider track; also the click-to-set target.
#[derive(Component)]
pub struct SliderTrack {
    /// Value this track edits.
    pub key: SliderKey,
}

/// Progress-fill bar whose width follows [`slider_norm`].
#[derive(Component)]
struct FillBar {
    key: SliderKey,
}

/// Numeric readout next to a slider.
#[derive(Component)]
struct ValueText {
    key: SliderKey,
}

/// Button cycling [`Settings::effects`] (PRD §8 tier).
#[derive(Component)]
pub struct EffectsButton;

/// Button cycling [`Settings::next_queue_size`] 1–6 (PRD §6.3).
#[derive(Component)]
pub struct QueueButton;

/// Button cycling [`Settings::soundtrack`] (Classic → Auto → Pulse → Drift).
#[derive(Component)]
pub struct SoundtrackButton;

/// Button toggling [`Settings::color_scheme`] (Classic ↔ Colorblind).
#[derive(Component)]
pub struct SchemeButton;

/// Button toggling [`Settings::reduce_flash`].
#[derive(Component)]
pub struct FlashButton;

#[derive(Component)]
struct EffectsText;

#[derive(Component)]
struct QueueText;

#[derive(Component)]
struct SoundtrackText;

#[derive(Component)]
struct SchemeText;

#[derive(Component)]
struct FlashText;

/// Which slider currently owns the pointer (drag focus).
#[derive(Debug, Default, Resource)]
struct SliderDragState {
    track: Option<Entity>,
}

/// Previous [`AppState`] value, used to remember where Back should return.
#[derive(Debug, Default, Resource)]
struct LastAppState(AppState);

// ---------------------------------------------------------------------------
// Systems (UI glue)
// ---------------------------------------------------------------------------

/// Show the screen only while [`AppState::Settings`] is active; hidden UI
/// also stops receiving `Interaction` (Bevy clears it for invisible nodes).
fn sync_visibility(state: Res<AppState>, mut roots: Query<&mut Visibility, With<SettingsRoot>>) {
    let wanted = if *state == AppState::Settings {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    for mut vis in roots.iter_mut() {
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

/// Remember the screen we came from when entering Settings; `Playing`
/// (which never hosts Settings) maps to `Title`.
fn track_settings_entry(
    state: Res<AppState>,
    mut last: ResMut<LastAppState>,
    mut ret: ResMut<SettingsReturn>,
) {
    if *state == last.0 {
        return;
    }
    if *state == AppState::Settings {
        ret.state = if last.0 == AppState::Playing {
            AppState::Title
        } else {
            last.0
        };
    }
    last.0 = *state;
}

/// All button widgets in one query: slot capture, per-slot reset, effects
/// and queue cycling, global reset and back.
type ClickQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Interaction,
        Option<&'static BindRow>,
        Option<&'static ResetSlot>,
        Has<EffectsButton>,
        Has<QueueButton>,
        Has<SoundtrackButton>,
        Has<SchemeButton>,
        Has<FlashButton>,
        Has<ResetAllButton>,
        Has<BackButton>,
    ),
    (With<Button>, Changed<Interaction>),
>;

#[derive(SystemParam)]
struct ClickParams<'w, 's> {
    buttons: ClickQuery<'w, 's>,
    capture: ResMut<'w, RebindingCapture>,
    target: ResMut<'w, CaptureTarget>,
    bindings: ResMut<'w, KeyBindings>,
    settings: ResMut<'w, Settings>,
    app_state: ResMut<'w, AppState>,
    ret: ResMut<'w, SettingsReturn>,
}

fn button_clicks(mut params: ClickParams) {
    if *params.app_state != AppState::Settings {
        return;
    }
    for (
        _entity,
        interaction,
        row,
        reset,
        effects,
        queue,
        soundtrack,
        scheme,
        flash,
        reset_all,
        back,
    ) in params.buttons.iter()
    {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Some(row) = row {
            begin_rebind(row.slot, &mut params.capture, &mut params.target);
        } else if let Some(reset) = reset {
            params.bindings.reset_slot(reset.slot);
        } else if effects {
            cycle_effects(&mut params.settings.effects);
        } else if queue {
            cycle_next_queue_size(&mut params.settings.next_queue_size);
        } else if soundtrack {
            cycle_soundtrack(&mut params.settings.soundtrack);
        } else if scheme {
            cycle_color_scheme(&mut params.settings.color_scheme);
        } else if flash {
            toggle_reduce_flash(&mut params.settings.reduce_flash);
        } else if reset_all {
            reset_all_bindings(&mut params.bindings);
        } else if back {
            handle_back(
                &mut params.app_state,
                &mut params.ret,
                &mut params.capture,
                &mut params.target,
            );
        }
    }
}

/// Slider press/drag/release glue over the Bevy 0.19 picking messages. The
/// pressed track keeps capture focus so dragging past its edges still works;
/// the value follows [`slider_value_from_pos`] (validated by unit tests —
/// on-device pointer fidelity is checked in T14).
#[derive(SystemParam)]
struct SliderParams<'w, 's> {
    app_state: Res<'w, AppState>,
    presses: MessageReader<'w, 's, Pointer<Press>>,
    drags: MessageReader<'w, 's, Pointer<Drag>>,
    releases: MessageReader<'w, 's, Pointer<Release>>,
    drag_ends: MessageReader<'w, 's, Pointer<DragEnd>>,
    drag_state: ResMut<'w, SliderDragState>,
    tracks: Query<
        'w,
        's,
        (
            &'static SliderTrack,
            &'static ComputedNode,
            &'static UiGlobalTransform,
        ),
    >,
    settings: ResMut<'w, Settings>,
}

fn apply_pointer_to_slider(
    tracks: &Query<(&SliderTrack, &ComputedNode, &UiGlobalTransform)>,
    entity: Entity,
    position: Vec2,
    settings: &mut Settings,
) {
    let Ok((track, node, xform)) = tracks.get(entity) else {
        return;
    };
    let (min, max) = track.key.range();
    let left = xform.translation.x - node.size.x * 0.5;
    let value = slider_value_from_pos(position.x, left, node.size.x, min, max);
    apply_slider(track.key, value, settings);
}

fn slider_pointer(mut params: SliderParams) {
    if *params.app_state != AppState::Settings {
        params.drag_state.track = None;
        return;
    }
    for event in params.presses.read() {
        if event.button != PointerButton::Primary {
            continue;
        }
        if params.tracks.get(event.entity).is_ok() {
            params.drag_state.track = Some(event.entity);
            apply_pointer_to_slider(
                &params.tracks,
                event.entity,
                event.pointer_location.position,
                &mut params.settings,
            );
        }
    }
    for event in params.drags.read() {
        if params.drag_state.track == Some(event.entity) {
            apply_pointer_to_slider(
                &params.tracks,
                event.entity,
                event.pointer_location.position,
                &mut params.settings,
            );
        }
    }
    for event in params.releases.read() {
        if event.button == PointerButton::Primary {
            params.drag_state.track = None;
        }
    }
    for _event in params.drag_ends.read() {
        params.drag_state.track = None;
    }
}

/// Consume one capture frame while the screen is up; leaving Settings while
/// capturing also drops the capture so T12's gate reopens.
fn apply_capture_system(
    state: Res<AppState>,
    keys: Res<ButtonInput<KeyCode>>,
    mut capture: ResMut<RebindingCapture>,
    mut target: ResMut<CaptureTarget>,
    mut bindings: ResMut<KeyBindings>,
) {
    if *state != AppState::Settings {
        if capture.capturing {
            cancel_rebind(&mut capture, &mut target);
        }
        return;
    }
    apply_capture(&keys, &mut capture, &mut target, &mut bindings);
}

/// Rewrite all labels/fills from the live resources whenever anything they
/// display changes.
type LabelQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Text,
        Option<&'static BindValue>,
        Option<&'static ValueText>,
        Has<EffectsText>,
        Has<QueueText>,
        Has<SoundtrackText>,
        Has<SchemeText>,
        Has<FlashText>,
    ),
>;

#[derive(SystemParam)]
struct SyncParams<'w, 's> {
    settings: Res<'w, Settings>,
    bindings: Res<'w, KeyBindings>,
    capture: Res<'w, RebindingCapture>,
    target: Res<'w, CaptureTarget>,
    texts: LabelQuery<'w, 's>,
    fills: Query<'w, 's, (&'static mut Node, &'static FillBar)>,
}

fn sync_labels(params: SyncParams) {
    let dirty = params.settings.is_changed()
        || params.bindings.is_changed()
        || params.capture.is_changed()
        || params.target.is_changed();
    if !dirty {
        return;
    }
    let SyncParams {
        settings,
        bindings,
        capture,
        target,
        mut texts,
        mut fills,
    } = params;
    for (mut text, bind_value, value_text, effects, queue, soundtrack, scheme, flash) in
        texts.iter_mut()
    {
        if let Some(bind_value) = bind_value {
            *text = if capture.capturing && target.slot == Some(bind_value.slot) {
                Text::new(CAPTURE_HINT)
            } else {
                Text::new(slot_display(bindings.slot(bind_value.slot)))
            };
        } else if let Some(value_text) = value_text {
            *text = Text::new(value_label(value_text.key, &settings));
        } else if effects {
            *text = Text::new(effects_name(settings.effects));
        } else if queue {
            *text = Text::new(settings.next_queue_size.to_string());
        } else if soundtrack {
            *text = Text::new(settings.soundtrack.label());
        } else if scheme {
            *text = Text::new(settings.color_scheme.label());
        } else if flash {
            *text = Text::new(if settings.reduce_flash { "On" } else { "Off" });
        }
    }
    for (mut node, fill) in fills.iter_mut() {
        node.width = Val::Percent(slider_norm(fill.key, &settings) * 100.0);
    }
}

// ---------------------------------------------------------------------------
// Startup UI construction
// ---------------------------------------------------------------------------

const PANEL_BG: Color = Color::srgb(0.11, 0.11, 0.14);
const TRACK_BG: Color = Color::srgb(0.25, 0.25, 0.30);
const FILL_BG: Color = Color::srgb(0.35, 0.75, 1.0);
const BUTTON_BG: Color = Color::srgb(0.22, 0.22, 0.27);

fn label_node(text: String, size: f32) -> (Text, TextFont, TextColor) {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor::WHITE,
    )
}

fn section_header(parent: &mut ChildSpawnerCommands, text: &str) {
    parent.spawn(label_node(text.to_string(), 22.0));
}

fn add_slider(parent: &mut ChildSpawnerCommands, title: &str, settings: &Settings, key: SliderKey) {
    let norm = slider_norm(key, settings) * 100.0;
    parent
        .spawn(Node {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(10.0),
            ..default()
        })
        .with_children(|row| {
            row.spawn(label_node(title.to_string(), 16.0));
            row.spawn((
                Button,
                SliderTrack { key },
                BackgroundColor(TRACK_BG),
                Node {
                    width: Val::Px(220.0),
                    height: Val::Px(16.0),
                    ..default()
                },
            ))
            .with_children(|track| {
                track.spawn((
                    FillBar { key },
                    ImageNode::default(),
                    BackgroundColor(FILL_BG),
                    Node {
                        width: Val::Percent(norm),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                ));
            });
            row.spawn((
                ValueText { key },
                label_node(value_label(key, settings), 16.0),
            ));
        });
}

fn add_binding_row(parent: &mut ChildSpawnerCommands, slot: BindSlot, bindings: &KeyBindings) {
    parent
        .spawn(Node {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(10.0),
            ..default()
        })
        .with_children(|row| {
            row.spawn(label_node(slot_name(slot).to_string(), 16.0));
            row.spawn((
                Button,
                BindRow { slot },
                BackgroundColor(BUTTON_BG),
                Node {
                    width: Val::Px(180.0),
                    height: Val::Px(28.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
            ))
            .with_children(|button| {
                button.spawn((
                    BindValue { slot },
                    label_node(slot_display(bindings.slot(slot)), 16.0),
                ));
            });
            row.spawn((
                Button,
                ResetSlot { slot },
                BackgroundColor(BUTTON_BG),
                Node {
                    width: Val::Px(72.0),
                    height: Val::Px(28.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
            ))
            .with_children(|button| {
                button.spawn(label_node("reset".to_string(), 16.0));
            });
        });
}

fn add_cycle_row(
    parent: &mut ChildSpawnerCommands,
    title: &str,
    marker: impl Bundle,
    text_marker: impl Bundle,
    value: String,
) {
    parent
        .spawn(Node {
            display: Display::Flex,
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: Val::Px(10.0),
            ..default()
        })
        .with_children(|row| {
            row.spawn(label_node(title.to_string(), 16.0));
            row.spawn((
                Button,
                marker,
                BackgroundColor(BUTTON_BG),
                Node {
                    width: Val::Px(120.0),
                    height: Val::Px(28.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
            ))
            .with_children(|button| {
                button.spawn((text_marker, label_node(value, 16.0)));
            });
        });
}

fn build_settings_ui(mut commands: Commands, settings: Res<Settings>, bindings: Res<KeyBindings>) {
    commands
        .spawn((
            SettingsRoot,
            Visibility::Hidden,
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
            section_header(root, "Settings");
            add_slider(root, "Master", &settings, SliderKey::MasterVolume);
            add_slider(root, "SFX", &settings, SliderKey::SfxVolume);
            add_slider(root, "Music", &settings, SliderKey::MusicVolume);
            add_cycle_row(
                root,
                "Soundtrack",
                SoundtrackButton,
                SoundtrackText,
                settings.soundtrack.label().to_string(),
            );
            section_header(root, "Accessibility");
            add_cycle_row(
                root,
                "Colors",
                SchemeButton,
                SchemeText,
                settings.color_scheme.label().to_string(),
            );
            add_cycle_row(
                root,
                "Reduce flash",
                FlashButton,
                FlashText,
                if settings.reduce_flash { "On" } else { "Off" }.to_string(),
            );
            section_header(root, "Timing");
            add_slider(root, "DAS", &settings, SliderKey::Das);
            add_slider(root, "ARR", &settings, SliderKey::Arr);
            section_header(root, "Display");
            add_cycle_row(
                root,
                "Effects",
                EffectsButton,
                EffectsText,
                effects_name(settings.effects).to_string(),
            );
            add_cycle_row(
                root,
                "Next queue",
                QueueButton,
                QueueText,
                settings.next_queue_size.to_string(),
            );
            section_header(root, "Keys");
            for slot in crate::input::ALL_BIND_SLOTS {
                add_binding_row(root, slot, &bindings);
            }
            root.spawn(Node {
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(10.0),
                ..default()
            })
            .with_children(|foot| {
                foot.spawn((
                    Button,
                    BackButton,
                    BackgroundColor(BUTTON_BG),
                    Node {
                        width: Val::Px(120.0),
                        height: Val::Px(32.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                ))
                .with_children(|b| {
                    b.spawn(label_node("Back".to_string(), 18.0));
                });
                foot.spawn((
                    Button,
                    ResetAllButton,
                    BackgroundColor(BUTTON_BG),
                    Node {
                        width: Val::Px(160.0),
                        height: Val::Px(32.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                ))
                .with_children(|b| {
                    b.spawn(label_node("Reset keys".to_string(), 18.0));
                });
            });
        });
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Builds the settings UI and the `RebindingCapture` flow.
pub struct SettingsScreenPlugin;

impl Plugin for SettingsScreenPlugin {
    fn build(&self, app: &mut App) {
        // Defensive inits keep every handler parameter satisfied in headless
        // `MinimalPlugins` tests (mirrors the InputPlugin/SettingsPersistPlugin
        // precedent); all are no-ops when the resources/messages already exist.
        app.init_resource::<Settings>()
            .init_resource::<KeyBindings>()
            .init_resource::<AppState>()
            .init_resource::<RebindingCapture>()
            .init_resource::<CaptureTarget>()
            .init_resource::<SettingsReturn>()
            .init_resource::<SliderDragState>()
            .init_resource::<LastAppState>();
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        app.add_message::<Pointer<Press>>()
            .add_message::<Pointer<Drag>>()
            .add_message::<Pointer<Release>>()
            .add_message::<Pointer<DragEnd>>()
            .add_systems(Startup, build_settings_ui)
            .configure_sets(Update, CaptureOrder::Cleanup.after(CaptureOrder::Chord))
            .add_systems(
                Update,
                (
                    sync_visibility,
                    track_settings_entry,
                    button_clicks,
                    slider_pointer,
                    apply_capture_system.in_set(CaptureOrder::Cleanup),
                    sync_labels,
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

    use std::path::PathBuf;

    use bevy::window::{Window, WindowPlugin};

    use crate::input::ALL_BIND_SLOTS;

    // ---- pure handler tests ----

    #[test]
    fn begin_rebind_sets_capturing_and_target() {
        let mut capture = RebindingCapture::default();
        let mut target = CaptureTarget::default();
        begin_rebind(BindSlot::HardDrop, &mut capture, &mut target);
        assert!(capture.capturing);
        assert_eq!(target.slot, Some(BindSlot::HardDrop));
    }

    #[test]
    fn capture_key_sets_slot_and_clears_capture() {
        let mut capture = RebindingCapture { capturing: true };
        let mut target = CaptureTarget {
            slot: Some(BindSlot::RotateCw),
        };
        let mut bindings = KeyBindings::default();
        let mut keys = ButtonInput::<KeyCode>::default();
        keys.press(KeyCode::KeyJ);

        apply_capture(&keys, &mut capture, &mut target, &mut bindings);

        assert_eq!(
            *bindings.slot(BindSlot::RotateCw),
            vec![Bind::Key(KeyCode::KeyJ)]
        );
        assert!(!capture.capturing);
        assert_eq!(target.slot, None);
        // Other slots untouched.
        assert_eq!(
            *bindings.slot(BindSlot::MoveLeft),
            KeyBindings::default_slot(BindSlot::MoveLeft)
        );
    }

    #[test]
    fn capture_escape_cancels_without_binding() {
        let before = KeyBindings::default();
        let mut capture = RebindingCapture { capturing: true };
        let mut target = CaptureTarget {
            slot: Some(BindSlot::Pause),
        };
        let mut bindings = before.clone();
        let mut keys = ButtonInput::<KeyCode>::default();
        keys.press(KeyCode::Escape);

        apply_capture(&keys, &mut capture, &mut target, &mut bindings);

        assert!(!capture.capturing, "Escape ends the capture");
        assert_eq!(target.slot, None);
        assert_eq!(bindings, before, "Escape must not bind anything");
    }

    #[test]
    fn capture_ignores_keys_while_idle() {
        let mut capture = RebindingCapture::default();
        let mut target = CaptureTarget::default();
        let mut bindings = KeyBindings::default();
        let mut keys = ButtonInput::<KeyCode>::default();
        keys.press(KeyCode::KeyQ);
        apply_capture(&keys, &mut capture, &mut target, &mut bindings);
        assert_eq!(bindings, KeyBindings::default());
    }

    #[test]
    fn slider_value_from_pos_clamps_and_scales() {
        // Track spans x = 100..300.
        assert_eq!(slider_value_from_pos(100.0, 100.0, 200.0, 0.0, 1.0), 0.0);
        assert_eq!(slider_value_from_pos(200.0, 100.0, 200.0, 0.0, 1.0), 0.5);
        assert_eq!(slider_value_from_pos(300.0, 100.0, 200.0, 0.0, 1.0), 1.0);
        assert_eq!(slider_value_from_pos(-50.0, 100.0, 200.0, 0.0, 1.0), 0.0);
        assert_eq!(slider_value_from_pos(999.0, 100.0, 200.0, 0.0, 1.0), 1.0);
        // ms range mapping.
        assert_eq!(
            slider_value_from_pos(150.0, 100.0, 200.0, 0.0, 500.0),
            125.0
        );
        // Degenerate track yields the minimum, never NaN.
        assert_eq!(
            slider_value_from_pos(f32::NAN, 100.0, 0.0, 10.0, 20.0),
            10.0
        );
    }

    #[test]
    fn snap_ms_snaps_to_ten_ms_multiples() {
        assert_eq!(snap_ms(14.0), 10);
        assert_eq!(snap_ms(15.0), 20);
        assert_eq!(snap_ms(150.0), 150);
        assert_eq!(snap_ms(-5.0), 0);
        assert_eq!(snap_ms(f32::NAN), 0);
    }

    #[test]
    fn apply_slider_writes_clamps_and_snaps_settings() {
        let mut settings = Settings::default();
        apply_slider(SliderKey::MasterVolume, 0.42, &mut settings);
        assert_eq!(settings.master_volume, 0.42);
        apply_slider(SliderKey::SfxVolume, 5.0, &mut settings);
        assert_eq!(settings.sfx_volume, 1.0, "volume clamps at 1.0");
        apply_slider(SliderKey::MusicVolume, -1.0, &mut settings);
        assert_eq!(settings.music_volume, 0.0, "volume clamps at 0.0");
        apply_slider(SliderKey::Das, 123.4, &mut settings);
        assert_eq!(settings.das_ms, 120, "DAS snaps to 10 ms");
        apply_slider(SliderKey::Arr, 9999.0, &mut settings);
        assert_eq!(settings.arr_ms, 200, "ARR clamps to its range");
        assert_eq!(slider_norm(SliderKey::Das, &settings), 0.24);
        assert_eq!(value_label(SliderKey::Das, &settings), "120 ms");
        assert_eq!(value_label(SliderKey::MasterVolume, &settings), "42%");
    }

    #[test]
    fn reset_slot_restores_single_default() {
        let mut bindings = KeyBindings::default();
        bindings.set_slot(BindSlot::Hold, vec![Bind::Key(KeyCode::KeyV)]);
        bindings.reset_slot(BindSlot::Hold);
        assert_eq!(
            *bindings.slot(BindSlot::Hold),
            KeyBindings::default_slot(BindSlot::Hold)
        );
    }

    #[test]
    fn reset_all_restores_every_slot() {
        let mut bindings = KeyBindings::default();
        for slot in ALL_BIND_SLOTS {
            bindings.set_slot(slot, vec![Bind::WheelUp]);
        }
        reset_all_bindings(&mut bindings);
        assert_eq!(bindings, KeyBindings::default());
    }

    #[test]
    fn cycle_effects_wraps_low_medium_high() {
        let mut quality = EffectsQuality::Low;
        cycle_effects(&mut quality);
        assert_eq!(quality, EffectsQuality::Medium);
        cycle_effects(&mut quality);
        assert_eq!(quality, EffectsQuality::High);
        cycle_effects(&mut quality);
        assert_eq!(quality, EffectsQuality::Low);
    }

    #[test]
    fn cycle_next_queue_size_wraps_one_to_six() {
        let mut size = 5;
        cycle_next_queue_size(&mut size);
        assert_eq!(size, 6);
        cycle_next_queue_size(&mut size);
        assert_eq!(size, 1);
        let mut broken = 0;
        cycle_next_queue_size(&mut broken);
        assert_eq!(broken, 1, "out-of-range values recover to 1");
    }

    #[test]
    fn handle_back_uses_recorded_state_and_clears_capture() {
        let mut state = AppState::Settings;
        let mut ret = SettingsReturn {
            state: AppState::Paused,
        };
        let mut capture = RebindingCapture { capturing: true };
        let mut target = CaptureTarget {
            slot: Some(BindSlot::MoveLeft),
        };
        handle_back(&mut state, &mut ret, &mut capture, &mut target);
        assert_eq!(state, AppState::Paused);
        assert!(!capture.capturing);
        assert_eq!(target.slot, None);
        assert_eq!(ret.state, AppState::Title, "memory resets for next visit");
    }

    #[test]
    fn bind_display_formats_keys_wheels_and_joins() {
        assert_eq!(bind_display(Bind::WheelUp), "Wheel Up");
        assert_eq!(bind_display(Bind::WheelDown), "Wheel Down");
        assert_eq!(bind_display(Bind::Key(KeyCode::ArrowUp)), "↑");
        assert_eq!(bind_display(Bind::Key(KeyCode::KeyX)), "X");
        assert_eq!(bind_display(Bind::Key(KeyCode::Space)), "Space");
        assert_eq!(bind_display(Bind::Key(KeyCode::Escape)), "Esc");
        let cw = KeyBindings::default_slot(BindSlot::RotateCw);
        assert_eq!(slot_display(&cw), "↑ / X / Wheel Up");
        assert_eq!(slot_display(&[]), "(none)");
    }

    // ---- headless integration: plugin + spawned widget tree ----

    /// Unique temp dir for persistence assertions; avoids the process-wide
    /// `TETRIS_CONFIG_DIR` (which T15's parallel tests mutate under their own
    /// lock) by going through T15's path-injectable `save_to`/`load_from`
    /// twins.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("tetris-t16-{label}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).expect("temp dir created");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn settings_test_app() -> App {
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
        app.add_plugins(SettingsScreenPlugin);
        app.update(); // Startup: spawn the widget tree + first label sync
        app
    }

    fn set_state(app: &mut App, state: AppState) {
        *app.world_mut().resource_mut::<AppState>() = state;
        app.update();
    }

    fn click_button(app: &mut App, predicate: impl Fn(&World, Entity) -> bool) {
        let entity = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<Button>>();
            let candidates: Vec<Entity> =
                query.iter(world).filter(|e| predicate(world, *e)).collect();
            candidates.first().copied().expect("button entity exists")
        };
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
    }

    fn row_entity(app: &mut App, slot: BindSlot) -> Entity {
        let world = app.world_mut();
        let mut query = world.query::<(Entity, &BindRow)>();
        query
            .iter(world)
            .find(|(_, row)| row.slot == slot)
            .expect("bind row spawned")
            .0
    }

    #[test]
    fn settings_state_shows_one_row_per_bind_slot() {
        let mut app = settings_test_app();
        assert_eq!(
            *app.world().resource::<AppState>(),
            AppState::Playing,
            "screen hidden by default"
        );
        assert_eq!(root_visibility(&mut app), Visibility::Hidden);

        set_state(&mut app, AppState::Settings);
        let rows = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<(), With<BindRow>>();
            q.iter(world).count()
        };
        assert_eq!(rows, ALL_BIND_SLOTS.len());
        assert_eq!(root_visibility(&mut app), Visibility::Visible);

        set_state(&mut app, AppState::Title);
        assert_eq!(root_visibility(&mut app), Visibility::Hidden);
    }

    fn root_visibility(app: &mut App) -> Visibility {
        let world = app.world_mut();
        let mut q = world.query_filtered::<&Visibility, With<SettingsRoot>>();
        *q.single(world).expect("settings root exists")
    }

    #[test]
    fn slot_click_begins_capture() {
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        let entity = row_entity(&mut app, BindSlot::RotateCw);
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RebindingCapture>().capturing);
        assert_eq!(
            app.world().resource::<CaptureTarget>().slot,
            Some(BindSlot::RotateCw)
        );
        // T12's gate sees the flag — gameplay actions are muted (T12 owns the
        // gate; T16 only flips the shared bit).
    }

    #[test]
    fn capture_flow_key_then_escape() {
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        let entity = row_entity(&mut app, BindSlot::HardDrop);
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RebindingCapture>().capturing);

        // Inject a key press for the next capture frame.
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        app.update();
        assert_eq!(
            *app.world()
                .resource::<KeyBindings>()
                .slot(BindSlot::HardDrop),
            vec![Bind::Key(KeyCode::Enter)]
        );
        assert!(!app.world().resource::<RebindingCapture>().capturing);

        // Simulate the per-frame `ButtonInput::clear` the real
        // `keyboard_input_system` runs in PreUpdate (headless tests have no
        // input plugin).
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();

        // Escape path: capture again, then Esc clears with no binding change.
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RebindingCapture>().capturing);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        assert!(!app.world().resource::<RebindingCapture>().capturing);
        assert_eq!(
            *app.world()
                .resource::<KeyBindings>()
                .slot(BindSlot::HardDrop),
            vec![Bind::Key(KeyCode::Enter)],
            "cancel must not overwrite the slot"
        );
    }

    #[test]
    fn leaving_settings_mid_capture_clears_the_gate() {
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        let entity = row_entity(&mut app, BindSlot::Hold);
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
        assert!(app.world().resource::<RebindingCapture>().capturing);
        set_state(&mut app, AppState::Title);
        assert!(!app.world().resource::<RebindingCapture>().capturing);
    }

    #[test]
    fn back_button_returns_to_recording_state() {
        let mut app = settings_test_app();
        // Default memory is Title even when entering from Playing.
        set_state(&mut app, AppState::Settings);
        click_button(&mut app, |world, e| world.get::<BackButton>(e).is_some());
        assert_eq!(*app.world().resource::<AppState>(), AppState::Title);

        // Entering from Pause returns to Pause.
        set_state(&mut app, AppState::Paused);
        set_state(&mut app, AppState::Settings);
        click_button(&mut app, |world, e| world.get::<BackButton>(e).is_some());
        assert_eq!(*app.world().resource::<AppState>(), AppState::Paused);
    }

    #[test]
    fn reset_buttons_restore_defaults() {
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        app.world_mut()
            .resource_mut::<KeyBindings>()
            .set_slot(BindSlot::MoveLeft, vec![Bind::Key(KeyCode::KeyH)]);
        // Per-slot reset button on the MoveLeft row.
        let slot = BindSlot::MoveLeft;
        click_button(&mut app, move |world, e| {
            world
                .get::<ResetSlot>(e)
                .is_some_and(|r| r.slot == BindSlot::MoveLeft)
        });
        assert_eq!(
            *app.world().resource::<KeyBindings>().slot(slot),
            KeyBindings::default_slot(slot)
        );
        // Global reset restores every slot.
        for s in ALL_BIND_SLOTS {
            app.world_mut()
                .resource_mut::<KeyBindings>()
                .set_slot(s, vec![Bind::WheelDown]);
        }
        click_button(&mut app, |world, e| {
            world.get::<ResetAllButton>(e).is_some()
        });
        assert_eq!(
            *app.world().resource::<KeyBindings>(),
            KeyBindings::default()
        );
    }

    #[test]
    fn effects_and_queue_buttons_cycle_settings() {
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        click_button(&mut app, |world, e| world.get::<EffectsButton>(e).is_some());
        assert_eq!(
            app.world().resource::<Settings>().effects,
            EffectsQuality::High,
            "Medium default → click → High"
        );
        click_button(&mut app, |world, e| world.get::<QueueButton>(e).is_some());
        assert_eq!(app.world().resource::<Settings>().next_queue_size, 6);
        click_button(&mut app, |world, e| world.get::<QueueButton>(e).is_some());
        assert_eq!(app.world().resource::<Settings>().next_queue_size, 1);
    }

    #[test]
    fn cycle_soundtrack_wraps_all_variants() {
        use crate::state::Soundtrack;
        let mut track = Soundtrack::Classic;
        for want in [
            Soundtrack::Auto,
            Soundtrack::Pulse,
            Soundtrack::Drift,
            Soundtrack::Classic,
        ] {
            cycle_soundtrack(&mut track);
            assert_eq!(track, want);
        }
    }

    #[test]
    fn cycle_color_scheme_toggles_both_ways() {
        use crate::state::ColorScheme;
        let mut scheme = ColorScheme::Classic;
        cycle_color_scheme(&mut scheme);
        assert_eq!(scheme, ColorScheme::Colorblind);
        cycle_color_scheme(&mut scheme);
        assert_eq!(scheme, ColorScheme::Classic);
        let mut on = false;
        toggle_reduce_flash(&mut on);
        assert!(on);
        toggle_reduce_flash(&mut on);
        assert!(!on);
    }

    #[test]
    fn soundtrack_button_cycles_settings() {
        use crate::state::Soundtrack;
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        for want in [
            Soundtrack::Auto,
            Soundtrack::Pulse,
            Soundtrack::Drift,
            Soundtrack::Classic,
        ] {
            click_button(&mut app, |world, e| {
                world.get::<SoundtrackButton>(e).is_some()
            });
            assert_eq!(app.world().resource::<Settings>().soundtrack, want);
        }
    }

    #[test]
    fn accessibility_buttons_toggle_settings() {
        use crate::state::ColorScheme;
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        click_button(&mut app, |world, e| world.get::<SchemeButton>(e).is_some());
        assert_eq!(
            app.world().resource::<Settings>().color_scheme,
            ColorScheme::Colorblind
        );
        click_button(&mut app, |world, e| world.get::<FlashButton>(e).is_some());
        assert!(app.world().resource::<Settings>().reduce_flash);
        click_button(&mut app, |world, e| world.get::<FlashButton>(e).is_some());
        assert!(!app.world().resource::<Settings>().reduce_flash);
    }

    #[test]
    fn settings_mutations_reflect_through_persistence() {
        use crate::settings_persist::{load_from, save_to, PersistedBestScore};

        let dir = TempDir::new("persist");
        let mut app = settings_test_app();
        set_state(&mut app, AppState::Settings);
        {
            let mut settings = app.world_mut().resource_mut::<Settings>();
            settings.das_ms = 120;
            settings.master_volume = 0.5;
        }
        let (settings, bindings) = {
            let world = app.world();
            (
                world.resource::<Settings>().clone(),
                world.resource::<KeyBindings>().clone(),
            )
        };
        let best = PersistedBestScore::default();
        save_to(&dir.0, &settings, &bindings, best).expect("save");
        assert_eq!(load_from(&dir.0), (settings, bindings, best));

        // And the drag-math handler drives Settings exactly like the pointer
        // glue does (visual drag itself is checked in T14).
        let mut settings = Settings::default();
        let (min, max) = SliderKey::Das.range();
        let value = slider_value_from_pos(250.0, 100.0, 200.0, min, max);
        apply_slider(SliderKey::Das, value, &mut settings);
        assert_eq!(settings.das_ms, 380, "75% of 0..=500 → 375 → snaps to 380");
    }
}
