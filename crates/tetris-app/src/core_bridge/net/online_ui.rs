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

use netplay_gateway::wire;

use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

use super::gateway::{self, format_code, GuestLookupState, HostRoomState, NetGateway};
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
// Pure room-code entry (gateway-plan.md G3) — same keyboard-only shape as
// the IP entry above, over the gateway's confusion-free alphabet
// ---------------------------------------------------------------------------

/// A room code is exactly this many characters (`*R`/`*G` codes on the wire).
pub const ROOM_CODE_LEN: usize = 5;

/// The join-by-code charset as a display string: the wire crate's
/// confusion-free alphabet (no `I L O 0 1`), uppercase-normalized.
pub const ROOM_CODE_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// `true` when `c` is a member of the room-code alphabet — lowercase
/// keystrokes are members too (they normalize to uppercase on push).
#[must_use]
pub fn code_char(c: char) -> bool {
    c.is_ascii() && wire::is_code_byte(c as u8)
}

/// Append the normalized `c` to `text` while it is in the alphabet and the
/// entry has room. A 6th character is rejected. Returns whether it changed.
pub fn code_push(text: &mut String, c: char) -> bool {
    if text.len() >= ROOM_CODE_LEN || !code_char(c) {
        return false;
    }
    text.push(c.to_ascii_uppercase());
    true
}

/// Map a freshly pressed key to its room-code character. Letters and digits
/// `2..=9` (main row + numpad) map; everything else — including `I L O 0 1`,
/// which are outside the confusion-free alphabet — maps to `None`.
#[must_use]
pub fn key_to_code_char(key: KeyCode) -> Option<char> {
    let base = match key {
        KeyCode::KeyA => 'A',
        KeyCode::KeyB => 'B',
        KeyCode::KeyC => 'C',
        KeyCode::KeyD => 'D',
        KeyCode::KeyE => 'E',
        KeyCode::KeyF => 'F',
        KeyCode::KeyG => 'G',
        KeyCode::KeyH => 'H',
        KeyCode::KeyI => 'I',
        KeyCode::KeyJ => 'J',
        KeyCode::KeyK => 'K',
        KeyCode::KeyL => 'L',
        KeyCode::KeyM => 'M',
        KeyCode::KeyN => 'N',
        KeyCode::KeyO => 'O',
        KeyCode::KeyP => 'P',
        KeyCode::KeyQ => 'Q',
        KeyCode::KeyR => 'R',
        KeyCode::KeyS => 'S',
        KeyCode::KeyT => 'T',
        KeyCode::KeyU => 'U',
        KeyCode::KeyV => 'V',
        KeyCode::KeyW => 'W',
        KeyCode::KeyX => 'X',
        KeyCode::KeyY => 'Y',
        KeyCode::KeyZ => 'Z',
        KeyCode::Digit2 | KeyCode::Numpad2 => '2',
        KeyCode::Digit3 | KeyCode::Numpad3 => '3',
        KeyCode::Digit4 | KeyCode::Numpad4 => '4',
        KeyCode::Digit5 | KeyCode::Numpad5 => '5',
        KeyCode::Digit6 | KeyCode::Numpad6 => '6',
        KeyCode::Digit7 | KeyCode::Numpad7 => '7',
        KeyCode::Digit8 | KeyCode::Numpad8 => '8',
        KeyCode::Digit9 | KeyCode::Numpad9 => '9',
        _ => return None,
    };
    if code_char(base) {
        Some(base)
    } else {
        None
    }
}

/// Parse a submitted entry into a [`RoomCode`]: exactly [`ROOM_CODE_LEN`]
/// alphabet characters (case-normalized), anything else rejected.
#[must_use]
pub fn parse_room_code(text: &str) -> Option<wire::RoomCode> {
    let bytes = text.as_bytes();
    if bytes.len() != ROOM_CODE_LEN {
        return None;
    }
    let mut code = [0u8; ROOM_CODE_LEN];
    for (dst, &src) in code.iter_mut().zip(bytes) {
        if !wire::is_code_byte(src) {
            return None;
        }
        *dst = src.to_ascii_uppercase();
    }
    Some(code)
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
// Gateway status lines (gateway-plan.md G3)
// ---------------------------------------------------------------------------

/// One-line copy for a [`GuestLookupState`] (Join screen, Code mode). In and
/// out of flight the line narrates the lookup; `Idle`/`Found` return empty
/// so the session status (connecting/handshaking/…) shows instead — the
/// `*F` handoff must not blank out the existing FSM's copy. The shipped
/// strings: `resolving…` / `joining room…` / `no such room` / `match full` /
/// `gateway full — retry later` / `gateway offline — check connection or
/// join by IP`.
#[must_use]
pub fn lookup_status_text(state: &GuestLookupState) -> String {
    match state {
        GuestLookupState::Idle | GuestLookupState::Found { .. } => String::new(),
        GuestLookupState::Resolving(_) => "resolving…".to_string(),
        GuestLookupState::LookingUp(_) => "joining room…".to_string(),
        GuestLookupState::NotFound(_) => "no such room".to_string(),
        GuestLookupState::Busy(_) => "match full".to_string(),
        GuestLookupState::SlotExhausted(_) => "gateway full — retry later".to_string(),
        GuestLookupState::Timeout | GuestLookupState::GatewayUnreachable(_) => {
            "gateway offline — check connection or join by IP".to_string()
        }
    }
}

/// The room-path-specific connect failure (field fix): the gateway answered
/// `*F` (room found — [`GuestLookupState::Found`] is sticky for the whole
/// connect), and the netcode connect **through the relay** then timed out:
/// the host's router refused the relayed handshake (no mapping — the punch
/// was impossible, aged out, or the router is symmetric). This is a strictly
/// more actionable message than the generic timeout line, and applies ONLY
/// here — `*B`/`*E` copy, direct-IP joins, and every other loss are
/// untouched.
#[must_use]
pub fn room_connect_failure_text(status: &NetStatus) -> Option<&'static str> {
    match status {
        NetStatus::Lost(NetLossReason::Timeout) => {
            Some("host unreachable — ask host to enable UPnP or port-forward UDP 27015")
        }
        _ => None,
    }
}

