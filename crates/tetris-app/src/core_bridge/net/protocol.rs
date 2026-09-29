//! Wire protocol codec (netplay-plan.md N1).
//!
//! All messages travel as bincode 1.3 payloads over renet channels. The
//! codec is deliberately **strict**: fixed-integer encoding (byte-identical
//! to `bincode::serialize`/`bincode::deserialize` defaults) with trailing
//! bytes rejected, so a version-skewed peer that prepends/appends fields
//! fails `decode` instead of silently mis-parsing. Unknown enum variant
//! indices, truncated buffers, and trailing garbage all return
//! [`ProtocolError`] — `decode` never panics (required because v1 auth is
//! unsecure: hostile bytes must not take the host down; see N7's fuzz task).

use bincode::Options;
use serde::{Deserialize, Serialize};

use tetris_core::actions::Action;
use tetris_core::versus::{AttackRule, MatchSnapshot, Side};

/// Netcode protocol discriminator. Peers with a different id are rejected
/// (silently, at the packet layer) — bump together with
/// [`PROTOCOL_VERSION`] on any incompatible wire change.
pub const PROTOCOL_ID: u64 = 0x424C4B46_5F317631;

/// Application-level protocol version carried in [`NetMsg::Hello`]; peers
/// must agree exactly or the handshake kicks the guest.
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// One wire message. Field-level semantics live in netplay-plan.md §"Wire
/// protocol"; the `side`/`left`/`right` payload split of
/// [`NetMsg::SnapshotHash`] is consumed by N3's desync check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NetMsg {
    /// Guest → host handshake. `delay` is the guest's desired input delay
    /// (ticks); both sides adopt `D = max(host, guest)`.
    Hello { version: String, delay: u8 },
    /// Host → guest: start (or rematch) a mirror. `match_delay` is the
    /// negotiated `D` adopted after this `Hello` exchange.
    MatchStart {
        seed: u64,
        rule: AttackRule,
        match_delay: u8,
    },
    /// Guest → host: the guest's actions scheduled for absolute `tick`
    /// (`= local tick + D`), sent immediately when queued.
    TickInput { tick: u64, actions: Vec<Action> },
    /// Host → guest, every tick (empty lists included): the authoritative
    /// batch for `tick`.
    TickBatch {
        tick: u64,
        left: Vec<Action>,
        right: Vec<Action>,
    },
    /// Either direction, every 60 ticks: `side` is the sender's seat;
    /// `left`/`right` are per-side snapshot hashes for divergence check.
    SnapshotHash {
        side: Side,
        tick: u64,
        left: u64,
        right: u64,
    },
    /// Clean exit: the receiving side tears down to the title overlay.
    Bye,
}

/// Any failure to turn wire bytes into a [`NetMsg`] (truncated payload,
/// trailing garbage, unknown variant index, invalid field value). A single
/// opaque class: the net layer's only valid reaction is to ignore/kick, so
/// N2/N3 never branch on the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError(String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol error: {}", self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl From<bincode::Error> for ProtocolError {
    fn from(e: bincode::Error) -> Self {
        Self(e.to_string())
    }
}

/// Fixed-integer encoding keeps the wire format identical to the
/// `bincode::serialize`/`deserialize` top-level defaults (bincode 1.3's
/// `DefaultOptions` alone would switch lengths to varints); strictness
/// rejects trailing bytes.
fn codec() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}

/// Serializes a message for transmission. Infallible for [`NetMsg`]: every
/// field is a plain serde type, so the only bincode failure modes
/// (non-encodable values, writers that error) cannot occur.
pub fn encode(msg: &NetMsg) -> Vec<u8> {
    codec()
        .serialize(msg)
        .expect("NetMsg serialization is infallible")
}

/// Deserializes a message received from the wire. Never panics on
/// adversarial input; every malformed shape yields [`ProtocolError`].
pub fn decode(bytes: &[u8]) -> Result<NetMsg, ProtocolError> {
    codec().deserialize(bytes).map_err(ProtocolError::from)
}

