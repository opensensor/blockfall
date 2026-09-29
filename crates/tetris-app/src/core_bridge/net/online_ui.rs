//! Online menu flow (netplay-plan.md N5): the `OnlineFlow` stage machine over
//! the Title screen, the keyboard-only IP:port entry widget, per-`NetStatus`
//! status lines, error/leave overlays, and the mid-match exit — the UI half
//! of netplay, mounted from `MenuScreensPlugin::build()`.
//!
//! # Shape of the flow
//!
//! Mirrors the `VersusFlow` stage machine of `screens_menu.rs`: no new
//! [`AppState`] variant — the online panels render over the Title screen and
//! gate entirely on [`OnlineFlow`] + [`NetSession`] state, so solo and local
//! versus flows are untouched (`OnlineStage::Closed` is the default and the
//! recorded T26 stage tests keep passing byte-identical).
//!
//! ```text
//! Title ──"1 v 1"──▶ (existing local submenu, byte-identical)
//!       └─"Online"──▶ Mode ──▶ Host ──(challenger Ready → pick rule)──▶ MatchStart
//!                        └──▶ Join ──(ip:port + Enter)──▶ connecting → … → InMatch
//! ```
//!
//! The plan's "1 v 1 gains a Local/Online axis" is realized as a sibling
//! Title entry ("Online", [`OnlineButton`]) rather than a mode step *inside*
//! the local flow, because the recorded T26 stage tests
//! (`one_v_one_garbage_human_flow_starts_a_match` …) require "1 v 1" to open
//! the rules step directly and must pass unmodified.
//!
//! # Clipboard paste finding (plan task item)
//!
//! **Bevy 0.19 removed the OS clipboard entirely.** `bevy_window` 0.19.1 and
//! `bevy_winit` 0.19.1 expose no `Clipboard` type, no `Window::clipboard()`
//! and no paste event (verified against the exact registry sources: zero
//! matches for `clipboard`); only per-key `KeyboardInput` and optional IME
//! events remain. The entry widget is therefore **keyboard-only** (no
//! dependency added); Ctrl+V is a deliberate no-op there.
//!
//! # Teardown contract (N3) respected here
//!
//! * `NetEvent::Desync`/`ByeReceived` arrive with `SimPaused` already set by
//!   the lockstep and `VersusMatch::active` still `true` — the error overlay
//!   shows over the frozen boards and its "Back to title" runs
//!   [`net_leave_to_title`]: graceful `net_stop`, match deactivated,
//!   un-pause, `AppState::Title`, `NetSession → Idle`.
//! * Both overlay roots carry the explicit [`NET_OVERLAY_ZINDEX`] — strictly
//!   above the `ZIndex(0)` title/winner/HUD roots and the `ZIndex(1)` submenu
//!   roots (recorded click-swallow regressions `screens_menu.rs:960`,
//!   `:2055`), so overlay buttons pick first while the versus HUD stays
//!   visible underneath.
//! * `pause_chord_system` (screens_menu) is gated OFF while
//!   `NetSession::status == InMatch` — lockstep has no authoritative pause;
//!   Escape there toggles the "Leave match?" confirm instead, and Leave sends
//!   an app-level `Bye` (so the peer shows "opponent left") plus the full
//!   teardown.
//!
//! # Frame ordering
//!
//! The systems self-chain input/clicks → session watch → visibility. The
//! Title screen is hidden by `screens_menu`'s own visibility sync, which
//! reads [`OnlineFlow`] in the same frame but may run *before* the click in
//! the parallel schedule — one frame of latency on Title-hide, invisible in
//! practice (tests take an extra settle frame where relevant).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use bevy::prelude::*;
use bevy_renet::{RenetClient, RenetServer};

use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

use super::lockstep::{
    local_side, net_leave_to_title, NetOut, RenetClientOut, RenetServerOut, NET_OVERLAY_ZINDEX,
};
use super::protocol::NetMsg;
use super::session::{
    net_host, net_join, net_stop, NetEvent, NetLossReason, NetRole, NetSession, NetStatus,
};
use super::upnp::{mapping_held, start_mapping, teardown_mapping, UpnpPlugin, UpnpState};
use crate::core_bridge::{start_net_match, VersusMatch};
use crate::screens_menu::{VersusFlow, VersusMenuButton, VersusRematchButton, VersusStage};
use crate::settings_persist::NetProfile;
use crate::state::{AppState, RebindingCapture};

/// Port `net_host` binds when the player presses Host (netcode example
/// range; documented for players in N8).
pub const DEFAULT_HOST_PORT: u16 = 27_015;

/// Hard cap for the join-address entry (an IPv4 `a.b.c.d:ppppp` needs 21;
/// bracketed IPv6 forms are out of the charset, so this is generous).
pub const MAX_JOIN_ADDR_LEN: usize = 45;

/// Fallback connect-hint suffix when the "connect a UDP socket to a public
/// address and read `local_addr()`" trick finds no route (no default
/// gateway / offline machine).
pub const NO_ROUTE_HINT: &str = "no network route — use your public IP";

/// Opaque backdrop for the online panels (same values as the menu roots —
/// the `screens_menu` consts are private).
const PANEL_BG: Color = Color::srgb(0.09, 0.09, 0.12);
/// Dimming backdrop for the net overlays.
const DIM_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.62);
const BUTTON_BG: Color = Color::srgb(0.22, 0.22, 0.27);

// ---------------------------------------------------------------------------
// Pure entry logic (keyboard-only — see the clipboard note in the module docs)
// ---------------------------------------------------------------------------

/// Charset of the join entry: digits, dots and colons (plan N5).
#[must_use]
pub fn entry_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.' || c == ':'
}

/// Append `c` to `text` when it is in the charset and the entry has room.
/// Returns whether anything changed.
pub fn entry_push(text: &mut String, c: char) -> bool {
    if !entry_char(c) || text.len() >= MAX_JOIN_ADDR_LEN {
        return false;
    }
    text.push(c);
    true
}

/// Delete the last character; returns whether anything changed.
pub fn entry_backspace(text: &mut String) -> bool {
    text.pop().is_some()
}

/// Map a freshly pressed key to its entry character. `Shift` matters only
/// for the `;` → `:` colon; the rest of the charset is shift-invariant on
/// the keys we accept.
#[must_use]
pub fn key_to_entry_char(key: KeyCode, shift: bool) -> Option<char> {
    let base = match key {
        KeyCode::Digit0 | KeyCode::Numpad0 => '0',
        KeyCode::Digit1 | KeyCode::Numpad1 => '1',
        KeyCode::Digit2 | KeyCode::Numpad2 => '2',
        KeyCode::Digit3 | KeyCode::Numpad3 => '3',
        KeyCode::Digit4 | KeyCode::Numpad4 => '4',
        KeyCode::Digit5 | KeyCode::Numpad5 => '5',
        KeyCode::Digit6 | KeyCode::Numpad6 => '6',
        KeyCode::Digit7 | KeyCode::Numpad7 => '7',
        KeyCode::Digit8 | KeyCode::Numpad8 => '8',
        KeyCode::Digit9 | KeyCode::Numpad9 => '9',
        KeyCode::Period => '.',
        KeyCode::Semicolon if shift => ':',
        _ => return None,
    };
    Some(base)
}

/// Parse a submitted entry into a socket address: trimmed `IPv4:port` only
/// (brackets/percent zones are out of the charset). Rejects missing ports,
/// port `0`, bad octets and out-of-range ports.
#[must_use]
pub fn parse_join_addr(text: &str) -> Option<SocketAddr> {
    let trimmed = text.trim();
    let (ip, port) = trimmed.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let ip: Ipv4Addr = ip.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}

// ---------------------------------------------------------------------------
// Pure status/overlay copy
// ---------------------------------------------------------------------------

/// Overlay/loss copy for a [`NetLossReason`] (shared by the status line and
/// the `PeerLost` overlay; netcode flattens host-side losses, so `Transport`
/// keeps the generic wording).
#[must_use]
pub fn net_loss_text(reason: NetLossReason) -> String {
    match reason {
        // "match full" is indistinguishable from "host offline" for the
        // join-timeout path (plan risk note) — Denied shares the wording.
        NetLossReason::Timeout | NetLossReason::Denied => {
            "Connection lost — host offline or match full".to_string()
        }
        NetLossReason::PeerDisconnected => "opponent left".to_string(),
        NetLossReason::Transport => "Connection lost".to_string(),
    }
}

/// Status line per [`NetStatus`], role-aware where the meanings differ
/// (host `Ready` prompts the rule pick, guest `Ready` is the
/// "waiting-for-host" gap line the plan calls out).
#[must_use]
pub fn status_text(role: NetRole, status: &NetStatus) -> String {
    match status {
        NetStatus::Idle => String::new(),
        NetStatus::Listening => "listening — waiting for a challenger…".to_string(),
        NetStatus::BindFailed(_) => "bind failed — port in use".to_string(),
        NetStatus::Connecting => "connecting…".to_string(),
        NetStatus::Handshaking => "handshaking…".to_string(),
        NetStatus::Ready => match role {
            NetRole::Host => "challenger connected — pick a rule".to_string(),
            NetRole::Guest => "connected — waiting for the host to start the match".to_string(),
        },
        NetStatus::InMatch => "in match".to_string(),
        NetStatus::Lost(reason) => net_loss_text(*reason),
    }
}

