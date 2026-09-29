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
//! seed-propagation test — the fixed `TETRIS_TEST_NET_PORT` + `TEST_NET_LOCK`
//! dance exists to serialize tests that *need* a known port, and these tests
//! do not (the lock is unreachable from here anyway: `mod tests` in
//! `session.rs` is private and that file is not N6-owned). Zero collision
//! risk with the N2 socket tests or a parallel test binary by construction.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use tetris_core::actions::Action;
use tetris_core::versus::{AttackRule, Side, DEFAULT_RACE_LINES};

use super::lockstep::local_side;
use super::session::{net_host, net_join, net_stop, NetEvent, NetRole, NetSession, NetStatus};
use crate::core_bridge::wall_clock_seed;
use crate::core_bridge::{start_net_match, Controller, SimPaused, VersusMatch, VersusWinner};
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

/// Host-side grace window after `MatchStart` during which the simulation is
/// held in [`SimPaused`] so the guest's pre-match wire drain completes before
/// any `TickBatch` is produced (see [`start_desktop_match`]).
const MATCH_START_HOLD: Duration = Duration::from_millis(250);

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
    /// Host: while `Some`, [`SimPaused`] is held so no `TickBatch` can reach
    /// the guest before its `MatchStart` drain completes (see
    /// [`start_desktop_match`]).
    hold_until: Option<Instant>,
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
        hold_until: None,
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
/// The host simulation is held in [`SimPaused`] for [`MATCH_START_HOLD`]
/// after `MatchStart`: `guest_net_system`'s pre-match wire drain reads the
/// whole reliable channel the instant it transitions `InMatch`, discarding
/// every `TickBatch` queued behind `MatchStart` ("ignoring … before match
/// start") — and any batches produced by the host before the guest's first
/// `InMatch` poll land in exactly that window (one guest poll may cover
/// several host updates). A guest that loses batch 0 stalls at tick 0
/// forever. The hold lets the guest's drain complete before any batch is
/// produced. (The drain itself is N2's; once it stops dropping post-
/// `MatchStart` messages in the same pass, this hold is harmless belt-and-
/// braces.)
fn start_desktop_match(world: &mut World, harness: &mut NetDesktopHarness, rule: AttackRule) {
    if let Some(mut versus) = world.get_non_send_mut::<VersusMatch>() {
        versus.p1 = Controller::Bot;
    }
    *world.resource_mut::<AppState>() = AppState::Playing;
    let delay = world.resource::<NetSession>().input_delay;
    let seed = wall_clock_seed();
    info!("NET match_start rule={rule:?} seed={seed} delay={delay}");
    world.insert_resource(SimPaused(true));
    start_net_match(world, rule, Side::Left, seed, delay);
    // `last_crowned_seed` needs no reset: the fresh wall-clock seed is not yet
    // recorded, so the crowning block will fire for this match on both roles.
    harness.hold_until = Some(Instant::now() + MATCH_START_HOLD);
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

    // Release the post-MatchStart hold: by now the guest has polled the
    // wire and its pre-match drain is done (see start_desktop_match).
    if harness.hold_until.is_some_and(|until| now >= until) {
        harness.hold_until = None;
        if let Some(mut paused) = world.get_resource_mut::<SimPaused>() {
            paused.0 = false;
        }
    }

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
            start_desktop_match(world, &mut harness, rule);
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
    use crate::core_bridge::CoreBridgePlugin;
    use crate::state::{RebindingCapture, Settings};
    use bevy::time::Virtual;
    use std::net::{Ipv4Addr, UdpSocket};
    use tetris_core::versus::MatchSnapshot;

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
    ) {
        let (Some(session), Some(versus), Some(lockstep)) = (session, versus, lockstep) else {
            return;
        };
        if session.status != NetStatus::InMatch || !versus.active {
            return;
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

    /// Start the match with the host held in [`SimPaused`] until the guest
    /// mirror is live, mirroring [`start_desktop_match`]: the host's first
    /// batches are queued before the guest's first `InMatch` poll, and
    /// `guest_net_system`'s pre-match drain discards everything the reliable
    /// channel holds behind `MatchStart` — losing batch 0 would stall the
    /// mirror at tick 0 forever. While paused the host produces no batches
    /// at all, so the drain window closes over an empty stream; the test
    /// releases the hold only once it has *seen* the guest mirror active
    /// (its drain is provably done then), plus two settling frames.
    fn start_net_match_quiet(
        host: &mut App,
        guest: &mut App,
        rule: AttackRule,
        seed: u64,
        delay: u8,
    ) {
        host.world_mut().insert_resource(SimPaused(true));
        start_net_match(host.world_mut(), rule, Side::Left, seed, delay);
        assert_eq!(session_status(host), NetStatus::InMatch);
        drive_until(
            host,
            guest,
            Duration::from_secs(30),
            "guest mirror (paused window)",
            |_, g| {
                session_status(g) == NetStatus::InMatch
                    && g.world().non_send::<VersusMatch>().active
            },
        );
        for _ in 0..2 {
            host.update();
            guest.update();
        }
        host.world_mut().resource_mut::<SimPaused>().0 = false;
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
        start_net_match_quiet(&mut host, &mut guest, rule, seed, delay);

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
        start_net_match_quiet(&mut host, &mut guest, AttackRule::Garbage, seed, 4);
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
}
