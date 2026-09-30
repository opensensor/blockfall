//! Pure gateway room state machine (gateway-plan.md §"Room lifecycle").
//!
//! No sockets, no time: every entry point takes a [`VClock`] the caller
//! supplies, so GC windows, rate buckets, and pairing transitions are all
//! table-testable. `main.rs` owns the sockets and feeds this machine; the
//! replies/forwards it returns are addressed and ready to `send_to`.
//!
//! Model notes (documented spec refinements):
//! - `on_packet` takes the **destination port** the datagram arrived on.
//!   Each room owns its virtual data port exclusively, so a data packet is
//!   attributed to its room by that port (rooms are effectively keyed by
//!   code for control and by code+port for data).
//! - Virtual data ports are bound at `*R` time, not lazily on first data:
//!   the bind window equals the room lifetime either way, and binding at
//!   register gives `main.rs` a pollable socket before any guest exists.
//! - Both GC windows key on *host* activity (`*R` or host data, 15 s); the
//!   paired/unpaired distinction only shows up through the separate 10 s
//!   guest-slot release.
//! - Guest pinning is by full `SocketAddr` (first game-data source), while
//!   `*G` idempotency/busy is per-IP (plan wording).
//! - The gateway is IPv4-only (the `*F` payload carries 4-byte IPs, as
//!   planned); IPv6 sources are dropped.
//!
//! # Host-side NAT punch (field fix, 2026-09-30)
//!
//! The relay used to send guest traffic to `(host_ip, game_port)` as
//! advertised in `*R` — but a UDP server is silent until a client arrives,
//! so the host's router never had an inbound mapping for that port and
//! dropped the forwarded handshake (the exact field bug: room codes paired
//! fine, guests timed out). The fix lives on both sides of the wire: the
//! gateway tells the host its virtual data port via `*V` (sent immediately
//! after every `*A`), the host punches one datagram from its game socket,
//! and this machine routes guest data to the **observed** source:
//!
//! - Each room keeps `host_data_addr` — initialized `(host_ip, game_port)`
//!   from `*R`, then replaced by the source of every host-side data packet.
//! - A data-plane packet is host-side when its **source IP equals the
//!   room's `host_ip`** (IP-level attribution, not per-socket): under NAT the
//!   public port of the game socket is *not* the advertised `game_port`, so
//!   the port carries no information — only the IP (the room was keyed by
//!   the host's observed control-leg IP) does. A host-side packet refreshes
//!   the punch and the room's liveness; it never pins the guest slot and is
//!   never forwarded *as* guest data (it forwards *to* the pinned guest).
//! - Guest pinning is unchanged: first data from a source IP ≠ `host_ip`.
//! - Keepalive `*R` frames refresh `host_data_addr` **only until the first
//!   punch** — after that the observed address is authoritative (a keepalive
//!   arrives on the host's control socket, whose public port is unrelated to
//!   the game socket's mapping).
//! - **Aging window**: the punch mapping dies with the NAT's UDP timeout
//!   (commonly ~30–120 s of silence). A long-idle waiting room therefore
//!   relies on the game's auto-UPnP (which maps the game port explicitly and
//!   renews its own lease) or a manual forward — punch and UPnP are
//!   complementary; see the same note in the game's `gateway.rs` driver.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::wire::{self, Frame, RoomCode};

/// Monotonic virtual timestamp: milliseconds since gateway start. `main.rs`
/// derives it from `Instant::elapsed()`; tests fabricate it freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VClock(pub u64);

impl VClock {
    /// Milliseconds since the virtual epoch.
    #[must_use]
    pub fn millis(self) -> u64 {
        self.0
    }
}

/// Allocation side effects the host (binary or test) must mirror. Closed
/// events from explicit `*D` teardown surface on the next [`Gateway::on_tick`]
/// (the allocator itself is called immediately).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortEvent {
    /// The data port of an expired/torn-down room is closed; its slot is
    /// reusable.
    Closed(u16),
}

/// Virtual data-port allocator. The real one (in `main.rs`) binds and owns
/// nonblocking `UdpSocket`s; tests use a fake.
pub trait PortAllocator {
    /// Binds/reserves `port`; `Err` means unavailable (treated as slot
    /// exhaustion → `*S`).
    // Signature is plan-mandated: callers never branch on the reason.
    #[allow(clippy::result_unit_err)]
    fn bind(&mut self, port: u16) -> Result<(), ()>;
    /// Releases `port` (idempotent for already-closed ports).
    fn close(&mut self, port: u16);
}

/// Tunables; the defaults are the plan's normative values.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// UDP port control frames arrive on (`*` frames aimed at any other
    /// local port are dropped).
    pub control_port: u16,
    /// First virtual data port (default `27017`).
    pub data_port_start: u16,
    /// Number of slots (default `983` → range `27017..=27999`).
    pub data_ports: u16,
    /// Unpaired room expires this long after last host activity.
    pub unpaired_idle: Duration,
    /// Pinned guest slot releases this long after last guest data.
    pub guest_idle: Duration,
    /// Paired room expires fully this long after last host activity.
    pub paired_idle: Duration,
    /// Per-source-IP control-frame burst allowance.
    pub rate_burst: u32,
    /// Window over which the full burst refills (2 tokens/s at defaults).
    pub rate_refill: Duration,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            control_port: 27016,
            data_port_start: 27017,
            data_ports: 983,
            unpaired_idle: Duration::from_secs(15),
            guest_idle: Duration::from_secs(10),
            paired_idle: Duration::from_secs(15),
            rate_burst: 10,
            rate_refill: Duration::from_secs(5),
        }
    }
}