/// Overlay headline for a [`NetEvent`]; `None` for events that need no
/// overlay (`PeerConnected` rides the status line instead). Every variant
/// has an explicit path.
#[must_use]
pub fn net_event_text(event: &NetEvent) -> Option<String> {
    match event {
        NetEvent::PeerConnected => None,
        NetEvent::BindFailed(_) => Some("Cannot listen — port in use".to_string()),
        NetEvent::VersionMismatch => {
            Some("Version mismatch — both players need the same Blockfall build".to_string())
        }
        NetEvent::JoinTimeout => Some("Connection lost — host offline or match full".to_string()),
        NetEvent::PeerLost(reason) => Some(net_loss_text(*reason)),
        NetEvent::Desync { tick } => Some(format!("desync at tick {tick} — match aborted")),
        NetEvent::ByeReceived => Some("opponent left".to_string()),
    }
}

/// Role-aware winner copy for net matches (refines the N4 `winner_text`
/// `Net` arm): the local seat winning says "YOU WIN", the far seat
/// "OPPONENT WINS".
#[must_use]
pub fn net_winner_text(winner: Side, role: NetRole) -> String {
    if winner == local_side(role) {
        "YOU WIN".to_string()
    } else {
        "OPPONENT WINS".to_string()
    }
}

/// Best-effort local IPv4 via the connect-to-public `UdpSocket` trick (a UDP
/// `connect` associates the socket with the route *without sending a
/// packet*). `None` when no route exists — callers show [`NO_ROUTE_HINT`].
#[must_use]
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(("8.8.8.8", 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    }
}

/// `"192.168.1.5:27015"` / fallback string for the Host screen hint.
#[must_use]
pub fn format_host_hint(ip: Option<Ipv4Addr>, port: u16) -> String {
    match ip {
        Some(ip) => format!("{ip}:{port}"),
        None => format!("0.0.0.0:{port} — {NO_ROUTE_HINT}"),
    }
}

// ---------------------------------------------------------------------------
// UPnP status line (WAN play addendum — see net/upnp.rs)
// ---------------------------------------------------------------------------

/// One-line copy for the Host screen's UPnP status label. The exact
/// shipped strings:
/// * attempt running → `Opening router port…`
/// * mapped → `Friends join at <ext_ip>:<port>`
/// * failed / no router / no route → `UPnP unavailable — forward UDP
///   <port> manually (see README)` (never blocks play — LAN still works)
/// * disabled via the `U` toggle (only while hosting) → `Router mapping off
///   — press U to enable`
#[must_use]
pub fn upnp_status_text(state: &UpnpState, enabled: bool, hosting: bool, port: u16) -> String {
    if !enabled {
        return if hosting {
            "Router mapping off — press U to enable".to_string()
        } else {
            String::new()
        };
    }
    match state {
        UpnpState::Off => String::new(),
        UpnpState::Mapping { .. } => "Opening router port…".to_string(),
        UpnpState::Mapped { external_ip, port } => {
            format!("Friends join at {external_ip}:{port}")
        }
        UpnpState::Failed(_) => {
            format!("UPnP unavailable — forward UDP {port} manually (see README)")
        }
    }
}

// ---------------------------------------------------------------------------
// Online flow stage machine — the Online sibling of `VersusFlow`
// ---------------------------------------------------------------------------

/// Which online panel (if any) overlays the Title screen. `Closed` (the
/// default) leaves the plain title in charge — solo and local versus flows
/// are untouched while `Closed`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnlineStage {
    /// Online UI not open.
    #[default]
    Closed,
    /// Host or Join choice.
    Mode,
    /// Hosting: status, port + connect hint, rule pick once `Ready`.
    Host,
    /// Joining: address entry + submit.
    Join,
}

/// Online menu flow state (Bevy resource; the `VersusFlow` analogue).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub struct OnlineFlow {
    /// Current panel.
    pub stage: OnlineStage,
    /// Rule picked on the Host screen (reset when the flow closes).
    pub rule: AttackRule,
}

impl OnlineFlow {
    /// `true` while an online panel overlays the title.
    #[must_use]
    pub fn open(&self) -> bool {
        self.stage != OnlineStage::Closed
    }
}

/// Where Escape/Back takes the flow from `stage`.
#[must_use]
pub fn online_flow_back(stage: OnlineStage) -> OnlineStage {
    match stage {
        OnlineStage::Mode | OnlineStage::Host | OnlineStage::Join => OnlineStage::Closed,
        OnlineStage::Closed => OnlineStage::Closed,
    }
}

/// Whether leaving `stage` toward the title must `net_stop()` the session
/// first (Esc on Listening un-listens and frees the port — plan N5).
#[must_use]
pub fn online_stage_requires_stop(stage: OnlineStage) -> bool {
    matches!(stage, OnlineStage::Host | OnlineStage::Join)
}

// ---------------------------------------------------------------------------
// Live UI state + markers
// ---------------------------------------------------------------------------

/// Title-screen "Online" entry (lives here so the click handler that opens
/// [`OnlineStage::Mode`] sits with the rest of the flow).
#[derive(Component)]
pub struct OnlineButton;

/// The join-address entry widget state (keyboard-only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub struct JoinEntry {
    /// Current text.
    pub text: String,
    /// Last submit was rejected (label hint).
    pub invalid: bool,
    /// Enter was pressed this frame; the exclusive click system performs the
    /// actual `net_join` (a normal system cannot take `&mut World`).
    submit: bool,
}

impl JoinEntry {
    /// Flag a submit request (called by the entry input system).
    pub fn request_submit(&mut self) {
        self.submit = true;
    }

    /// Take a pending submit request.
    pub fn take_submit(&mut self) -> bool {
        std::mem::take(&mut self.submit)
    }
}

/// Cached connect hint shown on the Host screen (computed when entering the
/// Host stage, so [`local_ipv4`] runs once per hosting attempt, not per
/// frame).
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub struct HostHint {
    /// "a.b.c.d:port" for the challenger to type, or the fallback string.
    pub share: String,
}

/// The current error overlay ("Back to title"): `Some` shows
/// [`NetErrorRoot`] with this headline.
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub struct NetOverlay {
    /// Overlay headline; `None` hides the root.
    pub text: Option<String>,
}

/// Whether the mid-match "Leave match?" confirm overlay is open (Esc toggle
/// while `InMatch`; the pause chord is gated off there).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub struct LeaveConfirm {
    /// Open flag.
    pub open: bool,
}

/// One-frame edge-detector for the exclusive click system: button entities
/// that were `Interaction::Pressed` last frame (world queries inside
/// exclusive systems carry no change ticks of their own).
#[derive(Resource, Default)]
struct PressedLatch {
    pressed: Vec<Entity>,
}

/// One dispatched button press: which markers the pressed entity carries.
#[derive(Clone, Copy)]
struct ClickFlags {
    entity: Entity,
    online: bool,
    host: bool,
    join: bool,
    back: bool,
    garbage: bool,
    race: bool,
    submit: bool,
    error_back: bool,
    leave_yes: bool,
    leave_no: bool,
    rematch: bool,
    menu: bool,
}

/// Root of the Online Mode panel (Host / Join choice).
#[derive(Component)]
pub struct OnlineModeRoot;
/// Root of the Host panel (status, hint, rule pick).
#[derive(Component)]
pub struct OnlineHostRoot;
/// Root of the Join panel (address entry).
#[derive(Component)]
pub struct OnlineJoinRoot;
/// Root of the net error overlay (explicit [`NET_OVERLAY_ZINDEX`]).
#[derive(Component)]
pub struct NetErrorRoot;
/// Root of the mid-match leave-confirm overlay (explicit [`NET_OVERLAY_ZINDEX`]).
#[derive(Component)]
pub struct LeaveConfirmRoot;

/// "Host" button on the Mode panel.
#[derive(Component)]
pub struct OnlineHostButton;
/// "Join" button on the Mode panel.
#[derive(Component)]
pub struct OnlineJoinButton;
/// "Back" button on every online panel (walks the flow; `net_stop`s on
/// Host/Join).
#[derive(Component)]
pub struct OnlineBackButton;
/// Host rule pick "Garbage" (acts only on `Ready`).
#[derive(Component)]
pub struct OnlineRuleGarbageButton;
/// Host rule pick "Race" (acts only on `Ready`).
#[derive(Component)]
pub struct OnlineRuleRaceButton;
/// Join panel submit button (Enter also submits).
#[derive(Component)]
pub struct OnlineSubmitButton;
/// Error overlay "Back to title" button → [`net_leave_to_title`].
#[derive(Component)]
pub struct NetErrorBackButton;
/// Leave-confirm "Leave" button (Bye + teardown).
#[derive(Component)]
pub struct LeaveYesButton;
/// Leave-confirm "Stay" button.
#[derive(Component)]
pub struct LeaveNoButton;

/// Dynamic connect-hint label on the Host panel.
#[derive(Component)]
pub struct HostHintText;
/// Dynamic status label on the Host panel.
#[derive(Component)]
pub struct HostStatusText;
/// Dynamic UPnP/router-mapping status line on the Host panel (WAN play
/// addendum, net/upnp.rs).
#[derive(Component)]
pub struct UpnpStatusText;
/// Dynamic entry echo on the Join panel.
#[derive(Component)]
pub struct JoinEntryText;
/// Dynamic status label on the Join panel.
#[derive(Component)]
pub struct JoinStatusText;
/// Dynamic headline of the error overlay.
#[derive(Component)]
pub struct NetErrorText;

/// Registration guard: `OnlineUiPlugin` mounts at most once even when added
/// from `MenuScreensPlugin` *and* a test app.
#[derive(Resource)]
struct OnlineUiMounted;

// ---------------------------------------------------------------------------
// Shared actions (called from the exclusive systems)
// ---------------------------------------------------------------------------

