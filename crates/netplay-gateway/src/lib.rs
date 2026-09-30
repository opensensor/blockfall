//! # netplay-gateway
//!
//! Introduce + dumb port-paired UDP relay enabling cross-WAN Blockfall 1v1
//! without port forwarding (gateway-plan.md). Single process, **zero
//! external dependencies** (std only), one IPv4 UDP control port plus a
//! data-port range; netcode payloads are opaque ciphertext here. No DB, no
//! persistence, no TLS, no game knowledge.
//!
//! ## Modules
//!
//! - [`wire`] — the control-frame codec. This is the **shared contract**:
//!   the game (`tetris-app`, G2) depends on it, so the byte layouts below
//!   are normative and pinned by tests.
//! - [`room`] — the pure [`room::Gateway`] state machine (rooms, GC, rate
//!   limits) over a virtual clock, plus the [`room::PortAllocator`] seam.
//! - `main` (binary `netplay-gateway`) — thin socket loop: nonblocking
//!   control + per-room data sockets, 100 ms poll timeout, `--self-test`
//!   for CI smoke (G4).
//!
//! ## Wire format
//!
//! Two packet classes on the control port, demuxed by first byte:
//! `0x2A` (`*`, [`wire::CONTROL_PREFIX`]) = control; **any other first
//! byte** = game data (no netcode wire byte is ever `0x2A` — pinned by
//! `wire` tests against the renetcode source). All frames packed
//! big-endian:
//!
//! ```text
//! *R <code:5B> <game_port:u16>            host register/keepalive (every 2 s)
//! *A <code:5B>                            ack
//! *G <code:5B>                            guest lookup / re-lookup (idempotent)
//! *F <code:5B> <host_ip:4B> <vport:u16>   found: connect host_ip:vport (relay)
//! *B <code:5B>                            busy (paired to a different guest)
//! *E <code:5B>                            no such room
//! *C <code:5B>                            collision: live code, different host IP
//! *D <code:5B>                            host explicit release (net_stop/exit)
//! *S <code:5B>                            slot exhaustion (gateway full / bind failed)
//! ```
//!
//! Room codes: 5 bytes from the confusion-free alphabet
//! `ABCDEFGHJKMNPQRSTUVWXYZ23456789` (no `I L O 0 1`); the gateway
//! validates membership and case-normalizes on decode.
//!
//! ## Ops summary
//!
//! See `README.md` next to the sources: `netplay-gateway --listen
//! 0.0.0.0:27016 --data-start 27017 --data-count 983 --idle-secs 15`;
//! firewall UDP `27016-27999`; unauthenticated control frames (codes are
//! the secret, payloads stay encrypted by netcode); RSS target < 2 MB.

pub mod room;
pub mod wire;
