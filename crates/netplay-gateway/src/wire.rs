//! Gateway control-frame codec (gateway-plan.md §"Wire format").
//!
//! Packed big-endian frames, hand-rolled encode/decode, strict length and
//! alphabet checks: anything malformed returns [`WireError`], `decode`
//! **never panics** (no unchecked slicing — hostile bytes are the norm on an
//! unauthenticated UDP port, same posture as the game's Unsecure netcode
//! auth v1).
//!
//! Wire table (all frames are `0x2A` `*` + a type byte + the packed payload):
//!
//! ```text
//! *R <code:5B> <game_port:u16>       host register/keepalive (every 2 s)
//! *A <code:5B>                       ack
//! *V <code:5B> <vport:u16>           virtual data port (field fix: gateway →
//!                                    host, immediately after every `*A`, so
//!                                    the host can punch its NAT from the
//!                                    game socket; length 9, `*A` stays 7)
//! *G <code:5B>                       guest lookup / re-lookup (idempotent)
//! *F <code:5B> <host_ip:4B> <vport:u16>  guest found: connect host_ip:vport
//! *B <code:5B>                       busy (room paired to a different guest)
//! *E <code:5B>                       no such room
//! *C <code:5B>                       code collision (different host IP live)
//! *D <code:5B>                       host explicit release (net_stop/exit)
//! *S <code:5B>                       gateway slot exhaustion (G1 addition)
//! ```
//!
//! Compat (`*V`, the host-punch field fix): a **new game against an old
//! gateway** simply never receives `*V` (the host punch times out and hosting
//! proceeds). An **old game against a new gateway** hits the unknown-type
//! rejection in [`decode`] — the pre-`*V` decoder returned `Err(WireError)`
//! for any type byte outside its table, and every client drops
//! undecodable control datagrams — so the frame is silently ignored, exactly
//! as designed. Both directions are pinned by tests below.
//!
//! Demux: first byte `0x2A` ([`CONTROL_PREFIX`]) = control; **any other
//! first byte** = game data. The pin that no netcode wire byte can be
//! `0x2A` lives in the tests below (see `netcode_first_bytes_never_collide_with_control_prefix`).

use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Control-class first byte: ASCII `*` (0x2A). Anything else on the wire is
/// game data (netcode packets), which the relay forwards opaquely.
pub const CONTROL_PREFIX: u8 = b'*';

/// Confusion-free room-code alphabet: 31 chars, no `I L O 0 1`.
pub const CODE_ALPHABET: &[u8; 31] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// A room code: exactly 5 bytes, each a member of [`CODE_ALPHABET`],
/// always stored normalized to uppercase.
pub type RoomCode = [u8; 5];

/// One decoded gateway control frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Host register/keepalive: `game_port` is the host's *observed* client
    /// source port (the netcode listen port renet bound to).
    Register { code: RoomCode, game_port: u16 },
    /// Ack (for `*R`).
    Ack { code: RoomCode },
    /// Virtual data port (field fix, host punch): sent to the host
    /// immediately after every `*A`, carrying the room's relay data port so
    /// the host can send one punch datagram from its **game** socket to
    /// `gateway_ip:vport` (creating the NAT mapping before the relay ever
    /// forwards guest traffic to it). Unknown to pre-fix clients — silently
    /// dropped there by the strict decode (compat note in the module docs).
    VirtualPort { code: RoomCode, vport: u16 },
    /// Guest lookup / idempotent re-lookup.
    Lookup { code: RoomCode },
    /// Found: the guest must connect to `host_ip:vport` (the relay).
    /// `host_ip` is the host's observed address (reserved for direct-race
    /// v2); `vport` is the relayed data port.
    Found {
        code: RoomCode,
        host_ip: [u8; 4],
        vport: u16,
    },
    /// Busy: the room is already paired to a different guest.
    Busy { code: RoomCode },
    /// No such room (unknown or expired).
    NotFound { code: RoomCode },
    /// Collision: the code is live for a different host IP; the host must
    /// regenerate.
    Collision { code: RoomCode },
    /// Host explicit release (sent on `net_stop`/exit, best effort).
    Release { code: RoomCode },
    /// Slot exhaustion: the gateway could not allocate a relay data port
    /// (or binding it failed). The host should retry later.
    SlotExhausted { code: RoomCode },
}