/// Best-effort app-level [`NetMsg::Bye`] to the peer *before* tearing down,
/// so the survivor shows "opponent left" instead of waiting for a transport
/// loss.
fn send_bye(world: &mut World) {
    let (role, peer) = {
        let session = world.resource::<NetSession>();
        (session.role, session.peer)
    };
    if role == NetRole::Host {
        let Some(server) = world.get_resource_mut::<RenetServer>() else {
            return;
        };
        if let Some(peer) = peer {
            let mut out = RenetServerOut {
                server: server.into_inner(),
                peer,
            };
            out.send(&NetMsg::Bye);
        }
    } else if let Some(client) = world.get_resource_mut::<RenetClient>() {
        let mut out = RenetClientOut {
            client: client.into_inner(),
        };
        out.send(&NetMsg::Bye);
    }
}

/// Submit the Join entry: valid → remember the address in the [`NetProfile`]
/// and `net_join`; invalid → flag the label.
fn join_submit(world: &mut World) {
    let text = world.resource::<JoinEntry>().text.clone();
    let Some(addr) = parse_join_addr(&text) else {
        world.resource_mut::<JoinEntry>().invalid = true;
        return;
    };
    world.resource_mut::<JoinEntry>().invalid = false;
    {
        let mut profile = world.get_resource_or_insert_with(NetProfile::default);
        profile.last_join_addr = addr.to_string();
    }
    net_join(world, addr);
}

/// Host rule pick on `Ready` (the right side is forced `Controller::Net` by
/// [`start_net_match`]); a no-op while no challenger is ready yet.
fn host_start(world: &mut World, rule: AttackRule) {
    {
        let session = world.resource::<NetSession>();
        if session.status != NetStatus::Ready {
            return;
        }
    }
    let delay = world.resource::<NetSession>().input_delay;
    world.resource_mut::<OnlineFlow>().rule = rule;
    start_net_match(world, rule, Side::Left, wall_clock_seed(), delay);
}

/// Host "Rematch": re-arms its mirror and resends a fresh `MatchStart`
/// through [`start_net_match`] — *never* `start_versus`, whose local reseed
/// would fork the guest mirror (plan rematch rule). Guests never get here
/// (their button is hidden and this early-returns).
fn host_rematch(world: &mut World) {
    let role = world.resource::<NetSession>().role;
    if role != NetRole::Host {
        return;
    }
    let Some(rule) = world.get_non_send::<VersusMatch>().map(|v| v.rule) else {
        return;
    };
    let delay = world.resource::<NetSession>().input_delay;
    start_net_match(world, rule, Side::Left, wall_clock_seed(), delay);
}

fn wall_clock_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .max(1)
}

/// Full error dismissal: hide the overlay and leave everything behind
/// ([`net_leave_to_title`] — graceful `net_stop`, match teardown, un-pause,
/// Title screen; idempotent, so it is also right for pre-match errors).
fn error_back_to_title(world: &mut World) {
    world.resource_mut::<NetOverlay>().text = None;
    net_leave_to_title(world);
    world.resource_mut::<OnlineFlow>().stage = OnlineStage::Closed;
    world.resource_mut::<LeaveConfirm>().open = false;
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

/// Escape: toggles the mid-match leave-confirm while `InMatch` (the pause
/// chord is gated off there in `screens_menu`), and walks the OnlineFlow
/// back one stage otherwise (Host/Join additionally `net_stop()` — Esc on
/// Listening un-listens and frees the port).
pub fn online_esc_system(world: &mut World) {
    if world
        .get_resource::<RebindingCapture>()
        .is_some_and(|capture| capture.capturing)
    {
        return;
    }
    let escaped = world
        .get_resource::<ButtonInput<KeyCode>>()
        .is_some_and(|keys| keys.just_pressed(KeyCode::Escape));
    if !escaped {
        return;
    }
    let in_match = world
        .get_resource::<NetSession>()
        .is_some_and(|s| s.status == NetStatus::InMatch);
    if in_match {
        if world.resource::<NetOverlay>().text.is_some() {
            return;
        }
        let mut confirm = world.resource_mut::<LeaveConfirm>();
        confirm.open = !confirm.open;
        return;
    }
    let (stage, on_title) = (
        world.resource::<OnlineFlow>().stage,
        *world.resource::<AppState>() == AppState::Title,
    );
    if stage != OnlineStage::Closed && on_title {
        world.resource_mut::<OnlineFlow>().stage = online_flow_back(stage);
        if online_stage_requires_stop(stage) {
            net_stop(world);
        }
    }
}

/// Character input for the Join entry: charset keys append, Backspace edits,
/// Enter flags a submit (the exclusive click system performs `net_join`).
pub fn online_entry_input_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    capture: Res<RebindingCapture>,
    state: Res<AppState>,
    flow: Res<OnlineFlow>,
    mut entry: ResMut<JoinEntry>,
) {
    if capture.capturing
        || flow.stage != OnlineStage::Join
        || *state != AppState::Title
        || entry.submit
    {
        return;
    }
    let Some(keys) = keys else { return };
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if keys.just_pressed(KeyCode::Backspace) || keys.just_pressed(KeyCode::Delete) {
        if entry_backspace(&mut entry.text) {
            entry.invalid = false;
        }
        return;
    }
    for key in [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
        KeyCode::Numpad0,
        KeyCode::Numpad1,
        KeyCode::Numpad2,
        KeyCode::Numpad3,
        KeyCode::Numpad4,
        KeyCode::Numpad5,
        KeyCode::Numpad6,
        KeyCode::Numpad7,
        KeyCode::Numpad8,
        KeyCode::Numpad9,
        KeyCode::Period,
        KeyCode::Semicolon,
    ] {
        if keys.just_pressed(key) {
            if let Some(c) = key_to_entry_char(key, shift) {
                if entry_push(&mut entry.text, c) {
                    entry.invalid = false;
                }
            }
        }
    }
    if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) {
        entry.request_submit();
    }
}

/// `U` on the Host screen toggles the router port mapping (WAN play
/// addendum): off → best-effort `DeletePortMapping` + [`UpnpState::Off`];
/// on → retry `AddPortMapping` immediately when still `Listening`. The
/// choice persists in [`NetProfile::upnp_enabled`].
pub fn upnp_toggle_system(world: &mut World) {
    if world
        .get_resource::<RebindingCapture>()
        .is_some_and(|capture| capture.capturing)
    {
        return;
    }
    let on_host_screen = world
        .get_resource::<AppState>()
        .is_some_and(|state| *state == AppState::Title)
        && world
            .get_resource::<OnlineFlow>()
            .is_some_and(|flow| flow.stage == OnlineStage::Host);
    if !on_host_screen {
        return;
    }
    let pressed = world
        .get_resource::<ButtonInput<KeyCode>>()
        .is_some_and(|keys| keys.just_pressed(KeyCode::KeyU));
    if !pressed {
        return;
    }
    let enabled = {
        let mut profile = world.get_resource_or_insert_with(NetProfile::default);
        profile.upnp_enabled = !profile.upnp_enabled;
        profile.upnp_enabled
    };
    if enabled {
        start_mapping(world);
    } else {
        teardown_mapping(world);
    }
}

/// Button handling for every online root, the Title "Online" entry, the net
/// branches of the winner overlay (host rematch / menu) and the Enter-submit
/// flag. Exclusive: the handlers call the `&mut World` lifecycle functions
/// (`net_host`/`net_join`/`net_stop`/`start_net_match`/`net_leave_to_title`).
pub fn online_click_system(world: &mut World) {
    // Presses that just *appeared* this frame (edge-detected against the
    // latch — exclusive world queries have no `Changed` ticks).
    let mut fresh: Vec<ClickFlags> = Vec::new();
    {
        let mut query = world.query::<(
            Entity,
            &Interaction,
            Has<OnlineButton>,
            Has<OnlineHostButton>,
            Has<OnlineJoinButton>,
            Has<OnlineBackButton>,
            Has<OnlineRuleGarbageButton>,
            Has<OnlineRuleRaceButton>,
            Has<OnlineSubmitButton>,
            Has<NetErrorBackButton>,
            Has<LeaveYesButton>,
            Has<LeaveNoButton>,
            Has<VersusRematchButton>,
            Has<VersusMenuButton>,
        )>();
        let mut currently_pressed = Vec::new();
        for (
            entity,
            interaction,
            online,
            host,
            join,
            back,
            garbage,
            race,
            submit,
            error_back,
            yes,
            no,
            rematch,
            menu,
        ) in query.iter(world)
        {
            if *interaction != Interaction::Pressed {
                continue;
            }
            currently_pressed.push(entity);
            let touches_flow = online
                || host
                || join
                || back
                || garbage
                || race
                || submit
                || error_back
                || yes
                || no
                || rematch
                || menu;
            if touches_flow {
                fresh.push(ClickFlags {
                    entity,
                    online,
                    host,
                    join,
                    back,
                    garbage,
                    race,
                    submit,
                    error_back,
                    leave_yes: yes,
                    leave_no: no,
                    rematch,
                    menu,
                });
            }
        }
        let mut latch = world.resource_mut::<PressedLatch>();
        fresh.retain(|flags| !latch.pressed.contains(&flags.entity));
        latch.pressed = currently_pressed;
    }

    // Enter-key submit rides the same frame as the clicks.
    if world.resource_mut::<JoinEntry>().take_submit() {
        join_submit(world);
    }

    if fresh.is_empty() {
        return;
    }

    let on_title = *world.resource::<AppState>() == AppState::Title;
    let versus_free = world.resource::<VersusFlow>().stage == VersusStage::Title;
    let in_match = world.resource::<NetSession>().status == NetStatus::InMatch;

    for flags in fresh {
        let _ = flags.entity;
        if flags.online {
            if on_title && versus_free {
                world.resource_mut::<OnlineFlow>().stage = OnlineStage::Mode;
            }
        } else if flags.host {
            world.resource_mut::<OnlineFlow>().stage = OnlineStage::Host;
            net_host(world, DEFAULT_HOST_PORT);
            let port = world
                .resource::<NetSession>()
                .listen_addr
                .map_or(DEFAULT_HOST_PORT, |addr| addr.port());
            world.resource_mut::<HostHint>().share = format_host_hint(local_ipv4(), port);
        } else if flags.join {
            world.resource_mut::<OnlineFlow>().stage = OnlineStage::Join;
            let prefill = world
                .get_resource::<NetProfile>()
                .map(|profile| profile.last_join_addr.clone())
                .unwrap_or_default();
            let mut entry = world.resource_mut::<JoinEntry>();
            if !prefill.is_empty() {
                entry.text = prefill;
            }
            entry.invalid = false;
            entry.submit = false;
        } else if flags.back {
            let stage = world.resource::<OnlineFlow>().stage;
            world.resource_mut::<OnlineFlow>().stage = online_flow_back(stage);
            if online_stage_requires_stop(stage) {
                net_stop(world);
            }
        } else if flags.garbage {
            host_start(world, AttackRule::Garbage);
        } else if flags.race {
            host_start(
                world,
                AttackRule::Race {
                    target_lines: DEFAULT_RACE_LINES,
                },
            );
        } else if flags.submit {
            join_submit(world);
        } else if flags.error_back {
            error_back_to_title(world);
        } else if flags.leave_yes {
            send_bye(world);
            net_leave_to_title(world);
            world.resource_mut::<LeaveConfirm>().open = false;
        } else if flags.leave_no {
            world.resource_mut::<LeaveConfirm>().open = false;
        } else if in_match && flags.rematch {
            host_rematch(world);
        } else if in_match && flags.menu {
            net_leave_to_title(world);
            world.resource_mut::<NetOverlay>().text = None;
            world.resource_mut::<LeaveConfirm>().open = false;
        }
    }
}

