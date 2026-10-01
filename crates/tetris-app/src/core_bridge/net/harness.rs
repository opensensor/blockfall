//! Netplay test harness (netplay-plan.md N6): the `TETRIS_NET=host:<port>` /
//! `TETRIS_NET=join:<addr>` desktop bot-vs-bot-across-the-wire mode (real
//! `DefaultPlugins` app, human-run) plus the in-process two-`App` end-to-end
//! tests that ride CI's `cargo test --workspace` over **real renet/netcode
//! UDP on loopback** — real session FSM, real lockstep driver, real `Match`es.
//!
//! # Desktop mode (`TETRIS_NET`)
//!
//! Mirrors the `TETRIS_1V1` env-harness pattern (see
//! [`versus_harness_startup`](crate::core_bridge::versus)): the startup hook
//! (wired from `CoreBridgePlugin::build()`, `main.rs` stays frozen) binds or
//! joins, both peers arm their **local** seat to [`Controller::Bot`] (host
//! `Bot+Net`, guest `Net+Bot`) and play
//! [`DESKTOP_RULES`] — Garbage then Race — to crowned winners, then exit.
//! The host drives match starts through [`start_net_match`] (the N4/N5
//! rematch path: the guest re-arms from the wire `MatchStart`, never a local
//! reseed). Per match it logs
//!
//! ```text
//! NET match_start rule=… seed=… delay=…
//! NET match_done seed=… ticks=…
//! NET final_hash left=… right=…
//! ```
//!
//! and after the second match `NET complete matches=2` + `AppExit::Success`
//! (exit 0). Any [`NetEvent::Desync`], or a `PeerLost`/`JoinTimeout`/
//! `ByeReceived` before the mirror has resolved the running match, or 120 s
//! without lockstep progress inside a match → `NET fail …` + **exit 1**.
//! (The failure path calls [`std::process::exit`] directly: the frozen
//! `main()` discards the [`AppExit`] value `.run()` returns, so
//! `AppExit::Error` alone would leave the process at exit code 0.)
//!
//! Both peers print identical `final_hash` pairs — the sign-off check is a
//! plain diff of the two logs. Two-machine LAN runs use the same commands
//! with the host's LAN address (author's gate; netplay-plan.md N6/N5).
//!
//! # Fork injection (`TETRIS_NET_FORK=guest:<tick>` / `host:<tick>`)
//!
//! A test-only seam that simulates one peer's *logic* forking: at (or after)
//! the given lockstep tick the armed peer applies one extra local
//! [`Action::HardDrop`] straight to its own `Match`, off the batch stream.
//! (It cannot flow through `schedule_local`/`TickInput` — that path ships the
//! action to both peers, so a wire-visible action can never fork the mirrors;
//! the hook must diverge one side's state, exactly like N3's
//! `desync_detected_on_forked_mirror` fake-transport test.) The periodic
//! [`SnapshotHash`](super::protocol::NetMsg::SnapshotHash) exchange must
//! then fire [`NetEvent::Desync`] — which is precisely the non-vacuity proof
//! the E2E asserts: identical streams on clean runs, a detected divergence
//! once a fork is injected. In tests the [`NetForkHook`] resource is inserted
//! directly (no process-global env mutation); production reads
//! [`NET_FORK_ENV`] in the startup hook.
//!
//! # CI E2E (this module's `tests`)
//!
//! Two `MinimalPlugins` apps, one process, real UDP loopback: host seat
//! `Bot`, guest seat `Bot`, a full Garbage match to a crowned winner, with
//! per-60-tick snapshot-hash streams recorded *independently of the lockstep
//! windows* (a `FixedUpdate` recorder running after both step systems) and
//! asserted equal throughout, final snapshots equal, no `Desync`/`Lost`
//! events, clean `net_stop` teardown. The fork test arms the hook on the
//! guest and asserts the equality check **fires** (`Desync` + freeze +
//! unequal streams).
//!
//! Port strategy: **port 0** (OS-assigned), like N3's loopback smoke and N4's
//! seed-propagation test. None of these tests need a known port — and after
//! the N6 follow-up no netplay test binds a fixed port at all — so a
//! parallel foreign suite can no longer collide with them (the old shared
//! `TETRIS_TEST_NET_PORT` flaked exactly that way).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use tetris_core::actions::Action;
use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

use super::lockstep::local_side;
use super::session::{net_host, net_join, net_stop, NetEvent, NetRole, NetSession, NetStatus};
use crate::core_bridge::wall_clock_seed;
use crate::core_bridge::{start_net_match, Controller, VersusMatch, VersusWinner};
use crate::state::AppState;

/// Env var selecting the desktop netplay harness mode: `host:<port>` or
/// `join:<ip:port>` (netplay-plan.md N6; mirrors `TETRIS_1V1`).
pub const NET_ENV: &str = "TETRIS_NET";

/// Env var arming the fork-injection hook: `guest:<tick>` or `host:<tick>`
/// (test-only; see the module docs).
pub const NET_FORK_ENV: &str = "TETRIS_NET_FORK";

/// Matches the desktop harness plays before exiting (plan: garbage, race).
pub const DESKTOP_MATCHES: u32 = 2;

/// The two rules, in order: match 1 Garbage, match 2 Race (plan N6).
pub const DESKTOP_RULES: [AttackRule; 2] = [
    AttackRule::Garbage,
    AttackRule::Race {
        target_lines: DEFAULT_RACE_LINES,
    },
];

/// Pause between a logged match and the host's next `MatchStart` (lets both
/// peers observe the crowning and drain the last hash exchanges before the
/// mirror is reseeded).
const REMATCH_HOLD: Duration = Duration::from_secs(2);

/// Post-final-match grace before exiting (keeps the last `SnapshotHash`
/// exchanges flowing so both logs close on the same state).
const EXIT_HOLD: Duration = Duration::from_secs(2);

/// A live match whose lockstep tick has not advanced for this long is a
/// stall: the harness fails (exit 1) instead of hanging forever.
pub const NET_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Parsed [`NET_ENV`] mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetHarnessMode {
    /// Bind `0.0.0.0:<port>` and wait for the guest.
    Host(u16),
    /// Join the host at this address.
    Join(SocketAddr),
}

/// Parse a [`NET_ENV`] value (`host:34857`, `join:127.0.0.1:34857`,
/// `join:[::1]:34857`). Case-insensitive role, surrounding whitespace ok.
#[must_use]
pub fn parse_net_spec(value: &str) -> Option<NetHarnessMode> {
    let (role, rest) = value.trim().split_once(':')?;
    match role.trim().to_ascii_lowercase().as_str() {
        "host" => Some(NetHarnessMode::Host(rest.trim().parse().ok()?)),
        "join" => Some(NetHarnessMode::Join(rest.trim().parse().ok()?)),
        _ => None,
    }
}

/// Parse a [`NET_FORK_ENV`] value (`guest:<tick>` / `host:<tick>`).
#[must_use]
pub fn parse_fork_spec(value: &str) -> Option<(NetRole, u64)> {
    let (role, tick) = value.trim().split_once(':')?;
    let role = match role.trim().to_ascii_lowercase().as_str() {
        "host" => NetRole::Host,
        "guest" => NetRole::Guest,
        _ => return None,
    };
    Some((role, tick.trim().parse().ok()?))
}

/// Runtime [`NET_ENV`] value, if set and well-formed (malformed values are
/// warned about and otherwise ignored — the harness simply stays off).
fn net_env_mode() -> Option<NetHarnessMode> {
    let raw = std::env::var(NET_ENV).ok()?;
    match parse_net_spec(&raw) {
        Some(mode) => Some(mode),
        None => {
            warn!("net harness: ignoring malformed {NET_ENV}={raw:?}");
            None
        }
    }
}

/// Armed fork injection (module docs). `applied` latches so exactly one
/// action is ever diverted.
#[derive(Resource, Debug)]
pub struct NetForkHook {
    /// Which peer forks its own mirror.
    pub role: NetRole,
    /// Apply once the local lockstep tick reaches this value.
    pub tick: u64,
    /// The fork has been applied.
    pub applied: bool,
}

/// Fork one local action straight into the armed peer's mirror (`FixedUpdate`,
/// inert without the resource). Diverges the local `Match` from the peer's
/// wire-driven mirror, so the next hash check must flag a desync.
pub fn net_fork_system(
    mut hook: Option<ResMut<NetForkHook>>,
    session: Option<Res<NetSession>>,
    mut versus: Option<NonSendMut<VersusMatch>>,
    lockstep: Option<Res<super::lockstep::NetLockstep>>,
) {
    let (Some(hook), Some(session), Some(versus), Some(lockstep)) = (
        hook.as_deref_mut(),
        session,
        versus.as_deref_mut(),
        lockstep,
    ) else {
        return;
    };
    if hook.applied
        || session.status != NetStatus::InMatch
        || session.role != hook.role
        || !versus.active
        || lockstep.tick < hook.tick
    {
        return;
    }
    let side = local_side(session.role);
    versus.match_.apply(side, Action::HardDrop);
    hook.applied = true;
    warn!(
        "NET FORK: injected one off-wire action for {side:?} at tick {} ({NET_FORK_ENV}) — the hash check must fire",
        hook.tick
    );
}

