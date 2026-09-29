//! Netplay session lifecycle (netplay-plan.md N2): the [`NetSession`] state
//! machine, the [`NetPlugin`] transport wiring, and the `net_host`/`net_join`
//!/`net_stop` free-function API consumed by N3 (lockstep) and N5 (menu UI).
//!
//! # Ownership model
//!
//! `NetPlugin` registers the four bevy_renet plugins (renet server/client +
//! netcode transports — the netcode pair is **required in addition**, they
//! own `send_packets`; see the Verified API notes in [`super`]). All renet
//! systems are `run_if(resource_exists)`, so *inserting* the transport
//! resources starts the machinery and *removing* them stops it and frees the
//! UDP port synchronously — that is the whole `net_stop()` primitive, and it
//! keeps the plugin at zero runtime cost while [`NetStatus::Idle`] (no
//! transport resources → every gated system skipped).
//!
//! # Who writes what
//!
//! - **Free functions** (called by N5 from exclusive systems / the N6
//!   harness, via `&mut World`): `net_host` (bind `0.0.0.0:port`, never
//!   panics — a bind error becomes `BindFailed`), `net_join`, `net_stop`
//!   (graceful: sends netcode disconnect packets *before* dropping the
//!   transport resources, so the peer sees a clean loss instead of waiting
//!   for the 15 s transport timeout).
//! - **Bridging systems** (`Update`, gated on transport existence): drain
//!   `RenetServerEvent` observer notes, run the app-level handshake
//!   (host: `Hello` version gate + `D = max(local, peer)` adoption; guest:
//!   send `Hello` on connect, learn the negotiated `D` from
//!   `MatchStart.match_delay`), detect disconnects, and run the ~10 s
//!   `JoinTimeout` watchdog (the transport's own timeout is hard-coded at
//!   15 s for `ClientAuthentication::Unsecure`, so the app watchdog must
//!   fire first — Verified API notes).
//! - **Consumers**: every status transition emits a [`NetEvent`]
//!   (`Messages<NetEvent>`, the repo's message convention) for N5;
//!   `NetEvent::Desync { tick }` is emitted by N3's lockstep on this same
//!   message stream.
//!
//! # Channel-ownership boundary for N3
//!
//! While `Handshaking` (host) / `Handshaking | Ready` (guest) the systems
//! here **drain the renet message channels**; the instant the status enters
//! `InMatch` they stop polling, so N3's lockstep systems own every
//! `TickInput`/`TickBatch`/`SnapshotHash` message from the first match frame
//! on. N3 must therefore also consume `Bye` in its own drain — this module
//! only handles `Bye` while still inside the handshake window.
//!
//! # API surprises resolved here (beyond the Verified API notes)
//!
//! - The built-in `client_just_connected`/`client_just_disconnected`
//!   predicates each carry a `Local<bool>`, so each is usable exactly once
//!   per app per schedule; the bridge instead edge-detects on
//!   `is_connected()`/`is_disconnected()` directly — robust with any number
//!   of systems.
//! - `renet::RenetClient` is constructed in `Connecting` (not
//!   `Disconnected`), so a fresh `net_join` never false-fires the
//!   disconnect path, and the guest's `Hello` settles `Handshaking →
//!   Ready` exactly one frame after it is queued (the frame the netcode
//!   `RenetSend` stage flushes it).
//! - Dropping a peer's `App` **without** `net_stop()` sends no packets:
//!   UDP sockets close un-flushed, so the survivor only learns of the loss
//!   after netcode's hard-coded 15 s timeout. All teardown UIs must go
//!   through `net_stop()` (graceful, ~1 frame); the loopback test below
//!   documents the same.
//! - **Host-side loss reasons are coarse**: renet's `remove_connection`
//!   (`renet-2.0.0/src/server.rs:129-134`) emits `ClientDisconnected` with
//!   the reason the renet connection carries — which the netcode transport
//!   never sets — so every netcode-originated loss (clean peer disconnect
//!   *and* timeout) surfaces as `DisconnectReason::Transport`
//!   (`NetLossReason::Transport`) on the host. The guest keeps full
//!   netcode granularity via `disconnect_reason()` (Denied/Timeout/…).
//!   `NetcodeServerTransport::time_since_last_received_packet` is the
//!   escape hatch if N5 ever needs to split them host-side.
//! - `RenetServer::new_local_client` gives single-app tests a connection
//!   seam that bypasses netcode UDP entirely (used for the version-mismatch
//!   kick path; `process_local_client` shuttles the reliable channels).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use bevy_renet::netcode::{
    ClientAuthentication, NetcodeClientPlugin, NetcodeClientTransport, NetcodeDisconnectReason,
    NetcodeServerPlugin, NetcodeServerTransport, ServerAuthentication, ServerConfig,
};
use bevy_renet::renet::{
    ConnectionConfig, DefaultChannel, DisconnectReason as RenetDisconnectReason, ServerEvent,
};
use bevy_renet::{
    RenetClient, RenetClientPlugin, RenetServer, RenetServerEvent, RenetServerPlugin,
};

use super::protocol::{self, NetMsg, PROTOCOL_ID, PROTOCOL_VERSION};

/// Seconds a guest waits in [`NetStatus::Connecting`] before
/// [`NetEvent::JoinTimeout`] fires. Must stay below the netcode token's
/// hard-coded 15 s connect timeout (Verified API notes) so the UI never
/// waits on the transport.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var overriding the locally *desired* input delay in ticks (mirror of
/// the `SEED_ENV` pattern). Read at `net_host`/`net_join` time; the value
/// actually used is the negotiated `D = max(host, guest)` published on
/// [`NetSession::input_delay`].
pub const NET_DELAY_ENV: &str = "TETRIS_NET_DELAY";

/// Default desired input delay when `TETRIS_NET_DELAY` is unset
/// (≈133 ms at 60 Hz — netplay-plan.md Overview).
pub const DEFAULT_INPUT_DELAY: u8 = 8;

/// Lower clamp for [`NET_DELAY_ENV`] (N3's ring buffers assume the range).
pub const MIN_INPUT_DELAY: u8 = 2;
/// Upper clamp for [`NET_DELAY_ENV`].
pub const MAX_INPUT_DELAY: u8 = 30;