/// Session watcher: turns [`NetEvent`] messages into overlay headlines and,
/// when the session enters [`NetStatus::InMatch`] (host rule pick or guest
/// `MatchStart`), closes the online panels and takes the game screen.
pub fn online_session_watch_system(
    session: Option<Res<NetSession>>,
    mut events: MessageReader<NetEvent>,
    mut overlay: ResMut<NetOverlay>,
    mut flow: ResMut<OnlineFlow>,
    mut state: ResMut<AppState>,
    mut confirm: ResMut<LeaveConfirm>,
    mut last: Local<Option<NetStatus>>,
) {
    for event in events.read() {
        if let Some(text) = net_event_text(event) {
            overlay.text = Some(text);
        }
    }
    let Some(session) = session else { return };
    let status = session.status.clone();
    let just_live = status == NetStatus::InMatch && !matches!(&*last, Some(NetStatus::InMatch));
    if just_live {
        flow.stage = OnlineStage::Closed;
        flow.rule = AttackRule::default();
        confirm.open = false;
        if *state == AppState::Title {
            *state = AppState::Playing;
        }
    }
    *last = Some(status);
}

/// Rewrite the dynamic online labels.
#[allow(clippy::type_complexity)]
pub fn sync_online_labels(
    session: Option<Res<NetSession>>,
    hint: Res<HostHint>,
    entry: Res<JoinEntry>,
    overlay: Res<NetOverlay>,
    upnp: Option<Res<UpnpState>>,
    profile: Option<Res<NetProfile>>,
    mut labels: Query<
        (
            Has<HostHintText>,
            Has<HostStatusText>,
            Has<UpnpStatusText>,
            Has<JoinEntryText>,
            Has<JoinStatusText>,
            Has<NetErrorText>,
            &mut Text,
        ),
        Or<(
            With<HostHintText>,
            With<HostStatusText>,
            With<UpnpStatusText>,
            With<JoinEntryText>,
            With<JoinStatusText>,
            With<NetErrorText>,
        )>,
    >,
) {
    let status = session
        .as_deref()
        .map(|s| s.status.clone())
        .unwrap_or(NetStatus::Idle);
    let share = hint.share.clone();
    let hint_line = if share.is_empty() {
        String::new()
    } else {
        format!("share this address: {share}")
    };
    let host_line = status_text(NetRole::Host, &status);
    let upnp_line = upnp.as_deref().map_or_else(String::new, |state| {
        let enabled = profile
            .as_deref()
            .is_none_or(|profile| profile.upnp_enabled);
        let port = session
            .as_deref()
            .and_then(|s| s.listen_addr)
            .map_or(DEFAULT_HOST_PORT, |addr| addr.port());
        let hosting = session
            .as_deref()
            .is_some_and(|s| mapping_held(&s.status, s.role));
        upnp_status_text(state, enabled, hosting, port)
    });
    let echo = if entry.invalid {
        format!("{}▌  want IPv4 address:port", entry.text)
    } else {
        format!("{}▌", entry.text)
    };
    let join_line = status_text(NetRole::Guest, &status);
    let headline = overlay.text.clone().unwrap_or_default();
    for (is_hint, is_host, is_upnp, is_join_entry, is_join_status, is_error, mut text) in
        labels.iter_mut()
    {
        let wanted = if is_hint {
            &hint_line
        } else if is_host {
            &host_line
        } else if is_upnp {
            &upnp_line
        } else if is_join_entry {
            &echo
        } else if is_join_status {
            &join_line
        } else if is_error {
            &headline
        } else {
            continue;
        };
        if text.0 != *wanted {
            *text = Text::new(wanted);
        }
    }
}

