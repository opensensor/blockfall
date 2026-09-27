//! Snapshot-driven side-panel HUD (T13).
//!
//! Reads **only** [`GameSnapshot`] data (score/level/lines/combo/b2b/next/
//! hold/hold_used) — no rules are recomputed here. Panels are anchored to
//! the letterboxed playfield every `Update` via [`crate::render::letterbox`],
//! so they track window resizes for free.
//!
//! Layout: the left panel holds the hold box (top) plus the `SCORE` /
//! `LEVEL` / `LINES` stat texts, the conditional `COMBO` / `B2B` indicators
//! and the pause chord hint; the right panel holds the next queue
//! (spawn-rotation mini pieces, `Settings::next_queue_size` clamped to
//! `1..=6` slots, never more than `snapshot.next.len()`).
//!
//! Public API reusable by later tasks (T17 menus/overlays, T19 juice):
//! - [`HudAnchor`] / [`hud_anchor`] — window size → playfield + panel anchors.
//! - [`hud_text_center`] — per-slot text anchor inside the left panel.
//! - [`hold_center`] / [`next_center`] — preview slot anchors.
//! - Markers [`NextPreview`], [`HoldPreview`], [`HudText`]/[`HudTextSlot`],
//!   [`HudMiniCell`] for queries and styling.
//! - [`HudFixture`] — test/QA hook forcing the HUD to render from a chosen
//!   snapshot instead of the live core.
//!
//! Entities are updated in place each frame; spawn/despawn only happens on
//! count changes (queue size, conditional indicators, hold contents).

use bevy::prelude::*;
use bevy::window::Window;

use tetris_core::board::COLS;
use tetris_core::game::GameSnapshot;
use tetris_core::piece::{Piece, Rotation};

use crate::core_bridge::GameCore;
use crate::input::{Bind, BindSlot, KeyBindings};
use crate::render::{self, GHOST_ALPHA, VISIBLE_ROWS};
use crate::state::Settings;

/// Panel gap between the playfield edge and the panel, in cell units.
const PANEL_GAP: f32 = 0.75;
/// Panel half-width (preview box half-size), in cell units.
const PANEL_HALF: f32 = 2.0;
/// Mini-preview cell size, in playfield cell units.
const MINI_FACTOR: f32 = 0.5;
/// Hard cap on rendered next previews (PRD settings slider max).
const MAX_NEXT: usize = 6;
/// Fallback next-queue size when no `Settings` resource exists.
const DEFAULT_QUEUE_SIZE: usize = 5;

/// Playfield-anchored coordinates for HUD/menu placement. Derived from
/// [`crate::render::letterbox`] so every consumer shares one layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HudAnchor {
    /// Square cell edge length in world units.
    pub cell: f32,
    /// World x of the playfield left edge.
    pub field_left: f32,
    /// World x of the playfield right edge.
    pub field_right: f32,
    /// World y of the playfield top edge.
    pub field_top: f32,
    /// World y of the playfield bottom edge.
    pub field_bottom: f32,
    /// World x of the left panel center line.
    pub left_panel_x: f32,
    /// World x of the right panel center line.
    pub right_panel_x: f32,
}

/// Compute the current HUD anchor set for a window size (resize aware).
pub fn hud_anchor(window_w: f32, window_h: f32) -> HudAnchor {
    let (cell, _, _) = render::letterbox(window_w, window_h);
    let half_w = cell * COLS as f32 * 0.5;
    let half_h = cell * VISIBLE_ROWS as f32 * 0.5;
    let panel = half_w + (PANEL_GAP + PANEL_HALF) * cell;
    HudAnchor {
        cell,
        field_left: -half_w,
        field_right: half_w,
        field_top: half_h,
        field_bottom: -half_h,
        left_panel_x: -panel,
        right_panel_x: panel,
    }
}

/// Which HUD text a [`HudText`] entity renders.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HudTextSlot {
    /// `SCORE` label + value.
    Score,
    /// `LEVEL` label + value.
    Level,
    /// `LINES` label + value.
    Lines,
    /// Combo chain indicator, present only while `snapshot.combo > 0`.
    Combo,
    /// Back-to-back indicator, present only while `snapshot.b2b`.
    B2B,
    /// Pause chord hint reflecting the current [`KeyBindings`] pause slot.
    PauseHint,
}