/// Desktop-harness bookkeeping (module docs). Absent = mode disabled.
#[derive(Resource)]
struct NetDesktopHarness {
    mode: NetHarnessMode,
    matches_done: u32,
    /// Seed of the last match whose crowning has been logged. Keyed by seed
    /// (not a `logged` bool) so *both* roles count each `MatchStart`'s result:
    /// only the host runs [`start_desktop_match`], so the guest needs a reset
    /// that a fresh wire match seed provides.
    last_crowned_seed: Option<u64>,
    /// Host: when to send the next `MatchStart` (post-crown hold).
    restart_at: Option<Instant>,
    /// Both matches done: when to tear down and exit successfully.
    exit_at: Option<Instant>,
    /// Stall watchdog: last observed lockstep tick and when it was seen.
    stall_tick: u64,
    stall_since: Instant,
}

/// Startup hook (mirrors the `TETRIS_1V1` handling): with [`NET_ENV`] set,
/// bind/join the session and arm the desktop harness; with [`NET_FORK_ENV`]
/// also set, arm the fork hook. Inert no-op when unset (every headless test
/// app pays only the env reads).
pub fn net_harness_startup(world: &mut World) {
    let Some(mode) = net_env_mode() else { return };
    if let Ok(raw) = std::env::var(NET_FORK_ENV) {
        match parse_fork_spec(&raw) {
            Some((role, tick)) => {
                warn!("net harness: fork injection armed for {role:?} at tick {tick} ({NET_FORK_ENV})");
                world.insert_resource(NetForkHook {
                    role,
                    tick,
                    applied: false,
                });
            }
            None => warn!("net harness: ignoring malformed {NET_FORK_ENV}={raw:?}"),
        }
    }
    info!("net harness: {mode:?} (from {NET_ENV}) — bot-vs-bot across the wire");
    world.insert_resource(NetDesktopHarness {
        mode,
        matches_done: 0,
        last_crowned_seed: None,
        restart_at: None,
        exit_at: None,
        stall_tick: 0,
        stall_since: Instant::now(),
    });
    match mode {
        NetHarnessMode::Host(port) => net_host(world, port),
        NetHarnessMode::Join(addr) => net_join(world, addr),
    }
}

/// `NET fail …` + graceful peer teardown + hard exit 1. Direct process exit
/// because the frozen `main()` drops the [`AppExit`] value (module docs).
///
/// On unix this goes through libc `_exit` rather than [`std::process::exit`]:
/// a normal `exit()` runs atexit handlers while the winit/GPU threads are
/// still live mid-frame, which segfaults (exit 139, not 1 — verified on
/// X11 + NVIDIA). `_exit` skips the handlers entirely, so the exit code is
/// exactly 1; stderr is flushed first so the `NET fail` line survives.
fn net_harness_fail(world: &mut World, reason: &str) -> ! {
    error!("NET fail {reason}");
    net_stop(world);
    net_harness_exit_failure()
}

fn net_harness_exit_failure() -> ! {
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let _ = std::io::stdout().flush();
    #[cfg(unix)]
    {
        extern "C" {
            fn _exit(code: std::ffi::c_int) -> !;
        }
        // SAFETY: `_exit` is async-signal-safe and terminates the process
        // immediately; no Rust destructors run (intentional — the whole app
        // is being torn down).
        unsafe { _exit(1) }
    }
    #[cfg(not(unix))]
    std::process::exit(1)
}

