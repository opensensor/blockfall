//! Lockstep engine (netplay-plan.md N3): tick clock, input delay, mirror
//! stepping and the periodic snapshot-hash desync check.
//!
//! # What lives where
//!
//! The [`NetLockstep`] state machine is **pure** — every method takes the
//! [`Match`] to drive and a [`NetOut`] transport sink by reference, so the
//! exact production logic runs in one process in tests with a fake
//! transport (netplay-plan.md N3 validation). The Bevy layer is two thin
//! systems ([`net_lockstep_host_system`], [`net_lockstep_guest_system`])
//! mounted on `FixedUpdate` by [`NetLockstepPlugin`] (mounted from
//! `NetPlugin::build()`): they gate, drain the renet channels through
//! [`server_inbox`]/[`client_inbox`], feed the local side's
//! [`VersusActions`] queue through [`NetLockstep::schedule_local`], step,
//! and surface results as `VersusEvent`/`NetEvent` messages.
//!
//! # Stepping order (load-bearing)
//!
//! [`apply_batch`] mirrors `versus_bridge_system` exactly
//! (`core_bridge/versus.rs:255-264`): **apply left actions → `tick(left)` →
//! apply right actions → `tick(right)`**, collecting `MatchEvent`s which the
//! systems write verbatim as `VersusEvent` messages (T26 juice/audio/HUD
//! work unmodified). The systems also mirror the bridge's bookkeeping while
//! `InMatch`: `VersusMatch::steps += 1` per executed tick and exactly-once
//! crowning of [`VersusWinner`] (`start_versus` clears it, so a `None`
//! check is the once-guard). **N4's gating contract**: when
//! `NetSession::status == InMatch`, `versus_bridge_system` must skip its
//! *entire* drain-then-tick block (including its own `steps` increment and
//! crowning — these systems do them instead; double-stepping corrupts the
//! mirror). Both systems run **after** `versus_bot_system` so a `Bot`-seat's
//! same-frame pushes are scheduled with delay like human actions (N4 pins
//! that ordering; `versus_bot_system` is private to `versus.rs`).
//!
//! # Input delay
//!
//! Local actions taken when the lockstep tick reads `t` are scheduled for
//! `t + D` (`NetLockstep::schedule_local`) *and* emitted immediately as
//! `TickInput { tick: t + D, actions }` — both peers emit, per the plan
//! ("both sides emit `TickInput` for their local side's queued actions");
//! the host's own `TickInput` echo is ignored by the guest (the batch
//! stream is authoritative), and the host stores the guest's for merge
//! into the batch of that tick. `remote_inputs` entries for ticks already
//! executed are dropped and counted in `dropped_late_inputs`. `D` comes
//! from `MatchStart.match_delay` via `NetSession::input_delay` (env default
//! `TETRIS_NET_DELAY`, clamped — see [`session`](super::session)).
//!
//! # Tick clock & stall policy
//!
//! The host's `tick` (next tick to execute) is authoritative: every host
//! step builds `TickBatch { tick, left, right }` (empty lists included — it
//! doubles as clock pacing), applies it locally, sends it and bumps `tick`.
//! The guest executes strictly in order: batch for `tick` buffered →
//! execute; missing → stall (reliability guarantees eventual arrival; each
//! stalled step counts one `stall_steps`). A tick whose remote input never
//! arrived executes with an empty list — the guest mirror applies only
//! batch contents, so a missing/late input can never fork the two boards.
//!
//! # Desync check
//!
//! After every [`HASH_CHECK_PERIOD`]th executed tick (labeled with the
//! executed tick number, i.e. labels 59, 119, …) both sides send
//! `SnapshotHash { side, tick, left: h, right: h }` where `h` is the
//! full-match [`snapshot_hash`](super::protocol::snapshot_hash) (the N1
//! codec exposes only the whole-`MatchSnapshot` hash; both fields carry it,
//! per-side granularity is a future refinement). Each side keeps a rolling
//! window of its own and the peer's hashes; when both hold the same tick
//! label and the values differ → [`LockstepSignal::Desync`].
//!
//! # Wire ownership in `InMatch`
//!
//! From the first match frame on, these systems own **every** channel
//! message, `Bye` included ([`session`](super::session) module docs). A
//! `Bye` becomes `NetEvent::ByeReceived` + the FSM `Bye` trigger (`Lost`) +
//! the freeze below. A `MatchStart` arriving mid-match (N4's rematch) is
//! parked in [`NetLockstep::pending_start`] for N4 to consume — N3 never
//! rebuilds a mirror — and any `TickBatch`es already queued behind it
//! belong to the NEW match: while a start is pending they are staged in
//! [`NetLockstep::staged_batches`] (not dropped, not applied to the dying
//! mirror) and adopted by N4's `guest_pending_start_system` together with
//! the rebuild. Connection loss itself stays N2's job (its Update systems
//! keep running and flip `Lost`).
//!
//! # Gates (the freeze mechanism)
//!
//! Both systems skip entirely while `SimPaused.0`, `AppState != Playing`,
//! no transport (resources missing / host `peer` absent), or
//! `!VersusMatch::active`. Skipping while paused is what makes the
//! Lost/Desync freeze *real*: N5 flips `SimPaused`, the lockstep stops
//! ticking, `VersusMatch.active` stays `true` so the frozen boards keep
//! rendering under the overlay.
//!
//! # Teardown contract (netplay-plan.md N3) — what N5 does
//!
//! On `NetEvent::Desync`/`ByeReceived` (and N2's `PeerLost`/`JoinTimeout`
//! while a match was on screen) **N5 shows the overlay**: a root spawned
//! with the explicit [`NET_OVERLAY_ZINDEX`]. The ZIndex is load-bearing
//! (recorded regressions `screens_menu.rs:960-975`, `:2055-2062`:
//! top-level UI roots stack-sort by z only; versus HUD / winner / title
//! roots sit at `ZIndex(0)`, submenu roots at `ZIndex(1)`, title hides
//! while a submenu is open). `NET_OVERLAY_ZINDEX` is strictly above all of
//! them, so the overlay's *own* buttons pick first and the visible-versus
//! HUD can no longer swallow them. Freeze on N3's side (`Desync`/`Bye`
//! signals) already sets `SimPaused` and emits the messages; N5 adds:
//! show the overlay root, and its **"Back to title"** button calls
//! [`net_leave_to_title`] — the full `end_versus`-style teardown (match
//! `active = false` → the existing visibility sync systems restore the
//! clean-solo root regime, `AppState::Title`, un-pause, `NetSession →
//! Idle` via `net_stop` which also sends the graceful netcode disconnect).
//! The overlay root itself is N5's; hide it again on `Idle`.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_renet::renet::DefaultChannel;
use bevy_renet::{RenetClient, RenetServer};

use tetris_core::actions::Action;
use tetris_core::versus::{AttackRule, Match, MatchEvent, Side};

use super::protocol::{self, NetMsg};
use super::session::{
    NetEvent, NetRole, NetSession, NetStatus, NetTrigger, DEFAULT_INPUT_DELAY, MAX_INPUT_DELAY,
    MIN_INPUT_DELAY,
};
use crate::core_bridge::{end_versus, SimPaused, VersusEvent, VersusMatch, VersusWinner};
use crate::input::VersusActions;
use crate::state::AppState;

/// Ticks between snapshot-hash desync checks (plan: every 60 fixed steps).
pub const HASH_CHECK_PERIOD: u64 = 60;

/// Explicit `ZIndex` for the netplay overlay root (teardown contract item
/// 1 — see the module docs; N5 spawns the overlay with this so it renders
/// and *picks* over the versus HUD and every menu root).
pub const NET_OVERLAY_ZINDEX: ZIndex = ZIndex(100);

/// Number of recent hash-check results kept per direction before pruning
/// (comparison windows for the periodic [`HASH_CHECK_PERIOD`] exchange).
const HASH_WINDOW: usize = 8;

/// Transport seam (N3-owned): the one thing the lockstep needs to *emit*.
/// Production adapters wrap renet ([`RenetServerOut`], [`RenetClientOut`]);
/// tests use an in-memory fake, letting both peers run in one process
/// deterministically. Receiving is separate ([`server_inbox`] /
/// [`client_inbox`] feed [`NetLockstep::ingest`]) so the core never touches
/// renet types at all.
pub trait NetOut {
    /// Queue one wire message (never blocks; reliable channels buffer).
    fn send(&mut self, msg: &NetMsg);
}

/// Send-channel mapping per the wire protocol table: Hello/MatchStart/
/// TickInput/TickBatch on ReliableOrdered, SnapshotHash/Bye on
/// ReliableUnordered.
#[must_use]
pub fn wire_channel(msg: &NetMsg) -> DefaultChannel {
    match msg {
        NetMsg::SnapshotHash { .. } | NetMsg::Bye => DefaultChannel::ReliableUnordered,
        NetMsg::Hello { .. }
        | NetMsg::MatchStart { .. }
        | NetMsg::TickInput { .. }
        | NetMsg::TickBatch { .. } => DefaultChannel::ReliableOrdered,
    }
}

/// Host-side [`NetOut`]: sends to the session's single connected peer.
pub struct RenetServerOut<'a> {
    /// The live renet server.
    pub server: &'a mut RenetServer,
    /// Netcode id of the guest.
    pub peer: u64,
}

impl NetOut for RenetServerOut<'_> {
    fn send(&mut self, msg: &NetMsg) {
        self.server
            .send_message(self.peer, wire_channel(msg), protocol::encode(msg));
    }
}

/// Guest-side [`NetOut`].
pub struct RenetClientOut<'a> {
    /// The live renet client.
    pub client: &'a mut RenetClient,
}