/// The Host screen's single extra line (gateway-plan.md G3): the live room
/// code wins (`Room XXXXX — share with a friend`), an in-flight registration
/// shows the same code while announcing, and the offline fallback names the
/// reason. Empty when the gateway is disabled or idle — the line never
/// competes with LAN play, and the UPnP line renders independently below
/// (design decision recorded in the plan: room code > offline reason on
/// this label; the UPnP line stays its own line).
#[must_use]
pub fn host_room_text(state: &HostRoomState, gateway_enabled: bool) -> String {
    if !gateway_enabled {
        return String::new();
    }
    match state {
        HostRoomState::Idle => String::new(),
        HostRoomState::Advertising(code) => {
            format!("Room {} — announcing…", format_code(code))
        }
        HostRoomState::Announced(code) => {
            format!("Room {} — share with a friend", format_code(code))
        }
        HostRoomState::Offline(reason) => format!("gateway offline — {reason}"),
    }
}

/// The Join screen status line: in Code mode a live/terminal lookup owns
/// the line; otherwise (IP mode, or Code with no lookup in flight) it is the
/// existing role-aware session status.
#[must_use]
pub fn join_status_text(
    mode: JoinMode,
    gateway: Option<&NetGateway>,
    status: &NetStatus,
) -> String {
    if mode == JoinMode::Code {
        if let Some(gateway) = gateway {
            // Field fix: a successful `*F` whose connect then died says so
            // with the actionable room-path copy (see
            // [`room_connect_failure_text`]) instead of the generic line.
            if matches!(gateway.guest, GuestLookupState::Found { .. }) {
                if let Some(text) = room_connect_failure_text(status) {
                    return text.to_string();
                }
            }
            let line = lookup_status_text(&gateway.guest);
            if !line.is_empty() {
                return line;
            }
        }
    }
    status_text(NetRole::Guest, status)
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
    /// Which Join entry is active (G3). Set on entering `Join` from the
    /// gateway availability: Code whenever the gateway is enabled and the
    /// profile allows it, IP otherwise; the toggle flips it in-panel while
    /// available.
    pub join_mode: JoinMode,
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
// Join mode (G3): the Join panel offers Code (gateway room code) and IP
// (manual address) — one toggle, two entries, one status line
// ---------------------------------------------------------------------------

/// How the player addresses the host from the Join panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinMode {
    /// Gateway room code (the default whenever the gateway is available —
    /// plan G3: "default Code when gateway enabled").
    #[default]
    Code,
    /// Manual `IPv4:port` (the pre-G3 path, always available).
    Ip,
}

/// The other mode (Code ⇄ IP toggle).
#[must_use]
pub fn next_join_mode(mode: JoinMode) -> JoinMode {
    match mode {
        JoinMode::Code => JoinMode::Ip,
        JoinMode::Ip => JoinMode::Code,
    }
}

/// Code mode exists only when the gateway is both enabled (endpoint known)
/// *and* not explicitly disabled in the [`NetProfile`]; an unavailable Code
/// mode hides the entry and renders the toggle inert ("looks disabled").
#[must_use]
pub fn join_mode_available(gateway_enabled: bool, profile_enabled: bool) -> bool {
    gateway_enabled && profile_enabled
}

/// The mode the Join panel actually runs: `Code` when available, else IP.
#[must_use]
pub fn effective_join_mode(requested: JoinMode, available: bool) -> JoinMode {
    if available {
        requested
    } else {
        JoinMode::Ip
    }
}

