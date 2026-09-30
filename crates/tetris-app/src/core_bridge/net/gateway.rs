//! Client-side gateway integration (gateway-plan.md G2): host room
//! registration, guest join-by-code, and the background DNS resolver that
//! fixes the trycloudflare-shaped gap on the join path.
//!
//! # What this layer is (and is not)
//!
//! The **wire codec** lives in the shared `netplay-gateway` crate
//! ([`netplay_gateway::wire`]) — the normative table the gateway also speaks.
//! This module is the *client*: it never reimplements the room FSM or the
//! netcode session. Hosting is one control UDP leg (a single ephemeral
//! `UdpSocket` shared by the host-registration and guest-lookup lifecycles —
//! control frames are matched to their pending op by room code, so one socket
//! per app is enough and cheap); the guest then hands the relay's
//! `gateway_ip:vport` to the existing [`net_join`] and rides the unchanged
//! client FSM. `vport` is **at the gateway**, so the address fed to `net_join`
//! is `resolve(gateway_host).ipv4 : vport` — *not* the `*F` `host_ip` (that is
//! the host's observed address, reserved for direct-race v2).
//!
//! # Driver shape (mirrors [`super::upnp`])
//!
//! Nothing slow runs on a frame. DNS ([`std::net::ToSocketAddrs`]) runs on a
//! `std::thread` whose handle is generation-stamped so a restart can never
//! interleave with a stale answer; results arrive over an mpsc mailbox the
//! [`gateway_control_system`] polls each frame (`PreUpdate`). Literal-IP
//! endpoints (and the
//! whole test suite, which uses `127.0.0.1`) resolve synchronously and never
//! spawn a thread. `NetStatus::Listening` starts room registration (`*R`
//! immediately, then every [`KEEPALIVE_INTERVAL`]); leaving it (any `net_stop`
//! path) sends `*D` once, best-effort (a single non-blocking send — a 7-byte
//! datagram on a non-blocking socket never blocks, so it is trivially inside
//! [`TEARDOWN_BUDGET`]). Guest lookup is `*G` → [`classify`]; every reply
//! variant has a mapped [`GuestLookupState`] + [`NetGatewayEvent`].
//!
//! # Zero cost when disabled
//!
//! Every system is `run_if`-gated on [`NetGateway::enabled`], so with the
//! gateway off there are no threads, no bound sockets, and no per-frame work
//! beyond reading one bool resource.
//!
//! # Host-side NAT punch (field fix, 2026-09-30 — gateway-plan.md)
//!
//! Symptom: room codes paired (guest got `*F`, showed "connecting…") but the
//! host never saw the guest's handshake. Cause: the relay forwarded to
//! `host_ip:game_port`, and the host's game socket — a UDP *server* — had
//! never sent a packet, so the host's router had no inbound mapping and
//! dropped the forwarded connect (LAN-hairpin through the WAN gateway fails
//! identically; the loopback E2E has no NAT and cannot see it).
//!
//! The gateway now answers every `*A` with `*V <code> <vport>` (the room's
//! virtual data port). With the gateway armed, `session::net_host` only
//! *binds* the game socket and holds it as
//! [`session::PendingHostSocket`](super::session::PendingHostSocket)
//! (`NetStatus::Listening` arrives immediately — the port is reserved); this
//! driver then:
//!
//! 1. sends `*R` (unchanged announce/keepalive);
//! 2. on `*A`: waits up to [`VPORT_TIMEOUT`] for `*V`;
//! 3. on `*V`: fires **one** punch datagram ([`PUNCH_BYTE`]) from the game
//!    socket at `gateway_ip:vport` — `gateway_ip` from the driver's own
//!    resolver — so the host router opens the mapping the relay forwards to,
//!    and hands the socket to netcode ([`session::handover_pending_host_socket`]);
//! 4. on timeout (`*A` never arrives in [`HANDOVER_TIMEOUT`], or `*V` never
//!    follows it): hands over punch-less — LAN/direct hosting must work, and
//!    the Host screen's `gateway offline` line explains the rest.
//!
//! Connect requests arriving during that ≤ 3 s window sit in the game
//! socket's OS receive buffer and are read by netcode right after the
//! handover (test-pinned).
//!
//! **Compat**: a *new game against an old gateway* never receives `*V` and
//! hands over after the timeout (punch-less, exactly the old behavior); an
//! *old game against a new gateway* drops `*V` silently (the strict pre-fix
//! wire decode rejects unknown type bytes, and clients drop undecodable
//! control datagrams). Both directions are pinned by tests.
//!
//! **Aging window**: a punch only keeps the NAT mapping alive while traffic
//! flows (~30–120 s of silence kills it, router-dependent) — a room left
//! empty after announcing relies on the auto-UPnP mapping (armed on
//! `Listening`, lease-renewed — untouched by this flow) or a manual forward.
//! Punch and UPnP are complementary: the punch covers the common
//! "guest joins shortly after hosting starts" case even when UPnP is off or
//! the router denies mapping; UPnP covers long-idle waiting rooms. (Same
//! note in the `netplay-gateway` crate's `room.rs` docs.)
//!
//! # Enablement (deviation from the plan prose — recorded in gateway-plan.md)
//!
//! [`parse_gateway_env`] is the spec's pure string map (unset → the default
//! endpoint, empty → off, otherwise the value). [`NetGateway::from_env`] is
//! *stricter on the unset case*: it enables **only** when `TETRIS_GATEWAY` is
//! explicitly set to a non-empty value. Rationale: the plan's own testing
//! mandate is "no network", and this plugin mounts inside `NetPlugin`, which
//! every existing headless net test builds and drives to `Listening`. A
//! default-on endpoint (`blockfall.opensensor.io`) would make those tests spawn
//! DNS threads and send live UDP to the production gateway. The always-on,
//! user-facing default is a product decision that lands with G3's
//! `NetProfile::gateway_enabled` toggle, which composes with the endpoint
//! (and `DEFAULT_GATEWAY_ENDPOINT`) via the public `enabled` field.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use netplay_gateway::wire::{self, Frame, RoomCode};

use super::session::{
    handover_pending_host_socket, net_join, NetRole, NetSession, NetStatus, PendingHostSocket,
};

/// Env var selecting the gateway (`host:port`). Unset → **disabled** in a
/// headless build (see the module-doc deviation note); empty string → disabled
/// everywhere; a non-empty value → enabled against that endpoint.
pub const GATEWAY_ENV: &str = "TETRIS_GATEWAY";

/// The endpoint used when the gateway is armed by default (G3 applies this
/// through `NetProfile::gateway_enabled`); never contacted automatically.
pub const DEFAULT_GATEWAY_ENDPOINT: &str = "blockfall.opensensor.io:27016";

/// Host keepalive cadence: re-`*R` this often while `Listening` (spec: every
/// 2 s; the gateway refreshes the room's GC clock on each).
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// How long a host waits for `*A` after `*R` (or after a collision retry)
/// before giving up and going [`HostRoomState::Offline`].
pub const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a background DNS resolution may take before the pending op is
/// failed as [`NetGatewayEvent::GatewayUnreachable`].
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a guest waits for the `*F`/`*E`/`*B`/`*S` answer to its `*G`
/// before [`NetGatewayEvent::LookupTimeout`]. Kept well under the transport's
/// own connect timeout — the UI never waits on netcode.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Best-effort ceiling on the teardown `*D` send (fire-and-forget; a
/// non-blocking UDP send is effectively instantaneous).
pub const TEARDOWN_BUDGET: Duration = Duration::from_millis(200);

/// How many distinct room codes a host tries before declaring
/// [`HostRoomState::Offline`] on repeated `*C` collisions.
pub const MAX_CODE_ATTEMPTS: u32 = 4;

/// Field fix (module docs): how long the host waits for the room ack after
/// the first `*R` before handing the bound game socket to netcode anyway —
/// LAN/direct hosting must never wait on the gateway.
pub const HANDOVER_TIMEOUT: Duration = Duration::from_secs(3);

/// Field fix (module docs): how long the host waits for the `*V` punch port
/// after the `*A` before handing over punch-less (an old gateway never sends
/// it, and a lost `*V` must not wedge hosting).
pub const VPORT_TIMEOUT: Duration = Duration::from_secs(3);

/// Field fix (module docs): the single punch datagram's payload — content is
/// irrelevant (it is relay-dropped until the guest arrives); only its
/// source tuple opens the NAT mapping. `+` (0x2B) can never be confused
/// with a control frame (`0x2A` prefix) or a netcode first byte (0..=6 |
/// seq<<4).
pub const PUNCH_BYTE: u8 = 0x2B;

/// Control frames are ≤ 13 bytes; the socket is non-blocking, so the drain
/// loop simply stops on `WouldBlock` — an over-large datagram is netcode
/// noise and simply fails to decode (dropped).
const CONTROL_READ_BUF: usize = 64;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Pure parse of a `TETRIS_GATEWAY` value: `None` (unset) → the default
/// endpoint string; empty/whitespace → `None` (disabled); otherwise the
/// trimmed value. `Some(endpoint)` means "the endpoint to use". Unit-tested
/// directly so no test needs to touch the process environment (which is global
/// and races across parallel tests).
#[must_use]
pub fn parse_gateway_env(raw: Option<&str>) -> Option<String> {
    match raw {
        None => Some(DEFAULT_GATEWAY_ENDPOINT.to_string()),
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    }
}

/// Result of [`parse_gateway_env`], flattened for storage on
/// [`NetGateway`] (`enabled` + `endpoint`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayConfig {
    /// Whether the gateway should run at all.
    pub enabled: bool,
    /// The `host:port` control endpoint (empty when disabled).
    pub endpoint: String,
}

