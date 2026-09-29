//! Netplay: online 1v1 versus over deterministic lockstep (netplay-plan.md).
//!
//! Wave-1 surface only (N1): the wire-protocol codec ([`protocol`]) plus the
//! pinned API knowledge for everything N2 (session) and N3 (lockstep) code
//! against. `NetPlugin` will be mounted from `CoreBridgePlugin::build()` in
//! N2, mirroring how `VersusBridgePlugin` is mounted today.
//!
//! # Verified API notes (spike, 2026-09-28/29)
//!
//! Every item below was read off the exact downloaded crate sources and is
//! cited `file:line` against `~/.cargo/registry/src/index.crates.io-*/`.
//! Resolved versions (per `Cargo.lock`): **bevy_renet 5.0.0, renet 2.0.0,
//! renet_netcode 2.0.0, renetcode 2.0.0 (the lower netcode-protocol crate
//! renet_netcode wraps), bincode 1.3.3, bevy_ecs/bevy_app 0.19.1**. The
//! plan's version assumptions hold; `bevy_renet` resolves exactly as planned.
//!
//! ## Crate layering
//!
//! `bevy_renet` (Bevy glue) → `renet` (reliable channels, `RenetClient`/
//! `RenetServer`) → `renet_netcode` (UDP transport) → `renetcode` (netcode
//! protocol: tokens, auth, crypto). `bevy_renet::netcode` re-exports
//! `renet_netcode::*`, which re-exports `renetcode::{ClientAuthentication,
//! ConnectToken, ServerAuthentication, ServerConfig, NetcodeError,
//! DisconnectReason as NetcodeDisconnectReason, generate_random_bytes,
//! NETCODE_KEY_BYTES /*32*/, NETCODE_USER_DATA_BYTES /*1024*/}`
//! (`renet_netcode-2.0.0/src/lib.rs:10-13`). So `bevy_renet::netcode::*` is
//! the one import path N2 needs for everything netcode.
//!
//! ## Message payload encoding — raw bytes only (no Serialize integration)
//!
//! renet 2.0 has **no** serde integration: payloads are byte buffers.
//! - Server: `RenetServer::send_message<I: Into<u8>, B: Into<Bytes>>(&mut
//!   self, client_id: ClientId, channel_id: I, message: B)`
//!   (`renet-2.0.0/src/server.rs:191`); `broadcast_message` (`:151`) and
//!   `broadcast_message_except` (`:160`) fan out.
//! - Client: `RenetClient::send_message<I: Into<u8>, B: Into<Bytes>>(&mut
//!   self, channel_id: I, message: B)`
//!   (`renet-2.0.0/src/remote_connection.rs:319`).
//! - Receive is a **poll-until-empty call** on each side:
//!   `fn receive_message<I: Into<u8>>(&mut self, channel_id: I) ->
//!   Option<Bytes>` (client: `remote_connection.rs:337`; per-client server:
//!   `server.rs:199`). Pattern: `while let Some(b) =
//!   client.receive_message(ch) { … }`.
//! - `Bytes` is `renet::Bytes` (re-export of the `bytes` crate). `Vec<u8>`
//!   converts via `Into<Bytes>`, so `protocol::encode(&msg)` output goes
//!   straight in; `&bytes[..]` feeds `protocol::decode`.
//! - bevy_renet's own example confirms the idiom: `bincode::serialize(&msg)`
//!   → `send_message` on `DefaultChannel::ReliableOrdered`
//!   (`bevy_renet-5.0.0/examples/simple.rs:157,180,206`).
//! - `ClientId = u64` (`renet-2.0.0/src/lib.rs:13`).
//!
//! ## ChannelConfig / SendType shapes
//!
//! `renet::ChannelConfig { channel_id: u8, max_memory_usage_bytes: usize,
//! send_type: SendType }` (`renet-2.0.0/src/channel/mod.rs:22-34`).
//! `SendType` (`channel/mod.rs:11-20`):
//! - `Unreliable`
//! - `ReliableOrdered { resend_time: Duration }`
//! - `ReliableUnordered { resend_time: Duration }`
//!
//! **Plan correction**: the plan's note said `resend_time` is a
//! `ReliableUnordered`-only knob — verified wrong: **both** reliable variants
//! take `resend_time`. `max_memory_usage_bytes` is the backpressure limit:
//! when exceeded, unreliable channels drop new messages, **reliable channels
//! disconnect the connection** (`channel/mod.rs:26-28`) — N3's per-tick
//! batches are tiny (~tens of bytes) but the queue must not be left to grow.
//!
//! Default channels (`DefaultChannel`, `channel/mod.rs:38-74`): id
//! 0 = `Unreliable`, 1 = `ReliableUnordered`, 2 = `ReliableOrdered`, 5 MiB
//! memory each, `resend_time` 300 ms. `ConnectionConfig::default()`
//! (`remote_connection.rs:101-109`) = `available_bytes_per_tick: 60_000` +
//! these three configs for both directions — the plan's table (Reliable-
//! Ordered for Hello/MatchStart/TickInput/TickBatch, ReliableUnordered for
//! SnapshotHash/Bye) maps to `DefaultChannel::ReliableOrdered` /
//! `DefaultChannel::ReliableUnordered` directly (`I: Into<u8>` accepts
//! `DefaultChannel`, `channel/mod.rs:44-50`). No custom channel config is
//! needed for v1.
//!
//! ## bevy_renet resources, plugins, sets, events
//!
//! Resources (both `#[derive(Resource)]` newtypes with `Deref`/`DerefMut` to
//! the inner renet type, `bevy_renet-5.0.0/src/lib.rs:113-127`):
//! - `bevy_renet::RenetServer(pub renet::RenetServer)` — `RenetServer::new(
//!   ConnectionConfig)`;
//! - `bevy_renet::RenetClient(pub renet::RenetClient)` — `RenetClient::new(
//!   ConnectionConfig)`.
//!
//! Insert with `commands.insert_resource(...)` (examples use
//! `app.insert_resource`). **All renet/netcode systems are
//! `run_if(resource_exists::<...>)`** (`lib.rs:39-45,57-60`;
//! `netcode.rs:27-33,43-49,93-103`): inserting the resources starts the
//! machinery, removing them stops it — that is the `net_stop()` teardown
//! primitive (see teardown below).
//!
//! System sets (`lib.rs:24,33`): `RenetReceive` (in `PreUpdate`, transports
//! receive) and `RenetSend` (in `PostUpdate`, transports send). Gameplay
//! systems can live in `Update`: it runs after `PreUpdate` (data already
//! received) and before `PostUpdate` (sends flushed this frame) — one
//! drain-then-send system in `Update` per side is sufficient, like
//! `versus_bridge_system` is today.
//!
//! Plugins:
//! - `RenetServerPlugin` (`lib.rs:35-60`): `PreUpdate` `update_system`
//!   (`server.update(delta)`, gated on `RenetServer` res) then
//!   `emit_server_events_system` in `RenetReceive` — pumps
//!   `server.get_event()` and fires each as
//!   `commands.trigger(RenetServerEvent(event))`.
//! - `RenetClientPlugin` (`lib.rs:62-70`): `PreUpdate`
//!   `client.update(time.delta())`, gated on `RenetClient` res. **No client
//!   event pump exists** — client connection state is polled with the
//!   built-in predicates `client_connected/_disconnected/_connecting/
//!   client_just_connected/client_just_disconnected` (`lib.rs:75-111`;
//!   `just_*` use a `Local<bool>` — they are `run_if` conditions, each usable
//!   exactly once per app per schedule).
//! - **`NetcodeServerPlugin` + `NetcodeClientPlugin` are required in
//!   addition** (plan gap — the plan only names the two renet plugins):
//!   they drive `transport.update()` in `PreUpdate`/`RenetReceive` and,
//!   critically, **`send_packets` in `PostUpdate`/`RenetSend`**
//!   (`bevy_renet-5.0.0/src/netcode.rs:25-61,90-105`). Without the netcode
//!   plugin no UDP bytes ever leave the process. They also auto-
//!   `disconnect_all` / `disconnect` on `AppExit` (see teardown).
//! - `NetcodeServerTransport::update` is ordered `.after(RenetServerPlugin::
//!   update_system).before(emit_server_events_system)` (`netcode.rs:29-32`),
//!   so connection events and messages arrive in the same consistent frame.
//!
//! Events — **Bevy 0.19 observer-style triggers, not `Messages<T>`**:
//! `bevy_renet::RenetServerEvent(pub renet::ServerEvent)` and
//! `bevy_renet::netcode::NetcodeErrorEvent(pub NetcodeTransportError)`
//! derive `bevy_ecs::event::Event` (`lib.rs:129-135`, `netcode.rs:133-140`).
//! `commands.trigger(...)` dispatches to observers; consume them with
//! `app.add_observer(|ev: On<RenetServerEvent>| …)` (verified idiom:
//! `examples/simple.rs:126,133,291`; `On<E>` system param at
//! `bevy_ecs-0.19.1/src/observer/system_param.rs:38`). There is **no
//! `EventReader`** and no buffered queue to drain — if an observer misses a
//! trigger it is gone, so N2's observer must record into state/`Messages`
//! synchronously (the repo's `Messages<NetEvent>` convention —
//! `core_bridge/versus.rs:240` `MessageWriter<VersusEvent>` — still exists
//! in 0.19: `bevy_ecs-0.19.1/src/message/messages.rs:95`,
//! `bevy_app-0.19.1/src/app.rs:427` `add_message`).
//! `renet::ServerEvent` (`renet-2.0.0/src/server.rs:12-15`):
//! `ClientConnected { client_id }`, `ClientDisconnected { client_id, reason:
//! renet::DisconnectReason }` — the latter derives `PartialEq, Eq`.
//! `renet::DisconnectReason` (`renet-2.0.0/src/error.rs:7-28`): `Transport`,
//! `DisconnectedByClient`, `DisconnectedByServer`,
//! `PacketSerialization(..)`, `PacketDeserialization(..)`,
//! `ReceivedInvalidChannelId(u8)`, `SendChannelError{..}`,
//! `ReceiveChannelError{..}`.
//!
//! ## netcode ServerConfig / ClientAuthentication fields
//!
//! `renetcode::ServerConfig` (`renetcode-2.0.0/src/server.rs:101-113`) —
//! **no `Default`; all five fields required**:
//! - `current_time: Duration` (wall clock, e.g.
//!   `SystemTime::now().duration_since(UNIX_EPOCH)`; drives timeout math —
//!   the plan snippet omitted this field),
//! - `max_clients: usize` (`panic!` if `> 1024`, `server.rs:117-121`),
//! - `protocol_id: u64`,
//! - `public_addresses: Vec<SocketAddr>` (advertised to clients; see port-0
//!   note),
//! - `authentication: ServerAuthentication` = `Secure { private_key:
//!   [u8; 32] }` | `Unsecure` (`server.rs:89-99`).
//!
//! `renetcode::ClientAuthentication` (`renetcode-2.0.0/src/client.rs:31-44`):
//! - `Secure { connect_token: ConnectToken }`,
//! - `Unsecure { protocol_id: u64, client_id: u64, server_addr: SocketAddr,
//!   user_data: Option<[u8; 1024]> }` (v1 choice, matches plan; `client_id`
//!   can be a wall-clock millis cast like the upstream example,
//!   `examples/simple.rs:51-56`).
//!
//! Transport constructors (bevy wrappers:
//! `bevy_renet::netcode::{NetcodeServerTransport, NetcodeClientTransport}`):
//! - `NetcodeServerTransport::new(ServerConfig, UdpSocket) ->
//!   Result<Self, std::io::Error>` — **takes a caller-bound socket** and sets
//!   it non-blocking internally (`renet_netcode-2.0.0/src/server.rs:22-32`).
//!   A bind failure (port in use) is the `io::Error` → N2's `BindFailed(msg)`.
//! - `NetcodeClientTransport::new(current_time: Duration,
//!   ClientAuthentication, UdpSocket) -> Result<Self, NetcodeError>` —
//!   binds nothing, caller passes `UdpSocket::bind("0.0.0.0:0")`
//!   (`renet_netcode-2.0.0/src/client.rs:21-30`, example
//!   `examples/simple.rs:48-59`).
//!
//! ## Port-0 queryability
//!
//! **Not queryable after construction on the server side.**
//! `NetcodeServerTransport` has **no `local_addr()`** (only
//! `addresses()`, `renet_netcode-2.0.0/src/server.rs:35-37` — which returns
//! `ServerConfig.public_addresses` verbatim,
//! `renetcode-2.0.0/src/server.rs:164-166`), and `socket` is a private
//! field. **Strategy for `net_host(0)`/test binds**: bind the
//! `std::net::UdpSocket` yourself, read `socket.local_addr()?` **before**
//! moving it into `NetcodeServerTransport::new`, then set
//! `public_addresses = vec![advertised_addr]` (the bound addr for tests; the
//! LAN/public addr for real hosts — port-forwarding means the advertised
//! addr and the bind addr can differ, so always set both consciously).
//! Client side **is** queryable: `NetcodeClientTransport::addr() ->
//! io::Result<SocketAddr>` (`renet_netcode-2.0.0/src/client.rs:32-34`).
//! For the in-crate two-`App` tests N2 should still prefer the plan's fixed
//! `TETRIS_TEST_NET_PORT` (deterministic default) — the port-0 dance adds
//! nondeterminism to advertised addresses and the collision caveat applies
//! either way.
//!
//! ## Transport drop / teardown semantics (un-listening)
//!
//! Verified: **no `Drop` impl anywhere** in renet 2.0.0, renet_netcode
//! 2.0.0, renetcode 2.0.0, bevy_renet 5.0.0 (grep `impl Drop` across all
//! four trees: zero hits). The transports *own* their `UdpSocket`
//! (`renet_netcode-2.0.0/src/server.rs:16`, `client.rs:15`), so
//! **removing/dropping the `NetcodeServerTransport` resource closes the
//! socket and frees the port synchronously** (the resource is not `NonSend`,
//! and ECS resource drop happens when `remove_resource` applies / world
//! drops). `net_stop()` = remove the `NetcodeServerTransport` (and
//! `RenetServer`) resources (+ likewise on the client side). UDP has no
//! TIME_WAIT, so a rebinding test can immediately reuse the port.
//! To notify the peer politely use `NetcodeServerTransport::disconnect_all`
//! — sends disconnect packets *immediately*
//! (`renet_netcode-2.0.0/src/server.rs:71-76`; contrast
//! `RenetServer::disconnect_all`, which only marks connections and waits
//! for the update loop, `renet-2.0.0/src/server.rs:144-148`). Both netcode
//! plugins already do the graceful variant automatically on `AppExit`
//! (system in `Last` reading `MessageReader<AppExit>`,
//! `bevy_renet-5.0.0/src/netcode.rs:53-61,124-129`).
//! Client-side half: `NetcodeClientTransport::disconnect()` sends the
//! disconnect packet instantly (`renet_netcode-2.0.0/src/client.rs:49-62`).
//!
//! ## Connect timeout configurability
//!
//! **Not exposed through bevy_renet/netcode 2.0 public API** (plan assumed
//! it might be). `renet::ConnectionConfig` (`remote_connection.rs:15-31`)
//! has only `available_bytes_per_tick` + the two channel-config vecs — no
//! timeout. Timeout lives per *connection token*: `connect_token
//! .timeout_seconds: i32` is enforced client-side
//! (`renetcode-2.0.0/src/client.rs:285-286`, request/response timeout
//! branches `:288-321`) and server-side per client
//! (`renetcode-2.0.0/src/server.rs:606-607`). With
//! `ClientAuthentication::Unsecure` the token is **generated internally
//! with hard-coded `expire_seconds = 300, timeout_seconds = 15`**
//! (`renetcode-2.0.0/src/client.rs:94-105`): an unreachable host yields
//! `DisconnectReason::ConnectionRequestTimedOut` after ~15 s of silence
//! (`renetcode-2.0.0/src/client.rs:13-16`). Consequences for N2:
//! - The plan's ~10 s `JoinTimeout` watchdog must be **app-side** (it fires
//!   before the transport's own 15 s give-up — good ordering: the UI never
//!   waits for netcode).
//! - Custom timeouts are possible without a token server: build a
//!   `ConnectToken::generate(current_time, PROTOCOL_ID, expire_seconds,
//!   client_id, timeout_seconds, vec![server_addr], None, &[0u8; 32])`
//!   yourself and pass `ClientAuthentication::Secure { connect_token }` —
//!   the zero key works against a `ServerAuthentication::Unsecure` host the
//!   same way `Unsecure` does (only `Unsecure` client auth exists against
//!   `Unsecure` server auth otherwise; `Secure`-with-zero-key is unverified
//!   beyond reading the code — do not rely on it unless the N2 loopback test
//!   exercises it).
//!
//! ## Failure modes N2 must map (verified behaviors, corrects plan risk note)
//!
//! - **Second guest is NOT silently dropped**: at `max_clients: 1` a full
//!   server sends a **`ConnectionDenied` packet** to the second requester
//!   (`renetcode-2.0.0/src/server.rs:303-316`; duplicate-address deny
//!   `:270-280`). The guest's client goes
//!   `Disconnected(ConnectionDenied)` — `NetcodeClientTransport::
//!   disconnect_reason()` returns `Some(NetcodeDisconnectReason::
//!   ConnectionDenied)` (`renet_netcode-2.0.0/src/client.rs:65-67`, enum at
//!   `renetcode-2.0.0/src/client.rs:13-20`). The *host* still sees no event
//!   (plan's host-side claim stands), but the **guest can distinguish
//!   "match full" (`ConnectionDenied`) from "host offline" (
//!   `ConnectionRequestTimedOut`)** — N2 may use this instead of the plan's
//!   merged "offline or full" message.
//! - Wrong-`protocol_id` peers: packets fail netcode decode and are dropped
//!   silently with a log line (`renetcode-2.0.0/src/client.rs:207-211`) →
//!   15 s timeout, no explicit event (matches plan).
//! - Once the guest client is disconnected, `NetcodeClientTransport::update`
//!   returns `Err(NetcodeTransportError::Netcode(Disconnected(reason)))`
//!   every subsequent frame and marks the renet client via
//!   `disconnect_due_to_transport()` (`renet_netcode-2.0.0/src/client.rs:
//!   86-92`) → bevy_renet fires `NetcodeErrorEvent` **every frame while the
//!   resources remain** (`netcode.rs:111-119`). N2 must tear down resources
//!   promptly on first disconnect, and its `NetcodeErrorEvent` observer must
//!   not react to the repeat spam.
//! - Guest-side peer loss mid-match: also arrives as `renet::
//!   DisconnectReason::Transport` on renet `RenetClient::disconnect_reason()`
//!   + `client_just_disconnected()`.
//!
//! ## Test-fixture notes (for N2's two-`App` loopback test)
//!
//! `MinimalPlugins` **includes `TimePlugin`**
//! (`bevy_internal-0.19.1/src/default_plugins.rs:160-167`) — the renet
//! plugins' systems require `Res<Time<Real>>` (`bevy_renet-5.0.0/src/lib.rs:
//! 54,72`), which that provides, so the plan's "add TimePlugin if
//! MinimalPlugins insufficient" fallback is already satisfied: drive the
//! apps with manual `app.update()` calls in the test loop (the repo's
//! headless pattern) — no `ScheduleRunner` execution needed. Time only
//! advances by however much real time passes between updates; keep the test
//! loop `std::thread::sleep`-y enough for netcode's connection dance to
//! progress, and never advance `Time<Real>` artificially (it is wall-clock).
//!
//! ## Wire-protocol codec (implemented in [`protocol`])
//!
//! `PROTOCOL_ID`, `PROTOCOL_VERSION`, `NetMsg` (all six variants incl.
//! `MatchStart.match_delay` for the negotiated D), `encode`/`decode`
//! (bincode 1.3, **strict**: fixed-integer encoding — identical bytes to the
//! `bincode::serialize` top-level defaults — with *trailing bytes rejected*,
//! unknown variants and short buffers yield `Err(ProtocolError)`, never a
//! panic), and `snapshot_hash` = FNV-1a-64 over the bincode bytes of
//! `MatchSnapshot` (process-stable, unlike `std::hash`).

// Consumed progressively: N2 (session/plugin), N3 (lockstep), N6 (harness).
pub mod harness;
pub mod lockstep;
pub mod online_ui;
#[allow(dead_code)]
pub mod protocol;
pub mod session;

pub use session::*;