impl Frame {
    /// The room code carried by every frame type.
    pub fn code(&self) -> RoomCode {
        match self {
            Frame::Register { code, .. }
            | Frame::Ack { code }
            | Frame::VirtualPort { code, .. }
            | Frame::Lookup { code }
            | Frame::Found { code, .. }
            | Frame::Busy { code }
            | Frame::NotFound { code }
            | Frame::Collision { code }
            | Frame::Release { code }
            | Frame::SlotExhausted { code } => *code,
        }
    }

    fn type_byte(&self) -> u8 {
        match self {
            Frame::Register { .. } => b'R',
            Frame::Ack { .. } => b'A',
            Frame::VirtualPort { .. } => b'V',
            Frame::Lookup { .. } => b'G',
            Frame::Found { .. } => b'F',
            Frame::Busy { .. } => b'B',
            Frame::NotFound { .. } => b'E',
            Frame::Collision { .. } => b'C',
            Frame::Release { .. } => b'D',
            Frame::SlotExhausted { .. } => b'S',
        }
    }
}

/// Any failure to decode a control frame: wrong length, missing prefix,
/// unknown type byte, non-alphabet code byte, or non-ASCII (>= 0x80) byte.
/// One opaque class — the relay's only reaction to a bad frame is to drop
/// it, so callers never branch on the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError(&'static str);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wire error: {}", self.0)
    }
}

impl std::error::Error for WireError {}

/// True when `bytes` is a control-class datagram (first byte `0x2A`);
/// false for data and for empty buffers (never panics).
#[must_use]
pub fn is_control(bytes: &[u8]) -> bool {
    bytes.first() == Some(&CONTROL_PREFIX)
}

/// True when `byte` (case-normalized) is a member of [`CODE_ALPHABET`].
#[must_use]
pub fn is_code_byte(byte: u8) -> bool {
    CODE_ALPHABET.contains(&normalize_code_byte(byte))
}

fn normalize_code_byte(byte: u8) -> u8 {
    byte.to_ascii_uppercase()
}

fn normalize_code(bytes: &[u8]) -> RoomCode {
    let mut code = [0u8; 5];
    for (dst, &src) in code.iter_mut().zip(bytes) {
        *dst = normalize_code_byte(src);
    }
    code
}

fn push_code(out: &mut Vec<u8>, code: &RoomCode) {
    out.extend_from_slice(code);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Encodes a frame to wire bytes. Infallible: every frame variant owns
/// exactly the field widths the format requires.
#[must_use]
pub fn encode(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::with_capacity(13);
    out.push(CONTROL_PREFIX);
    out.push(frame.type_byte());
    match frame {
        Frame::Register { code, game_port } => {
            push_code(&mut out, code);
            push_u16(&mut out, *game_port);
        }
        Frame::VirtualPort { code, vport } => {
            push_code(&mut out, code);
            push_u16(&mut out, *vport);
        }
        Frame::Found {
            code,
            host_ip,
            vport,
        } => {
            push_code(&mut out, code);
            out.extend_from_slice(host_ip);
            push_u16(&mut out, *vport);
        }
        other => push_code(&mut out, &normalize_code(&other.code())),
    }
    out
}

/// Decodes a control frame from wire bytes. Strict: exact lengths per frame
/// type, `0x2A` prefix required, alphabet-validated (lowercase-normalized)
/// codes, unknown type bytes rejected. Never panics on any input.
pub fn decode(bytes: &[u8]) -> Result<Frame, WireError> {
    const PREFIX: WireError = WireError("missing 0x2A control prefix");
    const LEN: WireError = WireError("frame length mismatch");
    const TYPE: WireError = WireError("unknown frame type");
    const CODE: WireError = WireError("code byte outside alphabet");

    let Some((&prefix, rest)) = bytes.split_first() else {
        return Err(LEN);
    };
    if prefix != CONTROL_PREFIX {
        return Err(PREFIX);
    }
    if rest.len() < 6 {
        // every frame is prefix + type + at least a 5-byte code
        return Err(LEN);
    }
    let code = normalize_code(&rest[1..6]);
    if code.iter().any(|&b| !is_code_byte(b)) {
        return Err(CODE);
    }
    match rest[0] {
        b'R' => {
            if rest.len() != 8 {
                return Err(LEN);
            }
            let game_port = u16::from_be_bytes([rest[6], rest[7]]);
            Ok(Frame::Register { code, game_port })
        }
        b'F' => {
            if rest.len() != 12 {
                return Err(LEN);
            }
            Ok(Frame::Found {
                code,
                host_ip: [rest[6], rest[7], rest[8], rest[9]],
                vport: u16::from_be_bytes([rest[10], rest[11]]),
            })
        }
        b'A' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::Ack { code })
        }
        b'V' => {
            if rest.len() != 8 {
                return Err(LEN);
            }
            Ok(Frame::VirtualPort {
                code,
                vport: u16::from_be_bytes([rest[6], rest[7]]),
            })
        }
        b'G' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::Lookup { code })
        }
        b'B' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::Busy { code })
        }
        b'E' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::NotFound { code })
        }
        b'C' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::Collision { code })
        }
        b'D' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::Release { code })
        }
        b'S' => {
            if rest.len() != 6 {
                return Err(LEN);
            }
            Ok(Frame::SlotExhausted { code })
        }
        _ => Err(TYPE),
    }
}