/// Which end of the connection this app is (plan: host = netcode server =
/// `Side::Left`, guest = netcode client = `Side::Right`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetRole {
    #[default]
    Host,
    Guest,
}

/// Why a connection was lost (host and guest share the vocabulary).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetLossReason {
    /// The peer closed cleanly or netcode reported a peer-side close.
    PeerDisconnected,
    /// Netcode connect/connection timeout (host offline, packet blackhole).
    Timeout,
    /// Netcode rejected the request — `max_clients: 1` is full
    /// ("match full", distinguishable client-side; Verified API notes).
    Denied,
    /// Transport-level failure (IO / renet channel error). On the **host**
    /// this is also what any netcode-originated peer loss reports — clean
    /// exit and timeout alike (netcode flattens them; see module docs).
    Transport,
}

/// Session FSM state. `Idle` (the default) means no sockets are held and
/// every net system is skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NetStatus {
    /// No transport, no peer.
    #[default]
    Idle,
    /// Host is bound and awaiting a challenger.
    Listening,
    /// The bind attempt failed (message is the `std::io::Error` string).
    BindFailed(String),
    /// Guest is running the netcode connect dance.
    Connecting,
    /// Netcode connected; the app-level `Hello` exchange is in flight.
    Handshaking,
    /// Handshake done; waiting for the host to start a match (`MatchStart`).
    Ready,
    /// A mirror is live — N3's lockstep owns the tick loop from here.
    InMatch,
    /// A peer was lost; the reason rides along for N5's overlay copy.
    Lost(NetLossReason),
}

/// The one Bevy `Message` N5 subscribes to; emitted for every transition
/// the UI needs. `Desync` is emitted by N3, `ByeReceived` on a clean peer
/// exit — both on this stream.
#[derive(Message, Clone, Debug, PartialEq)]
pub enum NetEvent {
    /// Handshake completed (host: valid `Hello` accepted; guest: `Hello`
    /// sent and in flight) — status became `Ready`.
    PeerConnected,
    /// A peer/connection was lost — status became `Lost`.
    PeerLost(NetLossReason),
    /// Host: the guest's `Hello.version` ≠ [`PROTOCOL_VERSION`]; the guest
    /// was kicked and the host is `Listening` again. (The kicked guest
    /// itself sees `PeerLost(PeerDisconnected)`.)
    VersionMismatch,
    /// A `net_host`/`net_join` bind failed (port in use …) — status became
    /// `BindFailed`.
    BindFailed(String),
    /// Still `Connecting` after [`JOIN_TIMEOUT`] — status became
    /// `Lost(Timeout)`. Netcode offers no host-side "match full" signal, so
    /// N5's copy stays "host offline or match full".
    JoinTimeout,
    /// Mirror divergence detected at `tick` (emitted by N3).
    Desync {
        /// Lockstep tick whose snapshot hashes stopped matching.
        tick: u64,
    },
    /// The peer sent a clean [`NetMsg::Bye`] — status became `Lost`.
    ByeReceived,
}

/// Events that drive the FSM; consumed by the pure [`next_status`] and by
/// [`NetSession::apply`]. One per meaningful occurrence — the bridging
/// systems translate renet/netcode observations into these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetTrigger {
    /// Host bind + transport construction succeeded.
    BindOk,
    /// Bind failed, carrying the error message.
    BindFailed(String),
    /// Guest initiated `net_join`.
    Connect,
    /// `net_stop()` — tear everything down to `Idle`.
    Stop,
    /// A peer's netcode connection came up (host: `ClientConnected`;
    /// guest: `is_connected()` edge).
    PeerConnected,
    /// Host accepted a well-formed `Hello` with the right version.
    HelloAccepted,
    /// Host rejected the `Hello` version and kicked the guest.
    HelloRejected,
    /// Guest's `Hello` was queued and has had a frame to flush.
    HelloSent,
    /// A match started (host: `start_net_match` in N4; guest: `MatchStart`
    /// received).
    MatchStart,
    /// The peer's connection dropped, carrying the mapped reason.
    PeerLost(NetLossReason),
    /// The [`JOIN_TIMEOUT`] watchdog fired.
    JoinTimeout,
    /// The peer sent [`NetMsg::Bye`].
    Bye,
}

/// The pure FSM transition function — the single source of truth for the
/// [`NetStatus`] transition table, testable without any transport (this is
/// what the N2 validation's "transition-table tests" run against).
///
/// `Some(next)` applies a legal transition, `None` ignores the trigger
/// (illegal from `current` for `role`, or a no-op).
#[must_use]
pub fn next_status(role: NetRole, current: &NetStatus, trigger: &NetTrigger) -> Option<NetStatus> {
    use NetLossReason as L;
    use NetRole as R;
    use NetStatus as S;
    use NetTrigger as T;

    let next = match (role, current.clone(), trigger.clone()) {
        // Universal teardown (every state but Idle — Idle matches none of
        // the arms above the final catch-all, so Stop there is a no-op).
        (
            _,
            S::Handshaking | S::Ready | S::InMatch | S::Connecting | S::Listening | S::Lost(_),
            T::Stop,
        ) => S::Idle,
        (_, S::BindFailed(_), T::Stop) => S::Idle,

        // Bind failure from Idle or a previous BindFailed (retry).
        (_, S::Idle | S::BindFailed(_), T::BindFailed(msg)) => S::BindFailed(msg),

        // Host lifecycle.
        (R::Host, S::Idle, T::BindOk) => S::Listening,
        (R::Host, S::Listening, T::PeerConnected) => S::Handshaking,
        (R::Host, S::Handshaking, T::HelloAccepted) => S::Ready,
        (R::Host, S::Handshaking, T::HelloRejected) => S::Listening,
        (R::Host, S::Ready, T::MatchStart) => S::InMatch,
        (R::Host, S::Handshaking | S::Ready | S::InMatch, T::PeerLost(reason)) => S::Lost(reason),
        (R::Host, S::Handshaking | S::Ready | S::InMatch, T::Bye) => S::Lost(L::PeerDisconnected),

        // Guest lifecycle.
        (R::Guest, S::Idle, T::Connect) => S::Connecting,
        (R::Guest, S::Connecting, T::PeerConnected) => S::Handshaking,
        (R::Guest, S::Handshaking, T::HelloSent) => S::Ready,
        // A MatchStart may race the Hello-flush frame; accept both.
        (R::Guest, S::Handshaking | S::Ready, T::MatchStart) => S::InMatch,
        (R::Guest, S::Connecting, T::JoinTimeout) => S::Lost(L::Timeout),
        (R::Guest, S::Connecting, T::PeerLost(reason)) => S::Lost(reason),
        (R::Guest, S::Handshaking | S::Ready | S::InMatch, T::PeerLost(reason)) => S::Lost(reason),
        (R::Guest, S::Handshaking | S::Ready | S::InMatch, T::Bye) => S::Lost(L::PeerDisconnected),

        // Everything else (role-mismatched triggers, PeerLost with no
        // peer, handshake triggers once InMatch/Lost, …) is ignored.
        _ => return None,
    };
    Some(next)
}