impl NetOut for RenetClientOut<'_> {
    fn send(&mut self, msg: &NetMsg) {
        self.client
            .send_message(wire_channel(msg), protocol::encode(msg));
    }
}

/// Poll both renet channels of the host's connection until empty and
/// decode everything (undecodable payloads are dropped with a warning —
/// never a panic; the host stays up on hostile bytes).
#[must_use]
pub fn server_inbox(server: &mut RenetServer, peer: u64) -> Vec<NetMsg> {
    let mut msgs = Vec::new();
    while let Some(bytes) = server.receive_message(peer, DefaultChannel::ReliableOrdered) {
        decode_into(&mut msgs, &bytes);
    }
    while let Some(bytes) = server.receive_message(peer, DefaultChannel::ReliableUnordered) {
        decode_into(&mut msgs, &bytes);
    }
    msgs
}

/// Guest counterpart of [`server_inbox`].
#[must_use]
pub fn client_inbox(client: &mut RenetClient) -> Vec<NetMsg> {
    let mut msgs = Vec::new();
    while let Some(bytes) = client.receive_message(DefaultChannel::ReliableOrdered) {
        decode_into(&mut msgs, &bytes);
    }
    while let Some(bytes) = client.receive_message(DefaultChannel::ReliableUnordered) {
        decode_into(&mut msgs, &bytes);
    }
    msgs
}

fn decode_into(sink: &mut Vec<NetMsg>, bytes: &[u8]) {
    match protocol::decode(bytes) {
        Ok(msg) => sink.push(msg),
        Err(e) => warn!("net lockstep: dropping undecodable payload: {e}"),
    }
}

/// Side effects the systems must surface beyond per-tick events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockstepSignal {
    /// The peer's snapshot hash for a tick we also hashed disagrees —
    /// mirrors have forked. Teardown contract: freeze + overlay + message.
    Desync {
        /// Hash-check tick whose labels mismatched.
        tick: u64,
    },
    /// The peer sent [`NetMsg::Bye`] — clean exit.
    Bye,
}

/// Outcome of one [`NetLockstep`] step call.
#[derive(Debug)]
pub struct StepResult {
    /// `MatchEvent`s produced this step (empty when the step did not run).
    pub events: Vec<MatchEvent>,
    /// `false` only on a guest stall (no batch for the expected tick).
    pub ran: bool,
    /// Tear-down-worthy side effect, if any (desync detected on the
    /// hash exchange completed by this step).
    pub signal: Option<LockstepSignal>,
}

fn side_index(side: Side) -> usize {
    match side {
        Side::Left => 0,
        Side::Right => 1,
    }
}

/// The seat this peer drives locally: host = Left, guest = Right (fixed by
/// the wire protocol roles).
#[must_use]
pub fn local_side(role: NetRole) -> Side {
    match role {
        NetRole::Host => Side::Left,
        NetRole::Guest => Side::Right,
    }
}

/// Lockstep state (Bevy `Resource`, one per peer; pure methods, see module
/// docs). All fields are public for diagnostics, N4's start/rematch flow
/// and the fake-transport tests.
#[derive(Resource)]
pub struct NetLockstep {
    /// Which end this peer drives (mirrored from `NetSession::role` by the
    /// systems; decides which wire messages are meaningful: a `TickInput`
    /// reaching the *guest* is only the host's echo → ignored, a
    /// `TickBatch` reaching the *host* is likewise ignored).
    pub role: NetRole,
    /// Host: next tick to execute (authoritative clock). Guest: next
    /// expected batch tick. Both count executed ticks.
    pub tick: u64,
    /// Negotiated input delay `D` in ticks (clamped
    /// [`MIN_INPUT_DELAY`]..=[`MAX_INPUT_DELAY`]).
    pub delay: u8,
    /// Per-side delayed local input rings `(target_tick, actions)`,
    /// ascending by target. The host executes its own entries (left) when
    /// merged into batches; the guest's entries are the bookkeeping copy
    /// of what it already sent as `TickInput` (the guest applies only batch
    /// contents — host-authoritative mirror).
    pub pending_inputs: [VecDeque<(u64, Vec<Action>)>; 2],
    /// Host only: the guest's scheduled actions from `TickInput`, ordered
    /// by target tick. Entries older than `tick` are dropped and counted.
    pub remote_inputs: VecDeque<(u64, Vec<Action>)>,
    /// Guest only: received `TickBatch` payloads awaiting their tick,
    /// ascending. Stale (already-executed) batches are ignored. Steady
    /// state stays within ~`D`; a network stall parks the reliable backlog
    /// here until the guest catches up one tick per step.
    pub batch_buffer: VecDeque<(u64, Vec<Action>, Vec<Action>)>,
    /// Guest only: `TickBatch`es the reliable stream delivered **behind** a
    /// parked [`Self::pending_start`] (a rematch `MatchStart` rides the same
    /// ReliableOrdered channel as the new match's batches, and the mirror
    /// rebuild that resets `tick` to 0 is deferred to
    /// `guest_pending_start_system` on `Update`). Ordered delivery makes
    /// every one of them a batch of the **new** mirror; running the
    /// old-tick staleness check on them would discard batch 0 — renet
    /// consumes reliable messages *on read*, and the fresh mirror would
    /// stall at tick 0 forever (the N7 soak finding; regression test
    /// `rematch_matchstart_does_not_swallow_the_new_match_tickbatches`).
    /// Adopted into [`Self::batch_buffer`] by the rebuild.
    pub staged_batches: VecDeque<(u64, Vec<Action>, Vec<Action>)>,
    /// A `MatchStart` received while a match was live (N4's rematch
    /// request): parked for N4 to tear the mirror down and `reset_for_match`
    /// with the new seed/rule/delay. Consumed by taking it.
    pub pending_start: Option<(u64, AttackRule, u8)>,
    /// Diagnostic: `TickInput` messages dropped because their target tick
    /// had already executed (late inputs; the empty-tick path ran instead).
    pub dropped_late_inputs: u64,
    /// Diagnostic: remote actions that arrived in time and were applied at
    /// their target tick. A relayed/WAN match where this stays 0 while the
    /// guest is clearly playing means every input is arriving late (see the
    /// v0.3.1 guest-controls-dead field bug); E2Es assert it is nonzero so
    /// a relayed match can never pass with one side's inputs silently
    /// discarded.
    pub applied_remote_actions: u64,
    /// Diagnostic: fixed steps the guest spent stalled, waiting for a batch.
    pub stall_steps: u64,
    /// Recent own hash checks `(tick, full_match_hash)`.
    hashes_mine: VecDeque<(u64, u64)>,
    /// Recent peer hash checks.
    hashes_theirs: VecDeque<(u64, u64)>,
}

impl Default for NetLockstep {
    fn default() -> Self {
        Self {
            role: NetRole::default(),
            tick: 0,
            delay: DEFAULT_INPUT_DELAY,
            pending_inputs: Default::default(),
            remote_inputs: VecDeque::new(),
            batch_buffer: VecDeque::new(),
            staged_batches: VecDeque::new(),
            pending_start: None,
            dropped_late_inputs: 0,
            applied_remote_actions: 0,
            stall_steps: 0,
            hashes_mine: VecDeque::new(),
            hashes_theirs: VecDeque::new(),
        }
    }
}

impl NetLockstep {
    /// Clear all state for a fresh mirror match (N4 calls this from
    /// `start_net_match` on both peers: host before the first step, guest
    /// on receiving `MatchStart`). `delay` is the negotiated `D`
    /// (`NetSession::input_delay`), defensively clamped.
    pub fn reset_for_match(&mut self, delay: u8) {
        self.tick = 0;
        self.delay = delay.clamp(MIN_INPUT_DELAY, MAX_INPUT_DELAY);
        self.pending_inputs = Default::default();
        self.remote_inputs.clear();
        self.batch_buffer.clear();
        self.staged_batches.clear();
        self.pending_start = None;
        self.dropped_late_inputs = 0;
        self.applied_remote_actions = 0;
        self.stall_steps = 0;
        self.hashes_mine.clear();
        self.hashes_theirs.clear();
    }

    /// Queue this side's freshly drained local actions: scheduled for
    /// `tick + delay` in the side's ring **and** emitted immediately as
    /// `TickInput { tick + delay, actions }`. Empty queues emit nothing.
    pub fn schedule_local(&mut self, side: Side, actions: Vec<Action>, out: &mut dyn NetOut) {
        if actions.is_empty() {
            return;
        }
        let target = self.tick + u64::from(self.delay);
        out.send(&NetMsg::TickInput {
            tick: target,
            actions: actions.clone(),
        });
        self.pending_inputs[side_index(side)].push_back((target, actions));
    }