/// World-space center of a text slot inside the left panel.
pub fn hud_text_center(anchor: &HudAnchor, slot: HudTextSlot) -> Vec2 {
    let c = anchor.cell;
    let y = match slot {
        HudTextSlot::Score => anchor.field_top - 4.0 * c,
        HudTextSlot::Level => anchor.field_top - 7.0 * c,
        HudTextSlot::Lines => anchor.field_top - 10.0 * c,
        HudTextSlot::Combo => anchor.field_top - 13.0 * c,
        HudTextSlot::B2B => anchor.field_top - 15.0 * c,
        HudTextSlot::PauseHint => anchor.field_top - 18.0 * c,
    };
    Vec2::new(anchor.left_panel_x, y)
}

/// World-space center of the hold box.
pub fn hold_center(anchor: &HudAnchor) -> Vec2 {
    Vec2::new(anchor.left_panel_x, anchor.field_top - 1.25 * anchor.cell)
}

/// World-space center of next-queue slot `index` (0 = first preview).
pub fn next_center(anchor: &HudAnchor, index: usize) -> Vec2 {
    let c = anchor.cell;
    Vec2::new(
        anchor.right_panel_x,
        anchor.field_top - 1.25 * c - 3.0 * c * index as f32,
    )
}

/// Normalized spawn-rotation footprint of a piece: cells shifted so the
/// top-left occupied cell is `(0, 0)`, plus the `(w, h)` box size.
pub fn spawn_footprint(piece: Piece) -> ([(i32, i32); 4], f32, f32) {
    let raw = piece.cells(Rotation::Spawn);
    let min_r = raw.iter().map(|(r, _)| *r).min().unwrap_or(0);
    let min_c = raw.iter().map(|(_, c)| *c).min().unwrap_or(0);
    let norm: Vec<(i32, i32)> = raw.iter().map(|(r, c)| (r - min_r, c - min_c)).collect();
    let w = norm.iter().map(|(_, c)| *c).max().unwrap_or(0) + 1;
    let h = norm.iter().map(|(r, _)| *r).max().unwrap_or(0) + 1;
    let mut out = [(0, 0); 4];
    out.copy_from_slice(&norm);
    (out, w as f32, h as f32)
}

/// Child transform offsets (centered box) for a mini preview with cell size
/// `s`, relative to the preview root center.
pub fn mini_offsets(piece: Piece, s: f32) -> [Vec2; 4] {
    let (cells, w, h) = spawn_footprint(piece);
    let mut out = [Vec2::ZERO; 4];
    for (i, (r, c)) in cells.iter().enumerate() {
        out[i] = Vec2::new(
            (*c as f32 + 0.5 - w * 0.5) * s,
            (h * 0.5 - *r as f32 - 0.5) * s,
        );
    }
    out
}

/// Marker component for mini-preview cell sprites (next queue and hold box).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudMiniCell;

/// Root marker of one next-queue mini preview.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextPreview {
    /// Piece currently previewed in this slot.
    pub piece: Piece,
    /// Queue slot index (`0..count`).
    pub index: usize,
}

/// Root marker of the hold box preview.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldPreview {
    /// Parked piece (`None` while hold is empty).
    pub piece: Option<Piece>,
    /// `true` while `snapshot.hold_used` — previews draw dimmed then.
    pub used: bool,
}

/// Marker component on HUD [`Text2d`] entities.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HudText {
    /// Which slot this text renders.
    pub slot: HudTextSlot,
}

/// Test/QA hook: when set, the HUD renders from this snapshot instead of the
/// live [`GameCore`]. Still snapshot-driven — only the source changes.
#[derive(Resource, Default, Clone)]
pub struct HudFixture(pub Option<GameSnapshot>);

/// Pooled text entities (combo/b2b come and go with snapshot flags).
#[derive(Resource, Default)]
pub struct HudTextEntities {
    score: Option<Entity>,
    level: Option<Entity>,
    lines: Option<Entity>,
    combo: Option<Entity>,
    b2b: Option<Entity>,
    pause_hint: Option<Entity>,
}