/// Root visibility for the online panels and overlays. The two overlay roots
/// sit at [`NET_OVERLAY_ZINDEX`], above the (still visible) versus HUD and
/// winner overlay, so their buttons keep picking while the match is frozen.
#[allow(clippy::type_complexity)]
pub fn sync_online_visibility(
    state: Res<AppState>,
    flow: Res<OnlineFlow>,
    overlay: Res<NetOverlay>,
    confirm: Res<LeaveConfirm>,
    session: Option<Res<NetSession>>,
    mut roots: Query<
        (
            &mut Visibility,
            Has<OnlineModeRoot>,
            Has<OnlineHostRoot>,
            Has<OnlineJoinRoot>,
            Has<NetErrorRoot>,
            Has<LeaveConfirmRoot>,
        ),
        Or<(
            With<OnlineModeRoot>,
            With<OnlineHostRoot>,
            With<OnlineJoinRoot>,
            With<NetErrorRoot>,
            With<LeaveConfirmRoot>,
        )>,
    >,
) {
    let in_match = session.is_some_and(|s| s.status == NetStatus::InMatch);
    let error_open = overlay.text.is_some();
    for (mut vis, mode, host, join, error, confirm_root) in roots.iter_mut() {
        let on_title = *state == AppState::Title;
        let wanted = if (mode && on_title && flow.stage == OnlineStage::Mode)
            || (host && on_title && flow.stage == OnlineStage::Host)
            || (join && on_title && flow.stage == OnlineStage::Join)
            || (error && error_open)
            || (confirm_root && confirm.open && in_match && !error_open)
        {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

/// Winner-overlay role gate: a guest's Rematch button is hidden (the guest
/// must never reseed locally — it waits for the host's fresh `MatchStart`).
pub fn sync_net_winner_gate(
    session: Option<Res<NetSession>>,
    mut rematch: Query<&mut Visibility, With<VersusRematchButton>>,
) {
    let guest_in_match = session
        .as_deref()
        .is_some_and(|s| s.status == NetStatus::InMatch && s.role == NetRole::Guest);
    for mut vis in rematch.iter_mut() {
        let wanted = if guest_in_match {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
        if *vis != wanted {
            *vis = wanted;
        }
    }
}

// ---------------------------------------------------------------------------
// Startup UI construction
// ---------------------------------------------------------------------------

fn label_node(text: String, size: f32) -> (Text, TextFont, TextColor) {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor::WHITE,
    )
}

fn online_button(parent: &mut ChildSpawnerCommands, text: &str, marker: impl Bundle) {
    parent
        .spawn((
            Button,
            marker,
            BackgroundColor(BUTTON_BG),
            Node {
                width: Val::Px(220.0),
                height: Val::Px(36.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|button| {
            button.spawn(label_node(text.to_string(), 18.0));
        });
}

fn online_root(
    commands: &mut Commands,
    marker: impl Bundle,
    background: Color,
    build: impl FnOnce(&mut ChildSpawnerCommands),
) {
    commands
        .spawn((
            marker,
            Visibility::Hidden,
            BackgroundColor(background),
            Node {
                display: Display::Flex,
                flex_direction: FlexDirection::Column,
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                row_gap: Val::Px(10.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
        ))
        .with_children(build);
}

fn build_online_ui(mut commands: Commands) {
    // Panel roots share the submenu discipline: `ZIndex(1)` so they pick
    // over the (hidden) title; the overlay roots carry the explicit
    // `NET_OVERLAY_ZINDEX` (teardown contract, recorded click-swallow bugs).
    online_root(
        &mut commands,
        (OnlineModeRoot, ZIndex(1)),
        PANEL_BG,
        |root| {
            root.spawn(label_node("ONLINE".to_string(), 40.0));
            online_button(root, "Host", OnlineHostButton);
            online_button(root, "Join", OnlineJoinButton);
            online_button(root, "Back", OnlineBackButton);
        },
    );

    online_root(
        &mut commands,
        (OnlineHostRoot, ZIndex(1)),
        PANEL_BG,
        |root| {
            root.spawn(label_node("HOST".to_string(), 40.0));
            root.spawn((HostHintText, label_node(String::new(), 22.0)));
            root.spawn((HostStatusText, label_node(String::new(), 22.0)));
            root.spawn((UpnpStatusText, label_node(String::new(), 18.0)));
            root.spawn(label_node(
                "pick a rule when your challenger joins — U toggles router mapping".to_string(),
                15.0,
            ));
            online_button(root, "Garbage", OnlineRuleGarbageButton);
            online_button(root, "Race", OnlineRuleRaceButton);
            online_button(root, "Back", OnlineBackButton);
        },
    );

    online_root(
        &mut commands,
        (OnlineJoinRoot, ZIndex(1)),
        PANEL_BG,
        |root| {
            root.spawn(label_node("JOIN".to_string(), 40.0));
            root.spawn((JoinEntryText, label_node(String::new(), 26.0)));
            root.spawn((JoinStatusText, label_node(String::new(), 20.0)));
            online_button(root, "Join match", OnlineSubmitButton);
            online_button(root, "Back", OnlineBackButton);
        },
    );

    online_root(
        &mut commands,
        (NetErrorRoot, NET_OVERLAY_ZINDEX),
        DIM_BG,
        |root| {
            root.spawn((NetErrorText, label_node(String::new(), 40.0)));
            online_button(root, "Back to title", NetErrorBackButton);
        },
    );

    online_root(
        &mut commands,
        (LeaveConfirmRoot, NET_OVERLAY_ZINDEX),
        DIM_BG,
        |root| {
            root.spawn(label_node("Leave match?".to_string(), 44.0));
            online_button(root, "Leave", LeaveYesButton);
            online_button(root, "Stay", LeaveNoButton);
        },
    );
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// N5 online menu UI. Mounted from `MenuScreensPlugin::build()`; defensive
/// about every resource it reads so it also runs in bare `MinimalPlugins`
/// headless apps. Systems self-chain input/clicks → watch → visibility.
pub struct OnlineUiPlugin;

impl Plugin for OnlineUiPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<OnlineUiMounted>() {
            return;
        }
        app.insert_resource(OnlineUiMounted)
            .init_resource::<OnlineFlow>()
            .init_resource::<JoinEntry>()
            .init_resource::<HostHint>()
            .init_resource::<NetOverlay>()
            .init_resource::<LeaveConfirm>()
            .init_resource::<PressedLatch>()
            .init_resource::<NetProfile>();
        // WAN play addendum: the UPnP driver lives with the Host screen that
        // renders its status line and owns the `U` toggle; mounting here
        // (not in NetPlugin) keeps `session.rs` untouched and lets tests
        // swap the runner before any Host click.
        app.add_plugins(UpnpPlugin);
        if !app.world().contains_resource::<NetSession>() {
            app.init_resource::<NetSession>();
        }
        if !app.world().contains_resource::<Messages<NetEvent>>() {
            app.add_message::<NetEvent>();
        }
        if !app.world().contains_resource::<ButtonInput<KeyCode>>() {
            app.init_resource::<ButtonInput<KeyCode>>();
        }
        app.add_systems(Startup, build_online_ui).add_systems(
            Update,
            (
                (
                    online_esc_system,
                    online_entry_input_system,
                    upnp_toggle_system,
                    online_click_system,
                )
                    .chain(),
                online_session_watch_system
                    .after(online_click_system)
                    .before(sync_online_visibility),
                (
                    sync_online_labels,
                    sync_net_winner_gate,
                    sync_online_visibility,
                )
                    .chain()
                    // Labels must see headlines the watcher parked this very
                    // frame (fire-and-assert tests and live play alike).
                    .after(online_session_watch_system),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure entry logic ----

    #[test]
    fn entry_charset_accepts_only_digits_dots_colons() {
        for c in "0123456789.:".chars() {
            assert!(entry_char(c), "{c} must be accepted");
        }
        for c in "aZ-/_ +\n\t,;[]%".chars() {
            assert!(!entry_char(c), "{c} must be rejected");
        }
    }

    #[test]
    fn entry_push_and_backspace() {
        let mut text = String::new();
        assert!(entry_push(&mut text, '1'));
        assert!(entry_push(&mut text, '.'));
        assert_eq!(text, "1.");
        assert!(
            !entry_push(&mut text, 'a'),
            "out-of-charset push is a no-op"
        );
        assert_eq!(text, "1.");
        assert!(entry_backspace(&mut text));
        assert_eq!(text, "1");
        assert!(entry_backspace(&mut text));
        assert_eq!(text, "");
        assert!(
            !entry_backspace(&mut text),
            "backspace on empty reports no change"
        );
    }

    #[test]
    fn entry_length_is_capped() {
        let mut text = "9".repeat(MAX_JOIN_ADDR_LEN);
        assert!(!entry_push(&mut text, '9'), "cap at {MAX_JOIN_ADDR_LEN}");
        assert_eq!(text.len(), MAX_JOIN_ADDR_LEN);
    }

    #[test]
    fn key_maps_charset_with_shift_fidelity() {
        use KeyCode::*;
        assert_eq!(key_to_entry_char(Digit0, false), Some('0'));
        assert_eq!(key_to_entry_char(Digit9, false), Some('9'));
        assert_eq!(key_to_entry_char(Numpad0, false), Some('0'));
        assert_eq!(key_to_entry_char(Numpad9, false), Some('9'));
        assert_eq!(key_to_entry_char(Period, false), Some('.'));
        assert_eq!(key_to_entry_char(Semicolon, true), Some(':'));
        assert_eq!(
            key_to_entry_char(Semicolon, false),
            None,
            "plain ; is not ':'"
        );
        assert_eq!(key_to_entry_char(KeyA, false), None);
        assert_eq!(key_to_entry_char(Space, false), None);
    }

    #[test]
    fn parse_join_addr_accepts_ipv4_port_forms() {
        assert_eq!(
            parse_join_addr("127.0.0.1:27015"),
            Some("127.0.0.1:27015".parse().unwrap())
        );
        assert_eq!(
            parse_join_addr("192.168.1.9:443"),
            Some("192.168.1.9:443".parse().unwrap())
        );
        assert_eq!(
            parse_join_addr("  10.0.0.1:65535 "),
            Some("10.0.0.1:65535".parse().unwrap()),
            "surrounding whitespace is trimmed"
        );
    }

    #[test]
    fn parse_join_addr_rejects_garbage() {
        for bad in [
            "",
            "   ",
            "1.2.3.4",         // no port
            "1.2.3.4:",        // empty port
            ":1234",           // no address
            "999.1.1.1:70",    // bad octet
            "1.2.3.4:99999",   // port out of range
            "1.2.3.4:0",       // port 0
            "1.2.3.4:x",       // non-numeric port
            "host.example:80", // no DNS
            "1.2.3",           // short
            "1.2.3.4.5:80",    // long
            "01.2.3.4:80",     // leading zeros rejected by Ipv4Addr
            "::1",             // bare IPv6 (no port form)
            "[::1]:1234",      // brackets not in the charset
        ] {
            assert_eq!(parse_join_addr(bad), None, "{bad:?} must be rejected");
        }
    }

    // ---- status/overlay copy ----

    #[test]
    fn status_text_covers_every_net_status() {
        use NetStatus as S;
        let cases = [
            (S::Idle, ""),
            (S::Listening, "challenger"),
            (S::BindFailed("boom".into()), "port in use"),
            (S::Connecting, "connecting"),
            (S::Handshaking, "handshaking"),
            (S::InMatch, "match"),
            (
                S::Lost(NetLossReason::Timeout),
                "host offline or match full",
            ),
            (S::Lost(NetLossReason::PeerDisconnected), "opponent left"),
        ];
        for (status, needle) in cases {
            for role in [NetRole::Host, NetRole::Guest] {
                let text = status_text(role, &status);
                assert!(
                    text.contains(needle),
                    "{role:?} {status:?} -> {text:?} must contain {needle:?}"
                );
            }
        }
    }

    #[test]
    fn guest_ready_waits_for_the_host_host_ready_picks() {
        assert!(
            status_text(NetRole::Guest, &NetStatus::Ready).contains("waiting for the host"),
            "guest-at-Ready gap line"
        );
        assert!(
            status_text(NetRole::Host, &NetStatus::Ready).contains("rule"),
            "host-at-Ready prompts the rule picker"
        );
    }

    #[test]
    fn every_net_event_variant_has_an_overlay_path() {
        assert_eq!(net_event_text(&NetEvent::PeerConnected), None);
        let bind = net_event_text(&NetEvent::BindFailed("os error 98".into()))
            .expect("BindFailed overlay");
        assert!(bind.contains("port in use"), "{bind}");
        let mismatch = net_event_text(&NetEvent::VersionMismatch).expect("VersionMismatch overlay");
        assert!(
            mismatch.to_lowercase().contains("version mismatch"),
            "{mismatch}"
        );
        let timeout = net_event_text(&NetEvent::JoinTimeout).expect("JoinTimeout overlay");
        assert_eq!(timeout, "Connection lost — host offline or match full");
        let denied = net_event_text(&NetEvent::PeerLost(NetLossReason::Denied)).expect("overlay");
        assert_eq!(denied, "Connection lost — host offline or match full");
        let peer_lost =
            net_event_text(&NetEvent::PeerLost(NetLossReason::Timeout)).expect("overlay");
        assert_eq!(peer_lost, "Connection lost — host offline or match full");
        assert_eq!(
            net_event_text(&NetEvent::PeerLost(NetLossReason::PeerDisconnected)).expect("overlay"),
            "opponent left"
        );
        let transport =
            net_event_text(&NetEvent::PeerLost(NetLossReason::Transport)).expect("overlay");
        assert!(transport.contains("Connection lost"), "{transport}");
        assert_eq!(
            net_event_text(&NetEvent::Desync { tick: 42 }).expect("Desync overlay"),
            "desync at tick 42 — match aborted"
        );
        assert_eq!(
            net_event_text(&NetEvent::ByeReceived).expect("ByeReceived overlay"),
            "opponent left"
        );
    }

    #[test]
    fn net_winner_text_is_role_aware() {
        assert_eq!(net_winner_text(Side::Left, NetRole::Host), "YOU WIN");
        assert_eq!(net_winner_text(Side::Right, NetRole::Host), "OPPONENT WINS");
        assert_eq!(net_winner_text(Side::Right, NetRole::Guest), "YOU WIN");
        assert_eq!(net_winner_text(Side::Left, NetRole::Guest), "OPPONENT WINS");
    }

    #[test]
    fn host_hint_falls_back_without_a_route() {
        assert_eq!(
            format_host_hint(Some([192, 168, 1, 5].into()), 27015),
            "192.168.1.5:27015"
        );
        let fallback = format_host_hint(None, 27015);
        assert!(fallback.contains(NO_ROUTE_HINT), "{fallback}");
        assert!(fallback.contains("27015"), "{fallback}");
    }

    // ---- stage machine ----

    #[test]
    fn online_flow_back_walks_every_stage_to_closed() {
        use OnlineStage as St;
        assert_eq!(online_flow_back(St::Mode), St::Closed);
        assert_eq!(online_flow_back(St::Host), St::Closed);
        assert_eq!(online_flow_back(St::Join), St::Closed);
        assert_eq!(online_flow_back(St::Closed), St::Closed);
    }

    #[test]
    fn only_host_and_join_stages_require_net_stop() {
        use OnlineStage as St;
        assert!(
            online_stage_requires_stop(St::Host),
            "Esc on Listening un-listens"
        );
        assert!(online_stage_requires_stop(St::Join));
        assert!(!online_stage_requires_stop(St::Mode));
        assert!(!online_stage_requires_stop(St::Closed));
    }

    #[test]
    fn online_flow_default_is_closed() {
        assert_eq!(OnlineFlow::default().stage, OnlineStage::Closed);
        assert!(!OnlineFlow::default().open());
    }

    // ---- headless App flow tests (mirror the T26 pattern in screens_menu) --
    //
    // Wiring tests are GREEN-after-implementation by necessity (N2/N3
    // precedent — wiring cannot fail before it exists); the pure tests above
    // are the RED→GREEN evidence. These pin the wiring: stage transitions,
    // session side effects, overlay triggers, teardown contract.

    use bevy::ecs::relationship::Relationship;
    use bevy::window::{Window, WindowPlugin};

    use crate::core_bridge::{start_versus, Controller, CoreBridgePlugin, SimPaused, VersusWinner};
    use crate::screens_menu::MenuScreensPlugin;
    use bevy::app::App;

    /// Production plugin tree minus `SettingsPersistPlugin` (temp-dir env
    /// stays untouched). `MenuScreensPlugin` will mount `OnlineUiPlugin`
    /// once the cross-module seam lands; the explicit add here is skipped by
    /// the `OnlineUiMounted` guard then, and loads the plugin standalone
    /// today.
    fn online_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WindowPlugin {
            primary_window: Some(Window {
                title: "blockfall n5 headless".into(),
                resolution: (1280, 720).into(),
                visible: false,
                ..default()
            }),
            ..default()
        });
        app.add_plugins((
            CoreBridgePlugin,
            crate::hud::HudPlugin,
            crate::input::InputPlugin,
            MenuScreensPlugin,
        ));
        // `MenuScreensPlugin` mounts this plugin once the screens wiring is
        // committed; until then (and in minimal trees) mount it here. The
        // resource check keeps us clear of Bevy's App-level duplicate-plugin
        // rejection in trees that already mount it.
        if !app.world().contains_resource::<OnlineFlow>() {
            app.add_plugins(OnlineUiPlugin);
        }
        app.update();
        // WAN play addendum: never let the real SSDP/SOAP client leave the
        // process in tests (the runner seam in net/upnp.rs).
        app.world_mut()
            .resource_mut::<super::super::upnp::UpnpDriver>()
            .runner = |_, _, _| {};
        app
    }

    fn under(world: &World, entity: Entity, root_pred: &impl Fn(&World, Entity) -> bool) -> bool {
        let mut node = entity;
        loop {
            if root_pred(world, node) {
                return true;
            }
            let Some(child_of) = world.get::<ChildOf>(node) else {
                return false;
            };
            node = child_of.get();
        }
    }

    /// Click a button under a matching root (T26 helper pattern), releasing
    /// a previously held `Pressed` first so repeated clicks of the same
    /// button stay fresh for the exclusive system's press latch.
    fn click_button(
        app: &mut App,
        root_pred: impl Fn(&World, Entity) -> bool,
        btn_pred: impl Fn(&World, Entity) -> bool,
    ) {
        let entity = {
            let world = app.world_mut();
            let mut buttons = world.query_filtered::<Entity, With<Button>>();
            buttons
                .iter(world)
                .find(|e| btn_pred(world, *e) && under(world, *e, &root_pred))
                .expect("button entity exists")
        };
        {
            let world = app.world_mut();
            if world
                .get::<Interaction>(entity)
                .is_some_and(|i| *i == Interaction::Pressed)
            {
                world.entity_mut(entity).insert(Interaction::None);
                // A released frame flushes the exclusive system's press
                // latch so this click registers as a fresh press.
                app.update();
            }
        }
        app.world_mut()
            .entity_mut(entity)
            .insert(Interaction::Pressed);
        app.update();
    }

    fn press_key(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset(key);
    }

    fn set_state(app: &mut App, state: AppState) {
        *app.world_mut().resource_mut::<AppState>() = state;
        app.update();
    }

    fn vis_of<R: Component>(app: &mut App) -> Visibility {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Visibility, With<R>>();
        *query.single(world).expect("root entity exists")
    }

    fn flow(app: &App) -> OnlineFlow {
        *app.world().resource::<OnlineFlow>()
    }

    fn status(app: &App) -> NetStatus {
        app.world().resource::<NetSession>().status.clone()
    }

    fn app_state(app: &App) -> AppState {
        *app.world().resource::<AppState>()
    }

    fn overlay_text(app: &mut App) -> String {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Text, With<NetErrorText>>();
        query.single(world).expect("error label").0.clone()
    }

    /// Open the online Mode panel. The Title "Online" button click is
    /// exercised in `screens_menu`'s own tests (the button lives on the
    /// title root built there); tests here drive the flow resource so they
    /// stay independent of the title wiring.
    fn enter_online(app: &mut App) {
        set_state(app, AppState::Title);
        app.world_mut().resource_mut::<OnlineFlow>().stage = OnlineStage::Mode;
        app.update();
    }

    fn mode_root(world: &World, e: Entity) -> bool {
        world.get::<OnlineModeRoot>(e).is_some()
    }
    fn host_root(world: &World, e: Entity) -> bool {
        world.get::<OnlineHostRoot>(e).is_some()
    }
    fn join_root(world: &World, e: Entity) -> bool {
        world.get::<OnlineJoinRoot>(e).is_some()
    }
    fn error_root(world: &World, e: Entity) -> bool {
        world.get::<NetErrorRoot>(e).is_some()
    }
    fn confirm_root(world: &World, e: Entity) -> bool {
        world.get::<LeaveConfirmRoot>(e).is_some()
    }
    fn over_root(world: &World, e: Entity) -> bool {
        world
            .get::<crate::screens_menu::VersusOverRoot>(e)
            .is_some()
    }

    #[test]
    fn mode_panel_shows_and_back_closes() {
        let mut app = online_app();
        enter_online(&mut app);
        assert_eq!(vis_of::<OnlineModeRoot>(&mut app), Visibility::Visible);
        assert_eq!(vis_of::<OnlineHostRoot>(&mut app), Visibility::Hidden);
        click_button(&mut app, mode_root, |world, e| {
            world.get::<OnlineBackButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
        assert_eq!(vis_of::<OnlineModeRoot>(&mut app), Visibility::Hidden);
        // The Mode stage holds no session, so nothing to stop.
        assert_eq!(status(&app), NetStatus::Idle);
    }

    #[test]
    fn esc_walks_online_stages_back_to_the_title() {
        let mut app = online_app();
        enter_online(&mut app);
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(flow(&app).stage, OnlineStage::Closed, "Mode → title");
    }

    #[test]
    fn host_button_listens_and_esc_unlistens_on_the_way_out() {
        let mut app = online_app();
        enter_online(&mut app);
        click_button(&mut app, mode_root, |world, e| {
            world.get::<OnlineHostButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, OnlineStage::Host);
        assert_eq!(vis_of::<OnlineHostRoot>(&mut app), Visibility::Visible);
        {
            let session = app.world().resource::<NetSession>();
            assert_eq!(session.role, NetRole::Host);
            // The fixed default port may be held by another process on the
            // dev box; Listening or BindFailed both prove net_host ran.
            assert!(
                matches!(
                    session.status,
                    NetStatus::Listening | NetStatus::BindFailed(_)
                ),
                "host attempt status {:?}",
                session.status
            );
        }
        assert!(
            !app.world().resource::<HostHint>().share.is_empty(),
            "connect hint computed on entering Host"
        );
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
        assert_eq!(status(&app), NetStatus::Idle, "Esc on Listening un-listens");
    }

    #[test]
    fn back_button_from_host_also_stops_the_session() {
        let mut app = online_app();
        enter_online(&mut app);
        click_button(&mut app, mode_root, |world, e| {
            world.get::<OnlineHostButton>(e).is_some()
        });
        click_button(&mut app, host_root, |world, e| {
            world.get::<OnlineBackButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
        assert_eq!(status(&app), NetStatus::Idle);
    }

    #[test]
    fn host_rule_pick_waits_for_ready_then_starts_the_match() {
        let mut app = online_app();
        enter_online(&mut app);
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
        }
        app.update();
        click_button(&mut app, host_root, |world, e| {
            world.get::<OnlineRuleGarbageButton>(e).is_some()
        });
        assert_eq!(
            status(&app),
            NetStatus::Listening,
            "no match before a challenger is Ready"
        );

        app.world_mut().resource_mut::<NetSession>().status = NetStatus::Ready;
        app.update();
        click_button(&mut app, host_root, |world, e| {
            world.get::<OnlineRuleGarbageButton>(e).is_some()
        });
        assert_eq!(status(&app), NetStatus::InMatch);
        {
            let versus = app.world().non_send::<VersusMatch>();
            assert!(versus.active, "mirror match live");
            assert_eq!(versus.p2, Controller::Net, "right side forced Net");
            assert_eq!(versus.p1, Controller::Human, "left seat stays Human");
            assert_eq!(versus.rule, AttackRule::Garbage);
        }
        assert_eq!(
            flow(&app).stage,
            OnlineStage::Closed,
            "flow closed on start"
        );
        assert_eq!(app_state(&app), AppState::Playing, "match takes the screen");
        assert_eq!(vis_of::<OnlineHostRoot>(&mut app), Visibility::Hidden);
    }

    #[test]
    fn joining_prefills_the_last_address_types_and_submits() {
        let mut app = online_app();
        app.world_mut().resource_mut::<NetProfile>().last_join_addr = "10.1.2.3:5555".to_string();
        enter_online(&mut app);
        click_button(&mut app, mode_root, |world, e| {
            world.get::<OnlineJoinButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, OnlineStage::Join);
        assert_eq!(
            app.world().resource::<JoinEntry>().text,
            "10.1.2.3:5555",
            "prefilled from the net profile"
        );
        assert_eq!(vis_of::<OnlineJoinRoot>(&mut app), Visibility::Visible);

        press_key(&mut app, KeyCode::Backspace);
        assert_eq!(app.world().resource::<JoinEntry>().text, "10.1.2.3:555");
        press_key(&mut app, KeyCode::Digit7);
        assert_eq!(app.world().resource::<JoinEntry>().text, "10.1.2.3:5557");

        press_key(&mut app, KeyCode::Enter);
        let session = app.world().resource::<NetSession>();
        assert_eq!(session.role, NetRole::Guest);
        assert!(
            matches!(
                session.status,
                NetStatus::Connecting | NetStatus::BindFailed(_)
            ),
            "net_join attempted: {:?}",
            session.status
        );
        assert_eq!(
            app.world().resource::<NetProfile>().last_join_addr,
            "10.1.2.3:5557",
            "submitted address persisted for next time"
        );
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
        assert_eq!(
            status(&app),
            NetStatus::Idle,
            "Esc on Join stops the attempt"
        );
    }

    #[test]
    fn invalid_join_address_is_flagged_not_submitted() {
        let mut app = online_app();
        enter_online(&mut app);
        app.world_mut().resource_mut::<OnlineFlow>().stage = OnlineStage::Join;
        app.world_mut().resource_mut::<JoinEntry>().text = "1.2.3".to_string();
        app.update();
        press_key(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), NetStatus::Idle, "nothing dialed");
        assert!(app.world().resource::<JoinEntry>().invalid);
        press_key(&mut app, KeyCode::Digit0);
        assert!(
            !app.world().resource::<JoinEntry>().invalid,
            "editing clears the flag"
        );
    }

    #[test]
    fn match_becoming_live_closes_panels_and_takes_the_game_screen() {
        // Covers the guest path too: the guest never presses a start button,
        // the received MatchStart flips the session and the watcher reacts.
        let mut app = online_app();
        enter_online(&mut app);
        app.world_mut().resource_mut::<OnlineFlow>().stage = OnlineStage::Join;
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Guest;
            session.status = NetStatus::InMatch;
        }
        app.update();
        app.update();
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
        assert_eq!(app_state(&app), AppState::Playing);
        assert_eq!(vis_of::<OnlineJoinRoot>(&mut app), Visibility::Hidden);
    }

    // ---- overlays: trigger + teardown per NetEvent variant ----

    fn fire(app: &mut App, event: NetEvent) {
        app.world_mut().write_message(event);
        app.update();
    }

    /// Trigger-and-teardown sweep: every overlay-worthy event shows the
    /// overlay with the right headline, and "Back to title" runs the full
    /// net teardown (`net_leave_to_title` contract: Idle session, Title
    /// screen, overlay gone).
    #[test]
    fn every_overlay_event_shows_and_back_to_title_tears_down() {
        let events = [
            (
                NetEvent::BindFailed("address already in use (os error 98)".into()),
                "port in use",
            ),
            (NetEvent::VersionMismatch, "Version mismatch"),
            (NetEvent::JoinTimeout, "host offline or match full"),
            (
                NetEvent::PeerLost(NetLossReason::Timeout),
                "host offline or match full",
            ),
            (
                NetEvent::PeerLost(NetLossReason::Denied),
                "host offline or match full",
            ),
            (
                NetEvent::PeerLost(NetLossReason::PeerDisconnected),
                "opponent left",
            ),
            (
                NetEvent::PeerLost(NetLossReason::Transport),
                "Connection lost",
            ),
            (NetEvent::Desync { tick: 7 }, "desync at tick 7"),
            (NetEvent::ByeReceived, "opponent left"),
        ];
        for (event, needle) in events {
            let mut app = online_app();
            set_state(&mut app, AppState::Title);
            fire(&mut app, event.clone());
            assert_eq!(
                vis_of::<NetErrorRoot>(&mut app),
                Visibility::Visible,
                "overlay shows for {event:?}"
            );
            let text = overlay_text(&mut app);
            assert!(text.contains(needle), "{event:?} -> {text:?}");
            // Explicit ZIndex on the overlay root (teardown contract: picks
            // over the ZIndex(0) title/HUD roots, above the ZIndex(1) panels).
            let world = app.world_mut();
            let mut zq = world.query_filtered::<&ZIndex, With<NetErrorRoot>>();
            assert_eq!(*zq.single(world).expect("overlay z"), NET_OVERLAY_ZINDEX);

            click_button(&mut app, error_root, |world, e| {
                world.get::<NetErrorBackButton>(e).is_some()
            });
            assert_eq!(vis_of::<NetErrorRoot>(&mut app), Visibility::Hidden);
            assert_eq!(status(&app), NetStatus::Idle, "teardown after {event:?}");
            assert_eq!(app_state(&app), AppState::Title);
            assert_eq!(flow(&app).stage, OnlineStage::Closed);
        }
    }

    #[test]
    fn peer_connected_has_no_overlay_and_updates_the_status_line() {
        let mut app = online_app();
        enter_online(&mut app);
        fire(&mut app, NetEvent::PeerConnected);
        assert_eq!(
            vis_of::<NetErrorRoot>(&mut app),
            Visibility::Hidden,
            "PeerConnected never overlays"
        );
        assert!(
            app.world().resource::<NetOverlay>().text.is_none(),
            "no headline parked"
        );
    }

    #[test]
    fn desync_freeze_keeps_boards_visible_behind_the_overlay() {
        // Teardown contract: SimPaused freeze, VersusMatch stays active, the
        // versus HUD keeps rendering under the overlay, and Back to title
        // performs the full teardown.
        let mut app = online_app();
        set_state(&mut app, AppState::Playing);
        {
            let world = app.world_mut();
            world.resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Bot,
                    );
                });
            });
        }
        // N3's lockstep would have set both of these; simulate the state it
        // hands over (Desync/Bye freeze path):
        app.world_mut().resource_mut::<SimPaused>().0 = true;
        fire(&mut app, NetEvent::Desync { tick: 42 });
        assert_eq!(
            vis_of::<NetErrorRoot>(&mut app),
            Visibility::Visible,
            "overlay over the frozen match"
        );
        assert!(app.world().non_send::<VersusMatch>().active);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(true));
        let hud_shown = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&Visibility, With<crate::hud::VersusHudRoot>>();
            q.iter(world).any(|v| *v == Visibility::Visible)
        };
        assert!(hud_shown, "frozen versus HUD still visible behind overlay");

        click_button(&mut app, error_root, |world, e| {
            world.get::<NetErrorBackButton>(e).is_some()
        });
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(*app.world().resource::<SimPaused>(), SimPaused(false));
        assert_eq!(app_state(&app), AppState::Title);
        assert_eq!(status(&app), NetStatus::Idle);
    }

    // ---- mid-match exit ----

    #[test]
    fn esc_in_match_opens_the_leave_confirm_and_stay_closes_it() {
        let mut app = online_app();
        set_state(&mut app, AppState::Playing);
        app.world_mut().resource_mut::<NetSession>().status = NetStatus::InMatch;
        app.update();
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(
            app_state(&app),
            AppState::Playing,
            "no pause state in net matches"
        );
        assert_eq!(
            *app.world().resource::<SimPaused>(),
            SimPaused(false),
            "the confirm does not freeze (lockstep has no authoritative pause)"
        );
        assert_eq!(vis_of::<LeaveConfirmRoot>(&mut app), Visibility::Visible);
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(vis_of::<LeaveConfirmRoot>(&mut app), Visibility::Hidden);
        press_key(&mut app, KeyCode::Escape);
        click_button(&mut app, confirm_root, |world, e| {
            world.get::<LeaveNoButton>(e).is_some()
        });
        assert_eq!(vis_of::<LeaveConfirmRoot>(&mut app), Visibility::Hidden);
    }

    #[test]
    fn leave_confirms_and_tears_down_to_title() {
        let mut app = online_app();
        set_state(&mut app, AppState::Playing);
        {
            let world = app.world_mut();
            world.resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Bot,
                    );
                });
            });
        }
        app.world_mut().resource_mut::<NetSession>().status = NetStatus::InMatch;
        app.update();
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(vis_of::<LeaveConfirmRoot>(&mut app), Visibility::Visible);
        click_button(&mut app, confirm_root, |world, e| {
            world.get::<LeaveYesButton>(e).is_some()
        });
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(app_state(&app), AppState::Title);
        assert_eq!(status(&app), NetStatus::Idle);
        assert_eq!(vis_of::<LeaveConfirmRoot>(&mut app), Visibility::Hidden);
    }

    // ---- winner overlay: role-gated rematch, net-aware menu ----

    fn net_match_over(app: &mut App, role: NetRole, winner: Side) {
        set_state(app, AppState::Playing);
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Garbage,
                        Controller::Human,
                        Controller::Net,
                    );
                });
            });
        let world = app.world_mut();
        world.resource_mut::<VersusWinner>().0 = Some(winner);
        let mut session = world.resource_mut::<NetSession>();
        session.role = role;
        session.status = NetStatus::InMatch;
        app.update();
        app.update();
    }

    #[test]
    fn guest_rematch_button_is_hidden_and_inert() {
        let mut app = online_app();
        net_match_over(&mut app, NetRole::Guest, Side::Left);
        assert_eq!(
            vis_of::<crate::screens_menu::VersusOverRoot>(&mut app),
            Visibility::Visible
        );
        let vis = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&Visibility, With<VersusRematchButton>>();
            *q.single(world).expect("rematch button")
        };
        assert_eq!(vis, Visibility::Hidden, "guest must not reseed locally");
        click_button(&mut app, over_root, |world, e| {
            world.get::<VersusRematchButton>(e).is_some()
        });
        assert_eq!(
            app.world().resource::<VersusWinner>().0,
            Some(Side::Left),
            "guest rematch click is inert (waits for the host's MatchStart)"
        );
    }

    #[test]
    fn host_rematch_reroutes_through_start_net_match() {
        let mut app = online_app();
        net_match_over(&mut app, NetRole::Host, Side::Right);
        let vis = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&Visibility, With<VersusRematchButton>>();
            *q.single(world).expect("rematch button")
        };
        assert_ne!(vis, Visibility::Hidden, "host keeps Rematch");
        // Give the old match some steps so a reseed is observable.
        app.world_mut().non_send_mut::<VersusMatch>().steps = 12;
        click_button(&mut app, over_root, |world, e| {
            world.get::<VersusRematchButton>(e).is_some()
        });
        {
            let versus = app.world().non_send::<VersusMatch>();
            assert!(versus.active, "rematch keeps the match up");
            assert_eq!(versus.steps, 0, "fresh mirror from a fresh MatchStart");
        }
        assert_eq!(app.world().resource::<VersusWinner>().0, None);
        assert_eq!(
            status(&app),
            NetStatus::InMatch,
            "session stays InMatch (re-armed, not restarted)"
        );
    }

    #[test]
    fn net_winner_menu_button_leaves_to_title() {
        let mut app = online_app();
        net_match_over(&mut app, NetRole::Host, Side::Left);
        click_button(&mut app, over_root, |world, e| {
            world
                .get::<crate::screens_menu::VersusMenuButton>(e)
                .is_some()
        });
        assert_eq!(app_state(&app), AppState::Title);
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(status(&app), NetStatus::Idle);
    }

    #[test]
    fn local_versus_flows_are_untouched_by_the_online_ui() {
        // Session Idle: none of the net gates fire; the classic local winner
        // overlay keeps its original behavior (Rematch visible + clickable).
        let mut app = online_app();
        set_state(&mut app, AppState::Playing);
        app.world_mut()
            .resource_scope::<AppState, ()>(|world, state| {
                world.resource_scope::<VersusWinner, ()>(|world, winner| {
                    let versus = world.non_send_mut::<VersusMatch>();
                    start_versus(
                        versus.into_inner(),
                        winner.into_inner(),
                        state.into_inner(),
                        AttackRule::Race { target_lines: 0 },
                        Controller::Human,
                        Controller::Human,
                    );
                });
            });
        app.update();
        app.update();
        let vis = {
            let world = app.world_mut();
            let mut q = world.query_filtered::<&Visibility, With<VersusRematchButton>>();
            *q.single(world).expect("rematch button")
        };
        assert_ne!(vis, Visibility::Hidden, "local rematch stays visible");
        assert_eq!(status(&app), NetStatus::Idle);
        assert_eq!(flow(&app).stage, OnlineStage::Closed);
    }

    // ---- WAN play addendum: UPnP status line + U toggle ----

    #[test]
    fn upnp_status_line_exact_copy() {
        assert_eq!(upnp_status_text(&UpnpState::Off, true, true, 27015), "");
        assert_eq!(
            upnp_status_text(&UpnpState::Mapping { port: 27015 }, true, true, 27015),
            "Opening router port…"
        );
        assert_eq!(
            upnp_status_text(
                &UpnpState::Mapped {
                    external_ip: "203.0.113.7".into(),
                    port: 27015
                },
                true,
                true,
                27015
            ),
            "Friends join at 203.0.113.7:27015"
        );
        assert_eq!(
            upnp_status_text(&UpnpState::Failed("718 conflict".into()), true, true, 27015),
            "UPnP unavailable — forward UDP 27015 manually (see README)"
        );
        assert_eq!(
            upnp_status_text(&UpnpState::Off, false, true, 27015),
            "Router mapping off — press U to enable"
        );
        assert_eq!(
            upnp_status_text(&UpnpState::Off, false, false, 27015),
            "",
            "the disabled hint only shows while hosting"
        );
    }

    fn upnp_label(app: &mut App) -> String {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Text, With<UpnpStatusText>>();
        query.single(world).expect("upnp label").0.clone()
    }

    #[test]
    fn host_screen_renders_the_upnp_state_line() {
        let mut app = online_app();
        enter_online(&mut app);
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some("0.0.0.0:27015".parse().expect("addr"));
        }
        *app.world_mut().resource_mut::<UpnpState>() = UpnpState::Mapping { port: 27015 };
        app.update();
        assert_eq!(upnp_label(&mut app), "Opening router port…");

        *app.world_mut().resource_mut::<UpnpState>() = UpnpState::Mapped {
            external_ip: "203.0.113.7".into(),
            port: 27015,
        };
        app.update();
        assert_eq!(
            upnp_label(&mut app),
            "Friends join at 203.0.113.7:27015",
            "the mapped line carries the public join address"
        );

        *app.world_mut().resource_mut::<UpnpState>() =
            UpnpState::Failed("no UPnP router answered on the network".into());
        app.update();
        assert_eq!(
            upnp_label(&mut app),
            "UPnP unavailable — forward UDP 27015 manually (see README)",
            "failure keeps it one line and never blocks play"
        );
    }

    #[test]
    fn u_key_on_the_host_screen_toggles_upnp_and_persists() {
        let mut app = online_app();
        enter_online(&mut app);
        app.world_mut().resource_mut::<OnlineFlow>().stage = OnlineStage::Host;
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some("0.0.0.0:27015".parse().expect("addr"));
        }
        *app.world_mut().resource_mut::<UpnpState>() = UpnpState::Mapped {
            external_ip: "203.0.113.7".into(),
            port: 27015,
        };
        app.update();

        press_key(&mut app, KeyCode::KeyU);
        assert!(
            !app.world().resource::<NetProfile>().upnp_enabled,
            "U disables in the profile"
        );
        assert_eq!(
            *app.world().resource::<UpnpState>(),
            UpnpState::Off,
            "disabling tears the mapping down"
        );
        assert_eq!(
            upnp_label(&mut app),
            "Router mapping off — press U to enable"
        );

        press_key(&mut app, KeyCode::KeyU);
        assert!(app.world().resource::<NetProfile>().upnp_enabled);
        assert_eq!(
            *app.world().resource::<UpnpState>(),
            UpnpState::Mapping { port: 27015 },
            "re-enabling retries immediately while Listening (silent test runner)"
        );
    }

    #[test]
    fn esc_on_listening_clears_the_upnp_state() {
        let mut app = online_app();
        enter_online(&mut app);
        app.world_mut().resource_mut::<OnlineFlow>().stage = OnlineStage::Host;
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some("0.0.0.0:27015".parse().expect("addr"));
        }
        *app.world_mut().resource_mut::<UpnpState>() = UpnpState::Mapped {
            external_ip: "203.0.113.7".into(),
            port: 27015,
        };
        app.update();
        assert_eq!(upnp_label(&mut app), "Friends join at 203.0.113.7:27015");

        press_key(&mut app, KeyCode::Escape);
        assert_eq!(status(&app), NetStatus::Idle);
        assert_eq!(
            *app.world().resource::<UpnpState>(),
            UpnpState::Off,
            "the net_stop teardown edge resets UPnP"
        );
    }
}