/// FNV-1a (64-bit) over the bincode bytes of a [`MatchSnapshot`].
///
/// Used as the periodic desync check (plan: "process-stable, unlike std
/// `Hash`" — the FNV constants and byte iteration make the value stable
/// across runs, builds and platforms for identical snapshots).
pub fn snapshot_hash(snapshot: &MatchSnapshot) -> u64 {
    let bytes = codec()
        .serialize(snapshot)
        .expect("MatchSnapshot serialization is infallible");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_cases() -> Vec<NetMsg> {
        vec![
            NetMsg::Hello {
                version: PROTOCOL_VERSION.to_string(),
                delay: 8,
            },
            NetMsg::Hello {
                version: String::new(),
                delay: 0,
            },
            NetMsg::MatchStart {
                seed: 0xDEAD_BEEF_CAFE_F00D,
                rule: AttackRule::Garbage,
                match_delay: 8,
            },
            NetMsg::MatchStart {
                seed: 0,
                rule: AttackRule::Race { target_lines: 40 },
                match_delay: 30,
            },
            NetMsg::TickInput {
                tick: u64::MAX,
                actions: vec![Action::MoveLeft, Action::HardDrop, Action::RotateCw],
            },
            NetMsg::TickInput {
                tick: 0,
                actions: Vec::new(),
            },
            NetMsg::TickBatch {
                tick: 123,
                left: vec![Action::SoftDrop],
                right: vec![Action::RotateCcw, Action::MoveRight],
            },
            NetMsg::TickBatch {
                tick: 123,
                left: Vec::new(),
                right: Vec::new(),
            },
            NetMsg::SnapshotHash {
                side: Side::Left,
                tick: 600,
                left: 1,
                right: u64::MAX,
            },
            NetMsg::SnapshotHash {
                side: Side::Right,
                tick: 0,
                left: 0,
                right: 0,
            },
            NetMsg::Bye,
        ]
    }

    #[test]
    fn roundtrip_every_variant() {
        for msg in roundtrip_cases() {
            let bytes = encode(&msg);
            let decoded = decode(&bytes).unwrap_or_else(|e| panic!("{msg:?}: {e}"));
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn decode_rejects_short_buffers() {
        for msg in roundtrip_cases() {
            let bytes = encode(&msg);
            assert!(!bytes.is_empty());
            for cut in 0..bytes.len() {
                let result = std::panic::catch_unwind(|| decode(&bytes[..cut]));
                let outcome = result.expect("decode panicked on truncated input");
                assert!(outcome.is_err(), "truncated {msg:?} at {cut} decoded ok");
            }
        }
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        for msg in roundtrip_cases() {
            let mut bytes = encode(&msg);
            let tail = [0u8, 0xFF, 0x42];
            for extra in tail {
                bytes.push(extra);
                assert!(
                    decode(&bytes).is_err(),
                    "trailing byte accepted after {msg:?}"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_unknown_variant() {
        // fixint encoding: the variant index is the first u32 (little
        // endian). Replace it with out-of-range values.
        for bogus_index in [6u32, 7, 99, u32::MAX] {
            let mut bytes = encode(&NetMsg::Bye);
            bytes[..4].copy_from_slice(&bogus_index.to_le_bytes());
            let result = std::panic::catch_unwind(move || decode(&bytes))
                .expect("decode panicked on unknown variant index");
            assert!(result.is_err(), "variant index {bogus_index} accepted");
        }
    }

    #[test]
    fn decode_rejects_empty_and_garbage_never_panics() {
        assert!(decode(&[]).is_err());
        // Deterministic pseudo-random byte soup: every input must return,
        // Ok or Err, without panicking (host robustness with Unsecure auth).
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        for len in 0..64u64 {
            let mut bytes = Vec::new();
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push((state & 0xFF) as u8);
            }
            let _ = std::panic::catch_unwind(move || decode(&bytes))
                .expect("decode panicked on garbage bytes");
        }
        // Byte values around valid variant tags with plausible tails.
        for prefix in [
            vec![0u8, 0, 0, 0],
            vec![5u8, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF],
            vec![3u8, 0, 0, 0, 0x80, 0x80],
            vec![0xFF; 8],
        ] {
            let _ = std::panic::catch_unwind(move || decode(&prefix))
                .expect("decode panicked on adversarial prefix");
        }
    }

    fn match_at(seed: u64, rule: AttackRule, ticks: usize) -> MatchSnapshot {
        use tetris_core::versus::Match;
        let mut m = Match::new(seed, rule);
        for _ in 0..ticks {
            m.tick(Side::Left);
            m.tick(Side::Right);
        }
        m.snapshot()
    }

    #[test]
    fn snapshot_hash_equal_for_same_state() {
        let a = match_at(42, AttackRule::Garbage, 25);
        let b = match_at(42, AttackRule::Garbage, 25);
        assert_eq!(a, b, "fixture determinism broken (core bug?)");
        assert_eq!(snapshot_hash(&a), snapshot_hash(&b));
        // and stable when re-hashing the same snapshot
        assert_eq!(snapshot_hash(&a), snapshot_hash(&a));
    }

    #[test]
    fn snapshot_hash_differs_for_different_states() {
        use tetris_core::versus::Match;
        let base = match_at(42, AttackRule::Garbage, 25);
        let other_seed = match_at(43, AttackRule::Garbage, 25);
        let other_rule = match_at(42, AttackRule::Race { target_lines: 40 }, 25);
        // Same seed/rule, but one extra HardDrop applied — a genuinely
        // different state (`GameSnapshot` carries no timers, so pure tick
        // counts only diverge once gravity advances; RED run proved 25 vs
        // 26 ticks are the *same* snapshot).
        let mut acted = Match::new(42, AttackRule::Garbage);
        acted.apply(Side::Left, Action::HardDrop);
        let acted = acted.snapshot();
        let hashes = [
            snapshot_hash(&base),
            snapshot_hash(&other_seed),
            snapshot_hash(&other_rule),
            snapshot_hash(&acted),
        ];
        assert_ne!(base, other_seed);
        assert_ne!(base, other_rule);
        assert_ne!(base, acted);
        for (i, hi) in hashes.iter().enumerate() {
            for (j, hj) in hashes.iter().enumerate() {
                if i != j {
                    assert_ne!(hi, hj, "hash collision between fixtures {i} and {j}");
                }
            }
        }
    }

    // ---- N7: byte-fuzz over `decode` (netplay-plan.md robustness audit) ---
    //
    // Host robustness on hostile bytes: v1 netcode auth is `Unsecure`, so
    // anyone reaching the UDP port with the right `PROTOCOL_ID` completes the
    // transport handshake and lands arbitrary payloads on the reliable
    // channels — every one of them flows through `decode`. The contract:
    //   * `decode` never panics, on any input (nor does `encode` of whatever
    //     it accepts),
    //   * every outcome is `Ok(msg)` **or** `Err(ProtocolError)` — nothing
    //     else exists in the signature, so a panic is the only way to fall
    //     over,
    //   * anything accepted round-trips byte-identically: the fixint +
    //     strict codec has exactly one encoding per value, so there are no
    //     non-canonical shapes to mis-read (malleability would survive even
    //     a panic-free decode, and this catches it).
    //
    // Generation is structured, not just noise: valid encodings of *every*
    // `NetMsg` variant (arbitrary field values) are truncated at arbitrary
    // points, byte-flipped 1..3 times, and extended with random tails —
    // plus pure random buffers. All hostile shapes target exactly the
    // places bincode can go wrong: variant discriminants, fixint sequence
    // lengths (`Vec<Action>`/`String`), UTF-8 boundaries, trailing bytes.
    //
    // 10k cases with a low shrink budget keeps this inside the normal
    // `cargo test` run (the whole suite runs it every push).

    use proptest::prelude::*;
    use proptest::sample::select;

    const ALL_ACTIONS: [Action; 8] = [
        Action::MoveLeft,
        Action::MoveRight,
        Action::SoftDrop,
        Action::HardDrop,
        Action::RotateCw,
        Action::RotateCcw,
        Action::Rotate180,
        Action::Hold,
    ];

    fn fuzz_config() -> ProptestConfig {
        ProptestConfig {
            cases: 10_000,
            max_shrink_iters: 32,
            ..Default::default()
        }
    }

    /// A [`NetMsg`] of any variant with arbitrary field values — the base
    /// for both the round-trip property and the mutation arms below.
    fn net_msg_strategy() -> impl Strategy<Value = NetMsg> {
        let version = ".{0,24}";
        let actions = prop::collection::vec(select(&ALL_ACTIONS), 0..24);
        let sides = select(vec![Side::Left, Side::Right]);
        prop_oneof![
            (version, any::<u8>()).prop_map(|(version, delay)| NetMsg::Hello { version, delay }),
            (any::<u64>(), any::<u8>(), 0u32..2).prop_map(|(seed, match_delay, rule_idx)| {
                let rule = if rule_idx == 0 {
                    AttackRule::Garbage
                } else {
                    AttackRule::Race {
                        target_lines: u32::MAX >> (rule_idx * 4),
                    }
                };
                NetMsg::MatchStart {
                    seed,
                    rule,
                    match_delay,
                }
            }),
            (any::<u64>(), actions.clone())
                .prop_map(|(tick, actions)| NetMsg::TickInput { tick, actions }),
            (any::<u64>(), actions.clone(), actions.clone())
                .prop_map(|(tick, left, right)| NetMsg::TickBatch { tick, left, right }),
            (sides, any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
                |(side, tick, left, right)| NetMsg::SnapshotHash {
                    side,
                    tick,
                    left,
                    right,
                }
            ),
            Just(NetMsg::Bye),
        ]
    }

    /// Deterministic hostile-shape bases: encodings of every variant (the
    /// shared round-trip fixtures plus minimal shapes) for the exhaustive
    /// truncation walk and the fixed-base mutation arm.
    fn variant_table_bases() -> Vec<Vec<u8>> {
        let mut v = roundtrip_cases();
        v.extend([
            NetMsg::MatchStart {
                seed: 0,
                rule: AttackRule::Garbage,
                match_delay: 0,
            },
            NetMsg::TickInput {
                tick: 0,
                actions: vec![Action::HardDrop],
            },
        ]);
        v.iter().map(encode).collect()
    }

    /// One mutation sequence over a valid base: truncate, flip 0..=3 bytes,
    /// append 0..=16 random bytes.
    fn mutate(base: Vec<u8>, cut: usize, flips: Vec<(usize, u8)>, tail: Vec<u8>) -> Vec<u8> {
        let mut bytes = base[..cut.min(base.len())].to_vec();
        for (index, value) in flips {
            if !bytes.is_empty() {
                let at = index % bytes.len();
                bytes[at] = value;
            }
        }
        bytes.extend_from_slice(&tail);
        bytes
    }

    /// The hostile-byte strategy: pure random buffers, plus structured
    /// mutations (truncate / flip / append-garbage) of valid encodings —
    /// bases sampled either from the fixed variant table or freshly
    /// generated messages.
    fn hostile_bytes_strategy() -> impl Strategy<Value = Vec<u8>> {
        let bases = std::sync::Arc::new(variant_table_bases());
        let fixed_base = {
            let bases = std::sync::Arc::clone(&bases);
            (0usize..bases.len()).prop_map(move |i| bases[i].clone())
        };
        let valid_base = net_msg_strategy().prop_map(|msg| encode(&msg));
        let mutated = (
            prop_oneof![fixed_base, valid_base],
            any::<usize>(),
            prop::collection::vec((any::<usize>(), any::<u8>()), 0..3),
            prop::collection::vec(any::<u8>(), 0..16),
        )
            .prop_map(|(base, cut, flips, tail)| mutate(base, cut, flips, tail));
        prop_oneof![
            // Pure noise: empty buffers up to a plausible MTU+.
            prop::collection::vec(any::<u8>(), 0..140),
            // Structured: mutations of valid encodings.
            mutated,
        ]
    }

    /// Compact hex rendering for failure messages.
    fn hex(bytes: &[u8]) -> String {
        let mut s = String::new();
        for b in bytes.iter().take(64) {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Oracle shared by the fuzz properties: `decode` never panics, and
    /// every accepted buffer re-encodes byte-identically (the strict codec
    /// has exactly one canonical encoding per value, so anything else is a
    /// malleability bug). `Err(ProtocolError)` needs no further check —
    /// it is the only non-Ok outcome the signature can even express.
    fn assert_decode_is_robust(bytes: &[u8]) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(bytes)));
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(_) => panic!("decode PANICKED on {}", hex(bytes)),
        };
        if let Ok(msg) = &outcome {
            let re = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode(msg)))
                .unwrap_or_else(|_| panic!("encode panicked for {msg:?}"));
            assert_eq!(
                re,
                bytes,
                "{msg:?} accepted from non-canonical bytes {}",
                hex(bytes)
            );
        }
    }

    proptest! {
        #![proptest_config(fuzz_config())]

        /// 10k hostile-byte cases: `decode` returns (`Ok` or
        /// `Err(ProtocolError)`) and never falls over; accepted buffers are
        /// exactly the canonical encodings.
        #[test]
        fn fuzz_decode_never_panics(ref bytes in hostile_bytes_strategy()) {
            assert_decode_is_robust(bytes);
        }

        /// 10k generated messages: the valid space round-trips exactly, so
        /// the fuzz's rejection behaviour can never hide a codec that just
        /// rejects everything.
        #[test]
        fn fuzz_roundtrip_any_msg(msg in net_msg_strategy()) {
            let bytes = encode(&msg);
            let decoded =
                decode(&bytes).unwrap_or_else(|e| panic!("roundtrip failed for {msg:?}: {e}"));
            assert_eq!(decoded, msg);
            assert_eq!(encode(&decoded), bytes);
        }
    }

    /// Deterministic backstop for the truncation arm (proptest only samples
    /// bases per case; this walks *every* prefix of *every* variant
    /// encoding — bare and with a garbage tail — under the same oracle).
    #[test]
    fn fuzz_variant_truncations_exhaustive() {
        for base in variant_table_bases() {
            for cut in 0..=base.len() {
                assert_decode_is_robust(&base[..cut]);
                assert_decode_is_robust(&[base[..cut].to_vec(), vec![0xAA, 0xFF]].concat());
            }
        }
    }
}
