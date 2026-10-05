Blockfall: New Game Modes PRD
Sep 30, 2026 · @Matt Davis
Summary
Blockfall should add nine game modes and a set of mutators over three releases, starting with Sprint, Ultra and Dig. Today a solo player has one mode, Marathon, and versus has two rules, Garbage and Race. The first release is cheap: each new mode is an end condition, a clock and a best-score slot on the existing engine.
Every mode was chosen to reuse what the repo already has: the deterministic core, the garbage-row code and the greedy bot. Nothing here needs accounts, a leaderboard server or more than two players in a match.
This extends the shipped product spec in PRD.md, which lists sprint and ultra as post-v1 work and asks in §14 whether Sprint should be a quick win.
Where the game is today
Blockfall v0.3.2 offers three ways to play, all on one ruleset, on desktop and Android.
Title-screen entry
What it is
How a run ends
Record kept
Start
Marathon, solo
Top-out only. Level rises every 10 lines; gravity caps at level 20
One best score in best.json
1v1
Local versus against a human or the bot, Garbage or Race rule
Garbage: a side tops out. Race: both reach 40 lines, higher score wins
None
Online
The same two rules over room codes or direct IP
Same as 1v1
None
Six facts in the code decide what a new mode costs:
• The core is deterministic. Game::new(seed) plus an action log replays exactly at 60 ticks per second. A mode built on tick counts stays replayable and safe for lockstep netplay.
• Game takes only a seed. It has no start level, goal, time limit or starting board. Every mode below needs a config here first.
• Garbage rows already exist. push_garbage_rows in versus.rs builds them, and the versus HUD already shows queued garbage.
• Versus is exactly two sides. Match holds a left and a right game, and a gateway room seats one host and one guest.
• The rule travels on the wire. MatchStart carries the AttackRule, and an unknown variant fails to decode. A new versus rule therefore needs a protocol version bump.
• The bot has one speed. The greedy solver waits a fixed 60 ticks after each lock (BOT_LOCK_COOLDOWN_STEPS).
The HUD has no clock today, and the title menu has no mode picker.
Goals and non-goals
The goal is more reasons to open the game, without forking the ruleset or adding a backend.
Goals
• G1. Short solo sessions. A solo player gets at least three modes that finish in a few minutes, each with its own record.
• G2. A solo path into versus. A player with no friend online still gets escalating versus play, against the bot.
• G3. New things to do with a friend. Two new versus rules run locally and online over the existing lockstep and gateway.
• G4. Modes are data. Marathon becomes one mode config among many. The same seed and actions give the same result before and after.
• G5. Phone parity. Every mode is fully playable on Android portrait with the existing touch controls.
Non-goals
• More than two players in a match. Match and gateway rooms are built for two.
• Accounts, server leaderboards and telemetry. These stay out of scope, as in PRD.md §4.
• Co-op on a shared wide board. The board width is a compile-time constant of 10.
• Hand-authored puzzle or mission content.
• Changes to SRS, the scoring table or the 7-bag.
• Rollback netcode.
Proposed modes
Seven solo modes, two versus rules and one set of mutators. Tuning numbers marked "starting value" are first guesses to adjust in playtests.
Mode
Players
Goal
Record kept
Size
Sprint
Solo
Clear 40 lines as fast as possible
Best time
S
Ultra
Solo
Highest score in 2 minutes
Best score
S
Dig
Solo
Clear 10 starting garbage rows as fast as possible
Best time
S
Survival
Solo
Outlast garbage that rises faster and faster
Longest time
M
Zen
Solo
No goal and no game over
Lifetime lines
S
Bot Ladder
Solo against the bot
Beat eight bots of rising speed
Highest rung beaten
M
Daily Challenge
Solo
One shared seed per day, same pieces for everyone
Result per day
M
Dig Duel
Two, local or online
First to clear 10 garbage rows
None
L
Switch
Two, local or online
Garbage battle where the boards swap every 30 seconds
None
L
Mutators
Any solo mode
Optional twists such as Invisible and No Hold
See open questions
S
Size: S is a mode config plus an end condition. M adds new behavior in the core or app. L changes Match and the wire protocol.
Sprint
The pure speed test, and the shortest path to a personal best.
• Rules: Gravity stays at level 1. A 3-second countdown runs before the first piece.
• Ends: At 40 lines, with a time. A top-out gives no result.
• HUD: Clock to 0.01 s, lines left, pieces placed.
• Why it is fun: One number to beat, and a retry costs a minute or two.
Ultra
Two minutes to score as much as possible.
• Rules: Marathon scoring and level progression, on a 7,200-tick clock.
• Ends: When the clock reaches zero or on top-out. The score stands either way.
• HUD: Countdown clock, score, a warning sound in the last 10 seconds.
• Why it is fun: It rewards T-spins, back-to-backs and combos rather than raw speed, so it plays differently from Sprint.
Dig
Start buried and dig out.
• Rules: The board starts with 10 garbage rows, one hole per row, no two adjacent rows sharing a hole column. Gravity stays at level 1.
• Ends: When the last garbage row clears, with a time. A top-out gives no result.
• HUD: Clock, garbage rows left.
• Why it is fun: Downstacking is its own skill, and every seed is a small puzzle.
Survival
The stack rises on its own, and it never stops.
• Rules: One garbage row queues every 300 ticks, shrinking by 15 ticks every 30 seconds to a floor of 60 (starting values). Queued rows land on the next lock, at most 4 at a time, as in versus.
• Ends: On top-out. The result is time survived.
• HUD: Clock, the existing queued-garbage meter, time until the next row.
• Why it is fun: Pressure builds steadily and every run ends in a close call.
Zen
Play with nothing at stake.
• Rules: Gravity stays at level 1. When a piece cannot spawn, the stack is wiped and play continues.
• Ends: Only when the player leaves.
• HUD: Lines this session and lifetime lines.
• Why it is fun: It suits a phone in one hand, and it is a safe place to practise T-spin setups.
Bot Ladder
A solo campaign of versus matches.
• Rules: Eight rungs under the Garbage rule. Each rung's bot waits less after a lock: from 120 ticks at rung 1 to 10 at rung 8 (starting values). Rung 4 is today's bot at 60.
• Ends: Win to unlock the next rung. A loss retries the same rung.
• HUD: The versus HUD, plus the rung number.
• Why it is fun: It gives solo players a versus progression with a final boss.
Daily Challenge
One run a day that friends can compare.
• Rules: The seed comes from the UTC date. The mode rotates between Sprint, Ultra and Dig by weekday.
• Ends: As the day's mode ends. The first finished run of the day is the one recorded.
• HUD: As the day's mode. The result screen shows one line to share, such as Blockfall Daily 2026-10-01 · Dig · 1:42.35.
• Why it is fun: Identical pieces make results comparable between friends with no server.
Dig Duel
A race both players can watch.
• Rules: Both sides start with the same 10 garbage rows and get the same piece sequence. No garbage is sent.
• Ends: The first side to clear its last garbage row wins. A top-out loses.
• HUD: The versus HUD, plus garbage rows left per side.
• Why it is fun: Progress is visible on both boards, and the lead can change on the last row.
Switch
The party mode: you inherit the mess you made.
• Rules: The Garbage rule, but every 1,800 ticks (30 seconds, starting value) the players swap boards, with hold and next queue. A 3-second warning precedes each swap.
• Ends: A side tops out, as in Garbage.
• HUD: The versus HUD, plus a swap countdown.
• Why it is fun: Burying your opponent now buries you later, so the best plan changes every 30 seconds.
Mutators
Optional toggles chosen on the mode screen before a solo run.
• Invisible: Locked cells fade out one second after they land.
• No Hold: The hold action does nothing.
• No Ghost: The landing preview is hidden.
• One Preview: The next queue shows one piece.
• 20G: The run starts at maximum gravity.
Prioritization and phasing
Ship in three releases, ordered by cost and by what each one unblocks. The version numbers are suggestions and no dates are set.
Each release passes its gate before the next one starts.
• Release 1 goes first. Its three S-size modes force the mode config, clock and records work that every later mode needs.
• Release 2 reuses the garbage code and the bot for solo play, with no change to netplay.
• Release 3 goes last. It is the only release that breaks wire compatibility. It starts with both rules in local 1v1, which needs no protocol change.
Shared technical requirements
Seven changes carry all ten items. R1 to R3 land in the first release and unblock everything after it.
#
Area
Requirement
Needed by
R1
Core: mode config
Game accepts a config: start level, whether levels advance, a goal (lines, ticks or garbage cleared), a starting board and the top-out behavior. The default config is today's Marathon.
All modes
R2
Core: tick clock
Game counts its own ticks and reports goal-reached and time-up as events. Times are stored as ticks, never wall-clock time.
Sprint, Ultra, Dig, Survival, Daily Challenge
R3
App: mode select, HUD, records
Start opens a mode list with a one-line description and the record for each mode. The HUD gains a clock and a goal counter. best.json becomes a per-mode records file, with the current best migrated to Marathon.
All modes
R4
Core: solo garbage
The queued-garbage logic in Match becomes usable by a single Game, fed by a timer.
Survival
R5
App: bot speed
The bot's pause after each lock becomes a per-match parameter instead of a constant.
Bot Ladder
R6
Core and net: versus rules
AttackRule gains Dig and Switch variants. Match gains a match-level tick count and a same-pieces option. PROTOCOL_VERSION is bumped and the snapshot hash covers the new state.
Dig Duel, Switch
R7
App: mutators
Invisible, No Ghost and One Preview are render-only. No Hold filters the action before it reaches the core. 20G uses the start level from R1.
Mutators
Three rules apply across all seven:
• The frozen contract reopens once. event.rs and versus.rs call the Game, GameEvent and Action contract frozen. R1 and R2 change it deliberately, in one release.
• The core never reads a clock or a date. The Daily Challenge date is read in the app and reaches the core only as a seed.
• Every mode is tested headlessly. Each gets core unit tests and a same-seed replay test. Both versus rules join the 20-match netplay soak and the relay end-to-end test.
Success metrics
With no telemetry, success is measured by tests in CI and by a local play counter the owner can read. The play target is a proposal, modelled on the three-sessions-a-week criterion in PRD.md §12.
Measure
Target
How it is checked
Marathon is unchanged
The same seed and action log give an identical final snapshot before and after R1
Regression test in tetris-core
New versus rules stay in sync
Zero snapshot-hash mismatches across the 20-match soak, for each rule
The existing nightly netplay soak
Solo modes complete headlessly
The bot finishes Sprint and Dig, and Ultra ends at exactly tick 7,200
CI
Phone parity
Every mode can be started, played and left using touch only, in portrait
Manual check of the APK at each release
Modes get played
Each shipped mode is started at least 3 times a week in the month after its release
A plays counter per mode in the records file
A mode that misses the play target after a month is reworked or taken off the menu.
Risks and open questions
The largest risk is the one-time change to the core contract; the rest are tuning and menu design.
Risk
Impact
Mitigation
Reopening the frozen core contract changes existing behavior
Marathon or netplay regressions
Do it once, in the first release, behind a default config. Gate the release on the Marathon regression test.
New versus rules change the wire format
Old and new builds cannot play each other; the version handshake refuses the match
Release desktop and Android builds together and say so in the changelog.
Ten entries crowd the phone menu
Mode select is hard to use in portrait
One scrolling list with a line per mode. Versus rules stay under 1v1 and Online.
The greedy bot gets faster, not smarter
Top ladder rungs may be trivial or unbeatable
Tune rung speeds in playtests. Ship fewer rungs if the curve is uneven.
Board swaps disorient players under online input delay
Switch feels unfair
Keep the 3-second warning, consider a brief input freeze at the swap, and playtest locally first.
Survival and Switch timings are guesses
Runs are too short or too long
Keep them as named constants and tune before release.
Decisions for the owner:
[ ] Sprint and Dig gravity: fixed at level 1 as proposed, or Marathon progression?
[ ] Mutator runs: their own records, the mode's normal record, or no record?
[ ] Daily Challenge: is one recorded attempt per day right when nothing enforces it?
[ ] Daily Challenge: how is the result line shared, given Bevy 0.19 has no clipboard?
[ ] Switch: does queued garbage follow the board or stay with the player?
[ ] Should the existing Race rule also give both players the same pieces?
[ ] Zen on top-out: wipe the whole stack, or only part of it?
[ ] Should this document be checked into the repo beside PRD.md and close its §14 item 3?