/// FNV-1a-64 over the bincode bytes of one side's `GameSnapshot` — the same
/// recipe as [`super::protocol::snapshot_hash`] (which is whole-match), so the
/// `NET final_hash left=… right=…` line carries genuinely per-side values.
fn side_hash(snapshot: &tetris_core::game::GameSnapshot) -> u64 {
    let bytes = bincode::serialize(snapshot).expect("GameSnapshot serialization is infallible");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Host: arm the local seat to `Bot` and start mirror `index` via
/// [`start_net_match`] (the N4 lifecycle: `SEED_ENV` wins over the wall-clock
/// seed, `MatchStart` goes on the wire, the guest mirrors from it).
///
/// No post-start hold is needed: the session-layer wire drains stop exactly
/// at the guest's `MatchStart` transition (`session` module docs'
/// consume-on-read discipline), so any `TickBatch` the host produces before
/// the guest mirror materializes stays on the reliable channel, is buffered
/// by N3's lockstep and replayed in order — the stall regression test
/// ([`matchstart_transition_frame_does_not_swallow_queued_tickbatches`] in
/// the tests) pins that. (The 250 ms `SimPaused` hold this used to arm was
/// a workaround for the pre-match drain discarding those batches.)
fn start_desktop_match(world: &mut World, rule: AttackRule) {
    if let Some(mut versus) = world.get_non_send_mut::<VersusMatch>() {
        versus.p1 = Controller::Bot;
    }
    *world.resource_mut::<AppState>() = AppState::Playing;
    let delay = world.resource::<NetSession>().input_delay;
    let seed = wall_clock_seed();
    info!("NET match_start rule={rule:?} seed={seed} delay={delay}");
    start_net_match(world, rule, Side::Left, seed, delay);
    // `last_crowned_seed` needs no reset: the fresh wall-clock seed is not yet
    // recorded, so the crowning block will fire for this match on both roles.
}

/// The desktop loop (mirrors `versus_harness_update`): host starts matches,
/// both sides arm their Bot seat, crowned matches are logged, the host
/// rematches through `MatchStart`, the second crowning exits clean; desync /
/// unexpected loss / stalls exit 1.
pub fn net_harness_update(world: &mut World) {
    if !world.contains_resource::<NetDesktopHarness>() {
        return;
    }
    let role = world.resource::<NetSession>().role;
    let status = world.resource::<NetSession>().status.clone();
    let versus_active = world
        .get_non_send::<VersusMatch>()
        .is_some_and(|v| v.active);
    let mut winner = world.resource::<VersusWinner>().0;
    let tick = world.resource::<super::lockstep::NetLockstep>().tick;
    let now = Instant::now();

    let mut harness = world
        .remove_resource::<NetDesktopHarness>()
        .expect("checked above");

    // Host: first match once the peer is Ready; later matches when the
    // post-crown hold expires (re-arms through the wire MatchStart).
    if role == NetRole::Host && harness.matches_done < DESKTOP_MATCHES {
        let start_now = match harness.restart_at {
            None => status == NetStatus::Ready && !versus_active,
            Some(at) => now >= at,
        };
        if start_now {
            harness.restart_at = None;
            let rule = DESKTOP_RULES[harness.matches_done as usize];
            start_desktop_match(world, rule);
            // The fresh mirror cleared VersusWinner; the `winner` read above
            // still holds the PREVIOUS match's crowning — drop it so the
            // rematch is never logged as an instant 0-tick winner.
            winner = None;
        }
    }

    // Guest: arm the local seat to a Bot once a mirror is live (survives the
    // rematch rebuild — setup_net_mirror preserves the local controller).
    if role == NetRole::Guest && versus_active {
        if let Some(mut versus) = world.get_non_send_mut::<VersusMatch>() {
            if versus.p2 != Controller::Bot {
                versus.p2 = Controller::Bot;
                info!("net harness: guest local seat armed (Controller::Bot)");
            }
        }
    }

    // Crowning is deterministic on both mirrors: log once per match, keyed by
    // the freshly-crowned match's seed so the guest — which never runs
    // `start_desktop_match` — still counts every wire `MatchStart`.
    if versus_active && winner.is_some() {
        let versus = world.non_send::<VersusMatch>();
        if harness.last_crowned_seed != Some(versus.seed) {
            harness.last_crowned_seed = Some(versus.seed);
            harness.matches_done += 1;
            let (left, right) = (
                versus.match_.left.snapshot(),
                versus.match_.right.snapshot(),
            );
            info!("NET match_done seed={} ticks={}", versus.seed, versus.steps);
            info!(
                "NET final_hash left={left} right={right}",
                left = side_hash(&left),
                right = side_hash(&right),
            );
            if harness.matches_done >= DESKTOP_MATCHES {
                harness.exit_at = Some(now + EXIT_HOLD);
            } else {
                harness.restart_at = Some(now + REMATCH_HOLD);
            }
        }
    }

    // Done: graceful netcode disconnect (peer learns instantly), exit 0.
    if harness.exit_at.is_some_and(|at| now >= at) {
        info!("NET complete matches={}", harness.matches_done);
        net_stop(world);
        world.insert_resource(harness);
        world.write_message(AppExit::Success);
        return;
    }

    // Session events: Desync always fails; a connection loss only fails if it
    // arrives before the running mirror has resolved (a peer that finished
    // and left while we hold on for EXIT_HOLD must not fail the run).
    let events: Vec<NetEvent> = world.resource_mut::<Messages<NetEvent>>().drain().collect();
    let resolved = harness.matches_done > 0 || winner.is_some();
    for event in events {
        match event {
            NetEvent::Desync { tick } => {
                net_harness_fail(world, &format!("desync detected at tick {tick}"))
            }
            NetEvent::PeerLost(reason) => {
                if resolved {
                    // Peer finished and left: clean success, mirroring the
                    // host's own exit path (`AppExit::Success`, no
                    // `std::process::exit` — that segfaults when invoked
                    // mid-frame with GPU/tracing atexit handlers loaded).
                    info!(
                        "NET peer left after resolved run (matches_done={}) exiting clean: {reason:?}",
                        harness.matches_done
                    );
                    net_stop(world);
                    world.insert_resource(harness);
                    world.write_message(AppExit::Success);
                    return;
                }
                net_harness_fail(world, &format!("connection lost: {reason:?}"))
            }
            NetEvent::JoinTimeout => {
                if !resolved {
                    net_harness_fail(world, "join timed out (start the host first)")
                }
            }
            NetEvent::ByeReceived => {
                if resolved {
                    info!(
                        "NET peer left after resolved run (matches_done={}) exiting clean",
                        harness.matches_done
                    );
                    net_stop(world);
                    world.insert_resource(harness);
                    world.write_message(AppExit::Success);
                    return;
                }
                net_harness_fail(world, "peer left before the match resolved")
            }
            NetEvent::PeerConnected | NetEvent::VersionMismatch | NetEvent::BindFailed(_) => {}
        }
    }

    // Stall watchdog: InMatch without lockstep progress for NET_STALL_TIMEOUT.
    if status == NetStatus::InMatch && versus_active && winner.is_none() {
        if tick != harness.stall_tick {
            harness.stall_tick = tick;
            harness.stall_since = now;
        } else if now - harness.stall_since >= NET_STALL_TIMEOUT {
            net_harness_fail(
                world,
                &format!("lockstep stalled at tick {tick} for {NET_STALL_TIMEOUT:?}"),
            )
        }
    } else {
        harness.stall_since = now;
        harness.stall_tick = tick;
    }

    world.insert_resource(harness);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::net::lockstep::{
        net_lockstep_guest_system, net_lockstep_host_system, NetLockstep, HASH_CHECK_PERIOD,
    };
    use crate::core_bridge::net::protocol;
    use crate::core_bridge::{CoreBridgePlugin, VersusEvent};
    use crate::input::VersusActions;
    use crate::state::{RebindingCapture, Settings};
    use bevy::time::Virtual;
    use std::net::{Ipv4Addr, UdpSocket};
    use tetris_core::versus::{MatchEvent, MatchSnapshot, MAX_GARBAGE_PER_LAND};

    // ---- pure parsing (never touches the environment) ---------------------

    #[test]
    fn parses_net_spec() {
        assert_eq!(
            parse_net_spec("host:34857"),
            Some(NetHarnessMode::Host(34857))
        );
        assert_eq!(
            parse_net_spec(" HOST : 8080 "),
            Some(NetHarnessMode::Host(8080))
        );
        assert_eq!(
            parse_net_spec("join:127.0.0.1:34857"),
            Some(NetHarnessMode::Join("127.0.0.1:34857".parse().unwrap()))
        );
        assert_eq!(
            parse_net_spec("Join:[::1]:9000"),
            Some(NetHarnessMode::Join("[::1]:9000".parse().unwrap()))
        );
        assert_eq!(parse_net_spec(""), None);
        assert_eq!(parse_net_spec("host"), None);
        assert_eq!(parse_net_spec("host:notaport"), None);
        assert_eq!(parse_net_spec("join:127.0.0.1"), None);
        assert_eq!(parse_net_spec("relay:1.2.3.4:5"), None);
    }

    #[test]
    fn parses_fork_spec() {
        assert_eq!(parse_fork_spec("guest:120"), Some((NetRole::Guest, 120)));
        assert_eq!(parse_fork_spec(" HOST :5"), Some((NetRole::Host, 5)));
        assert_eq!(parse_fork_spec("guest:abc"), None);
        assert_eq!(parse_fork_spec("peer:5"), None);
        assert_eq!(parse_fork_spec("guest"), None);
    }

    // ---- fork hook (no sockets): applies exactly one off-wire action ------

    #[test]
    fn fork_hook_applies_one_offwire_action_at_tick() {
        let mut app = peer_app(1.0);
        // Arm a host mirror without a transport (same seam as N4's
        // end_net_match test) and flip the session InMatch by hand.
        start_net_match(app.world_mut(), AttackRule::Garbage, Side::Left, 7, 4);
        app.world_mut().resource_mut::<NetSession>().status = NetStatus::InMatch;
        let before = app.world().non_send::<VersusMatch>().match_.snapshot();
        app.world_mut().resource_mut::<NetLockstep>().tick = 90;
        app.world_mut().insert_resource(NetForkHook {
            role: NetRole::Guest, // wrong role: must not fire
            tick: 90,
            applied: false,
        });
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().non_send::<VersusMatch>().match_.snapshot(),
            before,
            "the hook must not fork a peer of the other role"
        );
        *app.world_mut().resource_mut::<NetForkHook>() = NetForkHook {
            role: NetRole::Host,
            tick: 91,
            applied: false,
        };
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().non_send::<VersusMatch>().match_.snapshot(),
            before,
            "the hook waits for the tick"
        );
        *app.world_mut().resource_mut::<NetForkHook>() = NetForkHook {
            role: NetRole::Host,
            tick: 90,
            applied: false,
        };
        app.world_mut().run_schedule(FixedUpdate);
        let after = app.world().non_send::<VersusMatch>().match_.snapshot();
        assert_ne!(after, before, "the injected action must fork the mirror");
        assert!(app.world().resource::<NetForkHook>().applied);
        // Once is enough: a second fixed step adds nothing further.
        let once = after;
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().non_send::<VersusMatch>().match_.snapshot(),
            once,
            "the hook fires exactly once"
        );
    }

    #[test]
    fn startup_is_inert_without_env() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(CoreBridgePlugin);
        net_harness_startup(app.world_mut());
        assert!(!app.world().contains_resource::<NetDesktopHarness>());
        assert!(!app.world().contains_resource::<NetForkHook>());
        // The update system must be a no-op with no resource (no panic).
        app.world_mut().insert_resource(AppState::Title);
        net_harness_update(app.world_mut());
    }

    // =========================================================================
    // CI end-to-end: two apps, real renet/netcode UDP loopback, real session
    // FSM + lockstep driver + `Match`, bot-vs-bot to a crowned winner.
    //
    // Not RED-before-implementation in its happy path (the wiring cannot fail
    // before it exists — same honest note the N2 loopback test carries). The
    // non-vacuity lives in (a) the independent per-60-tick hash streams that
    // `e2e_fork_injection_detected` proves *can* and *does* diverge when one
    // peer's logic forks, and (b) the Desync assertion that only a working
    // hash check can satisfy.
    // =========================================================================

    /// Boundary hashes recorded **independently** of the lockstep windows: a
    /// `FixedUpdate` system after both step systems samples the match hash at
    /// exactly the ticks the wire `SnapshotHash` messages are emitted, so the
    /// test compares the same quantities the desync check does (and can spot
    /// a check that never fired because nothing was ever compared).
    #[derive(Resource, Default)]
    struct HashStream(Vec<(u64, u64)>);

    fn record_boundary_hashes(
        session: Option<Res<NetSession>>,
        versus: Option<NonSend<VersusMatch>>,
        lockstep: Option<Res<NetLockstep>>,
        mut stream: ResMut<HashStream>,
        mut last: Local<u64>,
        mut last_seed: Local<Option<u64>>,
    ) {
        let (Some(session), Some(versus), Some(lockstep)) = (session, versus, lockstep) else {
            return;
        };
        if session.status != NetStatus::InMatch || !versus.active {
            return;
        }
        if *last_seed != Some(versus.seed) {
            // A mirror (re)build — start the per-match stream fresh. In the
            // multi-match soak this also drops the one boundary a lagging
            // peer can still record from the OLD mirror (it ticks once more
            // before its rebuild lands), so the stream is per-match and
            // strictly increasing on both sides. The single-match E2Es
            // never change seed, so this never fires for them.
            *last_seed = Some(versus.seed);
            stream.0.clear();
            *last = 0;
        }
        let tick = lockstep.tick;
        if tick > 0 && tick != *last && tick.is_multiple_of(HASH_CHECK_PERIOD) {
            *last = tick;
            stream
                .0
                .push((tick, protocol::snapshot_hash(&versus.match_.snapshot())));
        }
    }

    /// One netplay peer: the full production stack (solo bridge + versus
    /// bridge + net plugin + input plugin) headless under `MinimalPlugins`,
    /// `AppState::Playing` open from frame 0 (mirrors N4's `net_versus_app`).
    fn peer_app(speed: f64) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((CoreBridgePlugin, crate::input::InputPlugin));
        app.init_resource::<AppState>()
            .init_resource::<Settings>()
            .init_resource::<RebindingCapture>()
            .init_resource::<HashStream>();
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        // Virtual time scales the fixed stepping only — `Time<Real>` (and
        // with it netcode's connection timing and the session watchdogs)
        // stays wall-clock, so the whole pipeline still runs for real.
        app.world_mut()
            .resource_mut::<Time<Virtual>>()
            .set_relative_speed_f64(speed);
        app.add_systems(
            FixedUpdate,
            record_boundary_hashes
                .after(net_lockstep_host_system)
                .after(net_lockstep_guest_system),
        );
        app
    }

    fn session_status(app: &App) -> NetStatus {
        app.world().resource::<NetSession>().status.clone()
    }

    fn lockstep_tick(app: &App) -> u64 {
        app.world().resource::<NetLockstep>().tick
    }

    fn stall_steps(app: &App) -> u64 {
        app.world().resource::<NetLockstep>().stall_steps
    }

    fn versus_snapshot(app: &App) -> MatchSnapshot {
        app.world().non_send::<VersusMatch>().match_.snapshot()
    }

    fn crowned(app: &App) -> Option<Side> {
        app.world().resource::<VersusWinner>().0
    }

    fn stream(app: &App) -> Vec<(u64, u64)> {
        app.world().resource::<HashStream>().0.clone()
    }

    /// Drive both apps (real wall clock, sped-up virtual time) until `done`,
    /// failing with a full pipeline diagnostic on timeout.
    fn drive_until(
        host: &mut App,
        guest: &mut App,
        timeout: Duration,
        label: &str,
        mut done: impl FnMut(&mut App, &mut App) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            host.update();
            guest.update();
            if done(host, guest) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "E2E stuck at {label}: host {:?} tick {} stalls {}, guest {:?} tick {} stalls {}",
                session_status(host),
                lockstep_tick(host),
                stall_steps(host),
                session_status(guest),
                lockstep_tick(guest),
                stall_steps(guest),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Host + guest connected and handshaked (`Ready` on both) over real UDP
    /// loopback on an OS-assigned port. `PeerConnected` messages are drained
    /// so later `Desync`/`Lost` scans see only mid-match events.
    fn connect_pair(speed: f64) -> (App, App) {
        let mut host = peer_app(speed);
        let mut guest = peer_app(speed);
        net_host(host.world_mut(), 0);
        let listen = host
            .world()
            .resource::<NetSession>()
            .listen_addr
            .expect("host must publish its bound address");
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), listen.port());
        net_join(guest.world_mut(), addr);
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "handshake (both Ready)",
            |h, g| session_status(h) == NetStatus::Ready && session_status(g) == NetStatus::Ready,
        );
        drain_events(&mut host);
        drain_events(&mut guest);
        (host, guest)
    }

    fn drain_events(app: &mut App) -> Vec<NetEvent> {
        app.world_mut()
            .resource_mut::<Messages<NetEvent>>()
            .drain()
            .collect()
    }

    /// Start the match **live** — no `SimPaused` "quiet window" gate (the N6
    /// workaround was removed together with the drain bug it papered over):
    /// the host begins producing batches immediately, so every E2E run
    /// crosses the exact `MatchStart`-transition window the stall regression
    /// test (`matchstart_transition_frame_does_not_swallow_queued_tickbatches`)
    /// pins — the guest must catch up from the batches queued on the wire.
    fn start_net_match_live(host: &mut App, rule: AttackRule, seed: u64, delay: u8) {
        start_net_match(host.world_mut(), rule, Side::Left, seed, delay);
        assert_eq!(session_status(host), NetStatus::InMatch);
    }

    /// The CI end-to-end test: a full Garbage match between two Bots across
    /// two apps and a real netcode UDP link, to a crowned winner, with the
    /// per-60-tick hash streams equal throughout and equal final snapshots.
    #[test]
    fn e2e_bot_vs_bot_garbage_match_over_udp() {
        let (mut host, mut guest) = connect_pair(12.0);

        let seed = 0xE2E5_E2E0_0000_0001;
        let rule = AttackRule::Garbage;
        let delay = 4;
        // Host arms its local seat first: setup_net_mirror preserves it.
        host.world_mut().non_send_mut::<VersusMatch>().p1 = Controller::Bot;
        start_net_match_live(&mut host, rule, seed, delay);

        // The guest mirror must materialize purely from the wire MatchStart.
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "guest mirror active",
            |_, g| {
                session_status(g) == NetStatus::InMatch
                    && g.world().non_send::<VersusMatch>().active
            },
        );
        guest.world_mut().non_send_mut::<VersusMatch>().p2 = Controller::Bot;

        assert_eq!(
            guest.world().non_send::<VersusMatch>().seed,
            seed,
            "the guest must mirror the wire seed"
        );

        // Play to a crowned winner, asserting every drained event is benign.
        let mut saw_desync = Vec::new();
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(120),
            "crowned winner",
            |h, g| {
                for event in drain_events(h).into_iter().chain(drain_events(g)) {
                    if matches!(event, NetEvent::Desync { .. }) {
                        saw_desync.push(event);
                    }
                }
                crowned(h).is_some() && crowned(g).is_some()
            },
        );
        assert!(
            saw_desync.is_empty(),
            "a clean match must never desync: {saw_desync:?}"
        );

        // Lockstep ran for real, and the loser really topped out.
        assert!(
            lockstep_tick(&host) > 120 && lockstep_tick(&guest) > 120,
            "match too short: host tick {}, guest tick {}",
            lockstep_tick(&host),
            lockstep_tick(&guest),
        );
        let (hw, gw) = (crowned(&host), crowned(&guest));
        assert_eq!(hw, gw, "both peers must crown the same winner");
        let dead = match hw {
            Some(Side::Left) => versus_snapshot(&guest).right.game_over,
            _ => versus_snapshot(&guest).left.game_over,
        };
        assert!(dead, "the loser's board is topped out");

        // Keep driving until both peers have recorded several hash
        // boundaries (the mirrors tick at equal rates, so the guest keeps a
        // constant lag instead of catching up — compare on the common
        // prefix).
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "hash streams >= 3 on both peers",
            |h, g| stream(h).len() >= 3 && stream(g).len() >= 3,
        );
        let (mut hs, gs) = (stream(&host), stream(&guest));
        let common = hs.len().min(gs.len());
        hs.truncate(common);
        let common = &gs[..gs.len().min(common)];
        assert!(
            hs.len() >= 3,
            "expected several hash boundaries over a full match, got {hs:?}"
        );
        assert_eq!(
            hs, common,
            "per-{}-tick SnapshotHash streams diverged",
            HASH_CHECK_PERIOD
        );
        assert_eq!(
            versus_snapshot(&host),
            versus_snapshot(&guest),
            "final snapshots must be equal"
        );
        assert_eq!(
            protocol::snapshot_hash(&versus_snapshot(&host)),
            protocol::snapshot_hash(&versus_snapshot(&guest)),
        );

        // Clean shutdown on both peers: Idle, no transport resources, port
        // released (a fresh socket can take it).
        let port = host
            .world()
            .resource::<NetSession>()
            .listen_addr
            .map(|a| a.port());
        net_stop(host.world_mut());
        net_stop(guest.world_mut());
        assert_eq!(session_status(&host), NetStatus::Idle);
        assert_eq!(session_status(&guest), NetStatus::Idle);
        assert!(!host.world().contains_resource::<bevy_renet::RenetServer>());
        assert!(!guest.world().contains_resource::<bevy_renet::RenetClient>());
        if let Some(port) = port {
            assert!(
                UdpSocket::bind(("127.0.0.1", port)).is_ok(),
                "net_stop must free the bound port {port}"
            );
        }
    }

    /// Non-vacuity proof for the E2E: with one off-wire action injected into
    /// the guest's mirror at tick 90 (the `TETRIS_NET_FORK` seam), the very
    /// equality assertions the clean test relies on **must** fire — the
    /// recorded hash streams part company at the next boundary and the
    /// lockstep reports `NetEvent::Desync { tick: 119 }` on both peers with
    /// the documented freeze (`SimPaused`).
    #[test]
    fn e2e_fork_injection_detected() {
        let (mut host, mut guest) = connect_pair(12.0);
        let seed = 0xE2E5_E2E0_0000_0002;
        start_net_match_live(&mut host, AttackRule::Garbage, seed, 4);
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "guest mirror active",
            |_, g| {
                session_status(g) == NetStatus::InMatch
                    && g.world().non_send::<VersusMatch>().active
            },
        );
        guest.world_mut().insert_resource(NetForkHook {
            role: NetRole::Guest,
            tick: 90,
            applied: false,
        });

        let mut host_desyncs = Vec::new();
        let mut guest_desyncs = Vec::new();
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(60),
            "desync detection",
            |h, g| {
                host_desyncs.extend(
                    drain_events(h)
                        .into_iter()
                        .filter(|e| matches!(e, NetEvent::Desync { .. })),
                );
                guest_desyncs.extend(
                    drain_events(g)
                        .into_iter()
                        .filter(|e| matches!(e, NetEvent::Desync { .. })),
                );
                !host_desyncs.is_empty() && !guest_desyncs.is_empty()
            },
        );

        assert!(guest.world().resource::<NetForkHook>().applied);
        for desyncs in [&host_desyncs, &guest_desyncs] {
            assert!(
                desyncs.contains(&NetEvent::Desync { tick: 119 }),
                "both peers must see Desync at the first boundary after the fork: {desyncs:?}"
            );
        }
        // Teardown contract: the freeze happened on both frozen peers.
        assert!(host.world().resource::<crate::core_bridge::SimPaused>().0);
        assert!(guest.world().resource::<crate::core_bridge::SimPaused>().0);

        // The independent stream equality check itself must now be false at
        // the forked boundary — the clean-run assertion is proven to be able
        // to fail.
        let common = stream(&host)
            .into_iter()
            .filter(|(t, _)| stream(&guest).iter().any(|(gt, _)| gt == t))
            .collect::<Vec<_>>();
        let diverged = common.iter().any(|(t, h)| {
            stream(&guest)
                .into_iter()
                .find(|(gt, _)| gt == t)
                .is_some_and(|(_, g)| g != *h)
        });
        assert!(
            diverged,
            "recorded streams must actually differ once forked (common: {common:?})"
        );

        net_stop(host.world_mut());
        net_stop(guest.world_mut());
    }

    /// Regression (N6 routed fixup — production stall): the guest's
    /// `MatchStart` transition frame must stop consuming the reliable channel
    /// at the transition, leaving every `TickBatch` queued behind `MatchStart`
    /// in the same poll window for N3's lockstep drain. The pre-fix
    /// `guest_net_system` drained the whole channel past the transition and
    /// discarded the batches ("ignoring … before match start") — and renet
    /// consumes reliable messages *on read*, so the lost batch 0 was never
    /// resent and the mirror stalled at tick 0 forever.
    ///
    /// The window is deterministic: handshaked pair, match started while the
    /// guest is never `update()`d — every packet the host sends sits unread in
    /// the guest's socket, and the netcode client delivers the whole backlog
    /// inside one poll (`update` reads until WouldBlock).
    #[test]
    fn matchstart_transition_frame_does_not_swallow_queued_tickbatches() {
        let (mut host, mut guest) = connect_pair(12.0);
        let seed = 0xE2E5_E2E0_0000_0003;
        host.world_mut().non_send_mut::<VersusMatch>().p1 = Controller::Bot;

        // Start the match with the guest frozen: `MatchStart` plus the host's
        // first batches pile up on the wire before the guest polls again.
        start_net_match(host.world_mut(), AttackRule::Garbage, Side::Left, seed, 4);
        assert_eq!(session_status(&host), NetStatus::InMatch);
        for _ in 0..40 {
            host.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            lockstep_tick(&host) >= 3,
            "host must have produced batches 0..{} while the guest was frozen",
            lockstep_tick(&host),
        );

        // The guest's single transition-frame poll: `MatchStart` and the
        // queued batches are all in its reliable channel right now.
        guest.update();
        assert_eq!(session_status(&guest), NetStatus::InMatch);

        // Fixed: the mirror materializes from the parked start, executes the
        // queued batches and ticks past 0. Pre-fix RED: tick 0 forever,
        // `stall_steps` climbing (batch 0 was discarded in the drain).
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "guest ticks past 0 (no transition-frame stall)",
            |_, g| lockstep_tick(g) > 0,
        );

        // And it executed *every* queued batch, in order: freeze the host at
        // its current tick, let the guest replay its backlog, then the two
        // mirrors must be snapshot-identical at that tick (a swallowed or
        // misordered batch either stalls the replay below or forks the board).
        let host_tick = lockstep_tick(&host);
        let deadline = Instant::now() + Duration::from_secs(30);
        while lockstep_tick(&guest) < host_tick {
            guest.update();
            assert!(
                Instant::now() < deadline,
                "guest never replayed the host's queued batches: tick {}/{}",
                lockstep_tick(&guest),
                host_tick,
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            versus_snapshot(&host),
            versus_snapshot(&guest),
            "guest must have applied every queued batch identically"
        );

        net_stop(host.world_mut());
        net_stop(guest.world_mut());
    }

    /// Regression (N7 soak finding — production rematch stall): a rematch
    /// `MatchStart` is parked by the lockstep's own InMatch drain, and the
    /// reliable channel delivers the NEW match's `TickBatch`es behind it in
    /// the same window. Pre-fix, those batches hit the old mirror's
    /// staleness check (`tick < self.tick`, renet consumes reliable
    /// messages on read) and batch 0 was gone forever — the freshly rebuilt
    /// mirror stalled at tick 0 forever (the 20-match soak reproduced it
    /// at match 2: guest tick 0, stalls 214 922, host tick 214 909). The
    /// fix stages batches that arrive behind a parked `pending_start` and
    /// `guest_pending_start_system` adopts them with the rebuild.
    ///
    /// Deterministic window, same trick as the N6 transition-frame test:
    /// start the rematch while the guest is never `update()`d, so
    /// `MatchStart` and a pile of new-match batches sit in its socket and
    /// land in one poll.
    #[test]
    fn rematch_matchstart_does_not_swallow_the_new_match_tickbatches() {
        let (mut host, mut guest) = connect_pair(12.0);
        let seed1 = 0xE2E5_E2E0_0000_0004;
        host.world_mut().non_send_mut::<VersusMatch>().p1 = Controller::Bot;
        start_net_match_live(&mut host, AttackRule::Garbage, seed1, 4);
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "guest mirror active (match 1)",
            |_, g| {
                session_status(g) == NetStatus::InMatch
                    && g.world().non_send::<VersusMatch>().active
            },
        );
        // Tick the first match a bit so the guest mirror's lockstep tick is
        // far above the rematch's batch 0 (that gap is what pre-fix made
        // the new batches look "stale").
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "match 1 ticks past 20",
            |h, g| lockstep_tick(h) > 20 && lockstep_tick(g) > 20,
        );

        // Rematch while the guest is frozen: `MatchStart` plus the host's
        // first new-match batches pile up unread.
        let seed2 = 0xE2E5_E2E0_0000_0005;
        start_net_match(
            host.world_mut(),
            AttackRule::Race {
                target_lines: DEFAULT_RACE_LINES,
            },
            Side::Left,
            seed2,
            4,
        );
        for _ in 0..40 {
            host.update();
            std::thread::sleep(Duration::from_millis(5));
        }
        let queued = lockstep_tick(&host);
        assert!(
            queued >= 3,
            "host must have produced new-match batches 0..{queued} while the guest was frozen"
        );

        // One poll delivers the whole backlog; the rebuild then adopts the
        // parked start *and* the staged batches. Pre-fix: batch 0 lost,
        // guest stuck at tick 0 with `stall_steps` climbing.
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "rematch guest ticks past 0",
            |_, g| {
                let versus = g.world().non_send::<VersusMatch>();
                versus.active && versus.seed == seed2 && lockstep_tick(g) > 0
            },
        );

        // And it executed *every* queued batch in order: freeze the host,
        // let the guest replay, the rebuilt mirrors must be
        // snapshot-identical at the host's tick.
        let host_tick = lockstep_tick(&host);
        let deadline = Instant::now() + Duration::from_secs(30);
        while lockstep_tick(&guest) < host_tick {
            guest.update();
            assert!(
                Instant::now() < deadline,
                "guest never replayed the rematch backlog: tick {}/{}",
                lockstep_tick(&guest),
                host_tick,
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            versus_snapshot(&host),
            versus_snapshot(&guest),
            "guest must have applied every rematch batch identically"
        );

        net_stop(host.world_mut());
        net_stop(guest.world_mut());
    }

    // =========================================================================
    // G5 — crown E2E through the gateway relay (gateway-plan.md G5).
    //
    // The N6 pair above joins directly (`net_join(host_ip:port)`); this one
    // joins **through a real in-process `netplay_gateway` relay**: the host
    // registers a room over the real control channel (`*R` → `*A`, G2 driver,
    // 2 s keepalives), the guest runs the real join-by-code path (`*G` →
    // `*F` → the G2 driver hands `gateway_ip:vport` to the unchanged
    // `net_join`), and the whole match rides the relay's virtual data port —
    // real control handshake, real virtual-port data plane, real netcode.
    //
    // Per-IP leg distinction (G1 constraint): both apps live in **one**
    // process, so their sockets would all source `127.0.0.1` and the relay
    // (which attributes data-plane legs by source IP — on the WAN they always
    // differ) could not tell them apart. The host legs therefore pin to
    // `127.0.0.2` (game socket via `net_host_on`, control socket via
    // `NetGateway::with_leg_bind`), while the guest keeps production-shaped
    // default binds (`0.0.0.0` → sources `127.0.0.1`). Nothing can bypass the
    // relay: the client transport only ever accepts packets from exactly the
    // `*F` endpoint (`127.0.0.1:vport`), and the host's server socket on
    // `127.0.0.2` speaks only to that socket's address — a crowned winner
    // with equal hash streams is itself the proof the data plane went
    // through the relay.
    // =========================================================================

    /// Drain one app's gateway event stream.
    fn drain_gateway_events(
        app: &mut App,
    ) -> Vec<crate::core_bridge::net::gateway::NetGatewayEvent> {
        app.world_mut()
            .resource_mut::<Messages<crate::core_bridge::net::gateway::NetGatewayEvent>>()
            .drain()
            .collect()
    }

    /// The crown test: two bots play a full Garbage match with every packet
    /// relayed by the real gateway, and the per-60-tick SnapshotHash streams
    /// stay equal throughout — plus the `*D`-on-stop lifecycle at teardown.
    #[test]
    fn e2e_bot_vs_bot_garbage_match_through_gateway_relay() {
        use crate::core_bridge::net::gateway::testutil::{
            raw_socket, recv_frame, spawn_real_gateway,
        };
        use crate::core_bridge::net::gateway::{join_room, NetGateway, NetGatewayEvent};
        use netplay_gateway::wire::{self, Frame};

        let rg = spawn_real_gateway(4);
        let endpoint = rg.ctrl.to_string();
        let host_leg = Ipv4Addr::new(127, 0, 0, 2);

        // 4x (not the direct test's 12x): the gateway's 10 ms real poll is
        // 2.5 SIM ticks per hop at this rate, so relay latency stays well
        // inside the input delay below while the match still runs fast.
        let mut host = peer_app(4.0);
        let mut guest = peer_app(4.0);
        // Arm the G2 gateway driver on both peers (real driver systems; only
        // the endpoint and the host leg's bind differ from production).
        host.world_mut().insert_resource(
            NetGateway::test_with_endpoint(&endpoint).with_leg_bind(host_leg.into()),
        );
        guest
            .world_mut()
            .insert_resource(NetGateway::test_with_endpoint(&endpoint));

        // Host up on the pinned loopback leg.
        crate::core_bridge::net::session::net_host_on(host.world_mut(), host_leg, 0);
        let listen = host
            .world()
            .resource::<NetSession>()
            .listen_addr
            .expect("host must publish its bound address");
        assert_eq!(
            listen.ip().to_string(),
            "127.0.0.2",
            "the host game leg must pin the relay-distinct loopback IP"
        );

        // Real control handshake: Listening edge -> *R -> *A -> announced code.
        let mut host_gw = Vec::new();
        let code = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                host.update();
                guest.update();
                host_gw.extend(drain_gateway_events(&mut host));
                if let Some(code) = host_gw.iter().find_map(|e| match e {
                    NetGatewayEvent::RoomAnnounced(code) => Some(*code),
                    _ => None,
                }) {
                    break code;
                }
                assert!(
                    Instant::now() < deadline,
                    "host room never announced: {host_gw:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert!(
            !host_gw
                .iter()
                .any(|e| matches!(e, NetGatewayEvent::RoomOffline(_))),
            "registration must not go offline: {host_gw:?}"
        );

        // Real join-by-code path: *G -> *F -> G2 hands the relay address to
        // the production net_join.
        join_room(guest.world_mut(), code);
        let relay_addr = {
            let mut guest_gw = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                host.update();
                guest.update();
                guest_gw.extend(drain_gateway_events(&mut guest));
                if let Some(addr) = guest_gw.iter().find_map(|e| match e {
                    NetGatewayEvent::Found { addr, .. } => Some(*addr),
                    _ => None,
                }) {
                    break addr;
                }
                assert!(
                    Instant::now() < deadline,
                    "room {code:?} never surfaced to the guest: {guest_gw:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(
            relay_addr.ip().to_string(),
            "127.0.0.1",
            "*F must point at the gateway, never the *F-carried host_ip"
        );
        assert_ne!(
            relay_addr.port(),
            rg.ctrl.port(),
            "the guest connects to the relay's virtual data port, not the control port"
        );

        // Real netcode connect + app handshake, entirely through the relay.
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "relay handshake (both Ready)",
            |h, g| session_status(h) == NetStatus::Ready && session_status(g) == NetStatus::Ready,
        );
        drain_events(&mut host);
        drain_events(&mut guest);

        // Full Garbage match, bot-vs-bot — identical assertions to the
        // direct-UDP crown test, with the relay as the only path.
        let seed = 0xE2E5_E2E0_0000_00A5;
        let rule = AttackRule::Garbage;
        // At 4x, one sim tick = 4.17 ms real, so D = 20 covers ~83 ms real:
        // ample for the two 10 ms relay hops, yet assertive — a regression
        // toward the v0.3.1 100 ms poll starts clipping it. A late drop here
        // is a real assertion: the 1x field failure dropped 258/259 guest
        // inputs with both boards agreeing on an inert guest.
        let delay = 20;
        host.world_mut().non_send_mut::<VersusMatch>().p1 = Controller::Bot;
        start_net_match_live(&mut host, rule, seed, delay);

        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "guest mirror active (via relay)",
            |_, g| {
                session_status(g) == NetStatus::InMatch
                    && g.world().non_send::<VersusMatch>().active
            },
        );
        guest.world_mut().non_send_mut::<VersusMatch>().p2 = Controller::Bot;
        assert_eq!(
            guest.world().non_send::<VersusMatch>().seed,
            seed,
            "the guest must mirror the wire seed through the relay"
        );

        let mut saw_desync = Vec::new();
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(120),
            "crowned winner (via relay)",
            |h, g| {
                for event in drain_events(h).into_iter().chain(drain_events(g)) {
                    if matches!(event, NetEvent::Desync { .. }) {
                        saw_desync.push(event);
                    }
                }
                crowned(h).is_some() && crowned(g).is_some()
            },
        );
        assert!(
            saw_desync.is_empty(),
            "a clean relayed match must never desync: {saw_desync:?}"
        );
        assert!(
            lockstep_tick(&host) > 120 && lockstep_tick(&guest) > 120,
            "match too short: host tick {}, guest tick {}",
            lockstep_tick(&host),
            lockstep_tick(&guest),
        );
        let (hw, gw) = (crowned(&host), crowned(&guest));
        assert_eq!(hw, gw, "both relayed peers must crown the same winner");
        let dead = match hw {
            Some(Side::Left) => versus_snapshot(&guest).right.game_over,
            _ => versus_snapshot(&guest).left.game_over,
        };
        assert!(dead, "the loser's board is topped out");

        // Per-60-tick hash streams equal on their whole common prefix (>= 3
        // boundaries), and the final snapshots match — same quantities the
        // wire desync check exchanges.
        drive_until(
            &mut host,
            &mut guest,
            Duration::from_secs(30),
            "hash streams >= 3 on both peers",
            |h, g| stream(h).len() >= 3 && stream(g).len() >= 3,
        );
        let (mut hs, gs) = (stream(&host), stream(&guest));
        let common = hs.len().min(gs.len());
        hs.truncate(common);
        let common = &gs[..gs.len().min(common)];
        assert!(
            hs.len() >= 3,
            "expected several hash boundaries over a full relayed match, got {hs:?}"
        );
        assert_eq!(
            hs, common,
            "per-{}-tick SnapshotHash streams diverged over the relay",
            HASH_CHECK_PERIOD
        );
        assert_eq!(
            versus_snapshot(&host),
            versus_snapshot(&guest),
            "final snapshots must be equal over the relay"
        );

        // Non-vacuity (v0.3.1 field fix): hash-stream equality alone is
        // satisfied by a match where every guest input arrived late — the
        // host would then deterministically run the empty-input path and
        // BOTH boards would agree on an inert guest. The guest's bot must
        // have actually acted through the relay (applied remote actions),
        // and no input may have been late enough to be dropped.
        let host_ls = host
            .world()
            .resource::<crate::core_bridge::net::lockstep::NetLockstep>();
        assert!(
            host_ls.applied_remote_actions > 0,
            "the guest side applied no actions at the host — the relayed \
             match passed vacuously with an inert guest"
        );
        assert_eq!(
            host_ls.dropped_late_inputs, 0,
            "guest inputs were dropped as late through the relay — the \
             path outlives the negotiated input delay (controls would \
             feel dead on this path)"
        );

        // Teardown: net_stop must hand the room back to the gateway (`*D`) —
        // verified by a fresh lookup answering `*E` well before the 15 s GC
        // could have expired the room.
        net_stop(host.world_mut());
        net_stop(guest.world_mut());
        assert_eq!(session_status(&host), NetStatus::Idle);
        assert_eq!(session_status(&guest), NetStatus::Idle);
        let mut released = false;
        let probe = raw_socket("127.0.0.4");
        for _ in 0..5 {
            // Drive frames so the G2 driver sends `*D` on the stop edge,
            // then probe once (≤ 5 control frames — inside the rate bucket).
            host.update();
            std::thread::sleep(Duration::from_millis(80));
            probe
                .send_to(&wire::encode(&Frame::Lookup { code }), rg.ctrl)
                .expect("probe lookup");
            if matches!(recv_frame(&probe), Some(Frame::NotFound { .. })) {
                released = true;
                break;
            }
        }
        assert!(
            released,
            "net_stop must release the room (*D): lookups kept answering, or the room outlived its *D window"
        );
    }

    /// Degradation path (G5): a dead gateway endpoint surfaces the shipped
    /// offline line on the guest, never crashes, and stays retryable — and
    /// the session layer never even touches netcode (so the 10 s JoinTimeout
    /// is structurally unreachable behind it).
    #[test]
    fn gateway_down_surfaces_offline_line_and_stays_retryable() {
        use crate::core_bridge::net::gateway::{join_room, GuestLookupState, NetGateway};
        use crate::core_bridge::net::online_ui::lookup_status_text;

        // Reserve a port and drop it: nothing listens, the `*G` goes
        // unanswered — exactly a downed gateway (same shape G3 pins at the
        // UI layer; this pins it on the client state machine + copy).
        let dead = UdpSocket::bind("127.0.0.1:0")
            .expect("dead-port probe")
            .local_addr()
            .expect("probe addr");

        let mut guest = peer_app(1.0);
        guest
            .world_mut()
            .insert_resource(NetGateway::test_with_endpoint(&dead.to_string()));

        join_room(guest.world_mut(), *b"ABCDE");
        let started = Instant::now();
        loop {
            guest.update();
            let state = guest.world().resource::<NetGateway>().guest.clone();
            if matches!(state, GuestLookupState::Timeout) {
                assert!(
                    started.elapsed() < crate::core_bridge::net::session::JOIN_TIMEOUT,
                    "offline line must surface ({} s) before the JoinTimeout would",
                    started.elapsed().as_secs_f32()
                );
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "lookup never timed out against a dead gateway: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = guest.world().resource::<NetGateway>().guest.clone();
        assert_eq!(
            lookup_status_text(&state),
            "gateway offline — check connection or join by IP",
            "the offline copy the Join screen renders"
        );
        assert_eq!(
            session_status(&guest),
            NetStatus::Idle,
            "a downed gateway never reaches netcode — JoinTimeout cannot fire behind it"
        );

        // Retryable: the terminal Timeout is not sticky across a fresh
        // join_room — the lookup goes back in flight with no crash.
        join_room(guest.world_mut(), *b"ABCDE");
        let mut retried = false;
        for _ in 0..50 {
            guest.update();
            let state = guest.world().resource::<NetGateway>().guest.clone();
            if matches!(
                state,
                GuestLookupState::LookingUp(_) | GuestLookupState::Resolving(_)
            ) {
                retried = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(retried, "join_room after a timeout must restart the lookup");
    }

    /// Degradation path (G5): a room already paired to another guest answers
    /// `*B` and the guest surfaces `match full` in well under the 10 s
    /// JoinTimeout — the busy signal arrives while the player is still
    /// looking at "joining room…", never as a silent timeout.
    #[test]
    fn gateway_busy_room_surfaces_match_full_before_join_timeout() {
        use crate::core_bridge::net::gateway::testutil::{
            raw_socket, recv_ack_then_vport, reg, spawn_real_gateway,
        };
        use crate::core_bridge::net::gateway::{join_room, GuestLookupState, NetGateway};
        use crate::core_bridge::net::online_ui::lookup_status_text;

        let rg = spawn_real_gateway(4);
        // Host leg registers ABCDE (from 127.0.0.2 per the G1 rule), and a
        // first game-data packet from a third loopback IP pins its guest
        // slot — the room is now busy.
        let host = raw_socket("127.0.0.2");
        host.send_to(&reg(b"ABCDE", 6000), rg.ctrl).unwrap();
        // Consume the field-fix *V pair and take the relay port from it.
        let vport = recv_ack_then_vport(&host);
        let pinned = raw_socket("127.0.0.3");
        pinned
            .send_to(b"\x00pin", SocketAddr::from(([127, 0, 0, 1], vport)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));

        // The guest app (on the default 127.0.0.1 leg) looks up the paired
        // room: *B -> Busy -> "match full".
        let mut guest = peer_app(1.0);
        guest
            .world_mut()
            .insert_resource(NetGateway::test_with_endpoint(&rg.ctrl.to_string()));
        let started = Instant::now();
        join_room(guest.world_mut(), *b"ABCDE");
        loop {
            guest.update();
            let state = guest.world().resource::<NetGateway>().guest.clone();
            if state == GuestLookupState::Busy(*b"ABCDE") {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "*B must arrive long before the JoinTimeout ({} s)",
                    started.elapsed().as_secs_f32()
                );
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "*B never surfaced to the guest app: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = guest.world().resource::<NetGateway>().guest.clone();
        assert_eq!(lookup_status_text(&state), "match full");
        assert_eq!(
            session_status(&guest),
            NetStatus::Idle,
            "a busy room must not enter netcode (no JoinTimeout behind the message)"
        );
        assert!(
            !drain_events(&mut guest).contains(&NetEvent::JoinTimeout),
            "JoinTimeout must not fire behind the busy reply"
        );
    }

    // =========================================================================
    // N7 — 20-match netplay soak (netplay-plan.md N7a).
    //
    // Chains [`SOAK_MATCHES`] matches over **one** connected pair on real
    // UDP loopback: alternating Garbage / Race-to-40, a wide seed sweep,
    // and rematches flowing through the wire `MatchStart` (N4's production
    // rematch path — no CI test exercised it before this; the desktop
    // harness's single rematch was the only live coverage). Per match the
    // independently recorded per-60-tick hash streams are compared on
    // every shared tick label (the same quantities the wire desync check
    // exchanges), the final snapshots are compared, every drained
    // `NetEvent` must be benign, and the host mirror's `GarbageSent`/
    // `GarbageReceived` stream is tallied as load evidence for the
    // `MAX_GARBAGE_PER_LAND` churn dimension.
    //
    // The Garbage matches carry the garbage-storm churn; the Race matches
    // run to 40 lines — many thousands of lockstep ticks — which is the
    // very-long / sustained-load dimension. (The Race rule is garbage-free
    // by construction: versus.rs `settle` only lands garbage when
    // `rule == AttackRule::Garbage`, so "churn" and "Race-to-40" are
    // deliberately separate match shapes, both inside the 20.)
    //
    // `#[ignore]`d for the nightly job like `crates/tetris-core/tests/soak.rs`:
    // `cargo test -p tetris-app --release -- net::harness --ignored`.
    // =========================================================================

    /// Matches the soak chains through one connection.
    const SOAK_MATCHES: usize = 20;

    /// Input delay for every soak match (mid of the negotiated range).
    const SOAK_DELAY: u8 = 4;

    /// Virtual fixed-step speed: stepping runs 24× wall clock while
    /// `Time<Real>` (netcode timing, watchdogs) stays real — see
    /// [`peer_app`].
    const SOAK_SPEED: f64 = 24.0;

    /// Host: a live match without crowning for this long fails the soak
    /// (generous: the 40-line Races run many thousands of ticks).
    const SOAK_STALL_TIMEOUT: Duration = Duration::from_secs(300);

    /// Match `i`'s rule: even = Garbage (garbage-storm churn), odd = Race
    /// to 40 (the very long run).
    fn soak_rule(i: usize) -> AttackRule {
        if i.is_multiple_of(2) {
            AttackRule::Garbage
        } else {
            AttackRule::Race {
                target_lines: DEFAULT_RACE_LINES,
            }
        }
    }

    /// Match `i`'s seed: a rotated-and-mixed sweep, deliberately far from
    /// the CI E2E seeds.
    fn soak_seed(i: usize) -> u64 {
        0x9E37_79B9_7F4A_7C15u64
            .rotate_left((((i as u64) * 7) % 64) as u32)
            .wrapping_mul(0x5851_F35C_F137_2235 ^ (i as u64))
    }

    /// Per-match garbage load tallies (host mirror's event stream): rows
    /// sent, rows landed, land events, and how many landings hit the
    /// [`MAX_GARBAGE_PER_LAND`] cap. `locks`/`clears` sanity-check the
    /// event pipeline itself (locks always > 0 in a played match).
    #[derive(Default, Clone, Copy, Debug)]
    struct SoakChurn {
        locks: u64,
        clears: u64,
        sent_rows: u64,
        landed_rows: u64,
        landed_events: u64,
        cap_hits: u64,
    }

    struct SoakMatchResult {
        rule: AttackRule,
        seed: u64,
        /// Lockstep ticks at crowning (the host clock; the guest mirrors it).
        ticks: u64,
        winner: Option<Side>,
        /// Shared per-60-tick hash labels compared for this match.
        shared_boundaries: usize,
        churn: SoakChurn,
        wall: Duration,
    }

    /// Drain both apps' `NetEvent`s (mid-soak every event except a
    /// spurious `PeerConnected` is fatal) and tally garbage churn from the
    /// host mirror's `VersusEvent` stream (both mirrors emit the same
    /// stream — counting one avoids doubling).
    fn soak_collect(h: &mut App, g: &mut App, failures: &mut Vec<NetEvent>, churn: &mut SoakChurn) {
        for event in drain_events(h).into_iter().chain(drain_events(g)) {
            match event {
                NetEvent::PeerConnected => {}
                other => failures.push(other),
            }
        }
        let drained: Vec<MatchEvent> = h
            .world_mut()
            .resource_mut::<Messages<VersusEvent>>()
            .drain()
            .map(|event| event.0)
            .collect();
        for event in drained {
            match event {
                MatchEvent::PieceLocked { lines, .. } => {
                    churn.locks += 1;
                    if lines > 0 {
                        churn.clears += u64::from(lines);
                    }
                }
                MatchEvent::GarbageSent { lines, .. } => churn.sent_rows += u64::from(lines),
                MatchEvent::GarbageReceived { lines, .. } => {
                    churn.landed_rows += u64::from(lines);
                    churn.landed_events += 1;
                    if lines >= MAX_GARBAGE_PER_LAND {
                        churn.cap_hits += 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// One soak match end to end, asserted per the header comment. Crowning
    /// detection keys on the **wire seed landing on the guest mirror** so a
    /// stale winner from the previous match can never satisfy the wait.
    fn soak_one_match(host: &mut App, guest: &mut App, index: usize) -> SoakMatchResult {
        let rule = soak_rule(index);
        let seed = soak_seed(index);
        let started = Instant::now();
        let mut failures: Vec<NetEvent> = Vec::new();
        let mut churn = SoakChurn::default();

        let require_clean = |failures: &Vec<NetEvent>, label: &str| {
            assert!(
                failures.is_empty(),
                "soak match {index} ({rule:?} seed {seed:#016x}) {label} saw fatal NetEvents: {failures:?}"
            );
        };

        // Start live — the guest mirrors purely from the wire `MatchStart`
        // (the production rematch path from match 2 on). Seats stay `Human`
        // (the production net shape); the [`soak_player_system`] registered
        // by the soak drives them through the real queued-action path.
        start_net_match_live(host, rule, seed, SOAK_DELAY);

        drive_until(
            host,
            guest,
            Duration::from_secs(60),
            "guest mirror (re)built from the wire MatchStart",
            |h, g| {
                soak_collect(h, g, &mut failures, &mut churn);
                let versus = g.world().non_send::<VersusMatch>();
                session_status(g) == NetStatus::InMatch && versus.active && versus.seed == seed
            },
        );
        require_clean(&failures, "mirror build");

        drive_until(
            host,
            guest,
            SOAK_STALL_TIMEOUT,
            "soak crowned winner",
            |h, g| {
                soak_collect(h, g, &mut failures, &mut churn);
                crowned(h).is_some() && crowned(g).is_some()
            },
        );
        require_clean(&failures, "play");
        let ticks = lockstep_tick(host);
        assert!(
            ticks > 60,
            "soak match {index} ({rule:?} seed {seed:#016x}) crowned before a full hash period (tick {ticks})"
        );
        let winner = crowned(host);
        assert_eq!(
            winner,
            crowned(guest),
            "soak match {index}: peers crowned different winners"
        );
        let snapshot = versus_snapshot(guest);
        match rule {
            AttackRule::Garbage => {
                let dead = match winner {
                    Some(Side::Left) => snapshot.right.game_over,
                    _ => snapshot.left.game_over,
                };
                assert!(
                    dead,
                    "soak match {index}: Garbage winner without a topped-out loser"
                );
            }
            AttackRule::Race { .. } => {
                // Legal Race endings: both sides reach the target (score
                // decides), or one side tops out before that (core `settle`
                // crowns on top-out under any rule).
                let dead = match winner {
                    Some(Side::Left) => snapshot.right.game_over,
                    _ => snapshot.left.game_over,
                };
                assert!(
                    (snapshot.finished.0 && snapshot.finished.1) || dead,
                    "soak match {index}: Race crowned with no finish and no top-out: {:?}",
                    snapshot.finished
                );
            }
            // T19 placeholder: Dig/Switch have no win condition yet
            // (behavior lands in T20/T21), so under the no-attack scaffold
            // the only path to a crown is a top-out — the loser is dead.
            // The soak itself only cycles Garbage/Race (see `soak_rule`).
            AttackRule::Dig | AttackRule::Switch { .. } => {
                let dead = match winner {
                    Some(Side::Left) => snapshot.right.game_over,
                    _ => snapshot.left.game_over,
                };
                assert!(
                    dead,
                    "soak match {index}: {rule:?} placeholder crowned without a topped-out loser"
                );
            }
        }
        // The scripted players actually played (event pipeline sanity).
        assert!(
            churn.locks >= 10,
            "soak match {index} ({rule:?} seed {seed:#016x}) crowned after only {} locks — scripted play degenerate",
            churn.locks
        );

        // Let at least one per-60-tick boundary land, compare every shared
        // label, then reset the streams for the next match.
        drive_until(
            host,
            guest,
            Duration::from_secs(30),
            "soak hash boundaries recorded",
            |h, g| {
                soak_collect(h, g, &mut failures, &mut churn);
                !stream(h).is_empty() && !stream(g).is_empty()
            },
        );
        require_clean(&failures, "hash tail");
        let (hs, gs) = (stream(host), stream(guest));
        assert!(
            hs.windows(2).all(|w| w[0].0 < w[1].0),
            "soak match {index}: host stream tick labels not strictly increasing: {hs:?}"
        );
        assert!(
            gs.windows(2).all(|w| w[0].0 < w[1].0),
            "soak match {index}: guest stream tick labels not strictly increasing: {gs:?}"
        );
        let mut shared_boundaries = 0usize;
        for (tick, ours) in &hs {
            if let Some((_, theirs)) = gs.iter().find(|(t, _)| t == tick) {
                assert_eq!(
                    ours, theirs,
                    "soak match {index} ({rule:?} seed {seed:#016x}): per-{HASH_CHECK_PERIOD}-tick SnapshotHash streams diverge at tick {tick}"
                );
                shared_boundaries += 1;
            }
        }
        assert!(
            shared_boundaries >= 1,
            "soak match {index}: no shared hash boundary between host {hs:?} and guest {gs:?}"
        );
        assert_eq!(
            versus_snapshot(host),
            versus_snapshot(guest),
            "soak match {index} ({rule:?} seed {seed:#016x}): final snapshots diverged"
        );
        // Per-match stream reset is owned by `record_boundary_hashes`
        // (seed-keyed), so a lagging peer's last old-match boundary can
        // never leak into the next match's stream.

        SoakMatchResult {
            rule,
            seed,
            ticks,
            winner,
            shared_boundaries,
            churn,
            wall: started.elapsed(),
        }
    }

    /// Soak lock pacing: one scripted piece per side per this many ticks
    /// (mirrors `BOT_LOCK_COOLDOWN_STEPS`, which the harness cannot use
    /// with its own seats — see [`soak_player_system`]).
    const SOAK_LOCK_COOLDOWN: u32 = 60;

    /// The soak's player (test-only): drives the **local** seat of each
    /// peer through the production queued-action path — actions land in
    /// the seat's [`VersusActions`] and flow via `schedule_local` →
    /// `TickInput`/`TickBatch` with the negotiated input delay, exactly
    /// like a human's. Each piece is one queued burst (rotate → slide →
    /// hard-drop) aiming at the production greedy solver
    /// ([`bot_move`](crate::core_bridge::bot_move)), which clears lines —
    /// so Garbage matches exchange real `MAX_GARBAGE_PER_LAND`-capped
    /// garbage and Race matches reach 40 lines.
    ///
    /// Why not the `Controller::Bot` seats the N6 E2E uses: the bot plan
    /// stepper assumes same-tick action application (the local bridge's
    /// contract) and wedge-aborts into spawn hard-drops when actions land
    /// `D` ticks late — in a net match its boards never clear and never
    /// race. Production net matches never have Bot seats, so this is a
    /// harness play-quality gap, not a netplay bug (audit-logged; the N6
    /// E2E keeps Bot seats, which pin the wiring just fine).
    #[derive(Resource, Default)]
    struct SoakPlayer {
        cooldown: u32,
    }

    fn soak_player_system(
        mut player: ResMut<SoakPlayer>,
        versus: Option<NonSend<VersusMatch>>,
        session: Option<Res<NetSession>>,
        mut actions: ResMut<VersusActions>,
    ) {
        if player.cooldown > 0 {
            player.cooldown -= 1;
            return;
        }
        let (Some(versus), Some(session)) = (versus, session) else {
            return;
        };
        if !versus.active || session.status != NetStatus::InMatch {
            return;
        }
        let side = local_side(session.role);
        let snapshot = match side {
            Side::Left => versus.match_.left.snapshot(),
            Side::Right => versus.match_.right.snapshot(),
        };
        let Some(active) = snapshot.active else {
            return;
        };
        let Some(mv) = crate::core_bridge::bot_move(&snapshot) else {
            return;
        };
        let mut queued: Vec<Action> = Vec::new();
        match (mv.rot as u32 + 4 - active.rot as u32) % 4 {
            1 => queued.push(Action::RotateCw),
            2 => queued.push(Action::Rotate180),
            3 => queued.push(Action::RotateCcw),
            _ => {}
        }
        for _ in 0..(mv.target_col - active.col).abs() {
            queued.push(if mv.target_col > active.col {
                Action::MoveRight
            } else {
                Action::MoveLeft
            });
        }
        queued.push(Action::HardDrop);
        if side == Side::Left {
            actions.left.extend(queued);
        } else {
            actions.right.extend(queued);
        }
        player.cooldown = SOAK_LOCK_COOLDOWN;
    }

    /// Register the soak player on both peers (after the session is
    /// connected; runs before the lockstep step systems so its actions are
    /// drained by them in the same fixed step).
    fn arm_soak_player(app: &mut App) {
        app.init_resource::<SoakPlayer>();
        app.add_systems(
            FixedUpdate,
            soak_player_system
                .before(net_lockstep_host_system)
                .before(net_lockstep_guest_system),
        );
    }

    #[test]
    #[ignore = "20-match netplay soak (netplay-plan.md N7); run: cargo test -p tetris-app --release -- net::harness --ignored"]
    fn netplay_soak_20_matches() {
        let (mut host, mut guest) = connect_pair(SOAK_SPEED);
        arm_soak_player(&mut host);
        arm_soak_player(&mut guest);
        let mut results = Vec::with_capacity(SOAK_MATCHES);
        for i in 0..SOAK_MATCHES {
            let result = soak_one_match(&mut host, &mut guest, i);
            println!(
                "SOAK {i:02} {rule:?} seed={seed:#016x} ticks={ticks} winner={winner:?} \
                 locks={locks} clears={clears} hashes={boundaries} sent={sent} \
                 landed={landed}x{landed_events} cap_hits={cap_hits} wall={wall:?}",
                rule = result.rule,
                seed = result.seed,
                ticks = result.ticks,
                winner = result.winner,
                locks = result.churn.locks,
                clears = result.churn.clears,
                boundaries = result.shared_boundaries,
                sent = result.churn.sent_rows,
                landed = result.churn.landed_rows,
                landed_events = result.churn.landed_events,
                cap_hits = result.churn.cap_hits,
                wall = result.wall,
            );
            results.push(result);
        }

        let total_boundaries: usize = results.iter().map(|r| r.shared_boundaries).sum();
        let total_ticks: u64 = results.iter().map(|r| r.ticks).sum();
        let churn = results.iter().fold(SoakChurn::default(), |mut a, r| {
            a.sent_rows += r.churn.sent_rows;
            a.landed_rows += r.churn.landed_rows;
            a.landed_events += r.churn.landed_events;
            a.cap_hits += r.churn.cap_hits;
            a.locks += r.churn.locks;
            a.clears += r.churn.clears;
            a
        });
        let race = results
            .iter()
            .filter(|r| matches!(r.rule, AttackRule::Race { .. }));
        let longest_race = race.map(|r| r.ticks).max().unwrap_or(0);
        println!(
            "SOAK summary: {} matches, ticks={total_ticks}, hash boundaries compared={total_boundaries}, \
             garbage sent={sent} landed={landed} in {landed_events} landings, MAX_GARBAGE_PER_LAND cap hits={cap_hits}, \
             longest Race={longest_race} ticks",
            results.len(),
            sent = churn.sent_rows,
            landed = churn.landed_rows,
            landed_events = churn.landed_events,
            cap_hits = churn.cap_hits,
        );
        assert!(
            total_boundaries >= 40,
            "soak compared too few hash boundaries to count: {total_boundaries}"
        );
        assert!(
            longest_race >= 1000,
            "Race matches were not the very long runs the soak exists for: {longest_race}"
        );
        // Garbage matches must be an actual garbage storm, not a stall.
        assert!(
            churn.sent_rows >= 100 && churn.landed_rows >= 100,
            "soak exchanged too little garbage to count: {churn:?}"
        );

        net_stop(host.world_mut());
        net_stop(guest.world_mut());
        assert_eq!(session_status(&host), NetStatus::Idle);
        assert_eq!(session_status(&guest), NetStatus::Idle);
    }
}