/// Pooled preview root entities.
#[derive(Resource, Default)]
pub struct HudPreviewEntities {
    hold_root: Option<Entity>,
    next_roots: Vec<Entity>,
}

#[allow(clippy::type_complexity)]
type TextQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Text2d,
        &'static mut TextFont,
        &'static mut Transform,
    ),
    With<HudText>,
>;

type CellsQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Sprite, &'static mut Transform),
    (
        With<HudMiniCell>,
        Without<NextPreview>,
        Without<HoldPreview>,
    ),
>;

/// Resolve the snapshot to render: fixture override wins over the live core.
fn hud_snapshot(fixture: Option<&HudFixture>, core: Option<&GameCore>) -> Option<GameSnapshot> {
    if let Some(snapshot) = fixture.and_then(|f| f.0.clone()) {
        return Some(snapshot);
    }
    core.map(|core| core.game.snapshot())
}

fn pick_window_size(windows: &Query<&Window>) -> Option<Vec2> {
    let window = windows.iter().next()?;
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return None;
    }
    Some(size)
}

fn slot_entity(entities: &mut HudTextEntities, slot: HudTextSlot) -> &mut Option<Entity> {
    match slot {
        HudTextSlot::Score => &mut entities.score,
        HudTextSlot::Level => &mut entities.level,
        HudTextSlot::Lines => &mut entities.lines,
        HudTextSlot::Combo => &mut entities.combo,
        HudTextSlot::B2B => &mut entities.b2b,
        HudTextSlot::PauseHint => &mut entities.pause_hint,
    }
}

/// Desired text for a slot this frame (`None` = slot must be absent).
fn slot_text(slot: HudTextSlot, snapshot: &GameSnapshot) -> Option<String> {
    let text = match slot {
        HudTextSlot::Score => format!("SCORE\n{}", snapshot.score),
        HudTextSlot::Level => format!("LEVEL\n{}", snapshot.level),
        HudTextSlot::Lines => format!("LINES\n{}", snapshot.lines),
        HudTextSlot::Combo if snapshot.combo > 0 => format!("COMBO x{}", snapshot.combo),
        HudTextSlot::B2B if snapshot.b2b => "B2B".to_string(),
        HudTextSlot::Combo | HudTextSlot::B2B | HudTextSlot::PauseHint => return None,
    };
    Some(text)
}

/// Human-readable label for one bound input.
pub fn bind_label(bind: &Bind) -> String {
    match bind {
        Bind::Key(KeyCode::Escape) => "Esc".to_string(),
        Bind::Key(key) => format!("{key:?}").trim_start_matches("Key").to_string(),
        Bind::WheelUp => "WheelUp".to_string(),
        Bind::WheelDown => "WheelDown".to_string(),
    }
}

/// `PAUSE  Esc / P` from the pause chord currently in the bindings.
pub fn pause_hint_text(binds: &[Bind]) -> String {
    if binds.is_empty() {
        return "PAUSE  (unbound)".to_string();
    }
    format!(
        "PAUSE  {}",
        binds.iter().map(bind_label).collect::<Vec<_>>().join(" / ")
    )
}

fn spawn_text(
    commands: &mut Commands,
    slot: HudTextSlot,
    text: String,
    center: Vec2,
    font_size: f32,
) -> Entity {
    commands
        .spawn((
            HudText { slot },
            Text2d::new(text),
            TextFont {
                font_size: bevy::text::FontSize::Px(font_size),
                ..default()
            },
            TextColor::WHITE,
            Transform::from_xyz(center.x, center.y, 0.5),
        ))
        .id()
}

