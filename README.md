# Tetris-like Puzzle Game (Rust / Bevy)

A fast, modern falling-block puzzle game built on the Bevy ECS engine.

## Status

Pre-scaffold. See [PRD.md](PRD.md) for the product requirements, gameplay spec,
architecture, and milestone plan (M0–M6).

## Planned stack

- Rust (stable) + Bevy (pinned stable minor)
- `crates/tetris-core` — pure, deterministic game rules (no engine deps)
- `crates/tetris-app` — Bevy binary: rendering, input, audio, UI

## Build (once scaffolded)

```sh
cargo run
```