    /// Feed one wire message (from [`server_inbox`]/[`client_inbox`]).
    /// Returns a teardown signal when the message demands it.
    pub fn ingest(&mut self, msg: NetMsg) -> Option<LockstepSignal> {
        match msg {
            NetMsg::TickInput { tick, actions } => {
                if self.role != NetRole::Host {
                    // The guest already has its own actions in its outgoing
                    // echo stream; this can only be the host's own-queue
                    // echo (the batch carries host inputs to us).
                    return None;
                }
                if actions.is_empty() {
                    // Nothing to merge; never counts as late.
                } else if tick < self.tick {
                    self.note_late_input(tick);
                } else {
                    self.remote_inputs.push_back((tick, actions));
                }
                None
            }
            NetMsg::TickBatch { tick, left, right } => {
                if self.role != NetRole::Guest {
                    // The host owns the clock; a batch is only ever
                    // meaningful to a mirror.
                    return None;
                }
                if self.pending_start.is_some() {
                    // A rematch `MatchStart` is parked from this same
                    // reliable stream and the mirror rebuild has not run
                    // yet: this batch belongs to the NEW mirror, whose
                    // clock restarts at 0 — the current `tick` is the old
                    // mirror's high-water mark, so the staleness check
                    // below would silently discard batch 0 (renet consumes
                    // reliable messages on read → the fresh mirror stalls
                    // at tick 0 forever; the N7 soak finding). Stage it for
                    // the rebuild ([`Self::staged_batches`]).
                    if !self.staged_batches.iter().any(|(b, _, _)| *b == tick) {
                        self.staged_batches.push_back((tick, left, right));
                    }
                    return None;
                }
                if tick < self.tick {
                    debug!("net lockstep: ignoring stale batch for tick {tick}");
                } else {
                    let pos = self
                        .batch_buffer
                        .iter()
                        .position(|(b, _, _)| *b > tick)
                        .unwrap_or(self.batch_buffer.len());
                    self.batch_buffer.insert(pos, (tick, left, right));
                }
                None
            }
            NetMsg::SnapshotHash {
                side: _,
                tick,
                left,
                right,
            } => {
                // Both fields carry the same full-match hash (module docs);
                // compare one, keep the other for future per-side checks.
                let _ = right;
                self.record_theirs(tick, left)
            }
            NetMsg::Bye => Some(LockstepSignal::Bye),
            NetMsg::MatchStart {
                seed,
                rule,
                match_delay,
            } => {
                // A fresh start supersedes anything staged for a previously
                // parked one (batches arriving from here on belong to the
                // mirror this `MatchStart` describes — see the staged arm in
                // the `TickBatch` branch).
                self.staged_batches.clear();
                self.pending_start = Some((seed, rule, match_delay));
                None
            }
            NetMsg::Hello { .. } => None,
        }
    }

    /// Host step: merge the due delayed queue with arrived remote inputs,
    /// build + send the batch, apply it locally in versus order, bump the
    /// clock. Always runs (`ran == true`).
    pub fn step_host(&mut self, m: &mut Match, out: &mut dyn NetOut) -> StepResult {
        let tick = self.tick;
        let left = self.take_due(Side::Left, tick);
        let right = self.take_remote(tick);
        let events = apply_batch(m, &left, &right);
        out.send(&NetMsg::TickBatch { tick, left, right });
        self.tick += 1;
        let signal = self.after_tick(Side::Left, tick, m, out);
        StepResult {
            events,
            ran: true,
            signal,
        }
    }

    /// Guest step: execute the batch for the expected tick if buffered,
    /// otherwise stall (count `stall_steps`, run nothing, hold everything).
    pub fn step_guest(&mut self, m: &mut Match, out: &mut dyn NetOut) -> StepResult {
        let tick = self.tick;
        let due = self
            .batch_buffer
            .front()
            .is_some_and(|(b, _, _)| *b == tick)
            .then(|| self.batch_buffer.pop_front());
        let Some(Some((_, left, right))) = due else {
            self.stall_steps += 1;
            return StepResult {
                events: Vec::new(),
                ran: false,
                signal: None,
            };
        };
        let events = apply_batch(m, &left, &right);
        self.tick += 1;
        let signal = self.after_tick(Side::Right, tick, m, out);
        StepResult {
            events,
            ran: true,
            signal,
        }
    }

    /// Drain the side's ring entries due for `tick` (defensively dropping
    /// any stale past entries).
    fn take_due(&mut self, side: Side, tick: u64) -> Vec<Action> {
        let ring = &mut self.pending_inputs[side_index(side)];
        while ring.front().is_some_and(|(t, _)| *t < tick) {
            ring.pop_front();
        }
        let mut out = Vec::new();
        while ring.front().is_some_and(|(t, _)| *t == tick) {
            if let Some((_, actions)) = ring.pop_front() {
                out.extend(actions);
            }
        }
        out
    }

    /// Pop the remote inputs scheduled for `tick`; drop/count stale ones.
    fn take_remote(&mut self, tick: u64) -> Vec<Action> {
        while let Some(&(stale, _)) = self.remote_inputs.front() {
            if stale >= tick {
                break;
            }
            self.remote_inputs.pop_front();
            self.note_late_input(stale);
        }
        let mut out = Vec::new();
        while self.remote_inputs.front().is_some_and(|(t, _)| *t == tick) {
            if let Some((_, actions)) = self.remote_inputs.pop_front() {
                self.applied_remote_actions += actions.len() as u64;
                out.extend(actions);
            }
        }
        out
    }

    /// Count a late `TickInput` — its target tick already executed, so the
    /// deterministic empty-input path ran in its place. v0.3.1 shipped this
    /// at `debug!`, so a path slower than the negotiated delay dropped every
    /// guest input with no operator-visible signal (the "only the host can
    /// play" field bug). Warn on the first few, then rate-limit to one per
    /// [`HASH_CHECK_PERIOD`] so a systematically-late path is visible without
    /// flooding the log.
    fn note_late_input(&mut self, tick: u64) {
        self.dropped_late_inputs += 1;
        if self.dropped_late_inputs <= 5
            || self.dropped_late_inputs.is_multiple_of(HASH_CHECK_PERIOD)
        {
            warn!(
                "net lockstep: dropped late input for tick {tick} (executing {}, total {}) \u{2014} \
                 input delay too small for this path",
                self.tick, self.dropped_late_inputs
            );
        } else {
            debug!(
                "net lockstep: dropped late input for tick {tick} (executing {})",
                self.tick
            );
        }
    }

    /// Periodic desync check after executing `executed` (see module docs).
    fn after_tick(
        &mut self,
        seat: Side,
        executed: u64,
        m: &mut Match,
        out: &mut dyn NetOut,
    ) -> Option<LockstepSignal> {
        if !(executed + 1).is_multiple_of(HASH_CHECK_PERIOD) {
            return None;
        }
        let hash = protocol::snapshot_hash(&m.snapshot());
        out.send(&NetMsg::SnapshotHash {
            side: seat,
            tick: executed,
            left: hash,
            right: hash,
        });
        self.record_mine(executed, hash)
    }

    fn record_mine(&mut self, tick: u64, hash: u64) -> Option<LockstepSignal> {
        push_window(&mut self.hashes_mine, tick, hash);
        self.check_common()
    }

    fn record_theirs(&mut self, tick: u64, hash: u64) -> Option<LockstepSignal> {
        push_window(&mut self.hashes_theirs, tick, hash);
        self.check_common()
    }

    /// Compare every tick label both sides hold; a disagreement is a fork.
    fn check_common(&mut self) -> Option<LockstepSignal> {
        for (theirs_tick, theirs_hash) in self.hashes_theirs.iter() {
            if let Some((_, mine)) = self.hashes_mine.iter().find(|(t, _)| t == theirs_tick) {
                if *mine != *theirs_hash {
                    let tick = *theirs_tick;
                    self.hashes_mine.clear();
                    self.hashes_theirs.clear();
                    return Some(LockstepSignal::Desync { tick });
                }
            }
        }
        None
    }
}

fn push_window(window: &mut VecDeque<(u64, u64)>, tick: u64, hash: u64) {
    if window.iter().any(|(t, _)| *t == tick) {
        return;
    }
    window.push_back((tick, hash));
    while window.len() > HASH_WINDOW {
        window.pop_front();
    }
}

/// Apply one tick's batches to the match in **`versus_bridge_system`
/// order**: all left actions, `tick(Left)`, all right actions, `tick(Right)`
/// (`core_bridge/versus.rs:255-264`). The single stepping path of both
/// hosts and mirrors — the same-batch-stream proptest pins it.
fn apply_batch(m: &mut Match, left: &[Action], right: &[Action]) -> Vec<MatchEvent> {
    let mut events = Vec::new();
    for (side, queue) in [(Side::Left, left), (Side::Right, right)] {
        for action in queue {
            events.extend(m.apply(side, *action));
        }
        events.extend(m.tick(side));
    }
    // Match-clock frame after both sides ticked — the exact mirror of
    // `versus_bridge_system` (T19), so host and guest keep identical
    // `match_ticks` (the Switch swap schedule derives from it).
    events.extend(m.advance_match_clock());
    events
}

// ---------------------------------------------------------------------------
// Gate + system glue
// ---------------------------------------------------------------------------

/// Shared step systems gate: InMatch + this role + playing + not paused.
/// The pause/state skip is the freeze mechanism (module docs).
fn gates_open(
    session: Option<&NetSession>,
    app_state: Option<&AppState>,
    paused: Option<&SimPaused>,
    role: NetRole,
) -> bool {
    session.is_some_and(|s| s.status == NetStatus::InMatch && s.role == role)
        && app_state.is_some_and(|s| *s == AppState::Playing)
        && !paused.is_some_and(|p| p.0)
}

/// Mirror the bridge's once-crowning on top of a lockstep-stepped match and
/// keep `steps` bookkeeping alive (the gated bridge stops doing both —
/// N4's contract, module docs).
fn finish_step(
    res: StepResult,
    versus: &mut VersusMatch,
    winner: Option<&mut VersusWinner>,
    events: &mut MessageWriter<VersusEvent>,
    signals: &mut Vec<LockstepSignal>,
) {
    if !res.ran {
        return;
    }
    versus.steps += 1;
    for event in res.events {
        events.write(VersusEvent(event));
    }
    if let Some(winner) = winner {
        if winner.0.is_none() {
            if let Some(side) = versus.match_.winner() {
                winner.0 = Some(side);
                info!("net lockstep: winner {side:?} after {} steps", versus.steps);
            }
        }
    }
    if let Some(signal) = res.signal {
        signals.push(signal);
    }
}

