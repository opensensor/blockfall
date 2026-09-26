//! Snapshot-driven playfield renderer (T11).
//!
//! Draws flat colored cells **exclusively** from [`Game::snapshot`]: settled
//! board cells, the active piece, and the ghost (positioned at the core's
//! `ghost_row`). No rules are recomputed here. The visible field is the
//! 10x20 grid of board rows below `HIDDEN_ROWS`, letterboxed inside the
//! window with equal cell size on both axes. `GameCore` is a non-send
//! resource (T10), read via the `Option<NonSend<GameCore>>` parameter.
//!
//! Rendering model: one [`Sprite`] component (the Bevy 0.19 sprite entity;
//! its built-in quad mesh is supplied by the sprite pipeline) per cell,
//! pooled in [`CellPool`] and fully refreshed every `Update` run. The 60 Hz
//! fixed-step sim runs inside `RunFixedMainLoop`, strictly ahead of
//! `Update`, so simply reading the newest snapshot each `Update` keeps
//! render decoupled from sim rate.
//!
//! Public API reusable by later tasks:
//! - [`PIECE_COLORS`] / [`piece_color`] — canonical PRD-style piece palette
//!   (HUD mini-grids and the hold box, T13).
//! - [`GHOST_ALPHA`] — ghost dimming factor (T13 preview dimming parity, T19).
//! - [`letterbox`] — window size -> `(cell, offset_x, offset_y)` centering
//!   math for any playfield-anchored UI (T13, T17 overlays, T19 juice).
//! - [`frame_cells`] — snapshot -> flat list of visible [`SnapshotCell`]s
//!   (board + active + ghost, already clipped to visible rows).
//!
//! [`Game::snapshot`]: tetris_core::game::Game::snapshot

use bevy::color::Alpha;
use bevy::prelude::*;
use bevy::window::Window;

use tetris_core::board::{COLS, HIDDEN_ROWS, ROWS};
use tetris_core::game::GameSnapshot;
use tetris_core::piece::Piece;

use crate::core_bridge::GameCore;

/// Rows of the visible field (board rows `HIDDEN_ROWS..ROWS`).
pub const VISIBLE_ROWS: usize = ROWS - HIDDEN_ROWS;

/// Srgba channels (red, green, blue) per piece, in [`Piece::ALL`] order,
/// following the Tetris Guideline palette (PRD: flat colored cells, one
/// distinct color per tetromino).
pub const PIECE_RGB: [(f32, f32, f32); 7] = [
    (0.0, 0.75, 0.75),  // I  cyan
    (0.25, 0.35, 0.85), // J  blue
    (0.9, 0.5, 0.1),    // L  orange
    (0.95, 0.8, 0.1),   // O  yellow
    (0.2, 0.7, 0.25),   // S  green
    (0.65, 0.3, 0.8),   // T  purple
    (0.85, 0.2, 0.2),   // Z  red
];

/// Piece palette as full-alpha [`Color`]s, indexed by [`Piece::ALL`] order
/// (I, J, L, O, S, T, Z). Shared by T13 HUD mini-grids/hold box.
pub const PIECE_COLORS: [Color; 7] = [
    Color::Srgba(Srgba {
        red: PIECE_RGB[0].0,
        green: PIECE_RGB[0].1,
        blue: PIECE_RGB[0].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[1].0,
        green: PIECE_RGB[1].1,
        blue: PIECE_RGB[1].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[2].0,
        green: PIECE_RGB[2].1,
        blue: PIECE_RGB[2].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[3].0,
        green: PIECE_RGB[3].1,
        blue: PIECE_RGB[3].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[4].0,
        green: PIECE_RGB[4].1,
        blue: PIECE_RGB[4].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[5].0,
        green: PIECE_RGB[5].1,
        blue: PIECE_RGB[5].2,
        alpha: 1.0,
    }),
    Color::Srgba(Srgba {
        red: PIECE_RGB[6].0,
        green: PIECE_RGB[6].1,
        blue: PIECE_RGB[6].2,
        alpha: 1.0,
    }),
];

/// Alpha applied to the active piece color when drawing the ghost.
pub const GHOST_ALPHA: f32 = 0.3;

/// Solid fill color for cells locked/spawned as `piece`.
pub fn piece_color(piece: Piece) -> Color {
    let idx = Piece::ALL.iter().position(|p| *p == piece).unwrap_or(0);
    PIECE_COLORS[idx]
}

/// Which layer of the playfield a drawn cell belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    /// Settled board cell.
    Board,
    /// Cell of the falling piece.
    Active,
    /// Ghost landing cell (active piece color, dimmed).
    Ghost,
}