/// Insert-or-update one pooled text entity; despawn when `want` is `None`.
fn sync_text_slot(
    commands: &mut Commands,
    slot: HudTextSlot,
    want: Option<String>,
    slot_ref: &mut Option<Entity>,
    anchor: &HudAnchor,
    texts: &mut TextQuery,
) {
    let center = hud_text_center(anchor, slot);
    let font_size = match slot {
        HudTextSlot::Score | HudTextSlot::Level | HudTextSlot::Lines => anchor.cell * 0.45,
        _ => anchor.cell * 0.36,
    };
    let font_size = font_size.max(8.0);
    match (slot_ref.take(), want) {
        (Some(entity), Some(text)) => match texts.get_mut(entity) {
            Ok((mut content, mut font, mut transform)) => {
                if content.0 != text {
                    content.0 = text;
                }
                font.font_size = bevy::text::FontSize::Px(font_size);
                transform.translation = center.extend(0.5);
                *slot_ref = Some(entity);
            }
            Err(_) => {
                *slot_ref = Some(spawn_text(commands, slot, text, center, font_size));
            }
        },
        (None, Some(text)) => {
            *slot_ref = Some(spawn_text(commands, slot, text, center, font_size));
        }
        (Some(entity), None) => {
            commands.entity(entity).despawn();
        }
        (None, None) => {}
    }
}

/// Score/level/lines always present; combo/b2b only while active; pause
/// hint reflects the live pause chord.
fn sync_hud_texts(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    fixture: Option<Res<HudFixture>>,
    bindings: Option<Res<KeyBindings>>,
    windows: Query<&Window>,
    mut entities: ResMut<HudTextEntities>,
    mut texts: TextQuery,
) {
    let Some(snapshot) = hud_snapshot(fixture.as_deref(), core.as_deref()) else {
        return;
    };
    let Some(size) = pick_window_size(&windows) else {
        return;
    };
    let anchor = hud_anchor(size.x, size.y);

    let pause_binds = bindings
        .as_deref()
        .map(|b| b.slot(BindSlot::Pause).clone())
        .unwrap_or_else(|| KeyBindings::default_slot(BindSlot::Pause));

    for slot in [
        HudTextSlot::Score,
        HudTextSlot::Level,
        HudTextSlot::Lines,
        HudTextSlot::Combo,
        HudTextSlot::B2B,
    ] {
        let want = slot_text(slot, &snapshot);
        let slot_ref = slot_entity(&mut entities, slot);
        sync_text_slot(&mut commands, slot, want, slot_ref, &anchor, &mut texts);
    }
    let slot_ref = slot_entity(&mut entities, HudTextSlot::PauseHint);
    sync_text_slot(
        &mut commands,
        HudTextSlot::PauseHint,
        Some(pause_hint_text(&pause_binds)),
        slot_ref,
        &anchor,
        &mut texts,
    );
}

/// Spawn one next-queue preview root with its four mini cells.
fn spawn_next_preview(
    commands: &mut Commands,
    piece: Piece,
    index: usize,
    center: Vec2,
    s: f32,
) -> Entity {
    let root = commands
        .spawn((
            NextPreview { piece, index },
            Transform::from_xyz(center.x, center.y, 0.5),
        ))
        .id();
    for offset in mini_offsets(piece, s) {
        let cell = commands
            .spawn((
                HudMiniCell,
                Sprite {
                    color: render::piece_color(piece),
                    custom_size: Some(Vec2::splat(s)),
                    ..default()
                },
                Transform::from_translation(offset.extend(0.0)),
            ))
            .id();
        commands.entity(root).add_child(cell);
    }
    root
}

/// Re-arm one existing preview root: anchor, palette, footprint layout.
type NextQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut NextPreview,
        &'static mut Transform,
        &'static Children,
    ),
    (
        With<NextPreview>,
        Without<HoldPreview>,
        Without<HudMiniCell>,
    ),
>;

type HoldQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut HoldPreview,
        &'static mut Transform,
        Option<&'static Children>,
    ),
    (
        With<HoldPreview>,
        Without<NextPreview>,
        Without<HudMiniCell>,
    ),
>;

/// Placement for one mini preview: root center plus cell edge length.
struct MiniLayout {
    center: Vec2,
    s: f32,
}