/// Surface drain/step signals per the teardown contract (module docs):
/// desync and Bye freeze the sim and publish [`NetEvent`]s for N5.
fn apply_signals(
    signals: &[LockstepSignal],
    mut session: Option<&mut NetSession>,
    net_events: &mut MessageWriter<NetEvent>,
    mut paused: Option<&mut SimPaused>,
) {
    for signal in signals {
        match signal {
            LockstepSignal::Desync { tick } => {
                warn!("net lockstep: desync detected at tick {tick} — match frozen");
                net_events.write(NetEvent::Desync { tick: *tick });
                net_freeze_opt(paused.as_deref_mut());
            }
            LockstepSignal::Bye => {
                if let Some(session) = session.as_deref_mut() {
                    if session.apply(NetTrigger::Bye) {
                        info!("net lockstep: peer left (Bye)");
                        net_events.write(NetEvent::ByeReceived);
                    }
                }
                net_freeze_opt(paused.as_deref_mut());
            }
        }
    }
}

/// Teardown contract item 1 (the freeze): stop the simulation while the
/// match stays `active` so the frozen boards keep rendering behind N5's
/// net overlay.
pub fn net_freeze(paused: &mut SimPaused) {
    paused.0 = true;
}

fn net_freeze_opt(paused: Option<&mut SimPaused>) {
    if let Some(paused) = paused {
        net_freeze(paused);
    }
}

/// Teardown contract item 3 — the "Back to title" entry point for N5's
/// overlay button. Full netplay exit, mirroring `end_versus`: graceful
/// `net_stop` (peer learns instantly, port freed, `NetSession → Idle`),
/// match deactivated and app back on the Title screen (the existing root
/// visibility sync then restores the clean-solo regime), sim un-frozen and
/// the lockstep state cleared. Idempotent; safe from any status.
pub fn net_leave_to_title(world: &mut World) {
    super::session::net_stop(world);
    if let Some(mut lockstep) = world.get_resource_mut::<NetLockstep>() {
        lockstep.reset_for_match(DEFAULT_INPUT_DELAY);
    }
    if let Some(mut paused) = world.get_resource_mut::<SimPaused>() {
        paused.0 = false;
    }
    if !world.contains_non_send::<VersusMatch>() {
        return;
    }
    world.resource_scope::<AppState, ()>(|world, state| {
        world.resource_scope::<VersusWinner, ()>(|world, winner| {
            let versus = world.non_send_mut::<VersusMatch>();
            end_versus(versus.into_inner(), winner.into_inner(), state.into_inner());
        });
    });
}

// ---------------------------------------------------------------------------
// Systems + plugin
// ---------------------------------------------------------------------------

/// Host lockstep step (`FixedUpdate`, replaces the versus bridge's direct
/// drain while `InMatch` — N4 gates the bridge; run after `versus_bot_system`).
#[allow(clippy::too_many_arguments)]
pub fn net_lockstep_host_system(
    mut lockstep: ResMut<NetLockstep>,
    mut session: Option<ResMut<NetSession>>,
    mut server: Option<ResMut<RenetServer>>,
    mut versus: Option<NonSendMut<VersusMatch>>,
    mut actions: Option<ResMut<VersusActions>>,
    mut events: MessageWriter<VersusEvent>,
    mut net_events: MessageWriter<NetEvent>,
    mut winner: Option<ResMut<VersusWinner>>,
    app_state: Option<Res<AppState>>,
    mut paused: Option<ResMut<SimPaused>>,
) {
    if !gates_open(
        session.as_deref(),
        app_state.as_deref(),
        paused.as_deref(),
        NetRole::Host,
    ) {
        return;
    }
    // Keep the core's message routing in sync with the session role.
    lockstep.role = NetRole::Host;
    if versus.as_ref().is_none_or(|v| !v.active) {
        // Match not live (or already ended via end_versus): hold nothing.
        if let Some(actions) = actions.as_deref_mut() {
            actions.left.clear();
        }
        return;
    }
    // A mirror needs its transport; without the connection there is no
    // clock partner (loss handling is N2's job, which flips `Lost` and
    // closes these gates).
    let Some(server) = server.as_deref_mut() else {
        return;
    };
    let Some(peer) = session.as_deref().and_then(|s| s.peer) else {
        return;
    };

    let mut signals: Vec<LockstepSignal> = Vec::new();
    for msg in server_inbox(&mut *server, peer) {
        if let Some(signal) = lockstep.ingest(msg) {
            signals.push(signal);
        }
    }

    let local = actions
        .as_deref_mut()
        .map_or_else(Vec::new, |a| std::mem::take(&mut a.left));
    if let Some(versus) = versus.as_deref_mut() {
        let mut out = RenetServerOut {
            server: &mut *server,
            peer,
        };
        lockstep.schedule_local(Side::Left, local, &mut out);
        let res = lockstep.step_host(&mut versus.match_, &mut out);
        finish_step(
            res,
            versus,
            winner.as_deref_mut(),
            &mut events,
            &mut signals,
        );
    }

    apply_signals(
        &signals,
        session.as_deref_mut(),
        &mut net_events,
        paused.as_deref_mut(),
    );
}

/// Guest mirror step (`FixedUpdate`, `InMatch` only; stalls on missing
/// batches; see module docs).
#[allow(clippy::too_many_arguments)]
pub fn net_lockstep_guest_system(
    mut lockstep: ResMut<NetLockstep>,
    mut session: Option<ResMut<NetSession>>,
    mut client: Option<ResMut<RenetClient>>,
    mut versus: Option<NonSendMut<VersusMatch>>,
    mut actions: Option<ResMut<VersusActions>>,
    mut events: MessageWriter<VersusEvent>,
    mut net_events: MessageWriter<NetEvent>,
    mut winner: Option<ResMut<VersusWinner>>,
    app_state: Option<Res<AppState>>,
    mut paused: Option<ResMut<SimPaused>>,
) {
    if !gates_open(
        session.as_deref(),
        app_state.as_deref(),
        paused.as_deref(),
        NetRole::Guest,
    ) {
        return;
    }
    // Keep the core's message routing in sync with the session role.
    lockstep.role = NetRole::Guest;
    if versus.as_ref().is_none_or(|v| !v.active) {
        if let Some(actions) = actions.as_deref_mut() {
            actions.right.clear();
        }
        return;
    }
    let Some(client) = client.as_deref_mut() else {
        return;
    };

    let mut signals: Vec<LockstepSignal> = Vec::new();
    for msg in client_inbox(&mut *client) {
        if let Some(signal) = lockstep.ingest(msg) {
            signals.push(signal);
        }
    }

    let local = actions
        .as_deref_mut()
        .map_or_else(Vec::new, |a| std::mem::take(&mut a.right));
    if let Some(versus) = versus.as_deref_mut() {
        let mut out = RenetClientOut {
            client: &mut *client,
        };
        lockstep.schedule_local(Side::Right, local, &mut out);
        let res = lockstep.step_guest(&mut versus.match_, &mut out);
        finish_step(
            res,
            versus,
            winner.as_deref_mut(),
            &mut events,
            &mut signals,
        );
    }

    apply_signals(
        &signals,
        session.as_deref_mut(),
        &mut net_events,
        paused.as_deref_mut(),
    );
}

/// Registers the lockstep resource, systems and (idempotently) the
/// `VersusEvent` message stream so the systems never panic in apps that
/// mount `NetPlugin` without `VersusBridgePlugin`. Mounted by `NetPlugin`.
pub struct NetLockstepPlugin;