/// Shared netplay session state (Bevy `Resource`; all fields safe to read
/// from render/UI code). Lives in every app via `NetPlugin`; meaningful
/// only while `status != Idle`.
#[derive(Resource)]
pub struct NetSession {
    /// Which end of the connection this app plays.
    pub role: NetRole,
    /// Current FSM state — N5's status-line source of truth.
    pub status: NetStatus,
    /// Negotiated input delay `D` in ticks. Before the handshake this is
    /// the *local desire* (`TETRIS_NET_DELAY`); after it the adopted
    /// `max(local, peer)` (host) / `MatchStart.match_delay` (guest). N3's
    /// lockstep reads this to schedule `TickInput`/`TickBatch`.
    pub input_delay: u8,
    /// The address `net_host` actually bound (the advertised
    /// `public_addresses` entry) — N5 shows it as the connect hint.
    pub listen_addr: Option<SocketAddr>,
    /// Host: netcode id of the peer occupying the single slot.
    pub peer: Option<u64>,
    /// Guest: `Hello` was queued this frame and still needs one frame to
    /// flush before `Ready` (internal).
    hello_pending: bool,
    /// Watchdog base for `Connecting` (internal).
    joining_since: Option<Instant>,
    /// `RenetServerEvent`s queued by the observer for the host bridge
    /// system to process (internal — observers can't write `Messages`).
    server_notes: VecDeque<ServerNote>,
}

impl Default for NetSession {
    fn default() -> Self {
        Self {
            role: NetRole::default(),
            status: NetStatus::default(),
            input_delay: desired_local_delay(),
            listen_addr: None,
            peer: None,
            hello_pending: false,
            joining_since: None,
            server_notes: VecDeque::new(),
        }
    }
}

impl NetSession {
    /// Apply a trigger through the pure transition function; returns
    /// whether the status actually changed.
    fn apply(&mut self, trigger: NetTrigger) -> bool {
        if let Some(next) = next_status(self.role, &self.status, &trigger) {
            self.status = next;
            true
        } else {
            false
        }
    }

    /// Consumer hook for N4's `start_net_match`: flip the host from
    /// `Ready` to `InMatch` when it sends `MatchStart` (the guest makes the
    /// same transition when it *receives* the message). Returns whether the
    /// flip happened — callers must not start a mirror when it returns
    /// `false` (host not `Ready`).
    pub fn enter_match(&mut self) -> bool {
        self.apply(NetTrigger::MatchStart)
    }
}

/// `TETRIS_NET_DELAY` (u8 ticks) clamped to [`MIN_INPUT_DELAY`]..=[
/// `MAX_INPUT_DELAY`], falling back to [`DEFAULT_INPUT_DELAY`] on anything
/// unparseable. Same clamps N3 documents for its mirror of this env.
fn desired_local_delay() -> u8 {
    std::env::var(NET_DELAY_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u8>().ok())
        .unwrap_or(DEFAULT_INPUT_DELAY)
        .clamp(MIN_INPUT_DELAY, MAX_INPUT_DELAY)
}

fn unix_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
}

/// Map a lower netcode-protocol disconnect reason onto [`NetLossReason`].
fn map_netcode_reason(reason: NetcodeDisconnectReason) -> NetLossReason {
    match reason {
        NetcodeDisconnectReason::ConnectTokenExpired
        | NetcodeDisconnectReason::ConnectionTimedOut
        | NetcodeDisconnectReason::ConnectionResponseTimedOut
        | NetcodeDisconnectReason::ConnectionRequestTimedOut => NetLossReason::Timeout,
        NetcodeDisconnectReason::ConnectionDenied => NetLossReason::Denied,
        NetcodeDisconnectReason::DisconnectedByClient
        | NetcodeDisconnectReason::DisconnectedByServer => NetLossReason::PeerDisconnected,
    }
}

/// Map a renet reliable-layer disconnect reason onto [`NetLossReason`].
fn map_renet_reason(reason: RenetDisconnectReason) -> NetLossReason {
    match reason {
        RenetDisconnectReason::DisconnectedByClient
        | RenetDisconnectReason::DisconnectedByServer => NetLossReason::PeerDisconnected,
        _ => NetLossReason::Transport,
    }
}

// ---------------------------------------------------------------------------
// Free-function API (N5 exclusive systems, N6 harness): all take `&mut
// World` because starting/stopping a session inserts *and* removes
// resources. Each is idempotent and never panics.
// ---------------------------------------------------------------------------

/// Start hosting on `0.0.0.0:port` (netcode server, `max_clients: 1`,
/// unsecure auth over [`PROTOCOL_ID`], advertising the bound address).
/// A bind failure yields [`NetStatus::BindFailed`] +
/// [`NetEvent::BindFailed`] — never a panic. Any previous session is torn
/// down first, so re-hosting is safe.
pub fn net_host(world: &mut World, port: u16) {
    net_stop(world);
    match bind_host(port) {
        Ok((transport, addr)) => {
            {
                let mut session = world.resource_mut::<NetSession>();
                session.role = NetRole::Host;
                session.input_delay = desired_local_delay();
                session.apply(NetTrigger::BindOk);
                session.listen_addr = Some(addr);
            }
            world.insert_resource(transport);
            world.insert_resource(RenetServer::new(ConnectionConfig::default()));
            info!(
                "net: hosting on {addr} (protocol id {PROTOCOL_ID:#x}, version {PROTOCOL_VERSION})"
            );
        }
        Err(message) => {
            {
                let mut session = world.resource_mut::<NetSession>();
                session.role = NetRole::Host;
                session.apply(NetTrigger::BindFailed(message.clone()));
            }
            world.write_message(NetEvent::BindFailed(message.clone()));
            warn!("net: host bind failed on port {port}: {message}");
        }
    }
}

