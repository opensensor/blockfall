//! Netplay test harness: in-process two-app end-to-end over real netcode UDP
//! (CI) plus the `TETRIS_NET=host:<port>` / `TETRIS_NET=join:<addr>` desktop
//! harness (N6). Module seam reserved by the orchestrator so wave-5 agents
//! (N5 ∥ N6) never edit `net/mod.rs` concurrently.