/// Per-source-IP control-frame token bucket: capacity `rate_burst`,
/// refilling one token per `rate_refill / rate_burst` ms (2/s by default).
/// Refill is floor-tracked in `last_refill`, so sustained 2 s keepalives
/// never deplete the bucket.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: u32,
    last_refill: VClock,
}

/// Idle-drop window for stale buckets: a source IP unseen this long loses
/// its bucket (bounds the map against spoofed sources).
const BUCKET_GC: u64 = 60_000;

#[derive(Clone, Debug)]
struct Room {
    code: RoomCode,
    host_ip: Ipv4Addr,
    game_port: u16,
    /// Where guest-bound data goes: initialized `(host_ip, game_port)` from
    /// `*R`, then re-pointed by every host-side packet on the data port
    /// (the NAT punch — the public port of the game socket is not the
    /// advertised `game_port` under port-preserving-NAT-off routers).
    host_data_addr: SocketAddr,
    /// A host data packet has been observed on the data port. Once set,
    /// keepalive `*R` frames stop refreshing `host_data_addr` (they arrive
    /// from the control socket, not the game socket).
    punched: bool,
    vport: u16,
    /// Last host activity (`*R` or host data) — drives both GC windows.
    last_host: VClock,
    guest: Option<Guest>,
}

#[derive(Clone, Copy, Debug)]
struct Guest {
    addr: SocketAddr,
    last_data: VClock,
}

/// The pure relay. Rooms live in a slot-indexed vec (index `i` ↔
/// `data_port_start + i`), which doubles as the slot allocator and makes
/// data-plane attribution (destination port → room) O(1).
#[derive(Debug)]
pub struct Gateway<Alloc: PortAllocator> {
    config: GatewayConfig,
    alloc: Alloc,
    slots: Vec<Option<Room>>,
    buckets: HashMap<IpAddr, Bucket>,
    pending_events: Vec<PortEvent>,
    rate_limited: u64,
}

impl<Alloc: PortAllocator> Gateway<Alloc> {
    /// Builds the machine; preallocates the (empty) slot table.
    #[must_use]
    pub fn new(config: GatewayConfig, alloc: Alloc) -> Self {
        let slots = (0..u32::from(config.data_ports))
            .map(|_| None)
            .collect::<Vec<Option<Room>>>();
        Self {
            config,
            alloc,
            slots,
            buckets: HashMap::new(),
            pending_events: Vec::new(),
            rate_limited: 0,
        }
    }