/// Bind the UDP socket *before* handing it to the transport — the transport
/// exposes no `local_addr()` (port-0 strategy from the Verified API notes).
fn bind_host(port: u16) -> Result<(NetcodeServerTransport, SocketAddr), String> {
    let socket = UdpSocket::bind(("0.0.0.0", port)).map_err(|e| e.to_string())?;
    let addr = socket.local_addr().map_err(|e| e.to_string())?;
    let config = ServerConfig {
        current_time: unix_now(),
        max_clients: 1,
        protocol_id: PROTOCOL_ID,
        public_addresses: vec![addr],
        authentication: ServerAuthentication::Unsecure,
    };
    let transport = NetcodeServerTransport::new(config, socket).map_err(|e| e.to_string())?;
    Ok((transport, addr))
}

/// Join the host at `addr` (netcode client, unsecure auth). Transitions to
/// `Connecting`; the [`JOIN_TIMEOUT`] watchdog, the netcode failure
/// reasons (incl. "match full" → `NetLossReason::Denied`) and the success
/// handshake all report through [`NetStatus`] + [`NetEvent`].
pub fn net_join(world: &mut World, addr: SocketAddr) {
    net_stop(world);
    match build_client(addr) {
        Ok((transport, client)) => {
            {
                let mut session = world.resource_mut::<NetSession>();
                session.role = NetRole::Guest;
                session.input_delay = desired_local_delay();
                session.apply(NetTrigger::Connect);
                session.joining_since = Some(Instant::now());
            }
            world.insert_resource(transport);
            world.insert_resource(client);
            info!("net: joining {addr} (protocol id {PROTOCOL_ID:#x}, version {PROTOCOL_VERSION})");
        }
        Err(message) => {
            {
                let mut session = world.resource_mut::<NetSession>();
                session.role = NetRole::Guest;
                session.apply(NetTrigger::BindFailed(message.clone()));
            }
            world.write_message(NetEvent::BindFailed(message.clone()));
            warn!("net: join failed for {addr}: {message}");
        }
    }
}

fn build_client(addr: SocketAddr) -> Result<(NetcodeClientTransport, RenetClient), String> {
    let current_time = unix_now();
    let client_id = u64::try_from(current_time.as_millis()).unwrap_or(0);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let transport = NetcodeClientTransport::new(
        current_time,
        ClientAuthentication::Unsecure {
            protocol_id: PROTOCOL_ID,
            client_id,
            server_addr: addr,
            user_data: None,
        },
        socket,
    )
    .map_err(|e| e.to_string())?;
    Ok((transport, RenetClient::new(ConnectionConfig::default())))
}

/// Tear the session down to `Idle`: politely notify the peer (netcode
/// disconnect packets go out *immediately*), then remove the transport
/// resources — which closes the sockets and frees the bound port
/// synchronously (Verified API notes: no `Drop` impls, transports own their
/// sockets). This is the "Esc-on-Listening" and universal teardown entry
/// point; safe to call in any state.
pub fn net_stop(world: &mut World) {
    if let (Some(mut transport), Some(mut server)) = (
        world.remove_resource::<NetcodeServerTransport>(),
        world.remove_resource::<RenetServer>(),
    ) {
        transport.disconnect_all(&mut server);
    }
    if let Some(mut transport) = world.remove_resource::<NetcodeClientTransport>() {
        transport.disconnect();
    }
    world.remove_resource::<RenetClient>();
    let mut session = world.resource_mut::<NetSession>();
    session.apply(NetTrigger::Stop);
    session.listen_addr = None;
    session.peer = None;
    session.hello_pending = false;
    session.joining_since = None;
    session.server_notes.clear();
}

// ---------------------------------------------------------------------------
// Plugin + bridging systems
// ---------------------------------------------------------------------------

/// Mounts the netplay stack into the app (called from
/// `CoreBridgePlugin::build()` exactly like `VersusBridgePlugin`): the four
/// bevy_renet plugins, [`NetSession`], `Messages<NetEvent>`, the
/// `RenetServerEvent` observer and the two bridging systems. Zero cost
/// while `Idle`: every renet system is `resource_exists`-gated and the two
/// bridge systems gate the same way, so with no transport resources
/// inserted there is nothing to run.
pub struct NetPlugin;

impl Plugin for NetPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            RenetServerPlugin,
            RenetClientPlugin,
            NetcodeServerPlugin,
            NetcodeClientPlugin,
        ))
        .init_resource::<NetSession>()
        .add_message::<NetEvent>()
        .add_observer(host_server_event_observer)
        .add_systems(
            Update,
            (
                host_net_system.run_if(resource_exists::<RenetServer>),
                guest_net_system.run_if(resource_exists::<RenetClient>),
            ),
        );
    }
}

/// Internal queue entry: the observer can only record (observers have no
/// `MessageWriter`), so `RenetServerEvent`s are buffered here and turned
/// into triggers by [`host_net_system`] in the same frame.
#[derive(Clone, Copy, Debug)]
enum ServerNote {
    Connected(u64),
    Disconnected(RenetDisconnectReason),
}

/// Buffer netcode connection events for the host bridge. Bevy 0.19
/// observer-trigger style: `bevy_renet` fires `RenetServerEvent`
/// (`On<RenetServerEvent>`) from `RenetServerPlugin::emit_server_events_system`
/// — there is no readable event queue (Verified API notes).
fn host_server_event_observer(event: On<RenetServerEvent>, mut session: ResMut<NetSession>) {
    if session.role != NetRole::Host {
        return;
    }
    let note = match **event {
        ServerEvent::ClientConnected { client_id } => ServerNote::Connected(client_id),
        ServerEvent::ClientDisconnected { reason, .. } => ServerNote::Disconnected(reason),
    };
    session.server_notes.push_back(note);
}