/// One visible cell of a render frame, derived purely from a snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCell {
    /// Full board row (`HIDDEN_ROWS..ROWS`).
    pub row: usize,
    /// Column (`0..COLS`).
    pub col: usize,
    /// Piece the cell is drawn with (board color, active/ghost color).
    pub piece: Piece,
    /// Layer this cell belongs to.
    pub kind: CellKind,
}

impl SnapshotCell {
    /// Fill color: piece color at full alpha, dimmed for the ghost.
    pub fn color(&self) -> Color {
        match self.kind {
            CellKind::Ghost => piece_color(self.piece).with_alpha(GHOST_ALPHA),
            _ => piece_color(self.piece),
        }
    }

    /// Draw order along the camera z axis (later = on top).
    pub fn z(&self) -> f32 {
        match self.kind {
            CellKind::Board => 0.0,
            CellKind::Ghost => 1.0,
            CellKind::Active => 2.0,
        }
    }
}

/// Letterbox fit of the 10x20 visible field inside a `window_w` x `window_h`
/// window: returns `(cell, offset_x, offset_y)` with a single square cell
/// size on both axes and the field centered in the remaining margin.
pub fn letterbox(window_w: f32, window_h: f32) -> (f32, f32, f32) {
    let cell = (window_w / COLS as f32).min(window_h / VISIBLE_ROWS as f32);
    let offset_x = (window_w - cell * COLS as f32) * 0.5;
    let offset_y = (window_h - cell * VISIBLE_ROWS as f32) * 0.5;
    (cell, offset_x, offset_y)
}

/// Flatten a snapshot into the visible cells to draw this frame: settled
/// board rows, active piece cells and ghost cells, each clipped to the
/// visible rows (spawn-buffer cells above row `HIDDEN_ROWS` are not drawn).
pub fn frame_cells(snapshot: &GameSnapshot) -> Vec<SnapshotCell> {
    let mut cells = Vec::with_capacity(ROWS * COLS + 8);
    for row in HIDDEN_ROWS..ROWS {
        for col in 0..COLS {
            if let Some(piece) = snapshot.board.get(row, col) {
                cells.push(SnapshotCell {
                    row,
                    col,
                    piece,
                    kind: CellKind::Board,
                });
            }
        }
    }
    if let Some(active) = snapshot.active {
        for (row, col) in active.cells() {
            push_visible(&mut cells, row, col, active.piece, CellKind::Active);
        }
        if let Some(ghost) = snapshot.ghost_row {
            for (row, col) in active.cells() {
                let shifted = ghost + (row - active.row);
                push_visible(&mut cells, shifted, col, active.piece, CellKind::Ghost);
            }
        }
    }
    cells
}

fn push_visible(cells: &mut Vec<SnapshotCell>, row: i32, col: i32, piece: Piece, kind: CellKind) {
    let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) else {
        return;
    };
    if !(HIDDEN_ROWS..ROWS).contains(&row) || col >= COLS {
        return;
    }
    cells.push(SnapshotCell {
        row,
        col,
        piece,
        kind,
    });
}

/// Marker component for pooled playfield cell sprites; stores the layer so
/// tests and later systems (T14 FPS diagnostics, T19 juice) can filter.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayfieldCell {
    /// Layer this sprite currently draws.
    pub kind: CellKind,
}

/// Pooled cell sprite entities (rebuilt to the needed count each frame).
#[derive(Resource, Default)]
struct CellPool {
    entities: Vec<Entity>,
}

/// Full refresh of all cell sprites from the newest snapshot. The fixed-step
/// sim has already run for this frame (`RunFixedMainLoop` precedes
/// `Update`), so this always reads the latest state; per-frame redraw is
/// fine at <= ~210 cells.
fn render_playfield(
    mut commands: Commands,
    core: Option<NonSend<GameCore>>,
    windows: Query<&Window>,
    mut pool: ResMut<CellPool>,
) {
    let (Some(core), Some(window)) = (core, windows.iter().next()) else {
        return;
    };
    let size = window.resolution.size();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let (cell, offset_x, offset_y) = letterbox(size.x, size.y);
    let left = -size.x * 0.5 + offset_x;
    let top = size.y * 0.5 - offset_y;

    let cells = frame_cells(&core.game.snapshot());
    // Keep the first `cells.len()` pooled entities, despawn the surplus tail.
    if pool.entities.len() > cells.len() {
        let surplus = pool.entities.split_off(cells.len());
        for entity in surplus {
            commands.entity(entity).despawn();
        }
    }

    for (index, frame_cell) in cells.iter().enumerate() {
        let x = left + (frame_cell.col as f32 + 0.5) * cell;
        let vis_row = frame_cell.row - HIDDEN_ROWS;
        let y = top - (vis_row as f32 + 0.5) * cell;
        let sprite = Sprite {
            color: frame_cell.color(),
            custom_size: Some(Vec2::splat(cell)),
            ..default()
        };
        let transform = Transform::from_xyz(x, y, frame_cell.z());
        if index < pool.entities.len() {
            let entity = pool.entities[index];
            commands.entity(entity).insert((
                PlayfieldCell {
                    kind: frame_cell.kind,
                },
                sprite,
                transform,
            ));
        } else {
            let entity = commands
                .spawn((
                    PlayfieldCell {
                        kind: frame_cell.kind,
                    },
                    sprite,
                    transform,
                ))
                .id();
            pool.entities.push(entity);
        }
    }
}