    /// Live room count (for the periodic log line).
    #[must_use]
    pub fn live_rooms(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// Monotonic count of control frames dropped by the rate limiter
    /// (drives the binary's periodic log summary).
    #[must_use]
    pub fn rate_limited(&self) -> u64 {
        self.rate_limited
    }

    /// Borrow the allocator (tests inspect it; the binary reads its own
    /// socket table through its concrete type).
    #[must_use]
    pub fn allocator(&self) -> &Alloc {
        &self.alloc
    }

    /// Feeds one received datagram: `dst_port` is the local UDP port it
    /// arrived on (control or a virtual data port), `src` the observed
    /// source. Returns datagrams to send, already addressed: control
    /// replies back to `src`, data forwards to the pinned peer.
    /// Everything malformed, rate-limited, or unattributable is silently
    /// dropped.
    #[must_use]
    pub fn on_packet(
        &mut self,
        dst_port: u16,
        src: SocketAddr,
        bytes: &[u8],
        now: VClock,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        if !wire::is_control(bytes) {
            return self.on_data(dst_port, src, bytes, now);
        }
        // Control traffic belongs to the control port only; data never
        // arrives there and control never belongs on a data port.
        if dst_port != self.config.control_port {
            return Vec::new();
        }
        let IpAddr::V4(src_ip) = src.ip() else {
            return Vec::new(); // IPv4-only relay (see module docs)
        };
        if !self.allow(src.ip(), now) {
            return Vec::new(); // rate limited: silent drop
        }
        match wire::decode(bytes) {
            Ok(Frame::Register { code, game_port }) => {
                self.register(code, src, src_ip, game_port, now)
            }
            Ok(Frame::Lookup { code }) => self.lookup(code, src),
            // *D is best-effort: no ack, keeps G2's reply classification simple.
            Ok(Frame::Release { code }) => {
                self.release(code, src_ip);
                Vec::new()
            }
            // Malformed frames and server→client types from a client: drop.
            Err(_) => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Advances GC: expires rooms past the host-idle window, releases
    /// guest slots past the guest-idle window, closes their ports. Returns
    /// port-close events, including any from explicit `*D` teardowns since
    /// the last tick.
    #[must_use]
    pub fn on_tick(&mut self, now: VClock) -> Vec<PortEvent> {
        let mut events = std::mem::take(&mut self.pending_events);
        for slot in self.slots.iter_mut() {
            let Some(room) = slot.as_mut() else { continue };
            if let Some(guest) = room.guest {
                if now.millis().saturating_sub(guest.last_data.millis())
                    >= self.config.guest_idle.as_millis() as u64
                {
                    room.guest = None; // fall back to listening; room lives on
                }
            }
            if now.millis().saturating_sub(room.last_host.millis())
                >= self.config.paired_idle.as_millis() as u64
            {
                let vport = room.vport;
                *slot = None;
                self.alloc.close(vport);
                events.push(PortEvent::Closed(vport));
            }
        }
        self.buckets
            .retain(|_, b| now.millis().saturating_sub(b.last_refill.millis()) < BUCKET_GC);
        events
    }

    fn on_data(
        &mut self,
        dst_port: u16,
        src: SocketAddr,
        bytes: &[u8],
        now: VClock,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        let offset = usize::from(dst_port.wrapping_sub(self.config.data_port_start));
        let Some(Some(room)) = self.slots.get_mut(offset) else {
            return Vec::new();
        };
        if src.ip() == room.host_ip {
            // Host-side leg (IP-level attribution — see the module docs on
            // the NAT punch): refresh liveness AND re-point the guest-bound
            // target at the observed source (the punch; this also follows a
            // mid-session NAT port rotation). Never a guest pin, never
            // forwarded as guest data.
            room.last_host = now;
            room.host_data_addr = src;
            room.punched = true;
            match room.guest {
                Some(guest) => vec![(guest.addr, bytes.to_vec())],
                // Netcode connects from the guest side: host→nobody is a drop.
                None => Vec::new(),
            }
        } else {
            let to_host = room.host_data_addr;
            match room.guest {
                Some(guest) if guest.addr == src => {
                    vec![(to_host, bytes.to_vec())]
                }
                Some(_) => Vec::new(), // third source: room already has its guest
                None => {
                    room.guest = Some(Guest {
                        addr: src,
                        last_data: now,
                    });
                    vec![(to_host, bytes.to_vec())]
                }
            }
        }
    }

    fn register(
        &mut self,
        code: RoomCode,
        src: SocketAddr,
        src_ip: Ipv4Addr,
        game_port: u16,
        now: VClock,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        // `*A` is answered with `*V <code> <vport>` right behind it (field
        // fix, module docs): the host needs the virtual data port to punch
        // its NAT from the game socket. Every Ack carries it — a lost `*V`
        // gets another chance on the next 2 s keepalive, and pre-`*V`
        // clients drop the extra datagram silently.
        let frames: Vec<Frame> = if let Some(idx) = self.find(code) {
            let room = self.slots[idx].as_mut().expect("find returned a live slot");
            if room.host_ip == src_ip {
                room.last_host = now;
                // A restart on the same IP reuses the code with a fresh
                // game port; keepalive refreshes it — and the initial
                // host_data_addr with it, until the first punch makes the
                // observed address authoritative.
                room.game_port = game_port;
                if !room.punched {
                    room.host_data_addr = SocketAddr::from((src_ip, game_port));
                }
                let vport = room.vport;
                vec![Frame::Ack { code }, Frame::VirtualPort { code, vport }]
            } else {
                vec![Frame::Collision { code }]
            }
        } else {
            match self.free_slot() {
                Some(idx) => {
                    let vport = self.config.data_port_start + idx as u16;
                    if self.alloc.bind(vport).is_err() {
                        vec![Frame::SlotExhausted { code }]
                    } else {
                        self.slots[idx] = Some(Room {
                            code,
                            host_ip: src_ip,
                            game_port,
                            host_data_addr: SocketAddr::from((src_ip, game_port)),
                            punched: false,
                            vport,
                            last_host: now,
                            guest: None,
                        });
                        vec![Frame::Ack { code }, Frame::VirtualPort { code, vport }]
                    }
                }
                None => vec![Frame::SlotExhausted { code }],
            }
        };
        frames
            .into_iter()
            .map(|frame| (src, wire::encode(&frame)))
            .collect()
    }

    fn lookup(&self, code: RoomCode, src: SocketAddr) -> Vec<(SocketAddr, Vec<u8>)> {
        let frame = match self.find(code).map(|idx| &self.slots[idx]) {
            None => Frame::NotFound { code },
            Some(Some(room)) => match room.guest {
                Some(guest) if guest.addr.ip() != src.ip() => Frame::Busy { code },
                // Unpaired, or paired to this same IP (idempotent re-lookup).
                _ => Frame::Found {
                    code,
                    host_ip: room.host_ip.octets(),
                    vport: room.vport,
                },
            },
            Some(None) => Frame::NotFound { code },
        };
        vec![(src, wire::encode(&frame))]
    }

    fn release(&mut self, code: RoomCode, src_ip: Ipv4Addr) {
        if let Some(idx) = self.find(code) {
            let room = self.slots[idx].as_ref().expect("find returned a live slot");
            if room.host_ip != src_ip {
                return; // *D from a non-host: ignore, do not help scanners
            }
            let vport = room.vport;
            self.slots[idx] = None;
            self.alloc.close(vport);
            self.pending_events.push(PortEvent::Closed(vport));
        }
    }

    fn find(&self, code: RoomCode) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|room| room.code == code))
    }

    fn free_slot(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }

    /// Token-bucket gate for control frames (silent drop over budget).
    fn allow(&mut self, src: IpAddr, now: VClock) -> bool {
        let burst = self.config.rate_burst.max(1);
        let refill_ms = (self.config.rate_refill.as_millis() as u64)
            .checked_div(u64::from(burst))
            .unwrap_or(1)
            .max(1);
        let bucket = self.buckets.entry(src).or_insert(Bucket {
            tokens: burst,
            last_refill: now,
        });
        let gained = now.millis().saturating_sub(bucket.last_refill.millis()) / refill_ms;
        if gained > 0 {
            bucket.tokens = bucket.tokens.saturating_add(gained as u32).min(burst);
            bucket.last_refill = VClock(bucket.last_refill.millis() + gained * refill_ms);
        }
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            true
        } else {
            self.rate_limited += 1;
            false
        }
    }
}

impl fmt::Display for PortEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortEvent::Closed(port) => write!(f, "closed data port {port}"),
        }
    }
}

/// Validates and case-normalizes a caller-supplied room code (5 alphabet
/// bytes; G2's code-entry widget validates character-by-character with
/// [`wire::is_code_byte`] instead).
#[must_use]
pub fn normalize_code(bytes: &[u8]) -> Option<RoomCode> {
    if bytes.len() != 5 {
        return None;
    }
    let mut code = [0u8; 5];
    for (dst, &src) in code.iter_mut().zip(bytes) {
        let normalized = src.to_ascii_uppercase();
        if !wire::CODE_ALPHABET.contains(&normalized) {
            return None;
        }
        *dst = normalized;
    }
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeAlloc {
        bound: Vec<u16>,
        /// Simulated bind failure: every `bind(port >= fail_from)` errors.
        fail_from: Option<u16>,
    }

    impl PortAllocator for FakeAlloc {
        fn bind(&mut self, port: u16) -> Result<(), ()> {
            if self.fail_from.is_some_and(|f| port >= f) {
                return Err(());
            }
            assert!(!self.bound.contains(&port), "double bind of {port}");
            self.bound.push(port);
            Ok(())
        }
        fn close(&mut self, port: u16) {
            self.bound.retain(|&p| p != port);
        }
    }

    fn sa(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])), port)
    }

    const CTRL: u16 = 27016;
    const HOST: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 51_000);
    const GUEST: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 40_000);
    const OTHER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), 40_001);

    fn gw() -> Gateway<FakeAlloc> {
        Gateway::new(GatewayConfig::default(), FakeAlloc::default())
    }

    fn reg(code: &[u8; 5], game_port: u16) -> Vec<u8> {
        wire::encode(&Frame::Register {
            code: *code,
            game_port,
        })
    }

    fn lookup(code: &[u8; 5]) -> Vec<u8> {
        wire::encode(&Frame::Lookup { code: *code })
    }

    fn release(code: &[u8; 5]) -> Vec<u8> {
        wire::encode(&Frame::Release { code: *code })
    }

    fn ack(code: &[u8; 5]) -> Vec<u8> {
        wire::encode(&Frame::Ack { code: *code })
    }

    /// Asserts exactly one reply, addressed to `src`, carrying the wanted
    /// frame bytes.
    macro_rules! assert_frame_to {
        ($replies:expr, $src:expr, $want:expr) => {{
            let replies: &[(SocketAddr, Vec<u8>)] = $replies;
            assert_eq!(replies.len(), 1, "expected exactly one reply");
            assert_eq!(replies[0].0, $src);
            assert_eq!(replies[0].1, $want);
        }};
    }

    fn decoded(replies: &[(SocketAddr, Vec<u8>)]) -> Vec<Frame> {
        replies
            .iter()
            .map(|(_, b)| wire::decode(b).expect("gateway emitted a valid frame"))
            .collect()
    }

    #[test]
    fn register_acks_and_binds_first_slot() {
        let mut g = gw();
        let replies = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // Field fix: every *A is followed by *V (see `register_*_virtual_port`).
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (HOST, ack(b"ABCDE")));
        assert_eq!(parse_vport(&replies[1].1), Some(27017));
        assert_eq!(g.live_rooms(), 1);
        assert_eq!(g.allocator().bound, vec![27017]);
    }

    #[test]
    fn keepalive_refreshes_game_port_and_never_collides_same_ip() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // Same IP, different source port: keepalive, not collision.
        let same_ip_other_port = sa([10, 0, 0, 1], 51_001);
        let replies = g.on_packet(
            CTRL,
            same_ip_other_port,
            &reg(b"ABCDE", 6000),
            VClock(10_000),
        );
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (same_ip_other_port, ack(b"ABCDE")));
        assert_eq!(g.live_rooms(), 1);
        // Keepalive refreshed the game port: guest data now forwards there.
        let fwd = g.on_packet(27017, GUEST, b"\x05hello", VClock(10_500));
        assert_eq!(fwd, vec![(sa([10, 0, 0, 1], 6000), b"\x05hello".to_vec())]);
        // And refreshed the GC clock: alive at 24.999 s, gone at 25 s.
        assert!(g.on_tick(VClock(24_999)).is_empty());
        assert_eq!(g.live_rooms(), 1);
        assert_eq!(g.on_tick(VClock(25_000)), vec![PortEvent::Closed(27017)]);
        assert_eq!(g.live_rooms(), 0);
    }

    #[test]
    fn cross_ip_register_collides_without_disturbing_room() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let replies = g.on_packet(CTRL, OTHER, &reg(b"ABCDE", 7000), VClock(1000));
        assert_frame_to!(
            &replies,
            OTHER,
            wire::encode(&Frame::Collision { code: *b"ABCDE" })
        );
        // Original room intact (still one room; ages out from t=0, not t=1000).
        assert_eq!(g.live_rooms(), 1);
        assert!(g.on_tick(VClock(14_999)).is_empty());
        assert_eq!(g.on_tick(VClock(15_000)).len(), 1);
    }

    #[test]
    fn lookup_found_busy_notfound_and_idempotent() {
        let mut g = gw();
        // Unknown code → *E.
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(0));
        assert_frame_to!(
            &replies,
            GUEST,
            wire::encode(&Frame::NotFound { code: *b"ZZZZZ" })
        );
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let found = wire::encode(&Frame::Found {
            code: *b"ABCDE",
            host_ip: [10, 0, 0, 1],
            vport: 27017,
        });
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(100));
        assert_frame_to!(&replies, GUEST, found.clone());
        // Re-lookup before any data: still *F (idempotent).
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(200));
        assert_frame_to!(&replies, GUEST, found.clone());
        // Pin the guest with first data…
        let _ = g.on_packet(27017, GUEST, b"\x00hi", VClock(300));
        // …same-IP re-lookup stays *F, different IP gets *B.
        let replies = g.on_packet(
            CTRL,
            sa([10, 0, 0, 2], 40_999),
            &lookup(b"ABCDE"),
            VClock(400),
        );
        assert_frame_to!(&replies, sa([10, 0, 0, 2], 40_999), found);
        let replies = g.on_packet(CTRL, OTHER, &lookup(b"ABCDE"), VClock(500));
        assert_frame_to!(
            &replies,
            OTHER,
            wire::encode(&Frame::Busy { code: *b"ABCDE" })
        );
    }

    #[test]
    fn guest_pins_on_first_data_and_forward_both_ways() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // Host data with no guest yet: dropped (netcode dials from the guest)
        // — and since the field fix it doubles as the NAT punch, re-pointing
        // guest-bound forwarding at the observed host socket (51 000 here,
        // not the advertised 5000).
        assert!(g.on_packet(27017, HOST, b"\x05h", VClock(10)).is_empty());
        // First non-host packet pins the guest and forwards to the host.
        let fwd = g.on_packet(27017, GUEST, b"\x00g", VClock(20));
        assert_eq!(fwd, vec![(HOST, b"\x00g".to_vec())]);
        // Host→guest and guest→host both flow.
        let fwd = g.on_packet(27017, HOST, b"\x05h2", VClock(30));
        assert_eq!(fwd, vec![(GUEST, b"\x05h2".to_vec())]);
        let fwd = g.on_packet(27017, GUEST, b"\x00g2", VClock(40));
        assert_eq!(fwd, vec![(HOST, b"\x00g2".to_vec())]);
        // A third source is dropped: the room already has its guest.
        assert!(g.on_packet(27017, OTHER, b"\x00x", VClock(50)).is_empty());
    }

    #[test]
    fn gc_unpaired_expires_at_window() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        assert!(g.on_tick(VClock(14_999)).is_empty());
        assert_eq!(g.live_rooms(), 1);
        assert_eq!(g.on_tick(VClock(15_000)), vec![PortEvent::Closed(27017)]);
        assert!(g.allocator().bound.is_empty());
        // Room gone: lookup answers *E, stray data on V is dropped.
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(16_000));
        assert_eq!(decoded(&replies), vec![Frame::NotFound { code: *b"ABCDE" }]);
        assert!(g
            .on_packet(27017, GUEST, b"\x00x", VClock(16_000))
            .is_empty());
    }

    #[test]
    fn gc_paired_room_dies_when_host_goes_silent() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(100));
        // Guest keeps talking; host never sends again → paired window rules.
        let _ = g.on_packet(27017, GUEST, b"\x00yak", VClock(9_000));
        assert!(g.on_tick(VClock(14_900)).is_empty());
        assert_eq!(g.live_rooms(), 1);
        assert_eq!(g.on_tick(VClock(15_000)), vec![PortEvent::Closed(27017)]);
        assert_eq!(g.live_rooms(), 0);
    }

    #[test]
    fn gc_guest_slot_releases_and_falls_back_to_listening() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(100));
        // Host keeps the room alive; guest goes quiet past guest_idle.
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(9_000));
        assert!(g.on_tick(VClock(10_099)).is_empty()); // guest idle 9.999 s
                                                       // Guest slot held: other source dropped.
        assert!(g
            .on_packet(27017, OTHER, b"\x00x", VClock(10_050))
            .is_empty());
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(10_000));
        assert!(g.on_tick(VClock(10_100)).is_empty()); // guest 10 s → released
        assert_eq!(g.live_rooms(), 1); // room itself lives on
                                       // Now a new guest can pin; the old guest is a third source → dropped.
        let fwd = g.on_packet(27017, OTHER, b"\x00new", VClock(10_200));
        assert_eq!(fwd, vec![(sa([10, 0, 0, 1], 5000), b"\x00new".to_vec())]);
        assert!(g
            .on_packet(27017, GUEST, b"\x00old", VClock(10_300))
            .is_empty());
    }

    #[test]
    fn host_data_also_refreshes_the_paired_clock() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(10));
        let _ = g.on_packet(27017, HOST, b"\x05alive", VClock(10_000));
        assert!(g.on_tick(VClock(24_999)).is_empty());
        assert_eq!(g.on_tick(VClock(25_000)), vec![PortEvent::Closed(27017)]);
    }

    #[test]
    fn explicit_release_is_immediate_and_reusable() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(10));
        // *D from a non-host IP does nothing.
        let _ = g.on_packet(CTRL, OTHER, &release(b"ABCDE"), VClock(20));
        assert_eq!(g.live_rooms(), 1);
        // *D from the host: immediate teardown, no frame back.
        let replies = g.on_packet(CTRL, HOST, &release(b"ABCDE"), VClock(30));
        assert!(replies.is_empty());
        assert_eq!(g.live_rooms(), 0);
        assert!(g.allocator().bound.is_empty()); // allocator closed immediately
        assert_eq!(g.on_tick(VClock(30)), vec![PortEvent::Closed(27017)]);
        // Code reusable right away; *D is silent for unknown codes.
        let replies = g.on_packet(CTRL, OTHER, &reg(b"ABCDE", 6000), VClock(31));
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (OTHER, ack(b"ABCDE")));
        let replies = g.on_packet(CTRL, HOST, &release(b"QQQQQ"), VClock(32));
        assert!(replies.is_empty());
    }

    #[test]
    fn rate_limit_bursts_ten_then_silences_until_refill() {
        let mut g = gw();
        for i in 0..10u64 {
            let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(i));
            assert_eq!(replies.len(), 1, "frame {i} should pass");
        }
        // Bucket empty: over-budget frames vanish silently (no reply at all).
        for i in 0..50u64 {
            let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(100 + i));
            assert!(replies.is_empty(), "frame at {} must be dropped", 100 + i);
        }
        // A second IP is unaffected.
        let replies = g.on_packet(CTRL, OTHER, &lookup(b"ZZZZZ"), VClock(100));
        assert_eq!(replies.len(), 1);
        // Refill is 2 tokens/s (10 per 5 s): one more allowed after 1 s.
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(1_100));
        assert_eq!(replies.len(), 1);
        // …but never above burst: a long gap still allows exactly 10, and
        // the 11th waits another refill step.
        for i in 0..10 {
            let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(700_000 + i));
            assert_eq!(replies.len(), 1, "burst token {i}");
        }
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(700_100));
        assert!(replies.is_empty());
        assert!(g.rate_limited() >= 51, "dropped frames must be counted");
    }

    #[test]
    fn rate_limit_covers_all_control_frames_not_data() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(0));
        // Exhaust the guest's bucket with lookups…
        let mut passed = 0;
        for i in 0..30u64 {
            if !g
                .on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(i))
                .is_empty()
            {
                passed += 1;
            }
        }
        assert!(passed <= 10, "control path leaked {passed} past burst");
        // The guest's data plane is NOT rate limited (room kept alive by
        // guest data; guest slot refreshes every packet).
        for i in 0..100u64 {
            assert!(!g
                .on_packet(27017, GUEST, b"\x00stream", VClock(i))
                .is_empty());
        }
    }

    #[test]
    fn slot_exhaustion_frees_and_reallocates() {
        let config = GatewayConfig {
            data_ports: 2,
            ..GatewayConfig::default()
        };
        let mut g = Gateway::new(config, FakeAlloc::default());
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(CTRL, sa([10, 0, 0, 4], 1), &reg(b"FGHJK", 5000), VClock(0));
        // Third room: no free slot → *S.
        let fifth = sa([10, 0, 0, 5], 1);
        let replies = g.on_packet(CTRL, fifth, &reg(b"MNPQR", 5000), VClock(0));
        assert_frame_to!(
            &replies,
            fifth,
            wire::encode(&Frame::SlotExhausted { code: *b"MNPQR" })
        );
        // Freeing a slot (*D) makes room again, reusing the freed port.
        let _ = g.on_packet(CTRL, HOST, &release(b"ABCDE"), VClock(1_000));
        let replies = g.on_packet(CTRL, fifth, &reg(b"MNPQR", 5000), VClock(1_100));
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (fifth, ack(b"MNPQR")));
        assert_eq!(g.allocator().bound, vec![27018, 27017]);
    }

    #[test]
    fn bind_failure_reports_exhausted_without_leaking_slot() {
        let config = GatewayConfig {
            data_ports: 4,
            ..GatewayConfig::default()
        };
        let mut g = Gateway::new(
            config,
            FakeAlloc {
                fail_from: Some(27017),
                ..FakeAlloc::default()
            },
        );
        let replies = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        assert_frame_to!(
            &replies,
            HOST,
            wire::encode(&Frame::SlotExhausted { code: *b"ABCDE" })
        );
        assert_eq!(g.live_rooms(), 0);
        assert!(g.allocator().bound.is_empty());
        // Nothing was poisoned: with binds working again the register succeeds.
        g.alloc.fail_from = None;
        let replies = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(10));
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (HOST, ack(b"ABCDE")));
    }

    #[test]
    fn port_attribution_is_strict() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let _ = g.on_packet(27017, GUEST, b"\x00pin", VClock(0));
        // Data on the control port: dropped.
        assert!(g.on_packet(CTRL, GUEST, b"\x00x", VClock(1)).is_empty());
        assert!(g.on_packet(CTRL, HOST, b"\x05x", VClock(1)).is_empty());
        // Control frames on a data port: dropped.
        let replies = g.on_packet(27017, GUEST, &lookup(b"ABCDE"), VClock(1));
        assert!(replies.is_empty());
        // Unallocated / out-of-range data ports: dropped.
        assert!(g.on_packet(27999, GUEST, b"\x00x", VClock(1)).is_empty());
        assert!(g.on_packet(1, GUEST, b"\x00x", VClock(1)).is_empty());
    }

    #[test]
    fn malformed_and_unknown_control_frames_dropped_silently() {
        let mut g = gw();
        for bytes in [
            vec![b'*'],
            b"*ZABCDE".to_vec(),    // unknown type
            b"*AABC".to_vec(),      // truncated
            b"*AAAAA\xFF".to_vec(), // non-alphabet code
            vec![b'*', b'A', 0, 0, 0, 0, 0],
        ] {
            assert!(g.on_packet(CTRL, GUEST, &bytes, VClock(0)).is_empty());
        }
        // Server→client frames from a client: dropped, no echo.
        let found = wire::encode(&Frame::Found {
            code: *b"ABCDE",
            host_ip: [1, 2, 3, 4],
            vport: 1,
        });
        assert!(g.on_packet(CTRL, GUEST, &found, VClock(0)).is_empty());
        assert_eq!(g.live_rooms(), 0);
    }

    #[test]
    fn codes_are_case_normalized_end_to_end() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"abcde", 5000), VClock(0));
        // Uppercase lookup finds the lowercase-registered room.
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(1));
        assert_eq!(
            decoded(&replies),
            vec![Frame::Found {
                code: *b"ABCDE",
                host_ip: [10, 0, 0, 1],
                vport: 27017,
            }]
        );
    }

    #[test]
    fn normalize_code_helper() {
        assert_eq!(normalize_code(b"abcDe"), Some(*b"ABCDE"));
        assert_eq!(normalize_code(b"ABCDE"), Some(*b"ABCDE"));
        assert_eq!(normalize_code(b"ABCD"), None);
        assert_eq!(normalize_code(b"ABCDEA"), None);
        assert_eq!(normalize_code(b"ABCDI"), None); // I excluded
        assert_eq!(normalize_code(b"ABCD0"), None); // 0 excluded
    }

    // ---- field fix: host-side NAT punch (written RED first against the
    // pre-fix room.rs — see the commit log for the captured failures) ----

    /// Hand-parses a `*V <code:5B> <vport:u16>` frame (len 9) directly from
    /// the bytes. Deliberately independent of `wire::Frame` so these tests
    /// compile and fail at *runtime* (RED) against the pre-fix wire codec,
    /// then stay honest after the variant lands.
    fn parse_vport(bytes: &[u8]) -> Option<u16> {
        if bytes.len() == 9 && bytes[0] == b'*' && bytes[1] == b'V' {
            Some(u16::from_be_bytes([bytes[7], bytes[8]]))
        } else {
            None
        }
    }

    #[test]
    fn register_replies_ack_then_virtual_port() {
        let mut g = gw();
        let replies = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        assert_eq!(replies.len(), 2, "expected [*A, *V] to the host");
        assert_eq!(replies[0], (HOST, ack(b"ABCDE")), "*A first, unchanged");
        assert_eq!(replies[1].0, HOST);
        assert_eq!(
            parse_vport(&replies[1].1),
            Some(27017),
            "*V carries the room's virtual data port, right after *A"
        );
        // Keepalive: same pairing every 2 s (a punch missed on packet loss
        // gets another chance; old clients drop the extra frame).
        let replies = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(2_000));
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], (HOST, ack(b"ABCDE")));
        assert_eq!(parse_vport(&replies[1].1), Some(27017));
    }

    #[test]
    fn virtual_port_accompanies_only_acks() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // Collision, *F lookups and *E answers never carry a *V.
        let replies = g.on_packet(CTRL, OTHER, &reg(b"ABCDE", 7000), VClock(100));
        assert_eq!(replies.len(), 1);
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(200));
        assert_eq!(replies.len(), 1);
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ZZZZZ"), VClock(300));
        assert_eq!(replies.len(), 1);
        // Slot exhaustion: *S only.
        let config = GatewayConfig {
            data_ports: 1,
            ..GatewayConfig::default()
        };
        let mut full = Gateway::new(config, FakeAlloc::default());
        let _ = full.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let replies = full.on_packet(CTRL, OTHER, &reg(b"FGHJK", 5000), VClock(0));
        assert_eq!(replies.len(), 1);
        assert_eq!(
            decoded(&replies),
            vec![Frame::SlotExhausted { code: *b"FGHJK" }]
        );
    }

    #[test]
    fn punch_updates_host_data_addr_and_guest_data_follows_it() {
        let mut g = gw();
        // *R advertises game_port 5000; the NAT's real public port is 40123 —
        // forwarding must follow the observed punch, never the advertised port.
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let punched = sa([10, 0, 0, 1], 40_123);
        // The punch datagram: no guest yet → no forward…
        assert!(g.on_packet(27017, punched, b"\x2b", VClock(10)).is_empty());
        // …and it never consumes the guest slot.
        let replies = g.on_packet(CTRL, GUEST, &lookup(b"ABCDE"), VClock(20));
        assert_eq!(
            decoded(&replies),
            vec![Frame::Found {
                code: *b"ABCDE",
                host_ip: [10, 0, 0, 1],
                vport: 27017,
            }]
        );
        // First guest packet pins and forwards to the PUNCHED address.
        let fwd = g.on_packet(27017, GUEST, b"\x00hello", VClock(30));
        assert_eq!(fwd, vec![(punched, b"\x00hello".to_vec())]);
        // Host replies from the punched socket flow back to the guest.
        let fwd = g.on_packet(27017, punched, b"\x05hi", VClock(40));
        assert_eq!(fwd, vec![(GUEST, b"\x05hi".to_vec())]);
    }

    #[test]
    fn host_ip_data_never_pins_the_guest() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // Any source with the host's IP is host-side — the original game
        // socket, the punch socket, a rotated mapping — and none of them
        // pair the room.
        assert!(g.on_packet(27017, HOST, b"\x05a", VClock(10)).is_empty());
        assert!(g
            .on_packet(27017, sa([10, 0, 0, 1], 41_000), b"\x2b", VClock(20))
            .is_empty());
        let replies = g.on_packet(CTRL, OTHER, &lookup(b"ABCDE"), VClock(30));
        assert_eq!(
            decoded(&replies),
            vec![Frame::Found {
                code: *b"ABCDE",
                host_ip: [10, 0, 0, 1],
                vport: 27017,
            }],
            "*F not *B: host-side data must never pair the room"
        );
        // A non-host source still pins on first data.
        assert_eq!(g.on_packet(27017, GUEST, b"\x00g", VClock(40)).len(), 1);
        // And a host-IP source after pairing forwards to the guest instead
        // of becoming a second guest.
        let fwd = g.on_packet(27017, sa([10, 0, 0, 1], 41_999), b"\x05h", VClock(50));
        assert_eq!(fwd, vec![(GUEST, b"\x05h".to_vec())]);
    }

    #[test]
    fn punch_refreshes_room_liveness() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        // No *R keepalives at all: a punch at 14 s refreshes the host clock.
        let _ = g.on_packet(27017, sa([10, 0, 0, 1], 40_123), b"\x2b", VClock(14_000));
        assert!(g.on_tick(VClock(14_999)).is_empty());
        assert_eq!(g.live_rooms(), 1);
        assert_eq!(g.on_tick(VClock(29_000)), vec![PortEvent::Closed(27017)]);
    }

    #[test]
    fn keepalive_after_punch_does_not_clobber_the_punched_address() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let punched = sa([10, 0, 0, 1], 40_123);
        let _ = g.on_packet(27017, punched, b"\x2b", VClock(100));
        // Keepalive *R arrives on the CONTROL socket — a different source
        // than the game socket — and must not reset the guest-bound target
        // back to the advertised (unreachable) game_port.
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(2_000));
        let fwd = g.on_packet(27017, GUEST, b"\x00x", VClock(2_500));
        assert_eq!(fwd, vec![(punched, b"\x00x".to_vec())]);
    }

    #[test]
    fn punch_relocates_mid_session_on_nat_port_rotation() {
        let mut g = gw();
        let _ = g.on_packet(CTRL, HOST, &reg(b"ABCDE", 5000), VClock(0));
        let first = sa([10, 0, 0, 1], 40_123);
        let _ = g.on_packet(27017, first, b"\x2b", VClock(100));
        let _ = g.on_packet(27017, GUEST, b"\x00p", VClock(200));
        // The NAT rotates the host's public mapping mid-session: the next
        // host-sourced packet re-points guest-bound forwarding (and keeps
        // flowing to the guest itself).
        let second = sa([10, 0, 0, 1], 42_000);
        let fwd = g.on_packet(27017, second, b"\x05r", VClock(1_000));
        assert_eq!(fwd, vec![(GUEST, b"\x05r".to_vec())]);
        let fwd = g.on_packet(27017, GUEST, b"\x00y", VClock(1_100));
        assert_eq!(fwd, vec![(second, b"\x00y".to_vec())]);
    }

    #[test]
    fn many_rooms_get_distinct_consecutive_ports() {
        let mut g = gw();
        for slot in 0..16u16 {
            let mut c = *b"AAAAA";
            c[0] = wire::CODE_ALPHABET[usize::from(slot) % wire::CODE_ALPHABET.len()];
            c[1] = wire::CODE_ALPHABET[(usize::from(slot) / 31 + 1) % wire::CODE_ALPHABET.len()];
            let _ = g.on_packet(
                CTRL,
                sa([10, 0, 0, u8::try_from(slot).unwrap() + 1], 1),
                &reg(&c, 5000),
                VClock(0),
            );
        }
        assert_eq!(g.live_rooms(), 16);
        assert_eq!(
            g.allocator().bound,
            (27017..27033).collect::<Vec<u16>>(),
            "slots allocate consecutively"
        );
    }
}