impl GatewayConfig {
    /// Reads [`GATEWAY_ENV`]. See [`NetGateway::from_env`] for the enablement
    /// policy (this returns `enabled = true` whenever a non-empty value is
    /// present, and for an unset var keeps the default endpoint but leaves the
    /// [`NetGateway`] off — the two differ only on the unset case).
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_option(std::env::var(GATEWAY_ENV).ok())
    }

    /// Enablement from an explicit env presence: `Some(raw)` → enabled iff
    /// `raw` is non-empty; `None` (var absent) → **disabled** (see module docs).
    #[must_use]
    pub fn from_presence(raw: Option<Option<String>>) -> Self {
        match raw {
            Some(inner) => match parse_gateway_env(inner.as_deref()) {
                Some(endpoint) => Self {
                    enabled: true,
                    endpoint,
                },
                None => Self {
                    enabled: false,
                    endpoint: String::new(),
                },
            },
            None => Self {
                enabled: false,
                endpoint: String::new(),
            },
        }
    }

    /// The spec's pure mapping applied to an optional env value: unset →
    /// enabled against [`DEFAULT_GATEWAY_ENDPOINT`]. Used by the unit tests and
    /// available to G3 for the product default.
    #[must_use]
    pub fn from_option(raw: Option<String>) -> Self {
        match parse_gateway_env(raw.as_deref()) {
            Some(endpoint) => Self {
                enabled: true,
                endpoint,
            },
            None => Self {
                enabled: false,
                endpoint: String::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Public state (the surface G3's UI consumes)
// ---------------------------------------------------------------------------

/// Host-side room-registration progress (Host screen copy source).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HostRoomState {
    /// Not hosting (or torn down): no room advertised, no code held.
    #[default]
    Idle,
    /// `*R` sent, awaiting `*A` (also during a `*C` collision retry).
    Advertising(RoomCode),
    /// `*A` received — `Room XXXXX` is live and shareable.
    Announced(RoomCode),
    /// The gateway could not be reached / refused the room. The reason is
    /// human-readable (Host screen fallback line); LAN play is unaffected.
    Offline(String),
}

impl HostRoomState {
    /// The room code shown on screen while a room is live or being set up.
    #[must_use]
    pub fn code(&self) -> Option<RoomCode> {
        match self {
            HostRoomState::Advertising(code) | HostRoomState::Announced(code) => Some(*code),
            HostRoomState::Idle | HostRoomState::Offline(_) => None,
        }
    }
}

/// Guest-side lookup progress (Join screen copy source). Terminal variants are
/// sticky until the next [`join_room`]; the system never resurrects them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum GuestLookupState {
    /// No lookup in flight.
    #[default]
    Idle,
    /// Resolving the gateway endpoint (DNS) before sending `*G`.
    Resolving(RoomCode),
    /// `*G` sent, awaiting the gateway's answer.
    LookingUp(RoomCode),
    /// `*F`: `addr` is the relay endpoint handed to `net_join`.
    Found {
        /// The room code that was looked up.
        code: RoomCode,
        /// `resolve(gateway_host).ipv4 : vport` — connect here.
        addr: SocketAddr,
    },
    /// `*E`: no such room (unknown or expired).
    NotFound(RoomCode),
    /// `*B`: the room is already paired to a different guest ("match full").
    Busy(RoomCode),
    /// `*S`: the gateway has no free relay slot (soft retry later).
    SlotExhausted(RoomCode),
    /// No answer to `*G` within [`LOOKUP_TIMEOUT`].
    Timeout,
    /// The gateway endpoint could not be resolved / was unreachable.
    GatewayUnreachable(String),
}

/// The Bevy `Message` G3 subscribes to (same convention as [`NetEvent`]): one
/// per meaningful step so the Host and Join screens can render per-step copy.
#[derive(Message, Clone, Debug, PartialEq)]
pub enum NetGatewayEvent {
    /// Host: `*A` — `Room XXXXX` is live; display it alongside the UPnP line.
    RoomAnnounced(RoomCode),
    /// Host: the room stopped being reachable (DNS failed, gateway full,
    /// collision exhausted). Renders the one-line "gateway offline" fallback.
    RoomOffline(String),
    /// Guest: resolving the gateway endpoint.
    Resolving(RoomCode),
    /// Guest: `*G` sent, waiting on the gateway answer.
    LookingUp(RoomCode),
    /// Guest: `*F` — the room was found and `addr` is being handed to
    /// `net_join` (UI flips to the existing Connecting status next frame).
    Found {
        /// The room code looked up.
        code: RoomCode,
        /// The relay endpoint connected to.
        addr: SocketAddr,
    },
    /// Guest: `*E` — no such room.
    NotFound(RoomCode),
    /// Guest: `*B` — match full (paired to another guest).
    Busy(RoomCode),
    /// Guest: `*S` — gateway out of relay slots (retry later).
    SlotExhausted(RoomCode),
    /// Guest: no reply within [`LOOKUP_TIMEOUT`].
    LookupTimeout,
    /// Guest/gateway: endpoint unreachable (DNS failure or socket error).
    GatewayUnreachable(String),
}

/// Formats a [`RoomCode`] for display (`b"ABCDE"` → `"ABCDE"`).
#[must_use]
pub fn format_code(code: &RoomCode) -> String {
    String::from_utf8_lossy(code).into_owned()
}

/// A pending host transport handover (field fix — module docs): built by
/// the `*V` path (with the punch target) or the handover timeout (without),
/// consumed by the driver system into an exclusive world command.
#[derive(Clone, Copy, Debug)]
struct HandoverRequest {
    /// Punch the held game socket toward this address (`gateway_ip:vport`
    /// from `*V`) before netcode takes the socket over.
    punch: Option<SocketAddr>,
}

// ---------------------------------------------------------------------------
// Reply classification (pure)
// ---------------------------------------------------------------------------

/// The typed outcome of one gateway reply, keyed to the side that asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupOutcome {
    /// `*F` → connect to `gateway_ip:vport` (the caller substitutes the
    /// resolved gateway IP — `host_ip` in the frame is the observed host, not
    /// the relay address).
    Found { vport: u16 },
    /// `*E` → no such room.
    NotFound,
    /// `*B` → busy (paired to a different guest).
    Busy,
    /// `*S` → slot exhaustion (soft retry later).
    SlotExhausted,
    /// `*A` — an ack (never a lookup reply; a host-side acknowledgement).
    Ack,
    /// `*V` — the room's virtual data port for the host punch (field fix;
    /// host-side only — the guest never sees it, and a client that predates
    /// it drops the frame at the wire decode).
    VirtualPort { vport: u16 },
    /// `*C` — collision (host-side only).
    Collision,
    /// Anything else (a `*R`/`*D`/`*G` we should never receive back).
    Unexpected,
}

/// Classifies one decoded [`Frame`] into a [`LookupOutcome`]. Every frame
/// variant is mapped; the caller correlates by room code and state.
#[must_use]
pub fn classify(frame: &Frame) -> LookupOutcome {
    match frame {
        Frame::Found { vport, .. } => LookupOutcome::Found { vport: *vport },
        Frame::NotFound { .. } => LookupOutcome::NotFound,
        Frame::Busy { .. } => LookupOutcome::Busy,
        Frame::SlotExhausted { .. } => LookupOutcome::SlotExhausted,
        Frame::Ack { .. } => LookupOutcome::Ack,
        Frame::VirtualPort { vport, .. } => LookupOutcome::VirtualPort { vport: *vport },
        Frame::Collision { .. } => LookupOutcome::Collision,
        _ => LookupOutcome::Unexpected,
    }
}

// ---------------------------------------------------------------------------
// Endpoint resolution
// ---------------------------------------------------------------------------

/// Splits `"host:port"` on the last colon (port may be absent for probes).
fn split_endpoint(endpoint: &str) -> Option<(String, u16)> {
    let (host, port) = endpoint.rsplit_once(':')?;
    let port = port.trim().parse::<u16>().ok()?;
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Fast, synchronous path: `Some(addr)` when the host part is a literal IP
/// (numeric endpoints never need a resolver thread — this is what the whole
/// test suite and any IP-literal config uses). `None` means "needs DNS".
#[must_use]
pub fn resolve_endpoint(endpoint: &str) -> Option<SocketAddr> {
    let (host, port) = split_endpoint(endpoint)?;
    let ip = host.parse::<IpAddr>().ok()?;
    Some(SocketAddr::new(ip, port))
}

/// Blocking resolve preferring the first IPv4 (runs on the DNS thread, never a
/// frame). A literal IP short-circuits (mirrors [`resolve_endpoint`] /
/// `upnp::resolve_addr`).
pub fn resolve_hostname(endpoint: &str) -> Result<SocketAddr, String> {
    let (host, port) = split_endpoint(endpoint).ok_or("malformed gateway endpoint")?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("could not resolve {host}: {e}"))?;
    addrs
        .find(|a| a.is_ipv4())
        .ok_or_else(|| format!("no IPv4 address for {host}"))
}

/// Background DNS runner signature: resolve `endpoint`, report to the mailbox
/// stamped with `gen` (a stale generation is dropped, never interleaved).
pub type ResolveRunner = fn(String, Sender<GatewayTask>, u64);

/// The default runner: real [`resolve_hostname`] on the caller's thread.
fn default_resolve_runner(endpoint: String, replies: Sender<GatewayTask>, gen: u64) {
    let result = resolve_hostname(&endpoint);
    let _ = replies.send(GatewayTask::Resolved { gen, result });
}

/// A mailbox message (currently only DNS results — UDP replies are polled on
/// the non-blocking control socket in the system).
#[derive(Debug)]
pub enum GatewayTask {
    /// A resolution finished (generation-stamped).
    Resolved {
        /// Attempt generation at spawn time.
        gen: u64,
        /// Resolved relay/control address, or a human-readable failure.
        result: Result<SocketAddr, String>,
    },
}

// ---------------------------------------------------------------------------
// Resource
// ---------------------------------------------------------------------------

/// The gateway client resource (Bevy `Resource`), inserted by
/// [`NetGatewayPlugin`]. `enabled`/`endpoint`/`host`/`guest` are safe to read
/// from render/UI; the rest is driver bookkeeping.
#[derive(Resource)]
pub struct NetGateway {
    /// Master switch — every system is gated on it.
    pub enabled: bool,
    /// The `host:port` control endpoint (empty when disabled).
    pub endpoint: String,
    /// Host-side room registration progress.
    pub host: HostRoomState,
    /// Guest-side lookup progress.
    pub guest: GuestLookupState,
    /// Resolved gateway control address (populated once resolved; diagnostic).
    pub gateway_addr: Option<SocketAddr>,
    /// Optional explicit local bind for the control socket — **test seam**,
    /// see [`Self::with_leg_bind`]. `None` (production) → `0.0.0.0`.
    leg_bind: Option<IpAddr>,

    dns: ResolveRunner,
    tx: Sender<GatewayTask>,
    rx: Mutex<Receiver<GatewayTask>>,
    gen: u64,

    /// One ephemeral, non-blocking control socket for both legs.
    socket: Option<UdpSocket>,
    resolved: Option<SocketAddr>,
    resolving: bool,
    resolve_started: Option<Instant>,

    prev_status: Option<NetStatus>,
    code: Option<RoomCode>,
    game_port: Option<u16>,
    last_register: Option<Instant>,
    ack_deadline: Option<Instant>,
    attempts: u32,

    /// Field fix (module docs): deadline after which the bound game socket
    /// is handed to netcode regardless of gateway progress — set on the
    /// first `*R`, refreshed to the `*V` window when `*A` lands.
    handover: Option<Instant>,
    /// Field fix: the handover (with punch target when `*V` arrived) queued
    /// for the driver system's world command.
    handover_request: Option<HandoverRequest>,

    guest_code: Option<RoomCode>,
    lookup_started: Option<Instant>,
}

impl Default for NetGateway {
    fn default() -> Self {
        Self::disabled()
    }
}

impl NetGateway {
    /// A disabled gateway (no env read, no threads, no sockets).
    #[must_use]
    pub fn disabled() -> Self {
        let (tx, rx) = channel();
        Self {
            enabled: false,
            endpoint: String::new(),
            host: HostRoomState::Idle,
            guest: GuestLookupState::Idle,
            gateway_addr: None,
            leg_bind: None,
            dns: default_resolve_runner,
            tx,
            rx: Mutex::new(rx),
            gen: 0,
            socket: None,
            resolved: None,
            resolving: false,
            resolve_started: None,
            prev_status: None,
            code: None,
            game_port: None,
            last_register: None,
            ack_deadline: None,
            attempts: 0,
            handover: None,
            handover_request: None,
            guest_code: None,
            lookup_started: None,
        }
    }

    /// Builds the resource from the process environment. Enablement policy is
    /// [`GatewayConfig::from_presence`] (opt-in on an explicitly-set non-empty
    /// value — see the module-doc deviation note).
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_config(&GatewayConfig::from_presence(
            std::env::var(GATEWAY_ENV).ok().map(Some),
        ))
    }

    /// Builds the resource from a [`GatewayConfig`].
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        let mut gw = Self::disabled();
        gw.enabled = config.enabled;
        gw.endpoint = config.endpoint.clone();
        gw
    }

    /// Test helper: an enabled gateway pointed at `endpoint` (a literal IP
    /// resolves synchronously, so no thread is ever spawned in tests).
    #[must_use]
    pub fn test_with_endpoint(endpoint: &str) -> Self {
        let mut gw = Self::disabled();
        gw.enabled = true;
        gw.endpoint = endpoint.to_string();
        gw
    }

    /// Test seam: bind the control socket to an explicit local address
    /// instead of `0.0.0.0`. The gateway-relay E2E (`harness.rs`) runs host
    /// and guest in **one process**, and the relay attributes data-plane legs
    /// per source IP (gateway-plan.md G1 — on the WAN they always differ), so
    /// the host leg pins its control socket to `127.0.0.2` while the
    /// production-shaped guest legs keep the default (`127.0.0.1`). The game
    /// socket gets the matching bind through [`super::session::net_host_on`].
    /// Production never sets this.
    #[must_use]
    pub fn with_leg_bind(mut self, ip: IpAddr) -> Self {
        self.leg_bind = Some(ip);
        self
    }

    /// Test seam (`harness.rs` legacy-relay race): expire the pending
    /// punch/handover window immediately instead of waiting the real
    /// [`VPORT_TIMEOUT`]/[`HANDOVER_TIMEOUT`] (mirrors how the session tests
    /// fast-forward [`super::session::JOIN_TIMEOUT`]).
    #[cfg(test)]
    pub(crate) fn expire_handover_window_for_test(&mut self) {
        self.handover = Some(Instant::now() - Duration::from_millis(1));
    }

    // -- sockets + resolution ------------------------------------------------

    fn ensure_socket(&mut self) -> bool {
        if self.socket.is_some() {
            return true;
        }
        let bind = SocketAddr::new(
            self.leg_bind.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            0,
        );
        match UdpSocket::bind(bind) {
            Ok(sock) => {
                let _ = sock.set_nonblocking(true);
                self.socket = Some(sock);
                true
            }
            Err(_) => false,
        }
    }

    /// Makes sure the gateway endpoint is resolved: literal IPs resolve now
    /// (no thread); otherwise a generation-stamped DNS thread is kicked once.
    fn ensure_resolved(&mut self) {
        if self.resolved.is_some() || self.resolving {
            return;
        }
        if let Some(addr) = resolve_endpoint(&self.endpoint) {
            self.resolved = Some(addr);
            self.gateway_addr = Some(addr);
            return;
        }
        self.gen += 1;
        self.resolving = true;
        self.resolve_started = Some(Instant::now());
        let (runner, tx, gen) = (self.dns, self.tx.clone(), self.gen);
        let endpoint = self.endpoint.clone();
        std::thread::spawn(move || runner(endpoint, tx, gen));
    }

    fn send_frame(&mut self, frame: &Frame) -> bool {
        let Some(dst) = self.resolved else {
            return false;
        };
        if !self.ensure_socket() {
            return false;
        }
        let bytes = wire::encode(frame);
        self.socket
            .as_ref()
            .expect("socket ensured")
            .send_to(&bytes, dst)
            .is_ok()
    }

    /// Drains every control reply currently queued (non-blocking).
    fn drain_replies(&mut self) -> Vec<Frame> {
        let mut out = Vec::new();
        let Some(sock) = self.socket.as_ref() else {
            return out;
        };
        let mut buf = [0u8; CONTROL_READ_BUF];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    if let Ok(frame) = wire::decode(&buf[..n]) {
                        out.push(frame);
                    }
                }
                Err(err)
                    if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                {
                    break
                }
                Err(_) => break,
            }
        }
        out
    }

    // -- host --------------------------------------------------------------

    fn begin_host(&mut self, game_port: Option<u16>, now: Instant) {
        self.attempts = 1;
        self.game_port = game_port;
        self.code = Some(wire::gen_code());
        self.ensure_resolved();
        self.try_register(now);
        self.last_register = Some(now);
        self.ack_deadline = Some(now + ACK_TIMEOUT);
        // Field fix: the handover clock starts at the first `*R` — if the
        // gateway never acks, the bound game socket goes to netcode anyway
        // (LAN/direct hosting must not wait on the gateway).
        self.handover = Some(now + HANDOVER_TIMEOUT);
        self.handover_request = None;
        self.host = HostRoomState::Advertising(self.code.unwrap_or_default());
    }

    fn try_register(&mut self, now: Instant) -> bool {
        let Some(game_port) = self.game_port else {
            return false;
        };
        let Some(code) = self.code else { return false };
        let sent = self.send_frame(&Frame::Register { code, game_port });
        if sent {
            self.last_register = Some(now);
        }
        sent
    }

    fn tick_host(&mut self, ev: &mut Vec<NetGatewayEvent>, now: Instant) {
        // Field fix: the punch/handover window expired without `*V` (old
        // gateway, or a lost frame) — hand the bound socket to netcode
        // punch-less. A no-op if the handover already ran.
        if self.handover.is_some_and(|d| now >= d) {
            self.handover = None;
            if self.handover_request.is_none() {
                self.handover_request = Some(HandoverRequest { punch: None });
            }
        }
        // A lost/never-started resolution while still advertising: try again
        // (idempotent), and time it out against the deadline if it hangs.
        if self.resolved.is_none() {
            self.ensure_resolved();
        }
        let due = self
            .last_register
            .is_none_or(|t| now.saturating_duration_since(t) >= KEEPALIVE_INTERVAL);
        if due {
            self.try_register(now);
        }
        if matches!(self.host, HostRoomState::Advertising(_))
            && self.ack_deadline.is_some_and(|d| now >= d)
        {
            self.host_offline(ev, "gateway did not answer — room not registered");
        }
    }

    fn host_offline(&mut self, ev: &mut Vec<NetGatewayEvent>, reason: &str) {
        self.host = HostRoomState::Offline(reason.to_string());
        ev.push(NetGatewayEvent::RoomOffline(reason.to_string()));
        self.reset_host();
    }

    fn reset_host(&mut self) {
        self.code = None;
        self.game_port = None;
        self.last_register = None;
        self.ack_deadline = None;
        self.attempts = 0;
        self.handover = None;
        self.handover_request = None;
    }

    fn teardown_host(&mut self) {
        // `*D` best-effort (fire-and-forget, single non-blocking send).
        if let Some(code) = self.code {
            let _ = self.send_frame(&Frame::Release { code });
        }
        self.reset_host();
        self.host = HostRoomState::Idle;
    }

    // -- guest -------------------------------------------------------------

    fn begin_guest_lookup(&mut self, code: RoomCode, now: Instant) -> Vec<NetGatewayEvent> {
        let mut ev = Vec::new();
        self.guest_code = Some(code);
        self.lookup_started = None;
        self.ensure_resolved();
        if self.resolved.is_some() && self.send_frame(&Frame::Lookup { code }) {
            self.guest = GuestLookupState::LookingUp(code);
            self.lookup_started = Some(now);
            ev.push(NetGatewayEvent::LookingUp(code));
        } else {
            self.guest = GuestLookupState::Resolving(code);
            ev.push(NetGatewayEvent::Resolving(code));
        }
        ev
    }

    fn tick_guest(&mut self, ev: &mut Vec<NetGatewayEvent>, now: Instant) {
        // DNS hung (no result and no error): fail the pending guest lookup.
        if self.resolving
            && self.guest_code.is_some()
            && self
                .resolve_started
                .is_some_and(|s| now.saturating_duration_since(s) >= RESOLVE_TIMEOUT)
        {
            self.resolving = false;
            self.resolve_started = None;
            self.fail_lookup_unreachable(ev, "gateway name did not resolve in time");
            return;
        }
        if matches!(self.guest, GuestLookupState::LookingUp(_))
            && self
                .lookup_started
                .is_some_and(|s| now.saturating_duration_since(s) >= LOOKUP_TIMEOUT)
        {
            self.guest = GuestLookupState::Timeout;
            self.guest_code = None;
            self.lookup_started = None;
            ev.push(NetGatewayEvent::LookupTimeout);
        }
    }

    fn fail_lookup_unreachable(&mut self, ev: &mut Vec<NetGatewayEvent>, reason: &str) {
        self.guest = GuestLookupState::GatewayUnreachable(reason.to_string());
        self.guest_code = None;
        self.lookup_started = None;
        ev.push(NetGatewayEvent::GatewayUnreachable(reason.to_string()));
    }

    fn fail_host_unreachable(&mut self, ev: &mut Vec<NetGatewayEvent>, reason: &str) {
        if matches!(self.host, HostRoomState::Advertising(_)) {
            self.host_offline(ev, reason);
        }
    }

    // -- reply handling ----------------------------------------------------

    /// Applies one decoded reply. Returns `Some(addr)` when the guest found a
    /// room and must `net_join(addr)` (the system queues the handoff).
    fn handle_frame(&mut self, frame: &Frame, ev: &mut Vec<NetGatewayEvent>) -> Option<SocketAddr> {
        let outcome = classify(frame);
        match outcome {
            LookupOutcome::Ack => {
                if matches!(self.host, HostRoomState::Advertising(_))
                    && self.code == Some(frame.code())
                {
                    self.host = HostRoomState::Announced(frame.code());
                    self.ack_deadline = None;
                    // Field fix: the ack opens the `*V` window — punch and
                    // hand over when it lands, hand over punch-less when it
                    // expires (old gateway, or a lost `*V`).
                    self.handover = Some(Instant::now() + VPORT_TIMEOUT);
                    ev.push(NetGatewayEvent::RoomAnnounced(frame.code()));
                }
            }
            LookupOutcome::VirtualPort { vport } => {
                // Field fix: the punch port for the held game socket. Only
                // meaningful for the live announced room; everything else
                // (wrong code, guest leg, post-handover straggler from a
                // keepalive `*A`) is a silent no-op — the queued handover
                // itself no-ops once the socket is gone.
                if matches!(self.host, HostRoomState::Announced(_))
                    && self.code == Some(frame.code())
                {
                    if let Some(gw) = self.resolved {
                        self.handover = None;
                        self.handover_request = Some(HandoverRequest {
                            punch: Some(SocketAddr::new(gw.ip(), vport)),
                        });
                    }
                }
            }
            LookupOutcome::Collision => {
                if matches!(self.host, HostRoomState::Advertising(_))
                    && self.code == Some(frame.code())
                {
                    if self.attempts < MAX_CODE_ATTEMPTS {
                        self.attempts += 1;
                        self.code = Some(wire::gen_code());
                        self.host = HostRoomState::Advertising(self.code.unwrap_or_default());
                        self.ack_deadline = Some(Instant::now() + ACK_TIMEOUT);
                        self.try_register(Instant::now());
                    } else {
                        self.host_offline(ev, "room code in use — try again later");
                    }
                }
            }
            LookupOutcome::SlotExhausted => {
                if matches!(self.host, HostRoomState::Advertising(_))
                    && self.code == Some(frame.code())
                {
                    self.host_offline(ev, "gateway full — retry later");
                } else if matches!(self.guest, GuestLookupState::LookingUp(_))
                    && self.guest_code == Some(frame.code())
                {
                    self.guest = GuestLookupState::SlotExhausted(frame.code());
                    self.guest_code = None;
                    self.lookup_started = None;
                    ev.push(NetGatewayEvent::SlotExhausted(frame.code()));
                }
            }
            LookupOutcome::Found { vport } => {
                if matches!(self.guest, GuestLookupState::LookingUp(_))
                    && self.guest_code == Some(frame.code())
                {
                    // `vport` lives at the GATEWAY: connect to the resolved
                    // gateway IP with the relay vport (not the `*F` host_ip).
                    let addr = self.resolved.map(|gw| SocketAddr::new(gw.ip(), vport));
                    if let Some(addr) = addr {
                        self.guest = GuestLookupState::Found {
                            code: frame.code(),
                            addr,
                        };
                        self.guest_code = None;
                        self.lookup_started = None;
                        ev.push(NetGatewayEvent::Found {
                            code: frame.code(),
                            addr,
                        });
                        return Some(addr);
                    }
                }
            }
            LookupOutcome::NotFound => {
                if matches!(self.guest, GuestLookupState::LookingUp(_))
                    && self.guest_code == Some(frame.code())
                {
                    self.guest = GuestLookupState::NotFound(frame.code());
                    self.guest_code = None;
                    self.lookup_started = None;
                    ev.push(NetGatewayEvent::NotFound(frame.code()));
                }
            }
            LookupOutcome::Busy => {
                if matches!(self.guest, GuestLookupState::LookingUp(_))
                    && self.guest_code == Some(frame.code())
                {
                    self.guest = GuestLookupState::Busy(frame.code());
                    self.guest_code = None;
                    self.lookup_started = None;
                    ev.push(NetGatewayEvent::Busy(frame.code()));
                }
            }
            LookupOutcome::Unexpected => {}
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Free-function API (mirrors the net_host/net_join `&mut World` style)
// ---------------------------------------------------------------------------

/// Begins a guest lookup for `code`: resolves the gateway (sync for a literal
/// IP, else on the DNS thread), sends `*G`, and reports the typed result
/// through [`GuestLookupState`] + [`NetGatewayEvent`]. On `*F` the control
/// system hands the relay address to [`net_join`] — the existing client FSM is
/// reused verbatim, no session logic is duplicated. No-op when disabled.
pub fn join_room(world: &mut World, code: RoomCode) {
    if !world.resource::<NetGateway>().enabled {
        return;
    }
    let now = Instant::now();
    let events = world.resource_scope(|_world, mut gateway: Mut<NetGateway>| {
        gateway.begin_guest_lookup(code, now)
    });
    for event in events {
        world.write_message(event);
    }
}

/// The Found-path handoff: takes a resolved relay [`SocketAddr`] and drives
/// the existing [`net_join`] (a thin pass-through — deliberately not a second
/// join FSM). Also exposed for callers/tests that already hold an address.
pub fn net_join_by_code(world: &mut World, addr: SocketAddr) {
    net_join(world, addr);
}

// ---------------------------------------------------------------------------
// Plugin + systems
// ---------------------------------------------------------------------------

/// Run condition: every gateway system is skipped (zero cost) when off.
fn gateway_enabled(gateway: Res<NetGateway>) -> bool {
    gateway.enabled
}

/// The single `Update` driver for both legs: poll DNS results, drain control
/// replies, run the host Listening edge + keepalive + teardown, run the guest
/// timeouts, and hand a `*F` to `net_join`. Gated on [`gateway_enabled`].
fn gateway_control_system(
    mut gateway: ResMut<NetGateway>,
    session: Option<Res<NetSession>>,
    mut events: MessageWriter<NetGatewayEvent>,
    mut commands: Commands,
) {
    if !gateway.enabled {
        return;
    }
    let now = Instant::now();

    // 1. DNS results (generation-stamped; a stale answer never lands).
    let drained: Vec<GatewayTask> = {
        let rx = gateway
            .rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rx.try_iter().collect()
    };
    for task in drained {
        let GatewayTask::Resolved { gen, result } = task;
        if gen != gateway.gen {
            continue;
        }
        gateway.resolving = false;
        gateway.resolve_started = None;
        match result {
            Ok(addr) => {
                gateway.resolved = Some(addr);
                gateway.gateway_addr = Some(addr);
            }
            Err(reason) => {
                let mut ev = Vec::new();
                gateway.fail_host_unreachable(&mut ev, &reason);
                if gateway.guest_code.is_some() {
                    gateway.fail_lookup_unreachable(&mut ev, &reason);
                }
                for event in ev {
                    events.write(event);
                }
            }
        }
    }

    // 2. Control replies.
    let mut join_addr: Option<SocketAddr> = None;
    let mut ev = Vec::new();
    for frame in gateway.drain_replies() {
        if let Some(addr) = gateway.handle_frame(&frame, &mut ev) {
            join_addr = Some(addr);
        }
    }
    for event in ev.drain(..) {
        events.write(event);
    }

    // 3. Host lifecycle.
    let status = session.as_ref().map(|s| s.status.clone());
    let role = session.as_ref().map(|s| s.role);
    let listening = role == Some(NetRole::Host) && matches!(status, Some(NetStatus::Listening));
    let was_listening = matches!(gateway.prev_status, Some(NetStatus::Listening));
    let listen_port = session
        .as_ref()
        .and_then(|s| s.listen_addr.map(|a| a.port()));

    if was_listening && !listening {
        match status {
            // A peer connected: hand the room to the live session instead of
            // releasing it. The netcode traffic itself keeps the relay room
            // alive from here on (host packets refresh the room's GC clock in
            // `room.rs`), and a `*D` on this edge would close the room's
            // virtual data port *mid-handshake* — pulling the relay out from
            // under the match about to be played (found by the G5 crown
            // test, gateway-plan.md). The announcement clears (the code is no
            // longer shareable) but `self.code` survives so a later stop
            // still sends the `*D` the spec asks for.
            Some(NetStatus::Handshaking | NetStatus::Ready | NetStatus::InMatch) => {
                gateway.host = HostRoomState::Idle;
            }
            // Listening → Idle/BindFailed/no session: hosting really ended.
            _ => gateway.teardown_host(),
        }
    } else if gateway.code.is_some()
        && matches!(status, None | Some(NetStatus::Idle))
        && !matches!(gateway.prev_status, None | Some(NetStatus::Idle))
    {
        // Stop edge from any hosting phase (a match that started while the
        // room was already handed over above): `net_stop`/exit releases the
        // room — "on net_stop/teardown sends `*D`" per the plan.
        gateway.teardown_host();
    }
    if listening
        && !was_listening
        && matches!(
            gateway.host,
            HostRoomState::Idle | HostRoomState::Offline(_)
        )
    {
        gateway.begin_host(listen_port, now);
    }
    if listening {
        gateway.tick_host(&mut ev, now);
    }
    for event in ev.drain(..) {
        events.write(event);
    }

    // 4. Guest timeouts.
    gateway.tick_guest(&mut ev, now);
    for event in ev.drain(..) {
        events.write(event);
    }

    // 5. Field fix — deferred host transport handover (module docs): punch
    // the held game socket once toward `gateway_ip:vport` (only ever before
    // netcode owns the socket), then build the server from it. Queued so
    // the resource insert lands after this system, like the join handoff.
    if let Some(request) = gateway.handover_request.take() {
        commands.queue(move |world: &mut World| {
            if let Some(target) = request.punch {
                if let Some(pending) = world.get_resource::<PendingHostSocket>() {
                    // One datagram, contents irrelevant — this exists for
                    // its NAT side effect; the relay answers it with the
                    // guest traffic. A failed send must not eat the
                    // handover: LAN/direct still works.
                    let _ = pending.socket.send_to(&[PUNCH_BYTE], target);
                }
            }
            handover_pending_host_socket(world);
        });
    }

    // 6. Found → net_join handoff (deferred exclusive command; the transport
    // insert applies cleanly after this system).
    if let Some(addr) = join_addr {
        commands.queue(move |world: &mut World| {
            net_join_by_code(world, addr);
        });
    }

    gateway.prev_status = status;
}

/// Mounts [`NetGateway`] + [`NetGatewayEvent`] + the control system. Added
/// from `NetPlugin::build()` (alongside `NetLockstepPlugin`); every system is
/// `run_if`-gated on [`NetGateway::enabled`] so the whole feature is zero-cost
/// when disabled.
///
/// Runs in `PreUpdate` rather than `Update`: the driver only polls (DNS
/// mailbox + non-blocking control socket) and reacts to the previous frame's
/// [`NetStatus`], so a frame of latency is immaterial — and keeping its node
/// out of the `Update` graph avoids perturbing the topological order of
/// unrelated `Update` systems (notably the settings rebinding-capture system
/// and `pause_chord_system`, which share `RebindingCapture`).
pub struct NetGatewayPlugin;

impl Plugin for NetGatewayPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<NetGatewayEvent>()
            .insert_resource(NetGateway::from_env())
            .add_systems(PreUpdate, gateway_control_system.run_if(gateway_enabled));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // ---- config parse (pure; no env, no threads) --------------------------

    #[test]
    fn config_default_empty_custom() {
        // default (unset) → the spec's default endpoint string
        assert_eq!(
            parse_gateway_env(None).as_deref(),
            Some(DEFAULT_GATEWAY_ENDPOINT)
        );
        assert!(parse_gateway_env(Some("")).is_none());
        assert!(parse_gateway_env(Some("   ")).is_none());
        assert_eq!(
            parse_gateway_env(Some("gw.example:1234")).as_deref(),
            Some("gw.example:1234")
        );
        assert_eq!(
            parse_gateway_env(Some("  gw.example:1234  ")).as_deref(),
            Some("gw.example:1234"),
            "endpoint is trimmed"
        );
    }

    #[test]
    fn from_presence_enables_only_on_explicit_nonempty() {
        // var absent → disabled (headless default; the deviation is documented)
        assert!(!GatewayConfig::from_presence(None).enabled);
        // var present + empty → disabled
        let off = GatewayConfig::from_presence(Some(Some(String::new())));
        assert!(!off.enabled);
        // var present + value → enabled
        let on = GatewayConfig::from_presence(Some(Some("1.2.3.4:9".into())));
        assert!(on.enabled);
        assert_eq!(on.endpoint, "1.2.3.4:9");
    }

    #[test]
    fn from_option_maps_spec_default() {
        let cfg = GatewayConfig::from_option(None);
        assert!(cfg.enabled);
        assert_eq!(cfg.endpoint, DEFAULT_GATEWAY_ENDPOINT);
        assert!(!GatewayConfig::from_option(Some(String::new())).enabled);
    }

    #[test]
    fn disabled_gateway_has_no_endpoint() {
        let gw = NetGateway::disabled();
        assert!(!gw.enabled);
        assert!(gw.endpoint.is_empty());
        assert_eq!(gw.host, HostRoomState::Idle);
        assert_eq!(gw.guest, GuestLookupState::Idle);
        assert!(gw.resolved.is_none());
        assert!(gw.socket.is_none());
    }

    // ---- code generation via the shared wire crate ------------------------

    #[test]
    fn room_codes_are_alphabet_valid_length_5() {
        for _ in 0..500 {
            let code = wire::gen_code();
            assert_eq!(code.len(), 5);
            for &b in code.iter() {
                assert!(wire::is_code_byte(b), "byte {b:#04x} outside alphabet");
            }
        }
    }

    #[test]
    fn format_code_round_trips_bytes() {
        assert_eq!(format_code(b"ABCDE"), "ABCDE");
    }

    // ---- reply classification (every Frame variant) -----------------------

    #[test]
    fn classify_maps_every_frame_variant() {
        let cases: Vec<(Frame, LookupOutcome)> = vec![
            (
                Frame::Register {
                    code: *b"ABCDE",
                    game_port: 1,
                },
                LookupOutcome::Unexpected,
            ),
            (Frame::Ack { code: *b"ABCDE" }, LookupOutcome::Ack),
            (Frame::Lookup { code: *b"ABCDE" }, LookupOutcome::Unexpected),
            (
                Frame::Found {
                    code: *b"ABCDE",
                    host_ip: [1, 2, 3, 4],
                    vport: 27099,
                },
                LookupOutcome::Found { vport: 27099 },
            ),
            (Frame::Busy { code: *b"ABCDE" }, LookupOutcome::Busy),
            (Frame::NotFound { code: *b"ABCDE" }, LookupOutcome::NotFound),
            (
                Frame::Collision { code: *b"ABCDE" },
                LookupOutcome::Collision,
            ),
            (
                Frame::Release { code: *b"ABCDE" },
                LookupOutcome::Unexpected,
            ),
            (
                Frame::SlotExhausted { code: *b"ABCDE" },
                LookupOutcome::SlotExhausted,
            ),
        ];
        for (frame, want) in cases {
            assert_eq!(classify(&frame), want, "classify {frame:?}");
        }
    }

    // ---- endpoint resolution ----------------------------------------------

    #[test]
    fn resolve_endpoint_is_literal_ip_only() {
        assert_eq!(
            resolve_endpoint("127.0.0.1:27016"),
            Some("127.0.0.1:27016".parse().unwrap())
        );
        assert_eq!(resolve_endpoint("localhost:27016"), None, "needs DNS");
        assert_eq!(resolve_endpoint("no-colon"), None);
        assert_eq!(resolve_endpoint(":1234"), None, "empty host");
        assert_eq!(resolve_endpoint("h:notaport"), None);
    }

    #[test]
    fn resolve_hostname_localhost_prefers_ipv4() {
        // "localhost" only — no external DNS dependency.
        let addr = resolve_hostname("localhost:0").expect("localhost resolves");
        assert!(addr.is_ipv4(), "expected IPv4, got {addr}");
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn resolve_hostname_literal_ip_short_circuits() {
        assert_eq!(
            resolve_hostname("10.0.0.7:27016").expect("literal"),
            "10.0.0.7:27016".parse::<SocketAddr>().unwrap()
        );
    }

    // ---- pure transition tests (no App, no sockets) -----------------------

    #[test]
    fn host_frame_ack_announces() {
        let mut gw = NetGateway::disabled();
        gw.host = HostRoomState::Advertising(*b"ABCDE");
        gw.code = Some(*b"ABCDE");
        let mut ev = Vec::new();
        assert_eq!(
            gw.handle_frame(&Frame::Ack { code: *b"ABCDE" }, &mut ev),
            None
        );
        assert_eq!(gw.host, HostRoomState::Announced(*b"ABCDE"));
        assert!(ev.contains(&NetGatewayEvent::RoomAnnounced(*b"ABCDE")));
    }

    #[test]
    fn host_collision_retries_with_a_fresh_bounded_code() {
        let mut gw = NetGateway::disabled();
        gw.host = HostRoomState::Advertising(*b"ABCDE");
        gw.code = Some(*b"ABCDE");
        gw.attempts = 1;
        // resolved=None ⇒ try_register is a no-op, but state still cycles.
        let mut ev = Vec::new();
        for expected_attempts in 2..=MAX_CODE_ATTEMPTS {
            let before = gw.code.unwrap();
            gw.handle_frame(&Frame::Collision { code: before }, &mut ev);
            assert_eq!(gw.attempts, expected_attempts);
            let now_code = gw.code.unwrap();
            if expected_attempts < MAX_CODE_ATTEMPTS {
                assert!(matches!(gw.host, HostRoomState::Advertising(_)));
                assert_ne!(before, now_code, "collision must regenerate");
            }
        }
        // attempts == MAX: the *next* collision takes the offline branch.
        let final_code = gw.code.unwrap();
        gw.handle_frame(&Frame::Collision { code: final_code }, &mut ev);
        assert!(
            matches!(gw.host, HostRoomState::Offline(_)),
            "{:?}",
            gw.host
        );
        assert!(matches!(ev.last(), Some(NetGatewayEvent::RoomOffline(_))));
    }

    #[test]
    fn host_offline_on_ack_timeout() {
        let mut gw = NetGateway::disabled();
        gw.host = HostRoomState::Advertising(*b"ABCDE");
        gw.code = Some(*b"ABCDE");
        gw.ack_deadline = Some(Instant::now() - Duration::from_millis(1));
        let mut ev = Vec::new();
        gw.tick_host(&mut ev, Instant::now());
        assert!(matches!(gw.host, HostRoomState::Offline(_)));
        assert!(matches!(ev.first(), Some(NetGatewayEvent::RoomOffline(_))));
    }

    #[test]
    fn guest_found_uses_gateway_ip_with_vport() {
        let mut gw = NetGateway::disabled();
        gw.resolved = Some("203.0.113.9:27016".parse().unwrap());
        gw.guest = GuestLookupState::LookingUp(*b"ABCDE");
        gw.guest_code = Some(*b"ABCDE");
        let mut ev = Vec::new();
        let addr = gw.handle_frame(
            &Frame::Found {
                code: *b"ABCDE",
                host_ip: [10, 20, 30, 40], // observed host — MUST be ignored
                vport: 27050,
            },
            &mut ev,
        );
        let want: SocketAddr = "203.0.113.9:27050".parse().unwrap();
        assert_eq!(addr, Some(want), "connect gateway_ip:vport, not host_ip");
        assert_eq!(
            gw.guest,
            GuestLookupState::Found {
                code: *b"ABCDE",
                addr: want
            }
        );
    }

    #[test]
    fn guest_terminal_variants_are_mapped() {
        for (frame, want) in [
            (
                Frame::NotFound { code: *b"ABCDE" },
                GuestLookupState::NotFound(*b"ABCDE"),
            ),
            (
                Frame::Busy { code: *b"ABCDE" },
                GuestLookupState::Busy(*b"ABCDE"),
            ),
            (
                Frame::SlotExhausted { code: *b"ABCDE" },
                GuestLookupState::SlotExhausted(*b"ABCDE"),
            ),
        ] {
            let mut gw = NetGateway::disabled();
            gw.guest = GuestLookupState::LookingUp(*b"ABCDE");
            gw.guest_code = Some(*b"ABCDE");
            let mut ev = Vec::new();
            assert_eq!(gw.handle_frame(&frame, &mut ev), None);
            assert_eq!(gw.guest, want);
            assert_eq!(ev.len(), 1);
        }
    }

    #[test]
    fn frames_for_other_codes_are_ignored() {
        let mut gw = NetGateway::disabled();
        gw.host = HostRoomState::Advertising(*b"ABCDE");
        gw.code = Some(*b"ABCDE");
        let mut ev = Vec::new();
        gw.handle_frame(&Frame::Ack { code: *b"ZZZZZ" }, &mut ev);
        assert!(matches!(gw.host, HostRoomState::Advertising(_)));
        assert!(ev.is_empty());
    }

    #[test]
    fn lookup_timeout_fires() {
        let mut gw = NetGateway::disabled();
        gw.guest = GuestLookupState::LookingUp(*b"ABCDE");
        gw.guest_code = Some(*b"ABCDE");
        gw.lookup_started = Some(Instant::now() - LOOKUP_TIMEOUT);
        let mut ev = Vec::new();
        gw.tick_guest(&mut ev, Instant::now());
        assert_eq!(gw.guest, GuestLookupState::Timeout);
        assert!(ev.contains(&NetGatewayEvent::LookupTimeout));
    }

    #[test]
    fn dns_timeout_classifies_lookup_as_unreachable() {
        // No-op runner: a resolve that never reports back (a hung getaddrinfo).
        fn never(_ep: String, _tx: Sender<GatewayTask>, _gen: u64) {}
        let mut gw = NetGateway::disabled();
        gw.enabled = true;
        gw.dns = never;
        // Pretend a lookup is mid-DNS past the resolve deadline.
        gw.guest_code = Some(*b"ABCDE");
        gw.guest = GuestLookupState::Resolving(*b"ABCDE");
        gw.resolving = true;
        gw.resolve_started = Some(Instant::now() - RESOLVE_TIMEOUT);
        let mut ev = Vec::new();
        gw.tick_guest(&mut ev, Instant::now());
        assert!(matches!(gw.guest, GuestLookupState::GatewayUnreachable(_)));
        assert!(matches!(
            ev.first(),
            Some(NetGatewayEvent::GatewayUnreachable(_))
        ));
    }

    // ---- headless-App flow tests ------------------------------------------

    fn app_with_gateway(gw: NetGateway) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetGatewayPlugin);
        app.world_mut().insert_resource(gw);
        app.world_mut().init_resource::<NetSession>();
        app
    }

    fn drain_events(app: &mut App) -> Vec<NetGatewayEvent> {
        app.world_mut()
            .resource_mut::<Messages<NetGatewayEvent>>()
            .drain()
            .collect()
    }

    /// A loopback peer that collects every datagram the gateway control client
    /// sends (so `*R`/`*A`/`*C`/`*D` can be pinned) and can inject replies.
    struct SpyGateway {
        addr: SocketAddr,
        socket: UdpSocket,
    }

    impl SpyGateway {
        fn bind() -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").expect("spy bind");
            socket
                .set_read_timeout(Some(Duration::from_millis(1_000)))
                .unwrap();
            let addr = socket.local_addr().unwrap();
            Self { addr, socket }
        }

        fn endpoint(&self) -> String {
            self.addr.to_string()
        }

        fn send(&self, bytes: &[u8], to: SocketAddr) {
            self.socket.send_to(bytes, to).expect("spy send");
        }

        fn recv_frame(&self) -> Option<(Frame, SocketAddr)> {
            let mut buf = [0u8; 64];
            match self.socket.recv_from(&mut buf) {
                Ok((n, src)) => wire::decode(&buf[..n]).ok().map(|f| (f, src)),
                Err(_) => None,
            }
        }
    }

    fn set_host_listening(app: &mut App, port: u16) {
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)));
        }
        app.update();
    }

    #[test]
    fn disabled_gateway_costs_nothing() {
        // env-empty (disabled resource) ⇒ no state change, no socket, even when
        // the session goes Listening.
        let mut app = app_with_gateway(NetGateway::disabled());
        set_host_listening(&mut app, 0);
        let events = drain_events(&mut app);
        assert!(events.is_empty(), "no gateway events when disabled");
        let gw = app.world().resource::<NetGateway>();
        assert_eq!(gw.host, HostRoomState::Idle);
        assert_eq!(gw.guest, GuestLookupState::Idle);
        assert!(gw.socket.is_none(), "no socket bound when disabled");
        assert!(gw.resolved.is_none());
    }

    #[test]
    fn registers_on_listening_keepalives_and_releases_on_teardown() {
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));

        // Listening edge ⇒ a `*R` aimed at the game listen port.
        set_host_listening(&mut app, 4321);
        let (frame, src) = spy.recv_frame().expect("a *R on the Listening edge");
        let code = match frame {
            Frame::Register { code, game_port } => {
                assert_eq!(game_port, 4321, "*R carries the listen port");
                code
            }
            other => panic!("expected *R, got {other:?}"),
        };

        // `*A` ⇒ announced.
        spy.send(&wire::encode(&Frame::Ack { code }), src);
        let mut announced = false;
        for _ in 0..50 {
            app.update();
            if app.world().resource::<NetGateway>().host == HostRoomState::Announced(code) {
                announced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(announced, "*A must announce the room");
        assert!(drain_events(&mut app).contains(&NetGatewayEvent::RoomAnnounced(code)));

        // Keepalive: force the 2 s cadence to be due, expect a fresh *R.
        app.world_mut().resource_mut::<NetGateway>().last_register =
            Some(Instant::now() - KEEPALIVE_INTERVAL);
        app.update();
        let mut saw_keepalive = false;
        while let Some((frame, _)) = spy.recv_frame() {
            if matches!(frame, Frame::Register { code: c, .. } if c == code) {
                saw_keepalive = true;
                break;
            }
        }
        assert!(saw_keepalive, "keepalive *R expected on the 2 s cadence");

        // Teardown edge ⇒ a single `*D`.
        app.world_mut().resource_mut::<NetSession>().status = NetStatus::Idle;
        app.update();
        let mut saw_release = false;
        while let Some((frame, _)) = spy.recv_frame() {
            if matches!(frame, Frame::Release { code: c } if c == code) {
                saw_release = true;
                break;
            }
        }
        assert!(saw_release, "*D on teardown");
        assert_eq!(
            app.world().resource::<NetGateway>().host,
            HostRoomState::Idle
        );
    }

    #[test]
    fn collision_triggers_a_new_code_over_udp() {
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));
        set_host_listening(&mut app, 4444);

        let (frame, src) = spy.recv_frame().expect("first *R");
        let code = match frame {
            Frame::Register { code, .. } => code,
            other => panic!("expected *R, got {other:?}"),
        };
        // Reply `*C` ⇒ the client must send a `*R` with a *different* code.
        // The retry happens inside the control system, so the loop must run
        // frames while watching the spy for the fresh registration.
        spy.send(&wire::encode(&Frame::Collision { code }), src);
        let mut new_code = None;
        for _ in 0..50 {
            app.update();
            if let Some((Frame::Register { code: c, .. }, _)) = spy.recv_frame() {
                if c != code {
                    new_code = Some(c);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let new_code = new_code.expect("collision must retry a fresh code");
        assert!(new_code.iter().all(|&b| wire::is_code_byte(b)));
    }

    // ---- Field fix: `*V` punch + deferred transport handover ----

    use bevy_renet::netcode::NetcodeServerTransport;

    /// Listening edge with a **real** held game socket (what `net_host` does
    /// with the gateway armed), returning the socket's bound port.
    fn host_listening_with_pending_socket(app: &mut App) -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("game socket bind");
        let port = socket.local_addr().unwrap().port();
        app.world_mut()
            .insert_resource(PendingHostSocket { socket });
        set_host_listening(app, port);
        port
    }

    /// Runs frames until the held socket has been handed to netcode (or
    /// fails the assertion).
    fn await_handover(app: &mut App) {
        for _ in 0..100 {
            app.update();
            if app.world().contains_resource::<NetcodeServerTransport>() {
                assert!(
                    !app.world().contains_resource::<PendingHostSocket>(),
                    "handover must consume the pending socket"
                );
                assert_eq!(
                    app.world().resource::<NetSession>().status,
                    NetStatus::Listening,
                    "handover must not move NetStatus off Listening"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("transport never went live — handover did not run");
    }

    #[test]
    fn virtual_port_punches_held_socket_then_hands_over() {
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));
        let game_port = host_listening_with_pending_socket(&mut app);

        let (frame, src) = spy.recv_frame().expect("a *R on the Listening edge");
        let code = match frame {
            Frame::Register { code, game_port: p } => {
                assert_eq!(p, game_port);
                code
            }
            other => panic!("expected *R, got {other:?}"),
        };
        spy.send(&wire::encode(&Frame::Ack { code }), src);
        let mut announced = false;
        for _ in 0..50 {
            app.update();
            if app.world().resource::<NetGateway>().host == HostRoomState::Announced(code) {
                announced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(announced, "*A must announce before the *V applies");
        assert!(
            !app.world().contains_resource::<NetcodeServerTransport>(),
            "no transport may exist while the punch window is open"
        );

        // `*V` names the relay data port — the sink stands in for it.
        let sink = UdpSocket::bind("127.0.0.1:0").expect("sink bind");
        let vport = sink.local_addr().unwrap().port();
        sink.set_read_timeout(Some(Duration::from_millis(2_000)))
            .unwrap();
        spy.send(&wire::encode(&Frame::VirtualPort { code, vport }), src);

        await_handover(&mut app);
        // Exactly one punch datagram, from the game socket's port (the NAT
        // mapping the relay then aims guest traffic at), before handover.
        let mut buf = [0u8; 16];
        let (n, from) = sink.recv_from(&mut buf).expect("punch datagram");
        assert_eq!(&buf[..n], &[PUNCH_BYTE]);
        assert_eq!(
            from.port(),
            game_port,
            "the punch must leave the held game socket, not any other"
        );
    }

    #[test]
    fn ack_without_virtual_port_hands_over_on_timeout_punch_less() {
        // Old-gateway compat: `*A` but no `*V` ever arrives — after the
        // window the socket goes to netcode anyway (a lost *V must not hang
        // the host; LAN-direct guests can still reach the listening port).
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));
        host_listening_with_pending_socket(&mut app);
        let (frame, src) = spy.recv_frame().expect("*R");
        let code = match frame {
            Frame::Register { code, .. } => code,
            other => panic!("expected *R, got {other:?}"),
        };
        spy.send(&wire::encode(&Frame::Ack { code }), src);
        for _ in 0..50 {
            app.update();
            if matches!(
                app.world().resource::<NetGateway>().host,
                HostRoomState::Announced(_)
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        app.world_mut()
            .resource_mut::<NetGateway>()
            .expire_handover_window_for_test();
        await_handover(&mut app);
        assert!(
            app.world_mut()
                .resource::<NetGateway>()
                .handover_request
                .is_none(),
            "the queued handover must be consumed"
        );
    }

    #[test]
    fn never_acked_announce_still_hands_over_after_window() {
        // LAN/direct hosting with the gateway armed but unreachable: the
        // first-`*R`+3 s deadline hands over punch-less — the room line may
        // say gateway-offline, but hosting itself must not hang on DNS.
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));
        host_listening_with_pending_socket(&mut app);
        app.world_mut()
            .resource_mut::<NetGateway>()
            .expire_handover_window_for_test();
        await_handover(&mut app);
    }

    #[test]
    fn virtual_port_before_ack_is_ignored() {
        // A stray `*V` (unsolicited, wrong phase) must not punch, must not
        // hand over — the window only arms after `*A`.
        let spy = SpyGateway::bind();
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&spy.endpoint()));
        host_listening_with_pending_socket(&mut app);
        let (frame, src) = spy.recv_frame().expect("*R");
        let code = match frame {
            Frame::Register { code, .. } => code,
            other => panic!("expected *R, got {other:?}"),
        };
        spy.send(&wire::encode(&Frame::VirtualPort { code, vport: 9 }), src);
        for _ in 0..10 {
            app.update();
            assert!(
                app.world().contains_resource::<PendingHostSocket>()
                    && !app.world().contains_resource::<NetcodeServerTransport>(),
                "*V before *A must not hand over"
            );
        }
        // After the ack, the same `*V` path works (window armed).
        spy.send(&wire::encode(&Frame::Ack { code }), src);
        for _ in 0..50 {
            app.update();
            if matches!(
                app.world().resource::<NetGateway>().host,
                HostRoomState::Announced(_)
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let sink = UdpSocket::bind("127.0.0.1:0").expect("sink bind");
        let vport = sink.local_addr().unwrap().port();
        sink.set_read_timeout(Some(Duration::from_millis(2_000)))
            .unwrap();
        spy.send(&wire::encode(&Frame::VirtualPort { code, vport }), src);
        await_handover(&mut app);
        let mut buf = [0u8; 16];
        sink.recv_from(&mut buf).expect("punch after the ack");
    }

    #[test]
    fn stray_virtual_port_on_idle_gateway_is_inert() {
        // Old-game tolerance mirrors the wire-crate decoder pin at the
        // client level: no host state, no handover, no event.
        let mut gw = NetGateway::disabled();
        let mut ev = Vec::new();
        gw.handle_frame(
            &Frame::VirtualPort {
                code: *b"ABCDE",
                vport: 9,
            },
            &mut ev,
        );
        assert!(ev.is_empty());
        assert!(gw.handover_request.is_none());
        assert_eq!(gw.host, HostRoomState::Idle);
    }

    // The real in-process relay fixture lives in `super::testutil` (shared
    // with the G3 online_ui flow tests).
    use super::testutil::{raw_socket, recv_ack_then_vport, recv_frame, reg, spawn_real_gateway};

    #[test]
    fn real_gateway_host_registers_and_is_announced() {
        let rg = spawn_real_gateway(4);
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        set_host_listening(&mut app, 5151);

        // *A should turn Advertising → Announced within a few frames.
        let mut announced = false;
        for _ in 0..50 {
            app.update();
            let events = drain_events(&mut app);
            if events
                .iter()
                .any(|e| matches!(e, NetGatewayEvent::RoomAnnounced(_)))
            {
                announced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(announced, "real gateway must ack *R with *A");
        assert!(matches!(
            app.world().resource::<NetGateway>().host,
            HostRoomState::Announced(_)
        ));
    }

    #[test]
    fn real_gateway_independent_hosts_both_announce() {
        // Seed ABCDE as a live host on 127.0.0.2 (acked), then let the client
        // (on 127.0.0.1) register its own random, non-colliding code — both
        // legs coexist. The `*C` → fresh-code *retry* itself is exercised
        // deterministically over a spy in `collision_triggers_a_new_code_over_udp`
        // (the client's code is random, so forcing a live collision against the
        // real gateway is not deterministic here).
        let rg = spawn_real_gateway(4);
        let seeder = raw_socket("127.0.0.2");
        seeder
            .send_to(&reg(b"ABCDE", 6000), rg.ctrl)
            .expect("seed reg");
        assert!(matches!(recv_frame(&seeder), Some(Frame::Ack { .. })));
        // The client registers its own (random, non-colliding) code → announced.
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        set_host_listening(&mut app, 5151);
        let mut ok = false;
        for _ in 0..50 {
            app.update();
            if drain_events(&mut app)
                .iter()
                .any(|e| matches!(e, NetGatewayEvent::RoomAnnounced(_)))
            {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ok, "client registers a distinct code and is announced");
    }

    #[test]
    fn real_gateway_join_found_hands_off_to_net_join() {
        let rg = spawn_real_gateway(4);
        // Host leg on 127.0.0.2 registers ABCDE.
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        assert!(matches!(recv_frame(&host), Some(Frame::Ack { .. })));

        // Guest client on 127.0.0.1 looks up ABCDE ⇒ *F ⇒ net_join is invoked.
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        let code = *b"ABCDE";
        let events = {
            let world = app.world_mut();
            join_room(world, code);
            let mut acc = Vec::new();
            for _ in 0..50 {
                acc.extend(drain_events(&mut app));
                if acc
                    .iter()
                    .any(|e| matches!(e, NetGatewayEvent::Found { .. }))
                {
                    break;
                }
                app.update();
                std::thread::sleep(Duration::from_millis(20));
            }
            acc
        };
        let found = events
            .iter()
            .find_map(|e| match e {
                NetGatewayEvent::Found { addr, .. } => Some(*addr),
                _ => None,
            })
            .expect("Found for ABCDE");
        assert_eq!(found.ip().to_string(), "127.0.0.1");
        assert_ne!(found.port(), rg.ctrl.port(), "must be a relay vport");
        // The handoff drove net_join → the client FSM is now Connecting.
        app.update();
        assert_eq!(
            app.world().resource::<NetSession>().status,
            NetStatus::Connecting,
            "Found must hand off to net_join"
        );
    }

    #[test]
    fn real_gateway_join_busy_when_paired_elsewhere() {
        let rg = spawn_real_gateway(4);
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        // The field-fix *V announces the relay port directly — no lookup leg
        // needed to learn it.
        let vport = recv_ack_then_vport(&host);
        // Pin the guest slot with first data from 127.0.0.3.
        let guest = raw_socket("127.0.0.3");
        guest
            .send_to(b"\x00pin", SocketAddr::from(([127, 0, 0, 1], vport)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));

        // Client lookup from 127.0.0.1 ⇒ *B.
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        join_room(app.world_mut(), *b"ABCDE");
        let mut busy = false;
        for _ in 0..50 {
            if drain_events(&mut app).contains(&NetGatewayEvent::Busy(*b"ABCDE")) {
                busy = true;
                break;
            }
            app.update();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(busy, "a third IP must get *Busy after pairing");
    }

    #[test]
    fn real_gateway_join_notfound_unknown_code() {
        let rg = spawn_real_gateway(4);
        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        join_room(app.world_mut(), *b"ZZZZZ");
        let mut notfound = false;
        for _ in 0..50 {
            if drain_events(&mut app).contains(&NetGatewayEvent::NotFound(*b"ZZZZZ")) {
                notfound = true;
                break;
            }
            app.update();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(notfound);
    }

    #[test]
    fn real_gateway_host_slot_exhaustion_offlines() {
        // One slot, pre-consumed on 127.0.0.2 ⇒ the client's register is *S.
        let rg = spawn_real_gateway(1);
        let seeder = raw_socket("127.0.0.2");
        seeder.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        assert!(matches!(recv_frame(&seeder), Some(Frame::Ack { .. })));

        let mut app = app_with_gateway(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        set_host_listening(&mut app, 5151);
        let mut offline = false;
        for _ in 0..50 {
            if drain_events(&mut app)
                .iter()
                .any(|e| matches!(e, NetGatewayEvent::RoomOffline(_)))
            {
                offline = true;
                break;
            }
            app.update();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(offline, "*S (slot full) must offline the host");
    }
}

// ---------------------------------------------------------------------------
// Shared in-process relay fixture (test-only): the REAL
// `netplay_gateway::room::Gateway` driven over loopback UDP. Lives here so
// the G3 `online_ui` flow tests exercise the very same relay without
// duplicating it.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use netplay_gateway::room::{
        Gateway as Relay, GatewayConfig as RelayConfig, PortAllocator, VClock,
    };
    use std::collections::HashMap;
    use std::net::Ipv4Addr;

    pub struct RealGateway {
        pub ctrl: SocketAddr,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for RealGateway {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// Real allocator: one non-blocking `UdpSocket` per virtual data port on
    /// loopback (mirrors the binary's `SockAllocator`) so the **data plane**
    /// actually flows — needed to pin a guest for the `*B` path.
    struct RealAlloc {
        sockets: HashMap<u16, UdpSocket>,
    }
    impl RealAlloc {
        fn new() -> Self {
            Self {
                sockets: HashMap::new(),
            }
        }
        fn ports(&self) -> Vec<u16> {
            self.sockets.keys().copied().collect()
        }
        fn socket(&self, port: u16) -> Option<&UdpSocket> {
            self.sockets.get(&port)
        }
    }
    impl PortAllocator for RealAlloc {
        fn bind(&mut self, port: u16) -> Result<(), ()> {
            let sock =
                UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|_| ())?;
            sock.set_nonblocking(true).map_err(|_| ())?;
            self.sockets.insert(port, sock);
            Ok(())
        }
        fn close(&mut self, port: u16) {
            self.sockets.remove(&port);
        }
    }

    fn free_loopback_port() -> u16 {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe bind");
        sock.local_addr().expect("probe addr").port()
    }

    /// Runs the real `netplay_gateway::Gateway` on a thread: a control socket
    /// on `127.0.0.1:0` plus real per-room data sockets, drained both ways each
    /// 10 ms (mirrors the binary's `serve`). Host/guest legs in tests bind
    /// **distinct** loopback addresses — the relay distinguishes legs per-IP
    /// (G1 note).
    pub fn spawn_real_gateway(data_ports: u16) -> RealGateway {
        let control = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("control bind");
        control.set_nonblocking(true).unwrap();
        let ctrl = control.local_addr().unwrap();
        let control_port = ctrl.port();
        let data_base = free_loopback_port();
        let config = RelayConfig {
            control_port,
            data_port_start: data_base,
            data_ports,
            ..RelayConfig::default()
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = std::sync::Arc::clone(&stop);
        let mut gw = Relay::new(config, RealAlloc::new());
        let start = Instant::now();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                let now = VClock(start.elapsed().as_millis() as u64);
                // Control port.
                loop {
                    match control.recv_from(&mut buf) {
                        Ok((n, src)) => {
                            let replies = gw.on_packet(control_port, src, &buf[..n], now);
                            for (dst, reply) in replies {
                                let _ = control.send_to(&reply, dst);
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::WouldBlock | ErrorKind::Interrupted
                            ) =>
                        {
                            break
                        }
                        Err(_) => break,
                    }
                }
                // Data ports (replies/forwards leave from the receiving socket).
                for port in gw.allocator().ports() {
                    loop {
                        let received = gw
                            .allocator()
                            .socket(port)
                            .map(|sock| sock.recv_from(&mut buf));
                        let received = match received {
                            Some(result) => result,
                            None => break,
                        };
                        match received {
                            Ok((n, src)) => {
                                let replies = gw.on_packet(port, src, &buf[..n], now);
                                if let Some(sock) = gw.allocator().socket(port) {
                                    for (dst, reply) in replies {
                                        let _ = sock.send_to(&reply, dst);
                                    }
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::Interrupted
                                ) =>
                            {
                                break
                            }
                            Err(_) => break,
                        }
                    }
                }
                let _ = gw.on_tick(now);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        RealGateway {
            ctrl,
            stop,
            handle: Some(handle),
        }
    }

    /// A raw control socket bound to a specific loopback address.
    pub fn raw_socket(loopback: &str) -> UdpSocket {
        let sock = UdpSocket::bind((loopback, 0)).expect("raw bind");
        sock.set_read_timeout(Some(Duration::from_millis(1_500)))
            .unwrap();
        sock
    }

    pub fn recv_frame(sock: &UdpSocket) -> Option<Frame> {
        let mut buf = [0u8; 64];
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => wire::decode(&buf[..n]).ok(),
            Err(_) => None,
        }
    }

    /// Reads a host register reply pair — `*A` then the field-fix `*V` — and
    /// returns the announced virtual data port. Raw host legs must consume
    /// both frames before any further reads on the control socket.
    pub fn recv_ack_then_vport(sock: &UdpSocket) -> u16 {
        assert!(
            matches!(recv_frame(sock), Some(Frame::Ack { .. })),
            "expected *A"
        );
        match recv_frame(sock) {
            Some(Frame::VirtualPort { vport, .. }) => vport,
            other => panic!("expected *V after *A, got {other:?}"),
        }
    }

    pub fn reg(code: &[u8; 5], port: u16) -> Vec<u8> {
        wire::encode(&Frame::Register {
            code: *code,
            game_port: port,
        })
    }
}