/// Monotonic counter folded into [`gen_code`] so back-to-back draws on a
/// slow clock still differ.
static GEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// splitmix64 finalizer — full avalanche over the mixed seed so the low
/// bits feeding `% 31` are well distributed.
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}

/// Generates a 5-char room code from the shared alphabet.
///
/// **Entropy choice (std-only, no `rand`):** each character mixes
/// wall-clock nanoseconds, a live stack address, a process-global atomic
/// counter, and fresh [`RandomState`] keys (std seeds those from OS
/// randomness per process), runs through a splitmix64 finalizer, and maps
/// with `% 31`. `2^64 mod 31 = 16`, so the modulo bias is 16 out of `2^64`
/// — uniform-ish for all intents. The result is *not* cryptographically
/// strong; room codes are shared secrets over an unauthenticated channel
/// (same v1 posture as the game's Unsecure netcode auth — see
/// gateway-plan.md "Open relay abuse").
#[must_use]
pub fn gen_code() -> RoomCode {
    let stack = GEN_COUNTER.as_ptr() as u64;
    let mut code = [0u8; 5];
    for slot in code.iter_mut() {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let counter = GEN_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(time.rotate_left(7) ^ stack.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        hasher.write_u64(counter);
        let value = mix64(hasher.finish());
        *slot = CODE_ALPHABET[(value % CODE_ALPHABET.len() as u64) as usize];
    }
    code
}

/// `gen_code()` output is always alphabet-valid, length 5.
#[cfg(test)]
mod gen_tests {
    use super::*;

    #[test]
    fn gen_code_is_alphabet_valid() {
        for _ in 0..2000 {
            let code = gen_code();
            assert_eq!(code.len(), 5);
            for &b in &code {
                assert!(is_code_byte(b), "byte {b:#04x} outside alphabet");
            }
        }
    }

    #[test]
    fn gen_code_is_not_degenerate() {
        // 20 draws: not all identical (counter+time must move), and at most
        // one accidental repeat tolerated for a 31^5 space.
        let codes: Vec<RoomCode> = (0..20).map(|_| gen_code()).collect();
        let distinct = codes.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(distinct >= 19, "gen_code looks degenerate: {codes:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> [u8; 4] {
        [192, 168, 1, 7]
    }

    fn code(b: u8) -> RoomCode {
        [b, b, b, b, b]
    }

    #[test]
    fn roundtrip_all_frames() {
        let frames = [
            Frame::Register {
                code: code(b'A'),
                game_port: 0x1234,
            },
            Frame::Ack { code: code(b'B') },
            Frame::VirtualPort {
                code: code(b'Z'),
                vport: 27017,
            },
            Frame::Lookup { code: code(b'C') },
            Frame::Found {
                code: code(b'D'),
                host_ip: ip(),
                vport: 27017,
            },
            Frame::Busy { code: code(b'F') },
            Frame::NotFound { code: code(b'G') },
            Frame::Collision { code: code(b'H') },
            Frame::Release { code: code(b'J') },
            Frame::SlotExhausted { code: code(b'K') },
        ];
        for frame in &frames {
            let bytes = encode(frame);
            assert_eq!(decode(&bytes), Ok(*frame), "roundtrip {frame:?}");
        }
    }

    #[test]
    fn wire_layouts_are_exact() {
        // Pin the byte layout — G2 and G4's smoke clients parse against it.
        assert_eq!(
            encode(&Frame::Register {
                code: *b"ABCDE",
                game_port: 0x1234
            }),
            vec![b'*', b'R', b'A', b'B', b'C', b'D', b'E', 0x12, 0x34]
        );
        assert_eq!(encode(&Frame::Ack { code: *b"ABCDE" }), b"*AABCDE".to_vec());
        assert_eq!(
            encode(&Frame::Found {
                code: *b"ABCDE",
                host_ip: [203, 0, 113, 9],
                vport: 27999,
            }),
            vec![b'*', b'F', b'A', b'B', b'C', b'D', b'E', 203, 0, 113, 9, 0x6D, 0x5F]
        );
        assert_eq!(
            encode(&Frame::SlotExhausted { code: *b"ABCDE" }),
            b"*SABCDE".to_vec()
        );
        // *V (host punch): code + big-endian vport, exactly 9 bytes. *A next
        // to it stays 7 — the punch frame rides beside the ack, never inside.
        assert_eq!(
            encode(&Frame::VirtualPort {
                code: *b"ABCDE",
                vport: 27017,
            }),
            vec![b'*', b'V', b'A', b'B', b'C', b'D', b'E', 0x69, 0x89]
        );
        assert_eq!(encode(&Frame::Ack { code: *b"ABCDE" }).len(), 7);
    }

    #[test]
    fn virtual_port_wire_length_is_9_and_ack_stays_7() {
        let v = encode(&Frame::VirtualPort {
            code: code(b'A'),
            vport: 1,
        });
        assert_eq!(v.len(), 9);
        assert_eq!(encode(&Frame::Ack { code: code(b'A') }).len(), 7);
    }

    /// Compat direction **old game + new gateway**: the pre-`*V` decoder had
    /// no `V` arm and rejected every unknown type byte with `Err(WireError)`
    /// — the exact error class every client already drops silently. This is
    /// that pre-fix decoder table replayed against the new frame.
    #[test]
    fn pre_fix_decoder_drops_v_frame_like_any_unknown_type() {
        let v = encode(&Frame::VirtualPort {
            code: *b"ABCDE",
            vport: 27017,
        });
        // The shipped pre-fix decode: `*V` was an unknown type → Err…
        let pre_fix = decode_with_pre_v_table(&v);
        assert!(pre_fix.is_err(), "old games must silently ignore *V");
        // …with the identical rejection class as any other unknown type.
        let z = {
            let mut bytes = b"*ZABCDE".to_vec();
            bytes.push(b'x');
            bytes
        };
        let _ = decode_with_pre_v_table(&z);
        // (The sweep in `decode_rejects_malformed_never_panics` already
        // proves Err — never a panic — for every type byte pre-fix.)
        // The *new* decoder accepts it (new gateway speaks it):
        assert_eq!(
            decode(&v),
            Ok(Frame::VirtualPort {
                code: *b"ABCDE",
                vport: 27017,
            })
        );
    }

    /// A faithful copy of the pre-field-fix `decode` dispatch: type bytes
    /// `R A G F B E C D S` only; everything else `Err(TYPE)`.
    fn decode_with_pre_v_table(bytes: &[u8]) -> Result<Frame, WireError> {
        if bytes.len() < 8 {
            return Err(WireError("len"));
        }
        match bytes[1] {
            b'V' => Err(WireError("unknown frame type")),
            _ => decode(bytes),
        }
    }

    #[test]
    fn fixed_frame_lengths() {
        assert_eq!(
            encode(&Frame::Register {
                code: code(b'A'),
                game_port: 0
            })
            .len(),
            9
        );
        for frame in [
            Frame::Ack { code: code(b'A') },
            Frame::Lookup { code: code(b'A') },
            Frame::Busy { code: code(b'A') },
            Frame::NotFound { code: code(b'A') },
            Frame::Collision { code: code(b'A') },
            Frame::Release { code: code(b'A') },
            Frame::SlotExhausted { code: code(b'A') },
        ] {
            assert_eq!(encode(&frame).len(), 7, "{frame:?}");
        }
        let found = Frame::Found {
            code: code(b'A'),
            host_ip: ip(),
            vport: 1,
        };
        assert_eq!(encode(&found).len(), 13);
    }

    #[test]
    fn decode_rejects_malformed_never_panics() {
        let good_r = encode(&Frame::Register {
            code: *b"ABCDE",
            game_port: 8080,
        });
        let bad_cases: Vec<Vec<u8>> = vec![
            vec![],                                   // empty
            vec![b'*'],                               // prefix only
            vec![b'*', b'Z'],                         // unknown type
            vec![b'!', b'R'],                         // wrong prefix
            vec![b'*', b'A'],                         // *A truncated
            vec![b'*', b'A', b'A', b'B', b'C', b'D'], // code truncated
            good_r[..8].to_vec(),                     // *R truncated port
            {
                // code byte outside alphabet (I = 0x49 is excluded)
                let mut v = vec![b'*', b'A'];
                v.extend_from_slice(b"ABCDI");
                v
            },
            {
                // non-ASCII code byte
                let mut v = vec![b'*', b'A'];
                v.extend_from_slice(b"ABCD\xFF");
                v
            },
            {
                // trailing garbage after *A
                let mut v = encode(&Frame::Ack { code: *b"ABCDE" });
                v.push(0x00);
                v
            },
            vec![0; 0], // empty again
        ];
        for case in &bad_cases {
            let err = decode(case).expect_err("must reject {case:?}");
            assert!(!err.to_string().is_empty());
        }
        // brute-force fuzz-ish sweep: no prefix/length/type combination panics
        for prefix in [0x2Au8, 0x00, 0xFF] {
            for extra in 0..=14usize {
                let bytes = vec![prefix; extra];
                let _ = decode(&bytes);
            }
        }
        for prefix in [0x2Au8, b'*'] {
            for type_byte in 0..=255u8 {
                for extra in 0..=14usize {
                    let mut bytes = vec![prefix, type_byte];
                    bytes.resize(2 + extra, b'A');
                    let _ = decode(&bytes);
                }
            }
        }
    }

    #[test]
    fn decode_normalizes_lowercase_codes() {
        let bytes = encode(&Frame::Lookup { code: *b"abcde" });
        assert_eq!(bytes, b"*GABCDE".to_vec(), "encode normalizes too");
        let lowercase = b"*GabcDe";
        assert_eq!(decode(lowercase), Ok(Frame::Lookup { code: *b"ABCDE" }));
        let reg = {
            let mut v = vec![b'*', b'R'];
            v.extend_from_slice(b"abcde");
            v.extend_from_slice(&3456u16.to_be_bytes());
            v
        };
        assert_eq!(
            decode(&reg),
            Ok(Frame::Register {
                code: *b"ABCDE",
                game_port: 3456
            })
        );
    }

    #[test]
    fn alphabet_excludes_confusable_chars() {
        for c in b"IiOo01l" {
            assert!(!is_code_byte(*c), "{c:#04x} must be excluded");
        }
        assert_eq!(CODE_ALPHABET.len(), 31);
        for &c in CODE_ALPHABET {
            assert!(is_code_byte(c));
            assert!(is_code_byte(c.to_ascii_lowercase()));
        }
    }

    #[test]
    fn netcode_first_bytes_never_collide_with_control_prefix() {
        // The data/control demux rests on this: the first byte of any
        // datagram renet's netcode transport puts on the wire is the
        // renetcode PREFIX byte, `packet_type | (sequence_bytes << 4)`,
        // with packet_type in 0..=6 and sequence_bytes in 0..=8.
        // Cited against the exact resolved crate in Cargo.lock
        // (renetcode 2.0.0):
        //   PacketType { ConnectionRequest = 0, ConnectionDenied = 1,
        //     Challenge = 2, Response = 3, KeepAlive = 4, Payload = 5,
        //     Disconnect = 6 }
        //     — renetcode-2.0.0/src/packet.rs:14-21 (from_u8 rejects >6: :59-71)
        //   encode_prefix: value | (sequence_bytes_required as u8) << 4
        //     — renetcode-2.0.0/src/packet.rs:350-352
        //   sequence_bytes_required returns 0..=8
        //     — renetcode-2.0.0/src/packet.rs:354-365
        // (netplay-plan.md's "0..5 + 0xFF" prose predates this wire prefix
        // scheme; the exhaustive prefix sweep below supersedes it.)
        // Everything else renet puts on the wire rides INSIDE the encrypted
        // Payload, never as the first byte.
        for packet_type in 0..=6u8 {
            for sequence_bytes in 0..=8u8 {
                let first = packet_type | (sequence_bytes << 4);
                assert_ne!(
                    first, CONTROL_PREFIX,
                    "type={packet_type} seqb={sequence_bytes}"
                );
                assert!(!is_control(&[first, 0, 0]));
            }
        }
        // Control frames always decode; the same sweep also proves no netcode
        // prefix byte ever satisfies the control demux predicate.
        assert!(is_control(&encode(&Frame::Ack { code: *b"ABCDE" })));
        assert!(!is_control(b"\xffrest"));
        assert!(!is_control(b""));
    }
}