fn refresh_next_preview(
    preview: &mut NextPreview,
    transform: &mut Transform,
    cell_entities: &[Entity],
    piece: Piece,
    index: usize,
    layout: &MiniLayout,
    cells: &mut CellsQuery,
) {
    transform.translation = layout.center.extend(0.5);
    let relaid = preview.piece != piece;
    preview.piece = piece;
    preview.index = index;
    let offsets = mini_offsets(piece, layout.s);
    let color = render::piece_color(piece);
    for (i, child) in cell_entities.iter().enumerate() {
        if let Ok((mut sprite, mut sprite_transform)) = cells.get_mut(*child) {
            sprite.color = color;
            sprite.custom_size = Some(Vec2::splat(layout.s));
            if relaid {
                let offset = offsets.get(i).copied().unwrap_or(Vec2::ZERO);
                sprite_transform.translation = offset.extend(0.0);
            }
        }
    }
}

fn hold_cell_color(piece: Piece, used: bool) -> Color {
    let color = render::piece_color(piece);
    if used {
        color.with_alpha(GHOST_ALPHA)
    } else {
        color
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_hud_previews(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    fixture: Option<Res<HudFixture>>,
    settings: Option<Res<Settings>>,
    windows: Query<&Window>,
    mut entities: ResMut<HudPreviewEntities>,
    mut next_q: NextQuery,
    mut hold_q: HoldQuery,
    mut cells: CellsQuery,
) {
    let Some(snapshot) = hud_snapshot(fixture.as_deref(), core.as_deref()) else {
        return;
    };
    let Some(size) = pick_window_size(&windows) else {
        return;
    };
    let anchor = hud_anchor(size.x, size.y);
    let mini = anchor.cell * MINI_FACTOR;

    // Next queue: pool roots to min(queue_size clamped 1..=6, next.len()).
    let configured = settings.as_deref().map_or(DEFAULT_QUEUE_SIZE, |s| {
        s.next_queue_size.clamp(1, MAX_NEXT as u8) as usize
    });
    let wanted = configured.min(snapshot.next.len());
    while entities.next_roots.len() > wanted {
        let entity = entities.next_roots.pop().expect("length checked");
        commands.entity(entity).despawn();
    }
    while entities.next_roots.len() < wanted {
        let index = entities.next_roots.len();
        let entity = spawn_next_preview(
            &mut commands,
            snapshot.next[index],
            index,
            next_center(&anchor, index),
            mini,
        );
        entities.next_roots.push(entity);
    }
    for (index, entity) in entities.next_roots.iter().copied().enumerate() {
        let Ok((mut preview, mut transform, children)) = next_q.get_mut(entity) else {
            continue;
        };
        let layout = MiniLayout {
            center: next_center(&anchor, index),
            s: mini,
        };
        refresh_next_preview(
            &mut preview,
            &mut transform,
            &children[..],
            snapshot.next[index],
            index,
            &layout,
            &mut cells,
        );
    }

    // Hold box: root always present, 0 or 4 mini cells.
    let hold_root = match entities.hold_root {
        Some(entity) => entity,
        None => {
            let entity = commands
                .spawn((
                    HoldPreview {
                        piece: None,
                        used: false,
                    },
                    Transform::from_translation(hold_center(&anchor).extend(0.5)),
                ))
                .id();
            entities.hold_root = Some(entity);
            return;
        }
    };
    let Ok((mut hold, mut root_transform, children)) = hold_q.get_mut(hold_root) else {
        return;
    };
    root_transform.translation = hold_center(&anchor).extend(0.5);
    let changed = hold.piece != snapshot.hold || hold.used != snapshot.hold_used;
    hold.piece = snapshot.hold;
    hold.used = snapshot.hold_used;
    let wanted_cells = if snapshot.hold.is_some() { 4 } else { 0 };
    let children: Vec<Entity> = children.map(|c| c.iter().collect()).unwrap_or_default();
    for extra in children.iter().skip(wanted_cells) {
        commands.entity(*extra).despawn();
    }
    if changed {
        if let Some(piece) = snapshot.hold {
            let color = hold_cell_color(piece, snapshot.hold_used);
            let offsets = mini_offsets(piece, mini);
            for (i, child) in children.iter().take(wanted_cells).enumerate() {
                if let Ok((mut sprite, mut sprite_transform)) = cells.get_mut(*child) {
                    sprite.color = color;
                    sprite.custom_size = Some(Vec2::splat(mini));
                    if let Some(offset) = offsets.get(i) {
                        sprite_transform.translation = offset.extend(0.0);
                    }
                }
            }
            for offset in &offsets[children.len()..wanted_cells] {
                let cell = commands
                    .spawn((
                        HudMiniCell,
                        Sprite {
                            color,
                            custom_size: Some(Vec2::splat(mini)),
                            ..default()
                        },
                        Transform::from_translation(offset.extend(0.0)),
                    ))
                    .id();
                commands.entity(hold_root).add_child(cell);
            }
        }
    }
}

/// Snapshot-driven HUD: side panels anchored to the letterboxed playfield.
pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HudFixture>()
            .init_resource::<HudTextEntities>()
            .init_resource::<HudPreviewEntities>()
            .add_systems(Update, (sync_hud_texts, sync_hud_previews));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::{CoreBridgePlugin, PendingActions};
    use crate::state::AppState;

    use bevy::app::FixedUpdate;
    use bevy::window::WindowPlugin;
    use tetris_core::actions::Action;

    const EPS: f32 = 1e-4;

    fn hud_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris T13 hud smoke".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, HudPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app.init_resource::<Settings>();
        app
    }

    fn frame(app: &mut App, actions: &[Action]) {
        {
            let mut pending = app.world_mut().resource_mut::<PendingActions>();
            for action in actions {
                pending.push(*action);
            }
        }
        app.world_mut().run_schedule(FixedUpdate);
        let _ = app.world_mut().try_run_schedule(Update);
    }

    fn snapshot(app: &App) -> GameSnapshot {
        app.world().non_send::<GameCore>().game.snapshot()
    }

    fn text_of(app: &mut App, slot: HudTextSlot) -> Option<String> {
        let mut query = app.world_mut().query::<(&HudText, &Text2d)>();
        let items: Vec<(HudTextSlot, String)> = query
            .iter(app.world())
            .map(|(text, content)| (text.slot, content.0.clone()))
            .collect();
        items
            .into_iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, content)| content)
    }

    fn next_previews(app: &mut App) -> Vec<(usize, Piece)> {
        let mut query = app.world_mut().query::<&NextPreview>();
        let mut previews: Vec<(usize, Piece)> = query
            .iter(app.world())
            .map(|preview| (preview.index, preview.piece))
            .collect();
        previews.sort_by_key(|(index, _)| *index);
        previews
    }

    fn hold_state(app: &mut App) -> (HoldPreview, Vec<Color>) {
        let mut query = app.world_mut().query::<(&HoldPreview, &Children)>();
        let (hold, children) = {
            let (hold, children) = query.single(app.world()).unwrap();
            (*hold, children.iter().collect::<Vec<_>>())
        };
        drop(query);
        let colors = children
            .iter()
            .map(|child| {
                app.world()
                    .get::<Sprite>(*child)
                    .expect("hold mini cell")
                    .color
            })
            .collect();
        (hold, colors)
    }

    #[test]
    fn stat_texts_mirror_the_snapshot() {
        let mut app = hud_app(0xC0FFEE);
        frame(&mut app, &[Action::HardDrop]);
        let snapshot = snapshot(&app);
        assert!(snapshot.score > 0, "precondition: hard drop scored");
        assert_eq!(
            text_of(&mut app, HudTextSlot::Score),
            Some(format!("SCORE\n{}", snapshot.score))
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Level),
            Some(format!("LEVEL\n{}", snapshot.level))
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::Lines),
            Some(format!("LINES\n{}", snapshot.lines))
        );
    }

    #[test]
    fn next_queue_size_bounds_preview_count() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        assert_eq!(next_previews(&mut app).len(), 5, "default size 5");

        app.world_mut().resource_mut::<Settings>().next_queue_size = 3;
        let _ = app.world_mut().try_run_schedule(Update);
        let previews = next_previews(&mut app);
        assert_eq!(previews.len(), 3);
        assert_eq!(
            previews.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        app.world_mut().resource_mut::<Settings>().next_queue_size = 6;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            next_previews(&mut app).len(),
            5,
            "clamped to snapshot.next.len()"
        );
        let expected: Vec<(usize, Piece)> =
            snapshot(&app).next.iter().copied().enumerate().collect();
        assert_eq!(
            next_previews(&mut app),
            expected,
            "pieces match snapshot.next"
        );
    }

    #[test]
    fn next_previews_match_palette_and_spawn_footprint() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        let anchor = hud_anchor(1280.0, 720.0);
        let mini = anchor.cell * MINI_FACTOR;
        let collected: Vec<(NextPreview, Vec<(Vec2, Color)>)> = {
            let mut query = app.world_mut().query::<(&NextPreview, &Children)>();
            let items: Vec<(NextPreview, Vec<Entity>)> = query
                .iter(app.world())
                .map(|(preview, children)| (*preview, children.iter().collect()))
                .collect();
            drop(query);
            items
                .into_iter()
                .map(|(preview, children)| {
                    let cells = children
                        .iter()
                        .map(|child| {
                            let sprite = app.world().get::<Sprite>(*child).unwrap();
                            let transform = app.world().get::<Transform>(*child).unwrap();
                            (transform.translation.xy(), sprite.color)
                        })
                        .collect();
                    (preview, cells)
                })
                .collect()
        };
        assert_eq!(collected.len(), 5);
        for (preview, cells) in &collected {
            assert_eq!(cells.len(), 4, "four mini cells per preview");
            assert_eq!(next_center(&anchor, preview.index).x, anchor.right_panel_x);
            for (offset, color) in cells {
                let on_footprint = mini_offsets(preview.piece, mini)
                    .into_iter()
                    .any(|expected| (expected - *offset).length() < EPS);
                assert!(on_footprint, "mini cell {offset:?} off spawn footprint");
                assert_eq!(
                    *color,
                    render::piece_color(preview.piece),
                    "preview palette via piece_color"
                );
            }
        }
    }

    #[test]
    fn hold_dims_while_hold_used() {
        let mut app = hud_app(1);
        // First press: parks the active piece, hold_used -> true.
        frame(&mut app, &[Action::Hold]);
        // One more HUD tick so the freshly spawned hold root gets populated.
        let _ = app.world_mut().try_run_schedule(Update);
        let after_hold = snapshot(&app);
        assert!(after_hold.hold.is_some());
        assert!(after_hold.hold_used, "precondition: hold just used");
        let (hold, colors) = hold_state(&mut app);
        assert_eq!(hold.piece, after_hold.hold);
        assert!(hold.used);
        assert_eq!(colors.len(), 4);
        for color in &colors {
            assert_eq!(color.to_srgba().alpha, GHOST_ALPHA, "dimmed preview");
            assert_eq!(
                color.with_alpha(1.0),
                render::piece_color(after_hold.hold.expect("hold piece")),
                "dim keeps the piece hue"
            );
        }

        // Locking the swapped-in piece resets hold_used; hold stays populated.
        frame(&mut app, &[Action::HardDrop]);
        let settled = snapshot(&app);
        assert!(settled.hold.is_some());
        assert!(!settled.hold_used);
        let (hold, colors) = hold_state(&mut app);
        assert!(!hold.used);
        assert_eq!(colors.len(), 4);
        for color in &colors {
            assert_eq!(color.to_srgba().alpha, 1.0, "undimmed preview");
        }
    }

    #[test]
    fn resize_reanchors_panels() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::Hold]);
        let before: Vec<Vec2> = {
            let mut query = app.world_mut().query::<&Transform>();
            let mut positions: Vec<Vec2> = query
                .iter(app.world())
                .map(|t| t.translation.xy())
                .collect();
            positions.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap().then(a.y.total_cmp(&b.y)));
            positions
        };
        {
            let mut windows = app.world_mut().query::<&mut Window>();
            windows
                .single_mut(app.world_mut())
                .unwrap()
                .resolution
                .set_physical_resolution(1600, 900);
        }
        let _ = app.world_mut().try_run_schedule(Update);
        let after: Vec<Vec2> = {
            let mut query = app.world_mut().query::<&Transform>();
            let mut positions: Vec<Vec2> = query
                .iter(app.world())
                .map(|t| t.translation.xy())
                .collect();
            positions.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap().then(a.y.total_cmp(&b.y)));
            positions
        };
        assert_ne!(before, after, "HUD must reflow on resize");

        // Anchors follow hud_anchor/letterbox for the new size exactly.
        let anchor = hud_anchor(1600.0, 900.0);
        assert!((anchor.cell - render::letterbox(1600.0, 900.0).0).abs() < EPS);
        let (hold, _) = hold_state(&mut app);
        assert!(hold.used);
        let hold_pos = {
            let mut query = app.world_mut().query::<(&HoldPreview, &Transform)>();
            let (_, transform) = query.single(app.world()).unwrap();
            transform.translation.xy()
        };
        assert!((hold_pos - hold_center(&anchor)).length() < EPS);
        let next_pos = {
            let mut query = app.world_mut().query::<(&NextPreview, &Transform)>();
            query
                .iter(app.world())
                .find(|(preview, _)| preview.index == 4)
                .map(|(_, transform)| transform.translation.xy())
                .expect("fifth preview exists")
        };
        assert!((next_pos - next_center(&anchor, 4)).length() < EPS);
    }

    #[test]
    fn combo_and_b2b_indicators_follow_the_snapshot() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Combo),
            None,
            "combo 0 hidden"
        );
        assert_eq!(
            text_of(&mut app, HudTextSlot::B2B),
            None,
            "b2b false hidden"
        );

        app.world_mut().resource_mut::<HudFixture>().0 = Some(GameSnapshot {
            combo: 3,
            b2b: true,
            ..snapshot(&app)
        });
        let _ = app.world_mut().try_run_schedule(Update);
        let combo = text_of(&mut app, HudTextSlot::Combo).expect("combo>0 shows indicator");
        assert!(combo.contains("COMBO") && combo.contains('3'), "{combo}");
        let b2b = text_of(&mut app, HudTextSlot::B2B).expect("b2b shows indicator");
        assert!(b2b.contains("B2B"), "{b2b}");

        app.world_mut().resource_mut::<HudFixture>().0 = None;
        let _ = app.world_mut().try_run_schedule(Update);
        assert_eq!(
            text_of(&mut app, HudTextSlot::Combo),
            None,
            "despawned again"
        );
        assert_eq!(text_of(&mut app, HudTextSlot::B2B), None, "despawned again");
    }

    #[test]
    fn pause_hint_reflects_the_bound_pause_chord() {
        let mut app = hud_app(1);
        frame(&mut app, &[]);
        let hint = text_of(&mut app, HudTextSlot::PauseHint).expect("hint present");
        assert!(
            hint.contains("PAUSE") && hint.contains("Esc") && hint.contains("P"),
            "{hint}"
        );

        let mut bindings = KeyBindings::default();
        bindings.set_slot(BindSlot::Pause, vec![Bind::Key(KeyCode::KeyF)]);
        app.insert_resource(bindings);
        let _ = app.world_mut().try_run_schedule(Update);
        let hint = text_of(&mut app, HudTextSlot::PauseHint).expect("hint present");
        assert_eq!(hint, "PAUSE  F", "rebound chord picked up");
    }

    #[test]
    fn no_entity_growth_across_frames() {
        let mut app = hud_app(1);
        frame(&mut app, &[Action::Hold]);
        for _ in 0..5 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        let entities_before = app.world().entities().len();
        let mut mini_query = app.world_mut().query::<&HudMiniCell>();
        let mini_before = mini_query.iter(app.world()).count();
        assert_eq!(mini_before, 24, "5 next previews + 4 hold cells");

        for _ in 0..20 {
            let _ = app.world_mut().try_run_schedule(Update);
        }
        let mut mini_query = app.world_mut().query::<&HudMiniCell>();
        assert_eq!(mini_query.iter(app.world()).count(), mini_before);
        assert_eq!(app.world().entities().len(), entities_before, "no leaks");
    }
}