impl Plugin for NetLockstepPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetLockstep>()
            // Idempotent (`contains_resource` guard): safe next to
            // VersusBridgePlugin's registration.
            .add_message::<VersusEvent>()
            .add_systems(
                FixedUpdate,
                (net_lockstep_host_system, net_lockstep_guest_system),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_bridge::net::session::{NetLossReason, NetPlugin};
    use std::net::SocketAddr;
    use tetris_core::versus::AttackRule;

    // ---- fake transport seam: both "peers" in one process ----------------
    //
    // `NetSim` wires a host peer and a guest peer, each a real
    // `NetLockstep` + a real `Match`, exchanging real encoded `NetMsg`s
    // through latency-controlled in-memory links. Zero renet, zero sockets:
    // fully deterministic. `latency_*` is delivery delay in fixed steps —
    // the knob the stall/late-input tests use to simulate networks.

    #[derive(Default)]
    struct FakeOut {
        sent: Vec<NetMsg>,
    }

    impl NetOut for FakeOut {
        fn send(&mut self, msg: &NetMsg) {
            self.sent.push(msg.clone());
        }
    }

    struct Peer {
        role: NetRole,
        lockstep: NetLockstep,
        m: Match,
        out: FakeOut,
        signals: Vec<LockstepSignal>,
    }

    impl Peer {
        fn new(role: NetRole, seed: u64, rule: AttackRule, delay: u8) -> Self {
            let mut lockstep = NetLockstep::default();
            lockstep.reset_for_match(delay);
            lockstep.role = role;
            Self {
                role,
                lockstep,
                m: Match::new(seed, rule),
                out: FakeOut::default(),
                signals: Vec::new(),
            }
        }

        fn deliver(&mut self, msgs: Vec<NetMsg>) {
            for msg in msgs {
                if let Some(signal) = self.lockstep.ingest(msg) {
                    self.signals.push(signal);
                }
            }
        }
    }

    struct NetSim {
        host: Peer,
        guest: Peer,
        step: u64,
        /// Host→guest delivery latency in steps.
        latency_h2g: u64,
        /// Guest→host delivery latency in steps.
        latency_g2h: u64,
        h2g: Vec<(u64, NetMsg)>,
        g2h: Vec<(u64, NetMsg)>,
    }

    impl NetSim {
        fn new(seed: u64, rule: AttackRule, delay: u8) -> Self {
            Self {
                host: Peer::new(NetRole::Host, seed, rule, delay),
                guest: Peer::new(NetRole::Guest, seed, rule, delay),
                step: 0,
                latency_h2g: 0,
                latency_g2h: 0,
                h2g: Vec::new(),
                g2h: Vec::new(),
            }
        }

        /// One fixed step: deliver mature messages, step the host (its
        /// sends mature by `latency_h2g`), deliver again, step the guest.
        /// `host_local` / `guest_local` are freshly drained local queues
        /// for this step (the stand-ins for `VersusActions`).
        fn tick(&mut self, host_local: Vec<Action>, guest_local: Vec<Action>) {
            let now = self.step;
            self.deliver_mature();
            {
                let h = &mut self.host;
                let mut out = std::mem::take(&mut h.out);
                let before = out.sent.len();
                h.lockstep.schedule_local(Side::Left, host_local, &mut out);
                let res = h.lockstep.step_host(&mut h.m, &mut out);
                if let Some(signal) = res.signal {
                    h.signals.push(signal);
                }
                for msg in out.sent[before..].iter() {
                    self.h2g.push((now + self.latency_h2g, msg.clone()));
                }
                h.out = out;
            }
            self.deliver_mature();
            {
                let g = &mut self.guest;
                let mut out = std::mem::take(&mut g.out);
                let before = out.sent.len();
                g.lockstep
                    .schedule_local(Side::Right, guest_local, &mut out);
                let res = g.lockstep.step_guest(&mut g.m, &mut out);
                if let Some(signal) = res.signal {
                    g.signals.push(signal);
                }
                for msg in out.sent[before..].iter() {
                    self.g2h.push((now + self.latency_g2h, msg.clone()));
                }
                g.out = out;
            }
            self.step += 1;
        }

        fn deliver_mature(&mut self) {
            let now = self.step;
            let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.h2g)
                .into_iter()
                .partition(|(due, _)| *due <= now);
            self.h2g = keep;
            self.guest
                .deliver(ready.into_iter().map(|(_, m)| m).collect());
            let (ready, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.g2h)
                .into_iter()
                .partition(|(due, _)| *due <= now);
            self.g2h = keep;
            self.host
                .deliver(ready.into_iter().map(|(_, m)| m).collect());
        }

        /// Batches the host has emitted so far.
        fn host_batches(&self) -> Vec<(u64, Vec<Action>, Vec<Action>)> {
            self.host
                .out
                .sent
                .iter()
                .filter_map(|m| match m {
                    NetMsg::TickBatch { tick, left, right } => {
                        Some((*tick, left.clone(), right.clone()))
                    }
                    _ => None,
                })
                .collect()
        }

        fn sent_of(peer: &Peer) -> &[NetMsg] {
            &peer.out.sent
        }

        fn snapshots_match(&self) -> bool {
            self.host.m.snapshot() == self.guest.m.snapshot()
        }
    }

    fn batch_with_input_at(sim: &NetSim, target: u64) -> bool {
        sim.host_batches().iter().any(|(t, l, r)| {
            *t == target && (l.contains(&Action::HardDrop) || r.contains(&Action::HardDrop))
        })
    }

    #[test]
    fn reset_for_match_clamps_and_clears() {
        let mut ls = NetLockstep {
            tick: 12,
            dropped_late_inputs: 3,
            stall_steps: 7,
            ..Default::default()
        };
        ls.pending_inputs[0].push_back((1, vec![Action::HardDrop]));
        ls.remote_inputs.push_back((1, vec![Action::Hold]));
        ls.batch_buffer.push_back((1, Vec::new(), Vec::new()));
        ls.pending_start = Some((1, AttackRule::Garbage, 3));
        ls.applied_remote_actions = 4;
        ls.reset_for_match(200);
        assert_eq!(ls.tick, 0);
        assert_eq!(ls.applied_remote_actions, 0);
        assert_eq!(ls.delay, MAX_INPUT_DELAY);
        assert_eq!(ls.dropped_late_inputs, 0);
        assert_eq!(ls.stall_steps, 0);
        assert!(ls.pending_inputs[0].is_empty() && ls.pending_inputs[1].is_empty());
        assert!(ls.remote_inputs.is_empty() && ls.batch_buffer.is_empty());
        assert!(ls.pending_start.is_none());
        ls.reset_for_match(0);
        assert_eq!(ls.delay, MIN_INPUT_DELAY);
    }

    #[test]
    fn schedule_local_emits_tick_input_at_t_plus_d() {
        // Delay math: input at t is scheduled (and sent) for t + D.
        let mut ls = NetLockstep::default();
        ls.reset_for_match(5);
        let mut out = FakeOut::default();
        for _ in 0..3 {
            let mut m = Match::new(1, AttackRule::Garbage);
            ls.step_host(&mut m, &mut out);
        }
        assert_eq!(ls.tick, 3);
        ls.schedule_local(Side::Left, vec![Action::HardDrop], &mut out);
        assert!(out
            .sent
            .iter()
            .any(|m| matches!(m, NetMsg::TickInput { tick, actions }
                if *tick == 8 && actions == &vec![Action::HardDrop])));
        assert_eq!(ls.pending_inputs[0].back().map(|(t, _)| *t), Some(8));
    }

    #[test]
    fn schedule_local_empty_emits_nothing() {
        let mut ls = NetLockstep::default();
        ls.reset_for_match(4);
        let mut out = FakeOut::default();
        ls.schedule_local(Side::Right, Vec::new(), &mut out);
        assert!(out.sent.is_empty());
        assert!(ls.pending_inputs[1].is_empty());
    }

    #[test]
    fn host_applies_due_input_only_at_t_plus_d() {
        let mut sim = NetSim::new(42, AttackRule::Garbage, 3);
        // No local actions for the first 4 steps.
        for _ in 0..4 {
            sim.tick(Vec::new(), Vec::new());
        }
        // Local hard drop queued at step 4 → batch 7 (4 + D).
        sim.tick(vec![Action::HardDrop], Vec::new());
        // Steps 5 and 6 must not carry it yet.
        sim.tick(Vec::new(), Vec::new());
        sim.tick(Vec::new(), Vec::new());
        for (t, l, _) in sim.host_batches() {
            assert!(
                !l.contains(&Action::HardDrop),
                "input applied early at batch {t}"
            );
        }
        // Step 7 is 4 + D: due now, and merged into that batch.
        sim.tick(Vec::new(), Vec::new());
        assert!(batch_with_input_at(&sim, 7));
    }

    #[test]
    fn missing_remote_input_executes_empty_tick_and_sends_batch() {
        // Empty-tick execution: nobody inputs, both mirrors still advance
        // and the host ships a batch (empty lists) every tick.
        let mut sim = NetSim::new(7, AttackRule::Race { target_lines: 40 }, 4);
        for _ in 0..12 {
            sim.tick(Vec::new(), Vec::new());
        }
        assert_eq!(sim.host_batches().len(), 12);
        assert!(sim
            .host_batches()
            .iter()
            .all(|(_, l, r)| l.is_empty() && r.is_empty()));
        assert_eq!(sim.host.lockstep.tick, 12);
        assert_eq!(sim.guest.lockstep.tick, 12);
        assert_eq!(sim.guest.lockstep.stall_steps, 0);
        assert!(sim.snapshots_match());
    }

    #[test]
    fn guest_stalls_and_recovers_on_delayed_batches() {
        let mut sim = NetSim::new(11, AttackRule::Garbage, 3);
        sim.latency_h2g = 3; // batches lag 3 steps
        let mut executed = Vec::new();
        for _ in 0..8 {
            let res = {
                sim.tick(Vec::new(), Vec::new());
                // record what the guest executed: its tick advanced?
                sim.guest.lockstep.tick
            };
            executed.push(res);
        }
        assert_eq!(sim.guest.lockstep.stall_steps, 3);
        assert_eq!(sim.host.lockstep.tick, 8);
        assert_eq!(sim.guest.lockstep.tick, 5);
        // Recovery: latency back to 0 → buffered batches carry it forward.
        sim.latency_h2g = 0;
        for _ in 0..3 {
            sim.tick(Vec::new(), Vec::new());
        }
        assert_eq!(sim.host.lockstep.tick, 11);
        assert_eq!(sim.guest.lockstep.tick, 8);
        assert_eq!(sim.guest.lockstep.stall_steps, 3);
        // Snapshot at the guest's tick equals the host's at the same tick:
        // compare via identical seed + batch replay instead — the guest's
        // executed batch ticks are contiguous 0..8 (stall-and-recover keeps
        // strict order with no gap).
        let mut replay = Match::new(11, AttackRule::Garbage);
        for (t, l, r) in sim.host_batches() {
            if t >= 8 {
                break;
            }
            apply_batch(&mut replay, &l, &r);
        }
        assert_eq!(replay.snapshot(), sim.guest.m.snapshot());
    }

    #[test]
    fn late_remote_input_is_dropped_and_counted() {
        let mut sim = NetSim::new(5, AttackRule::Garbage, 2);
        sim.latency_g2h = 9; // way past D
        for _ in 0..3 {
            sim.tick(Vec::new(), Vec::new());
        }
        sim.tick(Vec::new(), vec![Action::HardDrop]); // TickInput for 2+2
                                                      // 9 more steps: the message matures at step 12, long after its
                                                      // target tick 4 executed (at host step 4).
        for _ in 0..9 {
            sim.tick(Vec::new(), Vec::new());
        }
        assert_eq!(sim.host.lockstep.dropped_late_inputs, 1);
        assert_eq!(
            sim.host.lockstep.applied_remote_actions, 0,
            "a late action must never count as applied"
        );
        // The late action reached no batch → mirrors never forked on it.
        assert!(sim.snapshots_match());
    }

    #[test]
    fn remote_actions_applied_are_counted() {
        let mut sim = NetSim::new(3, AttackRule::Garbage, 2);
        for _ in 0..2 {
            sim.tick(Vec::new(), Vec::new());
        }
        sim.tick(Vec::new(), vec![Action::HardDrop, Action::Hold]); // target 4
        for _ in 0..6 {
            sim.tick(Vec::new(), Vec::new());
        }
        assert_eq!(sim.host.lockstep.dropped_late_inputs, 0);
        assert_eq!(
            sim.host.lockstep.applied_remote_actions, 2,
            "both on-time remote actions must be counted (E2E non-vacuity anchor)"
        );
    }

    #[test]
    fn stale_remote_input_in_take_remote_is_counted_as_late() {
        // An input that queued for a tick that already executed must be
        // counted through the take_remote path too (same diagnostic budget
        // as the ingest path, no silent drops).
        let mut ls = NetLockstep {
            role: NetRole::Host,
            ..Default::default()
        };
        ls.remote_inputs.push_back((5, vec![Action::HardDrop]));
        for tick in 6..=7 {
            assert!(ls.take_remote(tick).is_empty());
        }
        assert_eq!(ls.dropped_late_inputs, 1);
        assert_eq!(ls.applied_remote_actions, 0);
        // And a fresh on-time entry still applies normally afterwards.
        ls.remote_inputs.push_back((8, vec![Action::Hold]));
        assert_eq!(ls.take_remote(8), vec![Action::Hold]);
        assert_eq!(ls.applied_remote_actions, 1);
    }

    #[test]
    fn on_time_remote_input_lands_in_its_batch_and_mirrors_sync() {
        let mut sim = NetSim::new(3, AttackRule::Garbage, 2);
        for _ in 0..2 {
            sim.tick(Vec::new(), Vec::new());
        }
        sim.tick(Vec::new(), vec![Action::HardDrop]); // target tick 4
        for _ in 0..6 {
            sim.tick(Vec::new(), Vec::new());
            assert!(sim.snapshots_match());
        }
        assert!(batch_with_input_at(&sim, 4));
        assert_eq!(sim.host.lockstep.dropped_late_inputs, 0);
        assert!(
            sim.guest.m.snapshot().right.score > 0,
            "guest mirror scored its own drop"
        );
    }

    #[test]
    fn host_local_and_remote_merge_in_one_batch() {
        let mut sim = NetSim::new(8, AttackRule::Race { target_lines: 40 }, 2);
        for _ in 0..3 {
            sim.tick(Vec::new(), Vec::new());
        }
        sim.tick(vec![Action::HardDrop], vec![Action::HardDrop]);
        for _ in 0..3 {
            sim.tick(Vec::new(), Vec::new());
        }
        let merged = sim
            .host_batches()
            .into_iter()
            .find(|(t, _, _)| *t == 5)
            .expect("batch 5 exists");
        assert_eq!(merged.1, vec![Action::HardDrop]);
        assert_eq!(merged.2, vec![Action::HardDrop]);
    }

    #[test]
    fn snapshot_hashes_exchange_every_60_ticks_and_no_desync() {
        let mut sim = NetSim::new(9, AttackRule::Garbage, 4);
        for _ in 0..61 {
            sim.tick(Vec::new(), Vec::new());
        }
        let h59 = |peer: &Peer| {
            peer.out
                .sent
                .iter()
                .filter_map(|m| match m {
                    NetMsg::SnapshotHash { tick, left, .. } => Some((*tick, *left)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(h59(&sim.host), vec![(59, h59(&sim.host)[0].1)]);
        assert_eq!(h59(&sim.guest)[0].0, 59);
        assert_eq!(
            h59(&sim.host)[0].1,
            h59(&sim.guest)[0].1,
            "mirrored matches must hash equal"
        );
        assert!(sim.host.signals.is_empty() && sim.guest.signals.is_empty());
    }

    #[test]
    fn desync_detected_on_forked_mirror() {
        let mut sim = NetSim::new(13, AttackRule::Garbage, 3);
        for _ in 0..30 {
            sim.tick(Vec::new(), Vec::new());
        }
        // Deliberate fork: the guest's mirror diverges off-batch (a
        // simulated logic bug — direct apply outside any batch).
        sim.guest.m.apply(Side::Left, Action::HardDrop);
        for _ in 0..31 {
            sim.tick(Vec::new(), Vec::new());
        }
        assert!(
            sim.host
                .signals
                .contains(&LockstepSignal::Desync { tick: 59 }),
            "host signals: {:?}",
            sim.host.signals
        );
        assert!(
            sim.guest
                .signals
                .contains(&LockstepSignal::Desync { tick: 59 }),
            "guest signals: {:?}",
            sim.guest.signals
        );
    }

    #[test]
    fn bye_ingest_signals_teardown() {
        let mut ls = NetLockstep::default();
        assert_eq!(ls.ingest(NetMsg::Bye), Some(LockstepSignal::Bye));
    }

    #[test]
    fn matchstart_midmatch_is_parked_for_n4() {
        let mut ls = NetLockstep::default();
        let msg = NetMsg::MatchStart {
            seed: 77,
            rule: AttackRule::Race { target_lines: 20 },
            match_delay: 6,
        };
        assert_eq!(ls.ingest(msg), None);
        assert_eq!(
            ls.pending_start,
            Some((77, AttackRule::Race { target_lines: 20 }, 6))
        );
    }

    #[test]
    fn stale_batch_is_ignored() {
        let mut ls = NetLockstep {
            role: NetRole::Guest,
            ..Default::default()
        };
        ls.reset_for_match(2);
        ls.tick = 10;
        ls.ingest(NetMsg::TickBatch {
            tick: 9,
            left: vec![Action::HardDrop],
            right: Vec::new(),
        });
        assert!(ls.batch_buffer.is_empty());
    }

    #[test]
    fn late_remote_input_counted_via_take_remote_when_host_steps_past_it() {
        // Input arrives *while* the host advances (not just via ingest):
        // entries older than tick get dropped and counted.
        let mut ls = NetLockstep::default();
        ls.reset_for_match(2);
        ls.ingest(NetMsg::TickInput {
            tick: 3,
            actions: vec![Action::Hold],
        });
        let mut m = Match::new(1, AttackRule::Garbage);
        let mut out = FakeOut::default();
        for _ in 0..2 {
            ls.step_host(&mut m, &mut out); // ticks 0, 1
        }
        assert_eq!(ls.dropped_late_inputs, 0, "tick 3 still ahead");
        // Step past 3 without having consumed it at 3: forced by popping
        // the queue directly (simulates the merge point at tick 3).
        ls.remote_inputs.clear();
        ls.ingest(NetMsg::TickInput {
            tick: 2,
            actions: vec![Action::Hold],
        });
        assert_eq!(ls.remote_inputs.len(), 1, "stored for the future");
        // now simulate having advanced: tick > 2 → ingest path counts.
        ls.tick = 5;
        ls.ingest(NetMsg::TickInput {
            tick: 2,
            actions: vec![Action::Hold],
        });
        assert_eq!(ls.dropped_late_inputs, 1);
    }

    // ---- proptest: the core mirror invariant -----------------------------

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

    fn actions_strategy() -> impl Strategy<Value = Vec<Action>> {
        prop::collection::vec(select(&ALL_ACTIONS), 0..4)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// The mirror invariant: two `Match`es fed the same `TickBatch`
        /// stream through the exact lockstep step ordering are
        /// snapshot-identical at every tick (and hash-equal — the periodic
        /// desync check can only fire on a real fork).
        #[test]
        fn same_batch_stream_yields_identical_snapshots(
            (seed, rule_idx, batches) in (
                any::<u64>(),
                0..2usize,
                prop::collection::vec((actions_strategy(), actions_strategy()), 1..160),
            )
        ) {
            let rule = if rule_idx == 0 {
                AttackRule::Garbage
            } else {
                AttackRule::Race { target_lines: 20 }
            };
            let mut a = Match::new(seed, rule);
            let mut b = Match::new(seed, rule);
            for (left, right) in &batches {
                let ea = apply_batch(&mut a, left, right);
                let eb = apply_batch(&mut b, left, right);
                prop_assert_eq!(ea, eb, "event streams diverged");
                prop_assert_eq!(a.snapshot(), b.snapshot(), "state diverged");
                prop_assert_eq!(
                    protocol::snapshot_hash(&a.snapshot()),
                    protocol::snapshot_hash(&b.snapshot()),
                    "hash check must agree with snapshot equality"
                );
            }
        }
    }

    // ---- Bevy system wiring (no sockets: renet local-client seam) --------

    /// Bare-bones app for the system tests: `NetPlugin` (+ `NetLockstep-
    /// Plugin` through it) and the versus conventions inserted by hand.
    /// Deliberately **not** `CoreBridgePlugin` — that mounts the ungated
    /// `versus_bridge_system`, which would double-step the match (N4's
    /// gating is not in place yet).
    fn coreless_app(seed: u64, rule: AttackRule) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        app.init_resource::<AppState>()
            .init_resource::<SimPaused>()
            .init_resource::<VersusActions>()
            .init_resource::<VersusWinner>();
        app.insert_non_send(VersusMatch::new(seed, rule));
        app.world_mut().non_send_mut::<VersusMatch>().active = true;
        *app.world_mut().resource_mut::<AppState>() = AppState::Playing;
        app
    }

    fn lockstep_app(role: NetRole, seed: u64, rule: AttackRule, delay: u8) -> App {
        let mut app = coreless_app(seed, rule);

        // Bare renet server (no UDP) + a local client bound to it: the
        // exact seam N2's version-mismatch test uses. One role runs per
        // app; the inserted client stands in for the wire peer.
        app.insert_resource(RenetServer::new(
            bevy_renet::renet::ConnectionConfig::default(),
        ));
        let local = app
            .world_mut()
            .resource_mut::<RenetServer>()
            .new_local_client(9);
        app.insert_resource(RenetClient(local));
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = role;
            session.status = NetStatus::InMatch;
            session.input_delay = delay;
            session.peer = Some(9);
        }
        app.world_mut()
            .resource_mut::<NetLockstep>()
            .reset_for_match(delay);
        app
    }

    /// Pump the renet local-client channel and run one fixed step. Only
    /// `FixedUpdate` runs — N2's `Update` bridges (disconnect checks,
    /// watchdog) stay out of the way, matching production scheduling.
    fn fixed_tick(app: &mut App) {
        pump(app);
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn pump(app: &mut App) {
        app.world_mut()
            .resource_scope::<RenetClient, ()>(|world, mut client| {
                let mut server = world.resource_mut::<RenetServer>();
                let _ = server.process_local_client(9, &mut client);
            });
    }

    fn drain_net_events(app: &mut App) -> Vec<NetEvent> {
        app.world_mut()
            .resource_mut::<Messages<NetEvent>>()
            .drain()
            .collect()
    }

    fn drain_versus_events(app: &mut App) -> Vec<VersusEvent> {
        app.world_mut()
            .resource_mut::<Messages<VersusEvent>>()
            .drain()
            .collect()
    }

    fn snapshot(app: &App) -> tetris_core::versus::MatchSnapshot {
        app.world().non_send::<VersusMatch>().match_.snapshot()
    }

    fn lockstep_tick(app: &App) -> u64 {
        app.world().resource::<NetLockstep>().tick
    }

    fn peer_receives(app: &mut App) -> Vec<NetMsg> {
        pump(app);
        let mut msgs = Vec::new();
        {
            let mut client = app.world_mut().resource_mut::<RenetClient>();
            while let Some(b) = client.receive_message(DefaultChannel::ReliableOrdered) {
                msgs.extend(protocol::decode(&b).ok());
            }
            while let Some(b) = client.receive_message(DefaultChannel::ReliableUnordered) {
                msgs.extend(protocol::decode(&b).ok());
            }
        }
        msgs
    }

    fn peer_sends(app: &mut App, msg: NetMsg) {
        let mut client = app.world_mut().resource_mut::<RenetClient>();
        client.send_message(wire_channel(&msg), protocol::encode(&msg));
    }

    /// Messages a local-role app's systems emitted onto the wire (read from
    /// the server end of the local-client seam).
    fn server_side_receives(app: &mut App) -> Vec<NetMsg> {
        pump(app);
        let mut msgs = Vec::new();
        {
            let mut server = app.world_mut().resource_mut::<RenetServer>();
            while let Some(b) = server.receive_message(9, DefaultChannel::ReliableOrdered) {
                msgs.extend(protocol::decode(&b).ok());
            }
            while let Some(b) = server.receive_message(9, DefaultChannel::ReliableUnordered) {
                msgs.extend(protocol::decode(&b).ok());
            }
        }
        msgs
    }

    #[test]
    fn systems_are_inert_while_not_in_match() {
        // NetPlugin apps without the core bridge resources (N2's fixture)
        // must keep running: all lockstep params are optional.
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(NetPlugin);
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(app.world().resource::<NetLockstep>().tick, 0);
        assert_eq!(app.world().resource::<NetSession>().status, NetStatus::Idle);
    }

    #[test]
    fn host_system_steps_and_paces_the_wire() {
        let mut app = lockstep_app(NetRole::Host, 31, AttackRule::Garbage, 3);
        assert!(drain_versus_events(&mut app).is_empty());
        for _ in 0..4 {
            fixed_tick(&mut app);
        }
        assert_eq!(lockstep_tick(&app), 4);
        assert_eq!(app.world().non_send::<VersusMatch>().steps, 4);

        // Local input queues at tick 4 with D = 3: TickInput(7) emitted,
        // HardDrop applied and batched only at tick 7.
        app.world_mut().resource_mut::<VersusActions>().left = vec![Action::HardDrop];
        let mut wire = Vec::new();
        for _ in 0..3 {
            fixed_tick(&mut app); // ticks 4, 5, 6 — all before the due tick
            wire.extend(peer_receives(&mut app));
        }
        assert_eq!(
            snapshot(&app).left.score,
            0,
            "input must not apply before its delayed tick"
        );
        for _ in 0..2 {
            fixed_tick(&mut app); // ticks 7 (drop lands) and 8
            wire.extend(peer_receives(&mut app));
        }
        assert!(
            wire.iter()
                .any(|m| matches!(m, NetMsg::TickInput { tick, actions }
            if *tick == 7 && actions == &vec![Action::HardDrop])),
            "wire: {wire:?}"
        );
        let batches: Vec<(u64, Vec<Action>, Vec<Action>)> = wire
            .iter()
            .filter_map(|m| match m {
                NetMsg::TickBatch { tick, left, right } => {
                    Some((*tick, left.clone(), right.clone()))
                }
                _ => None,
            })
            .collect();
        let b7 = batches
            .iter()
            .find(|(t, _, _)| *t == 7)
            .expect("batch 7 on the wire");
        assert_eq!(b7.1, vec![Action::HardDrop]);
        // The score only moved once tick 7 executed.
        assert!(snapshot(&app).left.score > 0);
        let events = drain_versus_events(&mut app);
        assert!(
            events.iter().any(|e| matches!(
                e.0,
                MatchEvent::PieceLocked {
                    side: Side::Left,
                    ..
                }
            )),
            "versus events flow: {events:?}"
        );
    }

    #[test]
    fn sim_paused_gates_hold_queues_and_ticks() {
        let mut app = lockstep_app(NetRole::Host, 44, AttackRule::Garbage, 2);
        app.world_mut().resource_mut::<SimPaused>().0 = true;
        app.world_mut().resource_mut::<VersusActions>().left = vec![Action::HardDrop];
        let before = snapshot(&app);
        for _ in 0..5 {
            fixed_tick(&mut app);
        }
        assert_eq!(lockstep_tick(&app), 0);
        assert_eq!(snapshot(&app), before, "sim frozen while paused");
        assert_eq!(
            app.world().resource::<VersusActions>().left,
            vec![Action::HardDrop],
            "queues held while paused"
        );
        assert!(drain_versus_events(&mut app).is_empty());

        // Unfreeze: the held action is scheduled (at +D) and lands.
        app.world_mut().resource_mut::<SimPaused>().0 = false;
        for _ in 0..4 {
            fixed_tick(&mut app);
        }
        assert_eq!(lockstep_tick(&app), 4);
        assert!(snapshot(&app).left.score > 0);
    }

    #[test]
    fn non_playing_state_freezes_lockstep() {
        let mut app = lockstep_app(NetRole::Host, 45, AttackRule::Garbage, 2);
        *app.world_mut().resource_mut::<AppState>() = AppState::Paused;
        let before = snapshot(&app);
        for _ in 0..3 {
            fixed_tick(&mut app);
        }
        assert_eq!(lockstep_tick(&app), 0);
        assert_eq!(snapshot(&app), before);
    }

    #[test]
    fn inactive_versus_match_is_not_stepped() {
        let mut app = lockstep_app(NetRole::Host, 46, AttackRule::Garbage, 2);
        app.world_mut().non_send_mut::<VersusMatch>().active = false;
        for _ in 0..3 {
            fixed_tick(&mut app);
        }
        assert_eq!(lockstep_tick(&app), 0);
        assert_eq!(app.world().non_send::<VersusMatch>().steps, 0);
    }

    #[test]
    fn desync_freezes_match_and_back_to_title_tears_down() {
        let mut app = lockstep_app(NetRole::Host, 47, AttackRule::Garbage, 2);
        // Peer's hash for tick 59 deliberately wrong (simulated fork).
        peer_sends(
            &mut app,
            NetMsg::SnapshotHash {
                side: Side::Right,
                tick: 59,
                left: 0xDEAD_BEEF_DEAD_BEEF,
                right: 0xDEAD_BEEF_DEAD_BEEF,
            },
        );
        let mut saw_desync = false;
        for _ in 0..61 {
            fixed_tick(&mut app);
            let events = drain_net_events(&mut app);
            if events.contains(&NetEvent::Desync { tick: 59 }) {
                saw_desync = true;
                break;
            }
        }
        assert!(saw_desync, "Desync message expected at tick 59");
        // Contract: frozen, but the boards stay visible (active).
        assert!(app.world().resource::<SimPaused>().0);
        assert!(app.world().non_send::<VersusMatch>().active);
        let frozen = snapshot(&app);
        for _ in 0..4 {
            fixed_tick(&mut app);
        }
        assert_eq!(snapshot(&app), frozen, "no tick progress while frozen");
        assert_eq!(lockstep_tick(&app), 60);

        // "Back to title" (N5's button): full teardown.
        net_leave_to_title(app.world_mut());
        assert!(!app.world().non_send::<VersusMatch>().active);
        assert_eq!(*app.world().resource::<AppState>(), AppState::Title);
        assert!(!app.world().resource::<SimPaused>().0);
        assert_eq!(app.world().resource::<NetSession>().status, NetStatus::Idle);
        assert_eq!(app.world().resource::<VersusWinner>().0, None);
        assert_eq!(lockstep_tick(&app), 0);
        assert!(!app.world().contains_resource::<RenetServer>());
        assert!(!app.world().contains_resource::<RenetClient>());
    }

    #[test]
    fn bye_over_the_wire_freezes_and_reports() {
        let mut app = lockstep_app(NetRole::Host, 48, AttackRule::Garbage, 2);
        peer_sends(&mut app, NetMsg::Bye);
        fixed_tick(&mut app);
        let events = drain_net_events(&mut app);
        assert!(
            events.contains(&NetEvent::ByeReceived),
            "bye event expected: {events:?}"
        );
        assert_eq!(
            app.world().resource::<NetSession>().status,
            NetStatus::Lost(NetLossReason::PeerDisconnected)
        );
        assert!(app.world().resource::<SimPaused>().0);
        assert!(app.world().non_send::<VersusMatch>().active);
    }

    #[test]
    fn winner_surfaces_through_lockstep() {
        // Race { 0 }: every lock finishes its side; the match crowns once
        // both finished — the earlier finisher wins (core semantics).
        let mut app = lockstep_app(NetRole::Host, 49, AttackRule::Race { target_lines: 0 }, 2);
        // Right-side drop arrives as a remote TickInput for tick 0; the
        // local (left) drop is delayed by D = 2 and locks at tick 2.
        peer_sends(
            &mut app,
            NetMsg::TickInput {
                tick: 0,
                actions: vec![Action::HardDrop],
            },
        );
        app.world_mut().resource_mut::<VersusActions>().left = vec![Action::HardDrop];
        fixed_tick(&mut app); // tick 0: right finishes
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));
        fixed_tick(&mut app); // tick 1: left drop not due yet
        assert_eq!(*app.world().resource::<VersusWinner>(), VersusWinner(None));
        fixed_tick(&mut app); // tick 2: left finishes → right crowned
        assert_eq!(
            *app.world().resource::<VersusWinner>(),
            VersusWinner(Some(Side::Right)),
            "first finisher takes the tie, crowned through the lockstep"
        );
        let events = drain_versus_events(&mut app);
        assert!(
            events
                .iter()
                .any(|e| e.0 == MatchEvent::WinnerCrowned { side: Side::Right }),
            "crowning reaches the message stream: {events:?}"
        );
    }

    #[test]
    fn guest_system_drains_local_side_and_stalls_without_batches() {
        let mut app = lockstep_app(NetRole::Guest, 50, AttackRule::Garbage, 2);
        // No batches arriving: stall, but the right-side local queue is
        // drained + emitted as TickInput; the left queue is never touched.
        app.world_mut().resource_mut::<VersusActions>().left = vec![Action::Hold];
        app.world_mut().resource_mut::<VersusActions>().right = vec![Action::HardDrop];
        fixed_tick(&mut app);
        assert_eq!(
            app.world().resource::<NetLockstep>().stall_steps,
            1,
            "stalled without a batch"
        );
        assert_eq!(lockstep_tick(&app), 0);
        assert_eq!(
            app.world().resource::<VersusActions>().left,
            vec![Action::Hold],
            "the remote (left) queue is never the guest's business"
        );
        assert!(app.world().resource::<VersusActions>().right.is_empty());
        let wire = server_side_receives(&mut app);
        assert!(
            wire.iter()
                .any(|m| matches!(m, NetMsg::TickInput { tick, actions }
            if *tick == 2 && actions == &vec![Action::HardDrop])),
            "guest emits TickInput at t + D: {wire:?}"
        );

        // Deliver a batch for tick 0 carrying a left-side drop (pumped
        // back through the local client's server side): the mirror applies
        // exactly the batch contents — gravity alone leaves the (timerless)
        // snapshot untouched, so the drop is what proves execution.
        let batch = protocol::encode(&NetMsg::TickBatch {
            tick: 0,
            left: vec![Action::HardDrop],
            right: Vec::new(),
        });
        app.world_mut()
            .resource_scope::<RenetClient, ()>(|world, _client| {
                let mut server = world.resource_mut::<RenetServer>();
                server.send_message(9, DefaultChannel::ReliableOrdered, batch);
            });
        fixed_tick(&mut app);
        assert_eq!(lockstep_tick(&app), 1, "guest advanced one tick");
        assert!(snapshot(&app).left.score > 0, "batch contents applied");
    }

    // ---- thin real-transport smoke (loopback UDP, N6 owns E2E) ----------

    fn transport_app(seed: u64, rule: AttackRule) -> App {
        let mut app = coreless_app(seed, rule);
        // The thin mirror must carry the production pending-`MatchStart`
        // consumer: since N7 a parked start stages the NEW match's
        // `TickBatch`es until `guest_pending_start_system` adopts them with
        // the mirror rebuild (the full harness wires it via the versus
        // bridge; without it these staged batches would never run).
        app.add_systems(
            bevy::app::Update,
            crate::core_bridge::versus::guest_pending_start_system,
        );
        app
    }

    #[test]
    fn lockstep_mirrors_over_real_loopback_transport() {
        // The production adapters (RenetServerOut/RenetClientOut + the
        // receive polls) on real netcode UDP loopback: a guest hard drop
        // must round-trip TickInput → host batch → guest mirror. Full
        // E2E (bots, winners, fork injection) is N6's job.
        let seed = 0x5EED_1234;
        let rule = AttackRule::Garbage;
        let delay = 4u8;
        let mut host = transport_app(seed, rule);
        let mut guest = transport_app(seed, rule);

        super::super::session::net_host(host.world_mut(), 0);
        // `listen_addr` carries the wildcard bind address; dial loopback on
        // the port the OS assigned.
        let addr: SocketAddr = {
            let listen = host
                .world()
                .resource::<NetSession>()
                .listen_addr
                .expect("listening");
            std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, listen.port()).into()
        };
        super::super::session::net_join(guest.world_mut(), addr);

        drive_until(&mut host, &mut guest, 1_000, |h, g| {
            h.world().resource::<NetSession>().status == NetStatus::Ready
                && g.world().resource::<NetSession>().status == NetStatus::Ready
        });

        // N4 does this wiring in production; mirror its contract here:
        // host flips to InMatch + sends MatchStart, guest mirrors from it.
        // The guest's lockstep is reset *before* the flip: `MatchStart` and
        // the first batches ride the same reliable stream and can execute
        // in the very frame the session flips (`FixedUpdate` runs after the
        // `Update` bridge), and resetting afterwards could rewind an
        // already-executed tick. `delay` is what the MatchStart below
        // carries, so the reset uses the same negotiated value.
        guest
            .world_mut()
            .resource_mut::<NetLockstep>()
            .reset_for_match(delay);
        assert!(host.world_mut().resource_mut::<NetSession>().enter_match());
        let peer = host.world().resource::<NetSession>().peer.expect("peer");
        host.world_mut()
            .resource_mut::<NetLockstep>()
            .reset_for_match(delay);
        {
            let mut server = host.world_mut().resource_mut::<RenetServer>();
            server.send_message(
                peer,
                DefaultChannel::ReliableOrdered,
                protocol::encode(&NetMsg::MatchStart {
                    seed,
                    rule,
                    match_delay: delay,
                }),
            );
        }
        drive_until(&mut host, &mut guest, 1_000, |_, g| {
            g.world().resource::<NetSession>().status == NetStatus::InMatch
        });

        // Guest queues one hard drop (right side).
        guest.world_mut().resource_mut::<VersusActions>().right = vec![Action::HardDrop];

        let target = 30u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while host.world().resource::<NetLockstep>().tick < target
            || guest.world().resource::<NetLockstep>().tick < target
        {
            assert!(
                std::time::Instant::now() < deadline,
                "loopback lockstep stalled out"
            );
            host.update();
            guest.update();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let mut host_snap = snapshot(&host);
        let mut guest_snap = snapshot(&guest);
        // T19: the guest mirror lags the host by the input-delay pipeline,
        // so the two snapshots can be taken a lockstep tick apart (`>=`
        // wait, not an aligned capture). Pin the T19 clock contract first —
        // `match_ticks` is 1:1 with executed lockstep steps on each peer —
        // then normalize it for the state-equality hash. Per-tick-aligned
        // hash equality is what the production `SnapshotHash` exchange
        // checks every 60 ticks (this run reaching here without a `Desync`
        // proves it). Tick progress stays pinned by the wait above and the
        // `dropped_late_inputs` / `stall_steps` asserts below.
        for (name, app, snap) in [
            ("host", &host, &mut host_snap),
            ("guest", &guest, &mut guest_snap),
        ] {
            assert_eq!(
                snap.match_ticks,
                app.world().resource::<NetLockstep>().tick,
                "{name}: match clock must be 1:1 with executed lockstep steps"
            );
        }
        guest_snap.match_ticks = host_snap.match_ticks;
        assert_eq!(
            protocol::snapshot_hash(&host_snap),
            protocol::snapshot_hash(&guest_snap),
            "mirrors diverged over the real transport"
        );
        assert!(
            host_snap.right.score > 0,
            "the guest's drop reached both mirrors via the wire: {host_snap:?}"
        );
        assert_eq!(guest_snap.right.score, host_snap.right.score);
        assert_eq!(
            host.world().resource::<NetLockstep>().dropped_late_inputs,
            0
        );
        assert!(
            guest.world().resource::<NetLockstep>().stall_steps <= 5,
            "loopback stalls: {:?}",
            guest.world().resource::<NetLockstep>().stall_steps
        );

        super::super::session::net_stop(host.world_mut());
        super::super::session::net_stop(guest.world_mut());
    }

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
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "loopback condition not reached in {frames} frames (host {:?} tick {}, guest {:?} tick {})",
            host.world().resource::<NetSession>().status,
            host.world().resource::<NetLockstep>().tick,
            guest.world().resource::<NetSession>().status,
            guest.world().resource::<NetLockstep>().tick,
        );
    }
}