/// Host bridge (`Update`, runs after the renet/netcode `PreUpdate` receive
/// stages so connection events and messages land in one coherent frame).
/// The wire drain stops as soon as the status leaves `Handshaking` — from
/// the first match frame on, N3's lockstep owns the channels (module docs).
fn host_net_system(
    mut session: ResMut<NetSession>,
    mut server: ResMut<RenetServer>,
    mut messages: MessageWriter<NetEvent>,
    mut commands: Commands,
) {
    if session.role != NetRole::Host {
        return;
    }
    for note in std::mem::take(&mut session.server_notes) {
        match note {
            ServerNote::Connected(client_id) => {
                if session.apply(NetTrigger::PeerConnected) {
                    session.peer = Some(client_id);
                    info!("net: challenger {client_id} connected, awaiting Hello");
                }
            }
            ServerNote::Disconnected(reason) => {
                session.peer = None;
                let loss = map_renet_reason(reason);
                if session.apply(NetTrigger::PeerLost(loss)) {
                    messages.write(NetEvent::PeerLost(loss));
                    info!("net: peer lost ({loss:?})");
                    stop_host_resources(&mut commands);
                }
            }
        }
    }

    if session.status != NetStatus::Handshaking {
        return;
    }
    let Some(peer) = session.peer else { return };

    // ReliableOrdered: the app-level Hello (N3 owns this channel once the
    // handshake is done — see the boundary note in the module docs).
    while let Some(bytes) = server.receive_message(peer, DefaultChannel::ReliableOrdered) {
        match protocol::decode(&bytes) {
            Ok(NetMsg::Hello { version, delay }) => {
                if version == PROTOCOL_VERSION {
                    session.input_delay = session.input_delay.max(delay);
                    if session.apply(NetTrigger::HelloAccepted) {
                        messages.write(NetEvent::PeerConnected);
                        info!(
                            "net: peer ready (version {version}, delay {})",
                            session.input_delay
                        );
                    }
                } else {
                    warn!("net: kicking peer with version {version} (expected {PROTOCOL_VERSION})");
                    session.apply(NetTrigger::HelloRejected);
                    session.peer = None;
                    messages.write(NetEvent::VersionMismatch);
                    server.disconnect(peer);
                }
            }
            Ok(other) => debug!("net: ignoring {other:?} before handshake"),
            Err(e) => warn!("net: dropping undecodable payload: {e}"),
        }
    }

    if session.status == NetStatus::Handshaking {
        // ReliableUnordered: a clean exit inside the handshake window.
        while let Some(bytes) = server.receive_message(peer, DefaultChannel::ReliableUnordered) {
            match protocol::decode(&bytes) {
                Ok(NetMsg::Bye) => {
                    if session.apply(NetTrigger::Bye) {
                        session.peer = None;
                        messages.write(NetEvent::ByeReceived);
                        stop_host_resources(&mut commands);
                    }
                }
                Ok(other) => debug!("net: ignoring {other:?} on unordered channel"),
                Err(e) => warn!("net: dropping undecodable payload: {e}"),
            }
        }
    }
}

fn stop_host_resources(commands: &mut Commands) {
    commands.remove_resource::<NetcodeServerTransport>();
    commands.remove_resource::<RenetServer>();
}

fn stop_client_resources(commands: &mut Commands) {
    commands.remove_resource::<NetcodeClientTransport>();
    commands.remove_resource::<RenetClient>();
}