/// Flat colored playfield renderer fed exclusively by `Game::snapshot()`.
pub struct RenderPlugin;

impl Plugin for RenderPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CellPool>()
            .add_systems(Update, render_playfield);
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
    use tetris_core::game::Game;

    const EPS: f32 = 1e-4;

    /// Letterbox math: square cells on both axes, centered, 10:20 aspect
    /// preserved, cell as large as possible at several window shapes.
    #[test]
    fn letterbox_fits_and_centers_square_cells() {
        let check = |w: f32, h: f32, expect_cell: f32| {
            let (cell, ox, oy) = letterbox(w, h);
            assert!(
                (cell - expect_cell).abs() < EPS,
                "window {w}x{h}: cell {cell} != expected {expect_cell}"
            );
            assert!(cell * COLS as f32 <= w + EPS, "field wider than window");
            assert!(cell * VISIBLE_ROWS as f32 <= h + EPS, "field taller");
            assert!(
                (2.0 * ox + cell * COLS as f32 - w).abs() < EPS,
                "not horizontally centered: {w}x{h} -> ox {ox}"
            );
            assert!(
                (2.0 * oy + cell * VISIBLE_ROWS as f32 - h).abs() < EPS,
                "not vertically centered: {w}x{h} -> oy {oy}"
            );
            // Maximality: scaling the cell up by any epsilon breaks a bound.
            let bigger = cell + EPS;
            assert!(bigger * COLS as f32 > w || bigger * VISIBLE_ROWS as f32 > h);
        };
        check(1280.0, 720.0, 36.0); // height-limited, side pillar boxes
        check(320.0, 240.0, 12.0); // tiny window
        check(3440.0, 1440.0, 72.0); // ultrawide
        check(800.0, 800.0, 40.0); // square: height-limited
        check(100.0, 200.0, 10.0); // exact 10:20 fit, zero margin
    }

    fn render_app(seed: u64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "tetris T11 render smoke".into(),
                resolution: (1280, 720).into(),
                resizable: true,
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((CoreBridgePlugin, RenderPlugin));
        app.insert_non_send(GameCore::new(seed));
        app.init_resource::<AppState>();
        app
    }

    /// One scripted frame: apply `actions` through the real fixed schedule,
    /// then run `Update` (render).
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

    type Drawn = (CellKind, Color, Option<Vec2>, Vec3);

    fn drawn(app: &mut App) -> Vec<Drawn> {
        let mut query = app
            .world_mut()
            .query::<(&PlayfieldCell, &Sprite, &Transform)>();
        query
            .iter(app.world())
            .map(|(cell, sprite, transform)| {
                (
                    cell.kind,
                    sprite.color,
                    sprite.custom_size,
                    transform.translation,
                )
            })
            .collect()
    }

    fn cells_of(app: &mut App, kind: CellKind) -> Vec<Drawn> {
        drawn(app)
            .into_iter()
            .filter(|(cell_kind, ..)| *cell_kind == kind)
            .collect()
    }

    fn count(app: &mut App) -> usize {
        let mut query = app.world_mut().query::<&PlayfieldCell>();
        query.iter(app.world()).count()
    }

    /// Snapshot cell counts recomputed independently of `frame_cells` using
    /// only core APIs, for cross-checking the drawn entity count.
    fn expected_count(snapshot: &GameSnapshot) -> usize {
        let visible = |row: i32, col: i32| {
            (row as usize) >= HIDDEN_ROWS
                && (row as usize) < ROWS
                && col >= 0
                && (col as usize) < COLS
        };
        let board = (HIDDEN_ROWS..ROWS)
            .flat_map(|r| (0..COLS).map(move |c| (r, c)))
            .filter(|&(r, c)| snapshot.board.get(r, c).is_some())
            .count();
        let (active, ghost) = match snapshot.active {
            Some(ps) => {
                let active = ps.cells().iter().filter(|&&(r, c)| visible(r, c)).count();
                let ghost = match snapshot.ghost_row {
                    Some(g) => ps
                        .cells()
                        .iter()
                        .filter(|&&(r, c)| visible(g + (r - ps.row), c))
                        .count(),
                    None => 0,
                };
                (active, ghost)
            }
            None => (0, 0),
        };
        board + active + ghost
    }

    #[test]
    fn hidden_spawn_cells_are_not_drawn() {
        // Seed 1 spawns T (cells on rows 0..=1 => fully hidden).
        let fresh = Game::new(1).snapshot();
        let ps = fresh.active.expect("fresh game has an active piece");
        assert!(ps.cells().iter().all(|(r, _)| (*r as usize) < HIDDEN_ROWS));
        let mut app = render_app(1);
        frame(&mut app, &[]);
        let fresh = app.world().non_send::<GameCore>().game.snapshot();
        // Hidden active piece draws no Active cells, but its ghost lands
        // inside the visible field and is drawn.
        assert_eq!(count(&mut app), expected_count(&fresh));
        assert!(cells_of(&mut app, CellKind::Active).is_empty());
        assert_eq!(cells_of(&mut app, CellKind::Ghost).len(), 4);

        frame(&mut app, &[Action::SoftDrop]);
        assert_eq!(
            count(&mut app),
            expected_count(&app.world().non_send::<GameCore>().game.snapshot())
        );
    }

    #[test]
    fn cells_track_scripted_actions_with_prd_colors_and_alpha() {
        let mut app = render_app(1);
        let first = Game::new(1).snapshot().active.unwrap().piece;
        let second = Game::new(1).peek_next(1)[0];

        // Lock the first piece; its cells become board cells, next spawns hidden.
        frame(&mut app, &[Action::HardDrop]);
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        assert_eq!(snapshot.active.unwrap().piece, second);
        let board = cells_of(&mut app, CellKind::Board);
        let ghost = cells_of(&mut app, CellKind::Ghost);
        assert_eq!(
            count(&mut app),
            expected_count(&snapshot),
            "settled + ghost only; second piece spawns fully hidden"
        );
        assert_eq!(board.len() + ghost.len(), count(&mut app));
        assert!(
            !board.is_empty(),
            "hard drop must leave visible settled cells"
        );
        for (_, color, ..) in &board {
            assert_eq!(*color, piece_color(first), "board cell palette");
        }
        for (_, color, ..) in &ghost {
            assert_eq!(
                color.to_srgba(),
                piece_color(second).with_alpha(GHOST_ALPHA).to_srgba(),
                "ghost dimming uses the active piece color"
            );
        }

        // Drop the second piece into the visible field.
        for _ in 0..3 {
            frame(&mut app, &[Action::SoftDrop]);
        }
        let snapshot = app.world().non_send::<GameCore>().game.snapshot();
        let active = cells_of(&mut app, CellKind::Active);
        assert_eq!(count(&mut app), expected_count(&snapshot));
        assert_eq!(
            active.len(),
            4,
            "fully visible active piece draws four cells"
        );
        for (_, color, ..) in &active {
            assert_eq!(*color, piece_color(second), "active piece palette");
        }

        // 1280x720 -> cell 36, field 360x720 centered: check one known cell.
        let ps = snapshot.active.unwrap();
        let (cell, ox, _) = letterbox(1280.0, 720.0);
        let expected_x = -640.0 + ox + (ps.col as f32 + 0.5) * cell;
        let xs: Vec<f32> = active.iter().map(|(_, _, _, t)| t.x).collect();
        assert!(
            xs.iter().any(|x| (x - expected_x).abs() < EPS),
            "active cell column {expected_x} missing from {xs:?}"
        );
        for (_, _, custom_size, translation) in &active {
            assert_eq!(*custom_size, Some(Vec2::splat(cell)));
            assert_eq!(translation.z, 2.0, "active draws on top");
        }
    }

    #[test]
    fn sprites_reflow_on_window_resize() {
        let mut app = render_app(1);
        for _ in 0..3 {
            frame(&mut app, &[Action::SoftDrop]);
        }
        let (cell_old, ox_old, _) = letterbox(1280.0, 720.0);
        {
            let mut windows = app.world_mut().query::<&mut Window>();
            windows
                .single_mut(app.world_mut())
                .unwrap()
                .resolution
                .set_physical_resolution(800, 800);
        }
        let _ = app.world_mut().try_run_schedule(Update);
        let (cell, _, _) = letterbox(800.0, 800.0);
        assert_ne!(cell_old, cell);
        assert!(cell_old > 0.0 && ox_old > 0.0);

        let active = cells_of(&mut app, CellKind::Active);
        assert!(!active.is_empty());
        for (_, _, custom_size, _) in &active {
            assert_eq!(*custom_size, Some(Vec2::splat(cell)));
        }
    }
}