/// Toggle label copy: the disabled state reads as a hint (join by IP is all
/// the gateway-off build can offer) rather than an interactive control.
#[must_use]
pub fn join_mode_label(mode: JoinMode, available: bool) -> String {
    if !available {
        return "gateway off — join by IP".to_string();
    }
    match mode {
        JoinMode::Code => "Mode: Code".to_string(),
        JoinMode::Ip => "Mode: IP".to_string(),
    }
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

/// The room-code entry widget state (G3; keyboard-only, same discipline as
/// [`JoinEntry`] — the two entries are independent, so switching modes
/// preserves each one's text).
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub struct CodeEntry {
    /// Current text (normalized to uppercase, at most [`ROOM_CODE_LEN`]).
    pub text: String,
    /// Last submit was rejected (label hint).
    pub invalid: bool,
    /// Enter was pressed this frame; the exclusive click system performs the
    /// actual gateway lookup (a normal system cannot take `&mut World`).
    submit: bool,
}

impl CodeEntry {
    /// Flag a submit request (called by the entry input system).
    pub fn request_submit(&mut self) {
        self.submit = true;
    }

    /// Take a pending submit request.
    pub fn take_submit(&mut self) -> bool {
        std::mem::take(&mut self.submit)
    }
}

/// Whether Code mode is on offer: gateway enabled **and** the (persisted)
/// profile toggle on. Absent resources read as "no gateway" / "default on".
#[must_use]
pub fn gateway_available(gateway: Option<&NetGateway>, profile: Option<&NetProfile>) -> bool {
    join_mode_available(
        gateway.is_some_and(|g| g.enabled),
        profile.is_none_or(|p| p.gateway_enabled),
    )
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
    mode: bool,
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
/// Join panel mode toggle (G3): flips Code ⇄ IP while the gateway is
/// available; inert (and labeled "gateway off") when it is not.
#[derive(Component)]
pub struct JoinModeButton;

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
/// Dynamic gateway room-code line on the Host panel (G3): the share line
/// when a room is live, the offline fallback otherwise, empty when the
/// gateway is off.
#[derive(Component)]
pub struct RoomStatusText;
/// Dynamic entry echo on the Join panel (renders the active entry: IP or
/// room code, per [`OnlineFlow::join_mode`]).
#[derive(Component)]
pub struct JoinEntryText;
/// Dynamic label inside the Join panel's mode toggle (G3).
#[derive(Component)]
pub struct JoinModeText;
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

/// Submit the Join entry, per the active [`JoinMode`]. IP mode: valid
/// `IPv4:port` → remember it in the [`NetProfile`] and `net_join`; invalid →
/// flag the label. Code mode: a full 5-char room code hands off to
/// [`gateway::join_room`] (the G2 driver resolves, sends `*G` and calls the
/// same `net_join` on `*F` — the client FSM is never forked).
fn join_submit(world: &mut World, mode: JoinMode) {
    match mode {
        JoinMode::Ip => {
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
        JoinMode::Code => {
            let text = world.resource::<CodeEntry>().text.clone();
            let Some(code) = parse_room_code(&text) else {
                world.resource_mut::<CodeEntry>().invalid = true;
                return;
            };
            world.resource_mut::<CodeEntry>().invalid = false;
            gateway::join_room(world, code);
        }
    }
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

/// Every key that can produce a room-code character (letters plus digits
/// `2..=9` on the main row and the numpad); each press still passes the
/// alphabet filter inside [`key_to_code_char`], so `I L O` stay inert.
const CODE_KEYS: [KeyCode; 42] = [
    KeyCode::KeyA,
    KeyCode::KeyB,
    KeyCode::KeyC,
    KeyCode::KeyD,
    KeyCode::KeyE,
    KeyCode::KeyF,
    KeyCode::KeyG,
    KeyCode::KeyH,
    KeyCode::KeyI,
    KeyCode::KeyJ,
    KeyCode::KeyK,
    KeyCode::KeyL,
    KeyCode::KeyM,
    KeyCode::KeyN,
    KeyCode::KeyO,
    KeyCode::KeyP,
    KeyCode::KeyQ,
    KeyCode::KeyR,
    KeyCode::KeyS,
    KeyCode::KeyT,
    KeyCode::KeyU,
    KeyCode::KeyV,
    KeyCode::KeyW,
    KeyCode::KeyX,
    KeyCode::KeyY,
    KeyCode::KeyZ,
    KeyCode::Digit2,
    KeyCode::Digit3,
    KeyCode::Digit4,
    KeyCode::Digit5,
    KeyCode::Digit6,
    KeyCode::Digit7,
    KeyCode::Digit8,
    KeyCode::Digit9,
    KeyCode::Numpad2,
    KeyCode::Numpad3,
    KeyCode::Numpad4,
    KeyCode::Numpad5,
    KeyCode::Numpad6,
    KeyCode::Numpad7,
    KeyCode::Numpad8,
    KeyCode::Numpad9,
];

/// Character input for the Join entry: charset keys append, Backspace edits,
/// Enter flags a submit (the exclusive click system performs the action).
/// Code mode (G3) drives [`CodeEntry`] through the room-code alphabet;
/// otherwise the IP entry keeps its digit/dot/colon behavior.
#[allow(clippy::too_many_arguments)]
pub fn online_entry_input_system(
    keys: Option<Res<ButtonInput<KeyCode>>>,
    capture: Res<RebindingCapture>,
    state: Res<AppState>,
    flow: Res<OnlineFlow>,
    gateway: Option<Res<NetGateway>>,
    profile: Option<Res<NetProfile>>,
    mut entry: ResMut<JoinEntry>,
    mut code: ResMut<CodeEntry>,
) {
    if capture.capturing
        || flow.stage != OnlineStage::Join
        || *state != AppState::Title
        || entry.submit
        || code.submit
    {
        return;
    }
    let Some(keys) = keys else { return };
    let mode = effective_join_mode(
        flow.join_mode,
        gateway_available(gateway.as_deref(), profile.as_deref()),
    );
    if mode == JoinMode::Code {
        if keys.just_pressed(KeyCode::Backspace) || keys.just_pressed(KeyCode::Delete) {
            if entry_backspace(&mut code.text) {
                code.invalid = false;
            }
            return;
        }
        for &key in &CODE_KEYS {
            if keys.just_pressed(key) {
                if let Some(c) = key_to_code_char(key) {
                    if code_push(&mut code.text, c) {
                        code.invalid = false;
                    }
                }
            }
        }
        if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) {
            code.request_submit();
        }
        return;
    }
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

/// Android soft-keyboard bridge: the system IME is shown exactly while the
/// online flow waits on typed input — the Join stage, where either the room
/// code ([`CodeEntry`]) or the direct `IP:port` address ([`JoinEntry`]) is
/// focused. winit shows/hides it via `InputMethodManager` (bevy_winit maps
/// [`bevy::window::Window::ime_enabled`]); soft-keyboard taps on a
/// NativeActivity arrive back as ordinary `KeyboardInput` events, which
/// [`online_entry_input_system`] consumes unchanged, so no IME commit
/// plumbing is needed. Desktop never opts in (a real keyboard is present).
pub fn online_ime_system(
    state: Res<AppState>,
    flow: Res<OnlineFlow>,
    mut windows: Query<&mut bevy::window::Window>,
) {
    let Ok(mut window) = windows.single_mut() else {
        return;
    };
    let want =
        cfg!(target_os = "android") && *state == AppState::Title && flow.stage == OnlineStage::Join;
    if window.ime_enabled != want {
        window.ime_enabled = want;
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
            Has<JoinModeButton>,
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
            mode,
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
                || mode
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
                    mode,
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

    // Enter-key submits ride the same frame as the clicks — the entry that
    // flagged one belongs to the effective mode (Code or IP).
    let current_mode = |w: &World| {
        effective_join_mode(
            w.resource::<OnlineFlow>().join_mode,
            gateway_available(
                w.get_resource::<NetGateway>(),
                w.get_resource::<NetProfile>(),
            ),
        )
    };
    if world.resource_mut::<JoinEntry>().take_submit() {
        let mode = current_mode(world);
        join_submit(world, mode);
    }
    if world.resource_mut::<CodeEntry>().take_submit() {
        let mode = current_mode(world);
        join_submit(world, mode);
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
            // G3: entering Join arms the mode — Code by default whenever the
            // gateway is enabled and the profile allows it, IP otherwise.
            let available = gateway_available(
                world.get_resource::<NetGateway>(),
                world.get_resource::<NetProfile>(),
            );
            world.resource_mut::<OnlineFlow>().join_mode =
                effective_join_mode(JoinMode::Code, available);
            // A fresh panel starts clean: empty code, no stale terminal
            // lookup line lingering from a previous attempt.
            {
                let mut code = world.resource_mut::<CodeEntry>();
                code.text.clear();
                code.invalid = false;
                code.submit = false;
            }
            if let Some(mut gateway) = world.get_resource_mut::<NetGateway>() {
                if gateway.enabled {
                    gateway.guest = GuestLookupState::Idle;
                }
            }
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
        } else if flags.mode {
            // Code ⇄ IP toggle — inert (visibly "gateway off") when Code
            // mode is unavailable.
            if gateway_available(
                world.get_resource::<NetGateway>(),
                world.get_resource::<NetProfile>(),
            ) {
                let next = next_join_mode(world.resource::<OnlineFlow>().join_mode);
                world.resource_mut::<OnlineFlow>().join_mode = next;
                // Reject hints belong to the mode that produced them.
                world.resource_mut::<CodeEntry>().invalid = false;
                world.resource_mut::<JoinEntry>().invalid = false;
            }
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
            let mode = current_mode(world);
            join_submit(world, mode);
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
#[allow(clippy::too_many_arguments)]
pub fn sync_online_labels(
    session: Option<Res<NetSession>>,
    hint: Res<HostHint>,
    entry: Res<JoinEntry>,
    code: Res<CodeEntry>,
    overlay: Res<NetOverlay>,
    upnp: Option<Res<UpnpState>>,
    profile: Option<Res<NetProfile>>,
    flow: Res<OnlineFlow>,
    gateway: Option<Res<NetGateway>>,
    mut labels: Query<
        (
            Has<HostHintText>,
            Has<HostStatusText>,
            Has<UpnpStatusText>,
            Has<RoomStatusText>,
            Has<JoinEntryText>,
            Has<JoinModeText>,
            Has<JoinStatusText>,
            Has<NetErrorText>,
            &mut Text,
        ),
        Or<(
            With<HostHintText>,
            With<HostStatusText>,
            With<UpnpStatusText>,
            With<RoomStatusText>,
            With<JoinEntryText>,
            With<JoinModeText>,
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
    // G3: one line per gateway leg — the Host share line wins the room code
    // (offline reason is the fallback), and the Join echo renders whichever
    // entry the mode has active.
    let available = gateway_available(gateway.as_deref(), profile.as_deref());
    let mode = effective_join_mode(flow.join_mode, available);
    let room_line = gateway
        .as_deref()
        .map_or_else(String::new, |g| host_room_text(&g.host, g.enabled));
    let ip_echo = if entry.invalid {
        format!("{}▌  want IPv4 address:port", entry.text)
    } else {
        format!("{}▌", entry.text)
    };
    let code_echo = if code.invalid {
        format!("{}▌  want 5 room-code chars", code.text)
    } else {
        format!("{}▌", code.text)
    };
    let echo = if mode == JoinMode::Code {
        code_echo
    } else {
        ip_echo
    };
    let mode_line = join_mode_label(mode, available);
    let join_line = join_status_text(mode, gateway.as_deref(), &status);
    let headline = overlay.text.clone().unwrap_or_default();
    for (
        is_hint,
        is_host,
        is_upnp,
        is_room,
        is_join_entry,
        is_mode,
        is_join_status,
        is_error,
        mut text,
    ) in labels.iter_mut()
    {
        let wanted = if is_hint {
            &hint_line
        } else if is_host {
            &host_line
        } else if is_upnp {
            &upnp_line
        } else if is_room {
            &room_line
        } else if is_join_entry {
            &echo
        } else if is_mode {
            &mode_line
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

fn label_node(text: String, size: f32) -> (Text, TextFont, TextColor, Pickable) {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        TextColor::WHITE,
        // Labels never own a click — the button underneath them does
        // (same standardized mechanism as `screens_menu::label_node`).
        Pickable::IGNORE,
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

/// The Join panel's Code ⇄ IP toggle with its dynamic label (G3). Built
/// locally (the panel's label is rewritten every frame) but with exactly
/// the picking shape of [`online_button`].
fn join_mode_button(parent: &mut ChildSpawnerCommands) {
    parent
        .spawn((
            Button,
            JoinModeButton,
            BackgroundColor(BUTTON_BG),
            Node {
                width: Val::Px(260.0),
                height: Val::Px(36.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
        ))
        .with_children(|button| {
            button.spawn((JoinModeText, label_node("Mode: Code".to_string(), 18.0)));
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
            // Full-screen containers are inert — same rule as
            // `screens_menu::add_menu_root` (clicks resolve to visible
            // buttons only; hidden UI is additionally excluded by
            // `sync_hidden_ui_unpickable`).
            Pickable::IGNORE,
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
            root.spawn((RoomStatusText, label_node(String::new(), 20.0)));
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
            join_mode_button(root);
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

/// Production gateway arming (G3): the user-facing default is **on**. The
/// endpoint comes from `TETRIS_GATEWAY` via [`gateway::parse_gateway_env`]
/// (unset → [`gateway::DEFAULT_GATEWAY_ENDPOINT`], explicitly empty → off),
/// and [`NetProfile::gateway_enabled`] is the persisted kill switch (a
/// saved `false` wins even over an explicit endpoint).
///
/// **Never mounted in test builds**: the headless suites' safety rests on
/// G2's policy (`TETRIS_GATEWAY` unset ⇒ disabled, see the gateway module
/// docs), and tests arm the gateway explicitly via
/// [`NetGateway::test_with_endpoint`]. Mounting this under `cfg(test)` would
/// make the existing `Listening` tests send live `*R` UDP to production.
#[cfg(not(test))]
fn gateway_compose_system(
    profile: Option<Res<NetProfile>>,
    mut gateway: Option<ResMut<NetGateway>>,
) {
    let Some(gateway) = gateway.as_mut() else {
        return;
    };
    let endpoint = gateway::parse_gateway_env(std::env::var(gateway::GATEWAY_ENV).ok().as_deref());
    let profile_on = profile.is_none_or(|profile| profile.gateway_enabled);
    gateway.enabled = profile_on && endpoint.is_some();
    gateway.endpoint = endpoint.unwrap_or_default();
}

impl Plugin for OnlineUiPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<OnlineUiMounted>() {
            return;
        }
        app.insert_resource(OnlineUiMounted)
            .init_resource::<OnlineFlow>()
            .init_resource::<JoinEntry>()
            .init_resource::<CodeEntry>()
            .init_resource::<HostHint>()
            .init_resource::<NetOverlay>()
            .init_resource::<LeaveConfirm>()
            .init_resource::<PressedLatch>()
            .init_resource::<NetProfile>();
        // The always-on gateway default is a product decision (G2 module
        // docs); it lands here, compiled out of test binaries.
        #[cfg(not(test))]
        app.add_systems(Startup, gateway_compose_system);
        // WAN play addendum: the UPnP driver lives with the Host screen that
        // renders its status line and owns the `U` toggle; mounting here
        // (not in NetPlugin) keeps `session.rs` untouched and lets tests
        // swap the runner before any Host click. Mounted on Android too:
        // SSDP discovery can't receive without a WifiManager multicast lock
        // (Java glue this native-activity build lacks), and the driver's
        // `Failed` state — rendered as "unavailable" on the Host screen —
        // is what the Host UI already absorbs; the `UpnpState`/`UpnpDriver`
        // resources must exist for `start_mapping`.
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
                    online_ime_system,
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

    // ---- G3: room-code entry (join-by-code) — pure widget logic ----

    #[test]
    fn code_charset_accepts_only_the_gateway_alphabet() {
        assert_eq!(
            ROOM_CODE_ALPHABET, "ABCDEFGHJKMNPQRSTUVWXYZ23456789",
            "alphabet is the wire crate's confusion-free set"
        );
        for c in ROOM_CODE_ALPHABET.chars() {
            assert!(code_char(c), "{c} must be accepted");
        }
        for c in ROOM_CODE_ALPHABET.to_lowercase().chars() {
            assert!(code_char(c), "lowercase {c} is a member (normalizes)");
        }
        for c in "ILO01-. _:+!?".chars() {
            assert!(!code_char(c), "{c} must be rejected");
        }
    }

    #[test]
    fn code_push_normalizes_and_rejects_the_sixth_char() {
        let mut text = String::new();
        assert!(code_push(&mut text, 'a'), "lowercase normalizes on push");
        assert_eq!(text, "A");
        for c in "bcd".chars() {
            assert!(code_push(&mut text, c));
        }
        assert_eq!(text, "ABCD");
        assert!(
            !code_push(&mut text, 'I'),
            "ambiguous glyph outside the alphabet is a no-op"
        );
        assert_eq!(text, "ABCD");
        assert!(code_push(&mut text, 'E'));
        assert_eq!(text, "ABCDE");
        assert!(!code_push(&mut text, 'F'), "6th char is rejected");
        assert_eq!(text, "ABCDE");
        assert!(entry_backspace(&mut text));
        assert_eq!(text, "ABCD");
        assert!(!entry_backspace(&mut String::new()));
    }

    #[test]
    fn key_to_code_char_maps_only_alphabet_keys() {
        use KeyCode::*;
        assert_eq!(key_to_code_char(KeyA), Some('A'));
        assert_eq!(key_to_code_char(KeyZ), Some('Z'));
        assert_eq!(key_to_code_char(KeyH), Some('H'));
        assert_eq!(key_to_code_char(KeyI), None, "I is outside the alphabet");
        assert_eq!(key_to_code_char(KeyL), None);
        assert_eq!(key_to_code_char(KeyO), None);
        assert_eq!(key_to_code_char(Digit2), Some('2'));
        assert_eq!(key_to_code_char(Digit9), Some('9'));
        assert_eq!(key_to_code_char(Numpad2), Some('2'));
        assert_eq!(key_to_code_char(Numpad9), Some('9'));
        assert_eq!(key_to_code_char(Digit0), None);
        assert_eq!(key_to_code_char(Digit1), None);
        assert_eq!(key_to_code_char(Period), None);
        assert_eq!(key_to_code_char(Space), None);
    }

    #[test]
    fn parse_room_code_accepts_exactly_five_alphabet_chars() {
        assert_eq!(parse_room_code("ABCDE"), Some(*b"ABCDE"));
        assert_eq!(
            parse_room_code("abcde"),
            Some(*b"ABCDE"),
            "lowercase normalizes"
        );
        assert_eq!(parse_room_code("AB234"), Some(*b"AB234"));
        assert_eq!(parse_room_code("ABCD"), None, "4 chars");
        assert_eq!(parse_room_code("ABCDEF"), None, "6 chars");
        assert_eq!(parse_room_code("ABCDI"), None, "I is not a member");
        assert_eq!(parse_room_code("ABCD0"), None, "0 is not a member");
        assert_eq!(parse_room_code(""), None);
    }

    #[test]
    fn code_entry_submit_is_a_one_shot_latch() {
        let mut entry = CodeEntry::default();
        assert!(!entry.take_submit());
        entry.request_submit();
        assert!(entry.take_submit());
        assert!(!entry.take_submit(), "submit is consumed once");
    }

    // ---- G3: status copy (Host share line, guest lookup lines) ----

    #[test]
    fn lookup_status_text_exact_copy() {
        use GuestLookupState as G;
        assert_eq!(lookup_status_text(&G::Idle), "");
        assert_eq!(lookup_status_text(&G::Resolving(*b"ABCDE")), "resolving…");
        assert_eq!(
            lookup_status_text(&G::LookingUp(*b"ABCDE")),
            "joining room…"
        );
        assert_eq!(lookup_status_text(&G::NotFound(*b"ABCDE")), "no such room");
        assert_eq!(lookup_status_text(&G::Busy(*b"ABCDE")), "match full");
        assert_eq!(
            lookup_status_text(&G::SlotExhausted(*b"ABCDE")),
            "gateway full — retry later"
        );
        assert_eq!(
            lookup_status_text(&G::Timeout),
            "gateway offline — check connection or join by IP"
        );
        assert_eq!(
            lookup_status_text(&G::GatewayUnreachable("dns died".into())),
            "gateway offline — check connection or join by IP"
        );
        assert_eq!(
            lookup_status_text(&G::Found {
                code: *b"ABCDE",
                addr: "1.2.3.4:5".parse().expect("addr"),
            }),
            "",
            "Found hands the line over to the session status"
        );
    }

    #[test]
    fn host_room_text_ships_the_share_line() {
        assert_eq!(
            host_room_text(&HostRoomState::Announced(*b"ABCDE"), true),
            "Room ABCDE — share with a friend"
        );
        let announcing = host_room_text(&HostRoomState::Advertising(*b"ABCDE"), true);
        assert!(announcing.contains("ABCDE"), "{announcing}");
        assert_eq!(
            host_room_text(&HostRoomState::Offline("no route".into()), true),
            "gateway offline — no route"
        );
        assert_eq!(host_room_text(&HostRoomState::Idle, true), "");
        assert_eq!(
            host_room_text(&HostRoomState::Announced(*b"ABCDE"), false),
            "",
            "a disabled gateway never shows a room line"
        );
    }

    // ---- G3: join mode (Code ⇄ IP) gating ----

    #[test]
    fn join_mode_defaults_to_code() {
        assert_eq!(JoinMode::default(), JoinMode::Code);
        assert_eq!(next_join_mode(JoinMode::Code), JoinMode::Ip);
        assert_eq!(next_join_mode(JoinMode::Ip), JoinMode::Code);
    }

    #[test]
    fn code_mode_needs_an_enabled_gateway_and_profile() {
        assert!(join_mode_available(true, true));
        assert!(
            !join_mode_available(false, true),
            "gateway off hides Code mode"
        );
        assert!(
            !join_mode_available(true, false),
            "explicitly disabled profile keeps Code away"
        );
        assert!(!join_mode_available(false, false));
        assert_eq!(
            effective_join_mode(JoinMode::Code, false),
            JoinMode::Ip,
            "unavailable Code falls back to IP"
        );
        assert_eq!(effective_join_mode(JoinMode::Code, true), JoinMode::Code);
    }

    #[test]
    fn join_mode_label_exact_copy() {
        assert_eq!(join_mode_label(JoinMode::Code, true), "Mode: Code");
        assert_eq!(join_mode_label(JoinMode::Ip, true), "Mode: IP");
        assert_eq!(
            join_mode_label(JoinMode::Ip, false),
            "gateway off — join by IP",
            "the toggle looks disabled when the gateway is off"
        );
    }

    #[test]
    fn join_status_text_prefers_lookup_copy_in_code_mode() {
        use GuestLookupState as G;
        let mut gw = NetGateway::disabled();
        gw.enabled = true;
        gw.guest = G::LookingUp(*b"ABCDE");
        assert_eq!(
            join_status_text(JoinMode::Code, Some(&gw), &NetStatus::Idle),
            "joining room…"
        );
        gw.guest = G::Idle;
        assert_eq!(
            join_status_text(JoinMode::Code, Some(&gw), &NetStatus::Connecting),
            "connecting…",
            "no lookup in flight → the session line takes over"
        );
        assert_eq!(
            join_status_text(JoinMode::Ip, Some(&gw), &NetStatus::Listening),
            status_text(NetRole::Guest, &NetStatus::Listening),
            "IP mode never shows lookup copy"
        );
        assert_eq!(
            join_status_text(JoinMode::Code, None, &NetStatus::Idle),
            "",
            "no gateway resource → session line only"
        );
    }

    // ---- status/overlay copy ----

    #[test]
    fn found_room_that_cannot_connect_shows_the_punch_failure_copy() {
        // Field fix: `*F` succeeded (sticky `Found`) but the netcode connect
        // through the relay timed out — the status line names the actual
        // suspect (host router) instead of the generic loss line.
        use GuestLookupState as G;
        let mut gw = NetGateway::disabled();
        gw.enabled = true;
        gw.guest = G::Found {
            code: *b"ABCDE",
            addr: "127.0.0.1:9".parse().unwrap(),
        };
        assert_eq!(
            join_status_text(
                JoinMode::Code,
                Some(&gw),
                &NetStatus::Lost(NetLossReason::Timeout)
            ),
            "host unreachable — ask host to enable UPnP or port-forward UDP 27015"
        );
        // Mid-connect the generic lines are untouched…
        assert_eq!(
            join_status_text(JoinMode::Code, Some(&gw), &NetStatus::Connecting),
            status_text(NetRole::Guest, &NetStatus::Connecting)
        );
        // …and neither other loss reasons nor IP mode get the new copy.
        assert_eq!(
            join_status_text(
                JoinMode::Code,
                Some(&gw),
                &NetStatus::Lost(NetLossReason::Denied)
            ),
            status_text(NetRole::Guest, &NetStatus::Lost(NetLossReason::Denied))
        );
        assert_eq!(
            join_status_text(
                JoinMode::Ip,
                Some(&gw),
                &NetStatus::Lost(NetLossReason::Timeout)
            ),
            status_text(NetRole::Guest, &NetStatus::Lost(NetLossReason::Timeout)),
            "IP-mode loss keeps the generic copy — no relay was involved"
        );
        // Never-Found room paths (lookup still authoritative) unchanged:
        gw.guest = G::Timeout;
        assert_eq!(
            join_status_text(
                JoinMode::Code,
                Some(&gw),
                &NetStatus::Lost(NetLossReason::Timeout)
            ),
            "gateway offline — check connection or join by IP"
        );
    }

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
        // Production `main.rs` initializes `Settings` ahead of the plugin
        // tree; G3 flow tests poll across real seconds, which finally lets
        // the fixed-step `gameplay_input_system` run in this tree, so the
        // resource it reads has to be present here too (same convention as
        // the N6 harness `peer_app`).
        app.init_resource::<crate::state::Settings>();
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

    // ---- G3: room-code flow (headless App over the real in-process relay) --
    //
    // Same fixture the G2 suite uses: the REAL `netplay_gateway::Gateway`
    // on loopback UDP (legs on distinct loopback IPs per the relay's
    // per-IP leg model). The Listening edge is set through the session
    // resource directly (mirroring the UPnP host-screen tests): `net_host`'s
    // fixed default port could land on `BindFailed` on the dev box, which
    // the `Listening | BindFailed` tolerance convention would accept — yet
    // the gateway must actually register for the share line to exist.

    use crate::core_bridge::net::gateway::testutil::{
        raw_socket, recv_ack_then_vport, recv_frame, reg, spawn_real_gateway,
    };
    use netplay_gateway::wire::Frame;

    fn online_app_with_gateway(endpoint: &str) -> App {
        let mut app = online_app();
        *app.world_mut().resource_mut::<NetGateway>() = NetGateway::test_with_endpoint(endpoint);
        app
    }

    fn room_label(app: &mut App) -> String {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Text, With<RoomStatusText>>();
        query.single(world).expect("room label").0.clone()
    }

    fn mode_label(app: &mut App) -> String {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Text, With<JoinModeText>>();
        query.single(world).expect("mode label").0.clone()
    }

    fn join_status_label(app: &mut App) -> String {
        let world = app.world_mut();
        let mut query = world.query_filtered::<&Text, With<JoinStatusText>>();
        query.single(world).expect("join status label").0.clone()
    }

    fn code_text(app: &App) -> String {
        app.world().resource::<CodeEntry>().text.clone()
    }

    fn enter_join(app: &mut App) {
        enter_online(app);
        click_button(app, mode_root, |world, e| {
            world.get::<OnlineJoinButton>(e).is_some()
        });
    }

    fn type_code(app: &mut App, text: &str) {
        for c in text.chars() {
            press_key(app, key_for_code_char(c));
        }
    }

    fn key_for_code_char(c: char) -> KeyCode {
        use KeyCode::*;
        match c.to_ascii_uppercase() {
            'A' => KeyA,
            'B' => KeyB,
            'C' => KeyC,
            'D' => KeyD,
            'E' => KeyE,
            'F' => KeyF,
            'G' => KeyG,
            'H' => KeyH,
            'I' => KeyI,
            'J' => KeyJ,
            'K' => KeyK,
            'L' => KeyL,
            'M' => KeyM,
            'N' => KeyN,
            'P' => KeyP,
            'Q' => KeyQ,
            'R' => KeyR,
            'S' => KeyS,
            'T' => KeyT,
            'U' => KeyU,
            'V' => KeyV,
            'W' => KeyW,
            'X' => KeyX,
            'Y' => KeyY,
            'Z' => KeyZ,
            '2' => Digit2,
            '3' => Digit3,
            '4' => Digit4,
            '5' => Digit5,
            '6' => Digit6,
            '7' => Digit7,
            '8' => Digit8,
            '9' => Digit9,
            other => panic!("not a room-code character: {other}"),
        }
    }

    #[test]
    fn host_screen_shares_the_room_code_when_the_gateway_announces() {
        let rg = spawn_real_gateway(4);
        let mut app = online_app_with_gateway(&rg.ctrl.to_string());
        enter_online(&mut app);
        click_button(&mut app, mode_root, |world, e| {
            world.get::<OnlineHostButton>(e).is_some()
        });
        assert_eq!(flow(&app).stage, OnlineStage::Host);
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some("0.0.0.0:27015".parse().expect("addr"));
        }
        let mut code = None;
        for _ in 0..100 {
            app.update();
            if let HostRoomState::Announced(c) = app.world().resource::<NetGateway>().host {
                code = Some(c);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let code = code.expect("the real gateway must ack *R with *A");
        assert_eq!(
            room_label(&mut app),
            format!("Room {} — share with a friend", format_code(&code)),
            "the share line carries the announced room code"
        );

        // Esc un-listens (existing N5 edge) → the driver's teardown edge
        // sends *D → the room is released for good (*G → *E on the relay).
        press_key(&mut app, KeyCode::Escape);
        assert_eq!(status(&app), NetStatus::Idle, "Esc on Listening un-listens");
        let guest = raw_socket("127.0.0.2");
        let mut released = false;
        for _ in 0..40 {
            app.update();
            guest
                .send_to(&wire::encode(&Frame::Lookup { code }), rg.ctrl)
                .expect("lookup send");
            if matches!(recv_frame(&guest), Some(Frame::NotFound { .. })) {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(released, "teardown must release the room (*D ⇒ later *E)");
        assert!(
            matches!(
                app.world().resource::<NetGateway>().host,
                HostRoomState::Idle
            ),
            "room state returns to Idle"
        );
        assert!(room_label(&mut app).is_empty(), "the share line clears");
    }

    #[test]
    fn join_defaults_to_code_mode_when_gateway_enabled() {
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        assert_eq!(
            flow(&app).join_mode,
            JoinMode::Code,
            "Code is the default whenever the gateway is enabled"
        );
        assert_eq!(mode_label(&mut app), "Mode: Code");

        // A profile that explicitly disabled the gateway gets IP, not Code.
        let mut off = online_app_with_gateway("127.0.0.1:9");
        off.world_mut().resource_mut::<NetProfile>().gateway_enabled = false;
        enter_join(&mut off);
        assert_eq!(flow(&off).join_mode, JoinMode::Ip);
        assert_eq!(mode_label(&mut off), "gateway off — join by IP");
    }

    #[test]
    fn gateway_disabled_hides_code_mode_and_holds_nothing() {
        let mut app = online_app(); // env unset ⇒ disabled (G2 headless policy)
        enter_join(&mut app);
        assert_eq!(flow(&app).join_mode, JoinMode::Ip, "no gateway ⇒ IP only");
        assert_eq!(mode_label(&mut app), "gateway off — join by IP");
        click_button(&mut app, join_root, |world, e| {
            world.get::<JoinModeButton>(e).is_some()
        });
        assert_eq!(flow(&app).join_mode, JoinMode::Ip, "the toggle is inert");
        press_key(&mut app, KeyCode::KeyA);
        assert!(
            app.world().resource::<CodeEntry>().text.is_empty(),
            "Code mode is unreachable, so keystrokes never reach it"
        );
        assert!(
            app.world().resource::<JoinEntry>().text.is_empty(),
            "letters are outside the IP charset too"
        );
        let gw = app.world().resource::<NetGateway>();
        assert!(!gw.enabled);
        assert_eq!(gw.host, HostRoomState::Idle, "no room was ever registered");
        assert_eq!(gw.guest, GuestLookupState::Idle);
    }

    #[test]
    fn join_mode_toggle_switches_code_and_ip() {
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        press_key(&mut app, KeyCode::KeyA);
        press_key(&mut app, KeyCode::KeyB);
        assert_eq!(code_text(&app), "AB");
        assert_eq!(app.world().resource::<JoinEntry>().text, "");

        click_button(&mut app, join_root, |world, e| {
            world.get::<JoinModeButton>(e).is_some()
        });
        assert_eq!(flow(&app).join_mode, JoinMode::Ip);
        assert_eq!(mode_label(&mut app), "Mode: IP");
        press_key(&mut app, KeyCode::Digit5);
        assert_eq!(
            app.world().resource::<JoinEntry>().text,
            "5",
            "the IP entry owns keystrokes in IP mode"
        );
        assert_eq!(code_text(&app), "AB", "each entry keeps its own text");

        click_button(&mut app, join_root, |world, e| {
            world.get::<JoinModeButton>(e).is_some()
        });
        assert_eq!(flow(&app).join_mode, JoinMode::Code);
        assert_eq!(code_text(&app), "AB");
    }

    #[test]
    fn code_entry_types_backspaces_and_rejects_the_sixth_char() {
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        type_code(&mut app, "ABCDE");
        assert_eq!(code_text(&app), "ABCDE");
        press_key(&mut app, KeyCode::KeyF);
        assert_eq!(code_text(&app), "ABCDE", "the 6th char is rejected");
        press_key(&mut app, KeyCode::Backspace);
        assert_eq!(code_text(&app), "ABCD");
        press_key(&mut app, KeyCode::KeyI);
        assert_eq!(code_text(&app), "ABCD", "I is outside the alphabet");
        press_key(&mut app, KeyCode::KeyE);
        assert_eq!(code_text(&app), "ABCDE");

        press_key(&mut app, KeyCode::Enter);
        let mut looking = false;
        for _ in 0..10 {
            if matches!(
                app.world().resource::<NetGateway>().guest,
                GuestLookupState::LookingUp(_)
            ) {
                looking = true;
                break;
            }
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(looking, "Enter starts the gateway lookup (*G)");
        assert_eq!(join_status_label(&mut app), "joining room…");
    }

    #[test]
    fn short_room_code_is_flagged_not_submitted() {
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        press_key(&mut app, KeyCode::KeyA);
        press_key(&mut app, KeyCode::Enter);
        assert!(
            app.world().resource::<CodeEntry>().invalid,
            "a short code is flagged"
        );
        assert_eq!(
            app.world().resource::<NetGateway>().guest,
            GuestLookupState::Idle,
            "nothing is dialed"
        );
        press_key(&mut app, KeyCode::KeyB);
        assert!(
            !app.world().resource::<CodeEntry>().invalid,
            "editing clears the flag"
        );
    }

    #[test]
    fn lookup_states_surface_as_join_status_lines() {
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        let cases = [
            (GuestLookupState::Resolving(*b"ABCDE"), "resolving…"),
            (GuestLookupState::LookingUp(*b"ABCDE"), "joining room…"),
            (GuestLookupState::NotFound(*b"ABCDE"), "no such room"),
            (GuestLookupState::Busy(*b"ABCDE"), "match full"),
            (
                GuestLookupState::SlotExhausted(*b"ABCDE"),
                "gateway full — retry later",
            ),
            (
                GuestLookupState::Timeout,
                "gateway offline — check connection or join by IP",
            ),
            (
                GuestLookupState::GatewayUnreachable("dns died".into()),
                "gateway offline — check connection or join by IP",
            ),
        ];
        for (state, want) in cases {
            app.world_mut().resource_mut::<NetGateway>().guest = state.clone();
            app.update();
            assert_eq!(join_status_label(&mut app), want, "{state:?}");
        }
    }

    #[test]
    fn unknown_room_code_surfaces_no_such_room() {
        let rg = spawn_real_gateway(4);
        let mut app = online_app_with_gateway(&rg.ctrl.to_string());
        enter_join(&mut app);
        type_code(&mut app, "ZZZZZ");
        press_key(&mut app, KeyCode::Enter);
        let mut surfaced = false;
        for _ in 0..100 {
            app.update();
            if join_status_label(&mut app) == "no such room" {
                surfaced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(surfaced, "the *E reply surfaces as \"no such room\"");
    }

    #[test]
    fn room_code_found_feeds_the_existing_join_fsm() {
        let rg = spawn_real_gateway(4);
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        assert!(matches!(recv_frame(&host), Some(Frame::Ack { .. })));

        let mut app = online_app_with_gateway(&rg.ctrl.to_string());
        enter_join(&mut app);
        type_code(&mut app, "ABCDE");
        press_key(&mut app, KeyCode::Enter);
        let mut handed_off = false;
        for _ in 0..100 {
            app.update();
            // The *F handoff drives the existing net_join (port-0 client
            // bind; the Listening | BindFailed tolerance convention applies
            // to binds, so accept the same pair here).
            if matches!(
                status(&app),
                NetStatus::Connecting | NetStatus::BindFailed(_)
            ) {
                handed_off = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            handed_off,
            "Found must hand off to net_join, got {:?}",
            status(&app)
        );
        if status(&app) == NetStatus::Connecting {
            assert_eq!(
                join_status_label(&mut app),
                "connecting…",
                "after *F the existing session FSM owns the line"
            );
        }
        assert_eq!(
            app.world().resource::<NetProfile>().last_join_addr,
            "",
            "the code path never overwrites the remembered IP address"
        );
    }

    #[test]
    fn paired_room_surfaces_match_full() {
        let rg = spawn_real_gateway(4);
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        // Consume the field-fix *V pair and take the relay port from it.
        let vport = recv_ack_then_vport(&host);
        // Pin the guest slot with first data from a third loopback IP.
        let guest = raw_socket("127.0.0.3");
        guest
            .send_to(b"\x00pin", SocketAddr::from(([127, 0, 0, 1], vport)))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));

        let mut app = online_app_with_gateway(&rg.ctrl.to_string());
        enter_join(&mut app);
        type_code(&mut app, "ABCDE");
        press_key(&mut app, KeyCode::Enter);
        let mut surfaced = false;
        for _ in 0..100 {
            app.update();
            if join_status_label(&mut app) == "match full" {
                surfaced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(surfaced, "the *B reply surfaces as \"match full\"");
    }

    /// The field symptom end-to-end: pairing succeeds (real relay answers
    /// `*G` with `*F`), but the host behind the relay is a dead announce —
    /// the connect through the relay goes nowhere. The status line must
    /// carry the actionable room-path copy, not the generic loss line, and
    /// the `*F`-sticky `Found` state is what selects it. (The watchdog is
    /// fast-forwarded through `joining_since` like the session tests do —
    /// no 10 s sleep.)
    #[test]
    fn found_room_that_cannot_connect_surfaces_the_punch_copy() {
        use crate::core_bridge::net::gateway::GuestLookupState;
        use crate::core_bridge::net::session::{NetSession, JOIN_TIMEOUT};

        let rg = spawn_real_gateway(4);
        // A "host" whose announced game port nobody binds: the relay pairs
        // the room, then every forwarded packet is dropped.
        let dead = raw_socket("127.0.0.2")
            .local_addr()
            .expect("probe bind")
            .port();
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", dead), rg.ctrl).unwrap();
        let _vport = recv_ack_then_vport(&host);

        let mut app = online_app_with_gateway(&rg.ctrl.to_string());
        enter_join(&mut app);
        type_code(&mut app, "ABCDE");
        press_key(&mut app, KeyCode::Enter);
        let mut connecting = false;
        for _ in 0..150 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(20));
            if join_status_label(&mut app) == "connecting…" {
                connecting = true;
                break;
            }
        }
        assert!(connecting, "*F must hand off to the netcode connect");
        assert!(
            matches!(
                app.world()
                    .resource::<crate::core_bridge::net::gateway::NetGateway>()
                    .guest,
                GuestLookupState::Found { .. }
            ),
            "`Found` must stay sticky through the connect attempt"
        );

        app.world_mut().resource_mut::<NetSession>().joining_since =
            Some(std::time::Instant::now() - JOIN_TIMEOUT);
        let mut surfaced = false;
        for _ in 0..10 {
            app.update();
            if join_status_label(&mut app)
                == "host unreachable — ask host to enable UPnP or port-forward UDP 27015"
            {
                surfaced = true;
                break;
            }
        }
        assert!(
            surfaced,
            "the watchdog loss must show the room-path punch copy"
        );
    }

    #[test]
    fn gateway_silence_surfaces_the_offline_line() {
        // No relay behind the endpoint: the *G goes unanswered and the G2
        // lookup timeout surfaces the offline copy (one 3 s wait — the only
        // real-time G3 flow test).
        let mut app = online_app_with_gateway("127.0.0.1:9");
        enter_join(&mut app);
        type_code(&mut app, "ABCDE");
        press_key(&mut app, KeyCode::Enter);
        let mut offline = false;
        for _ in 0..250 {
            app.update();
            if join_status_label(&mut app) == "gateway offline — check connection or join by IP" {
                offline = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(offline, "a silent gateway must surface the offline line");
    }
}