/// Guest bridge (`Update`): send `Hello` on connect, settle to `Ready`
/// once it has flushed, catch `MatchStart`/`Bye`, run the [`JOIN_TIMEOUT`]
/// watchdog and the disconnect mapping. Like the host system, it stops
/// draining the channels the moment `InMatch` is reached — N3 owns them.
fn guest_net_system(
    mut session: ResMut<NetSession>,
    mut client: ResMut<RenetClient>,
    transport: Option<Res<NetcodeClientTransport>>,
    mut messages: MessageWriter<NetEvent>,
    mut commands: Commands,
) {
    if session.role != NetRole::Guest {
        return;
    }

    // Connecting → connected: queue the app-level Hello (edge-detection on
    // is_connected; the built-in just_* predicates carry one-shot Locals).
    if session.status == NetStatus::Connecting
        && client.is_connected()
        && session.apply(NetTrigger::PeerConnected)
    {
        let hello = NetMsg::Hello {
            version: PROTOCOL_VERSION.to_string(),
            delay: session.input_delay,
        };
        client.send_message(DefaultChannel::ReliableOrdered, protocol::encode(&hello));
        session.hello_pending = true;
        info!("net: connected, sent Hello (delay {})", session.input_delay);
    }

    // Hello had its flush frame (PostUpdate RenetSend) → Ready.
    if session.status == NetStatus::Handshaking {
        if session.hello_pending {
            session.hello_pending = false;
        } else if session.apply(NetTrigger::HelloSent) {
            messages.write(NetEvent::PeerConnected);
            info!("net: hello sent, waiting for the host to start the match");
        }
    }

    // Wire drain until the match starts (N3 owns the channels in InMatch).
    if matches!(session.status, NetStatus::Handshaking | NetStatus::Ready) {
        while let Some(bytes) = client.receive_message(DefaultChannel::ReliableOrdered) {
            match protocol::decode(&bytes) {
                Ok(NetMsg::MatchStart {
                    seed,
                    rule,
                    match_delay,
                }) => {
                    if session.apply(NetTrigger::MatchStart) {
                        session.input_delay = match_delay;
                        info!("net: match start seed {seed} rule {rule:?} delay {match_delay}");
                    }
                }
                Ok(other) => debug!("net: ignoring {other:?} before match start"),
                Err(e) => warn!("net: dropping undecodable payload: {e}"),
            }
        }
    }
    if matches!(session.status, NetStatus::Handshaking | NetStatus::Ready) {
        while let Some(bytes) = client.receive_message(DefaultChannel::ReliableUnordered) {
            match protocol::decode(&bytes) {
                Ok(NetMsg::Bye) => {
                    if session.apply(NetTrigger::Bye) {
                        session.hello_pending = false;
                        messages.write(NetEvent::ByeReceived);
                        stop_client_resources(&mut commands);
                        return;
                    }
                }
                Ok(other) => debug!("net: ignoring {other:?} on unordered channel"),
                Err(e) => warn!("net: dropping undecodable payload: {e}"),
            }
        }
    }

    // JoinTimeout watchdog — fires before netcode's hard 15 s give-up.
    if session.status == NetStatus::Connecting
        && session
            .joining_since
            .is_some_and(|since| since.elapsed() >= JOIN_TIMEOUT)
        && session.apply(NetTrigger::JoinTimeout)
    {
        session.joining_since = None;
        messages.write(NetEvent::JoinTimeout);
        warn!("net: join timed out after {JOIN_TIMEOUT:?}");
        stop_client_resources(&mut commands);
        return;
    }

    // Connection loss (netcode denial, peer death, transport error). The
    // transport is dropped in the same frame it reports the loss: after a
    // disconnect its `update` errors *every* frame (Verified API notes).
    if session.status != NetStatus::Idle && client.is_disconnected() {
        let loss = transport
            .as_deref()
            .and_then(|t| t.disconnect_reason())
            .map(map_netcode_reason)
            .or_else(|| client.disconnect_reason().map(map_renet_reason))
            .unwrap_or(NetLossReason::PeerDisconnected);
        if session.apply(NetTrigger::PeerLost(loss)) {
            session.hello_pending = false;
            messages.write(NetEvent::PeerLost(loss));
            info!("net: connection lost ({loss:?})");
            stop_client_resources(&mut commands);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Pure FSM transition table (netplay-plan.md N2 validation) ----
    // RED→GREEN evidence: these run against next_status alone — no
    // transport, no App, no sockets.

    fn h(from: NetStatus, t: NetTrigger) -> Option<NetStatus> {
        next_status(NetRole::Host, &from, &t)
    }
    fn g(from: NetStatus, t: NetTrigger) -> Option<NetStatus> {
        next_status(NetRole::Guest, &from, &t)
    }

    #[test]
    fn host_handshake_path() {
        assert_eq!(
            h(NetStatus::Idle, NetTrigger::BindOk),
            Some(NetStatus::Listening)
        );
        assert_eq!(
            h(NetStatus::Listening, NetTrigger::PeerConnected),
            Some(NetStatus::Handshaking)
        );
        assert_eq!(
            h(NetStatus::Handshaking, NetTrigger::HelloAccepted),
            Some(NetStatus::Ready)
        );
        assert_eq!(
            h(NetStatus::Ready, NetTrigger::MatchStart),
            Some(NetStatus::InMatch)
        );
    }

    #[test]
    fn host_version_mismatch_returns_to_listening() {
        assert_eq!(
            h(NetStatus::Handshaking, NetTrigger::HelloRejected),
            Some(NetStatus::Listening)
        );
    }

    #[test]
    fn host_peer_loss_paths() {
        for from in [NetStatus::Handshaking, NetStatus::Ready, NetStatus::InMatch] {
            assert_eq!(
                h(
                    from.clone(),
                    NetTrigger::PeerLost(NetLossReason::PeerDisconnected)
                ),
                Some(NetStatus::Lost(NetLossReason::PeerDisconnected)),
                "PeerLost from {from:?}"
            );
        }
    }

    #[test]
    fn host_bye_sets_lost() {
        for from in [NetStatus::Handshaking, NetStatus::Ready, NetStatus::InMatch] {
            assert_eq!(
                h(from.clone(), NetTrigger::Bye),
                Some(NetStatus::Lost(NetLossReason::PeerDisconnected)),
                "Bye from {from:?}"
            );
        }
    }

    #[test]
    fn guest_full_path() {
        assert_eq!(
            g(NetStatus::Idle, NetTrigger::Connect),
            Some(NetStatus::Connecting)
        );
        assert_eq!(
            g(NetStatus::Connecting, NetTrigger::PeerConnected),
            Some(NetStatus::Handshaking)
        );
        assert_eq!(
            g(NetStatus::Handshaking, NetTrigger::HelloSent),
            Some(NetStatus::Ready)
        );
        assert_eq!(
            g(NetStatus::Ready, NetTrigger::MatchStart),
            Some(NetStatus::InMatch)
        );
    }

    #[test]
    fn guest_join_timeout_and_loss_paths() {
        assert_eq!(
            g(NetStatus::Connecting, NetTrigger::JoinTimeout),
            Some(NetStatus::Lost(NetLossReason::Timeout))
        );
        assert_eq!(
            g(
                NetStatus::Connecting,
                NetTrigger::PeerLost(NetLossReason::Denied)
            ),
            Some(NetStatus::Lost(NetLossReason::Denied))
        );
        for from in [
            NetStatus::Connecting,
            NetStatus::Handshaking,
            NetStatus::Ready,
            NetStatus::InMatch,
        ] {
            assert_eq!(
                g(from.clone(), NetTrigger::PeerLost(NetLossReason::Timeout)),
                Some(NetStatus::Lost(NetLossReason::Timeout)),
                "PeerLost from guest {from:?}"
            );
        }
    }

    #[test]
    fn guest_bye_sets_lost() {
        for from in [NetStatus::Handshaking, NetStatus::Ready, NetStatus::InMatch] {
            assert_eq!(
                g(from.clone(), NetTrigger::Bye),
                Some(NetStatus::Lost(NetLossReason::PeerDisconnected)),
                "Bye from guest {from:?}"
            );
        }
    }

    #[test]
    fn bind_failure_both_roles() {
        assert_eq!(
            h(NetStatus::Idle, NetTrigger::BindFailed("boom".into())),
            Some(NetStatus::BindFailed("boom".into()))
        );
        assert_eq!(
            g(NetStatus::Idle, NetTrigger::BindFailed("boom".into())),
            Some(NetStatus::BindFailed("boom".into()))
        );
        let failed = NetStatus::BindFailed("address already in use (os error 98)".into());
        assert_eq!(
            h(failed.clone(), NetTrigger::BindFailed("boom2".into())),
            Some(NetStatus::BindFailed("boom2".into()))
        );
        assert_eq!(
            g(failed.clone(), NetTrigger::BindFailed("boom2".into())),
            Some(NetStatus::BindFailed("boom2".into()))
        );
    }

    #[test]
    fn stop_from_every_state() {
        let states = [
            NetStatus::Idle,
            NetStatus::Listening,
            NetStatus::BindFailed("x".into()),
            NetStatus::Connecting,
            NetStatus::Handshaking,
            NetStatus::Ready,
            NetStatus::InMatch,
            NetStatus::Lost(NetLossReason::Timeout),
        ];
        for s in states {
            let stop = if s == NetStatus::Idle {
                None
            } else {
                Some(NetStatus::Idle)
            };
            assert_eq!(h(s.clone(), NetTrigger::Stop), stop, "host Stop from {s:?}");
            assert_eq!(
                g(s.clone(), NetTrigger::Stop),
                stop,
                "guest Stop from {s:?}"
            );
        }
    }

    #[test]
    fn illegal_triggers_ignored() {
        // Host never sends/awaits guest-side triggers …
        assert_eq!(h(NetStatus::Idle, NetTrigger::Connect), None);
        assert_eq!(h(NetStatus::Handshaking, NetTrigger::HelloSent), None);
        assert_eq!(h(NetStatus::Idle, NetTrigger::JoinTimeout), None);
        assert_eq!(h(NetStatus::Listening, NetTrigger::JoinTimeout), None);
        // … and the guest never sees host-side ones.
        assert_eq!(g(NetStatus::Handshaking, NetTrigger::HelloAccepted), None);
        assert_eq!(g(NetStatus::Handshaking, NetTrigger::HelloRejected), None);
        assert_eq!(g(NetStatus::Idle, NetTrigger::BindOk), None);
        // PeerLost with no peer to lose.
        assert_eq!(
            h(
                NetStatus::Listening,
                NetTrigger::PeerLost(NetLossReason::PeerDisconnected)
            ),
            None
        );
        assert_eq!(
            g(
                NetStatus::Idle,
                NetTrigger::PeerLost(NetLossReason::Timeout)
            ),
            None
        );
        // MatchStart needs a handshaking/Ready peer.
        assert_eq!(g(NetStatus::Connecting, NetTrigger::MatchStart), None);
        assert_eq!(h(NetStatus::Listening, NetTrigger::MatchStart), None);
        // Already in a match / lost: handshake triggers are inert.
        assert_eq!(h(NetStatus::InMatch, NetTrigger::PeerConnected), None);
        assert_eq!(
            h(
                NetStatus::Lost(NetLossReason::Timeout),
                NetTrigger::PeerConnected
            ),
            None
        );
        assert_eq!(g(NetStatus::InMatch, NetTrigger::HelloSent), None);
    }

    // ---- Wiring smoke (no sockets) ----

    #[test]
    fn net_stop_from_idle_is_noop() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        net_stop(app.world_mut());
        assert_eq!(app.world().resource::<NetSession>().status, NetStatus::Idle);
    }

    #[test]
    fn net_host_port_zero_listens_and_net_stop_frees_it() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        net_host(app.world_mut(), 0);
        {
            let session = app.world().resource::<NetSession>();
            assert_eq!(
                session.status,
                NetStatus::Listening,
                "port 0 bind must listen"
            );
            assert!(
                session.listen_addr.is_some(),
                "bound addr must be published"
            );
        }
        net_stop(app.world_mut());
        assert_eq!(app.world().resource::<NetSession>().status, NetStatus::Idle);
        assert!(!app.world().contains_resource::<RenetServer>());
        assert!(!app.world().contains_resource::<NetcodeServerTransport>());
    }

    #[test]
    fn net_host_on_occupied_port_reports_bind_failed_without_panic() {
        let _guard = TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Hold a port, then fail the same bind through net_host, then
        // recover on a free one.
        let keeper = UdpSocket::bind("127.0.0.1:0").expect("free port bind");
        let taken = keeper.local_addr().expect("local addr").port();
        let mut app = net_app();

        net_host(app.world_mut(), taken);
        {
            let session = app.world().resource::<NetSession>();
            assert!(
                matches!(session.status, NetStatus::BindFailed(_)),
                "occupied port must yield BindFailed, got {:?}",
                session.status
            );
        }
        let events = drain_events(&mut app);
        assert!(
            matches!(events.first(), Some(NetEvent::BindFailed(_))),
            "BindFailed message expected: {events:?}"
        );
        // Recovery through the same entry point.
        net_host(app.world_mut(), 0);
        assert_eq!(session_status(&app), NetStatus::Listening);
    }

    #[test]
    fn version_mismatch_kicks_through_the_host_system() {
        // Single app, renet's local-client seam (no UDP): a connection
        // that sends a wrong-version Hello must produce VersionMismatch,
        // kick the peer, and leave the host Listening.
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        net_host(app.world_mut(), 0);

        let mut local = app
            .world_mut()
            .resource_mut::<RenetServer>()
            .new_local_client(9);
        local.send_message(
            DefaultChannel::ReliableOrdered,
            protocol::encode(&NetMsg::Hello {
                version: "bogus-version".to_string(),
                delay: 8,
            }),
        );
        {
            let mut server = app.world_mut().resource_mut::<RenetServer>();
            server.process_local_client(9, &mut local).unwrap();
        }

        let mut saw_mismatch = false;
        for _ in 0..20 {
            {
                let mut server = app.world_mut().resource_mut::<RenetServer>();
                let _ = server.process_local_client(9, &mut local);
            }
            app.update();
            let events = drain_events(&mut app);
            if events.contains(&NetEvent::VersionMismatch) {
                saw_mismatch = true;
                break;
            }
        }
        assert!(
            saw_mismatch,
            "host must emit VersionMismatch for a bad Hello"
        );
        assert_eq!(
            app.world().resource::<NetSession>().status,
            NetStatus::Listening,
            "kicked host keeps listening"
        );
    }

    // ---- In-process two-`App` loopback integration (real netcode UDP) ----
    //
    // Not RED-before-implementation: transport wiring cannot fail before it
    // exists — the RED→GREEN evidence for this task is the pure FSM table
    // tests above. This fixture asserts the end-to-end contract:
    // net_host + net_join → connect → both peers Ready (host on valid
    // Hello) → clean guest exit → host sees the loss.
    //
    // Fixed port from `TETRIS_TEST_NET_PORT` (deterministic default
    // [`TEST_PORT_DEFAULT`]). Collision caveat: a *foreign* process holding
    // the port fails the fixture, so it is serialized with every other
    // socket test in the binary through the shared [`TEST_NET_LOCK`]
    // (N6's harness must take the same lock).

    use std::sync::Mutex;

    pub(crate) static TEST_NET_LOCK: Mutex<()> = Mutex::new(());

    /// Default loopback port for the socket tests (`TETRIS_TEST_NET_PORT`
    /// overrides). Collision caveat: pick one unlikely to clash with local
    /// services if this default does.
    pub const TEST_PORT_DEFAULT: u16 = 34_857;

    fn test_net_port() -> u16 {
        std::env::var("TETRIS_TEST_NET_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(TEST_PORT_DEFAULT)
    }

    /// Reserve an OS-assigned free port by briefly binding port 0.
    fn bind_any_port() -> SocketAddr {
        UdpSocket::bind("127.0.0.1:0")
            .expect("free port bind")
            .local_addr()
            .expect("local addr")
    }

    fn net_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        app
    }

    fn drain_events(app: &mut App) -> Vec<NetEvent> {
        app.world_mut()
            .resource_mut::<Messages<NetEvent>>()
            .drain()
            .collect()
    }

    fn session_status(app: &App) -> NetStatus {
        app.world().resource::<NetSession>().status.clone()
    }

    /// Drive both apps in lockstep until `done` reports success, or fail
    /// the assertion after `frames` iterations (~5 ms apart).
    fn drive_until(
        host: &mut App,
        guest: &mut App,
        frames: usize,
        done: impl Fn(&App, &App) -> bool,
    ) {
        for _ in 0..frames {
            host.update();
            guest.update();
            if done(host, guest) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "loopback netplay condition not reached in {frames} frames (host {:?}, guest {:?})",
            session_status(host),
            session_status(guest)
        );
    }

    #[test]
    fn loopback_connect_handshake_then_clean_bye() {
        let _guard = TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let port = test_net_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let mut host = net_app();
        let mut guest = net_app();
        net_host(host.world_mut(), port);
        assert_eq!(session_status(&host), NetStatus::Listening);
        net_join(guest.world_mut(), addr);
        assert_eq!(session_status(&guest), NetStatus::Connecting);

        // Both sides land in Ready: host on the valid Hello, guest once
        // its Hello has flushed.
        drive_until(&mut host, &mut guest, 1_000, |h, g| {
            session_status(h) == NetStatus::Ready && session_status(g) == NetStatus::Ready
        });
        let host_events = drain_events(&mut host);
        let guest_events = drain_events(&mut guest);
        assert!(
            host_events.contains(&NetEvent::PeerConnected),
            "host: {host_events:?}"
        );
        assert!(
            guest_events.contains(&NetEvent::PeerConnected),
            "guest: {guest_events:?}"
        );

        // Host-side mirror of N4's start hook.
        assert!(host.world_mut().resource_mut::<NetSession>().enter_match());
        assert_eq!(session_status(&host), NetStatus::InMatch);

        // Clean guest exit: host sends Bye, guest tears down on receipt.
        {
            let peer = host.world().resource::<NetSession>().peer.expect("peer id");
            host.world_mut().resource_mut::<RenetServer>().send_message(
                peer,
                DefaultChannel::ReliableUnordered,
                protocol::encode(&NetMsg::Bye),
            );
        }
        drive_until(&mut host, &mut guest, 1_000, |_, g| {
            matches!(session_status(g), NetStatus::Lost(_))
        });
        let guest_events = drain_events(&mut guest);
        assert!(
            guest_events.contains(&NetEvent::ByeReceived),
            "guest: {guest_events:?}"
        );
        assert!(!guest.world().contains_resource::<RenetClient>());
        assert!(!guest.world().contains_resource::<NetcodeClientTransport>());
        // The host initiated the Bye, so it stays InMatch (N3/N5 own that
        // teardown direction).
        assert_eq!(session_status(&host), NetStatus::InMatch);
        net_stop(host.world_mut());
    }

    #[test]
    fn loopback_peer_exit_surfaces_lost_on_host() {
        let _guard = TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let port = test_net_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        let mut host = net_app();
        let mut guest = net_app();
        net_host(host.world_mut(), port);
        net_join(guest.world_mut(), addr);
        drive_until(&mut host, &mut guest, 1_000, |h, g| {
            session_status(h) == NetStatus::Ready && session_status(g) == NetStatus::Ready
        });
        drain_events(&mut host);
        drain_events(&mut guest);

        // Graceful exit: net_stop sends the netcode disconnect packet
        // before dropping the client resources, so the host sees the loss
        // within a frame or two. (A raw `drop(guest)` would send nothing —
        // the survivor then waits out netcode's hard 15 s timeout, which
        // no CI test should do; see module docs.)
        net_stop(guest.world_mut());
        let deadline = Instant::now() + Duration::from_secs(5);
        let loss = loop {
            host.update();
            if let NetStatus::Lost(reason) = session_status(&host) {
                break reason;
            }
            assert!(Instant::now() < deadline, "host never observed the drop");
            std::thread::sleep(Duration::from_millis(10));
        };
        // Netcode flattens every server-side loss (clean disconnect packet
        // *and* timeout) to renet `Transport` — remove_connection reads the
        // renet connection's own reason, which the transport never set
        // (renet-2.0.0/src/server.rs:129-134) — so `Transport` is the
        // honest host-side loss reason for "the peer vanished".
        assert_eq!(loss, NetLossReason::Transport);
        let host_events = drain_events(&mut host);
        assert!(
            host_events.contains(&NetEvent::PeerLost(NetLossReason::Transport)),
            "host: {host_events:?}"
        );
        assert!(!host.world().contains_resource::<RenetServer>());
    }

    #[test]
    fn guest_watchdog_fires_on_dead_host() {
        let _guard = TEST_NET_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut app = net_app();
        // Nothing listens on this port; the guest must give up on the
        // app-side watchdog (~10 s) long before netcode's 15 s timeout.
        let addr: SocketAddr = format!("127.0.0.1:{}", bind_any_port().port())
            .parse()
            .unwrap();
        net_join(app.world_mut(), addr);
        assert_eq!(session_status(&app), NetStatus::Connecting);
        // Fast-forward the watchdog instead of sleeping 10 real seconds.
        app.world_mut().resource_mut::<NetSession>().joining_since =
            Some(Instant::now() - JOIN_TIMEOUT);
        app.update();
        let events = drain_events(&mut app);
        assert!(
            events.contains(&NetEvent::JoinTimeout),
            "events: {events:?}"
        );
        assert_eq!(
            session_status(&app),
            NetStatus::Lost(NetLossReason::Timeout)
        );
        assert!(!app.world().contains_resource::<RenetClient>());
        assert!(!app.world().contains_resource::<NetcodeClientTransport>());
    }
}
