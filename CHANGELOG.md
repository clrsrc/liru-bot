# Changelog

## v0.5.0 (unreleased) — coupled with clrsrc v1.4.0

### Engine
- **clrsrc v1.4.0** — the engine the live bot has been running: Lazy-SMP search
  (`Threads`), correction history, a Stockfish-style repetition rule inside the search
  tree, an opening-book repetition guard, book "oracle" mode and a rating-band book
  repertoire. The embedded network is now the live CLRU1 container; the public
  `EmbeddedConfig` is identical to the private one (the search-gate booleans
  `StmCoherent` / `URiskExtension` / `URiskFixedFormula` / `ConvFloor` default to **on**,
  matching the network). See clrsrc's own release notes for the engine side.

### Embedded backend (`--features embedded`)
- **Threads passthrough:** `uci_options.Threads` reaches the in-process engine.
- **Rating-based contempt** (rated games only, against a clearly weaker bot opponent:
  two stages by rating difference), with the contempt sign anchored to our colour so the
  prefill (opponent at the root) and the main search agree in one transposition table.
- **Rating band for the book repertoire:** the bot derives a per-game `OppBand`
  (stronger / equal / weaker by ±100 rating points, rated and casual) and injects it into
  the engine's book probe; the subprocess options are untouched.
- **Ponder-lite prefill** (`engine.book_ponder`, `engine.opponent_brain`): after our own
  move the engine searches the resulting position during the opponent's think so the next
  search starts with a warm TT/history. `book_ponder` only while still in the opening
  book, `opponent_brain` for the whole game. Race-safe reclaim (per-call cancel flag +
  process-global stop, re-raised until the join completes), runaway guard
  `min(opponent clock / 2, 10 s)`, reclaimed only by events that actually need the
  engine (not by the echo of our own move). Gated to `challenge.concurrency: 1` and the
  embedded backend; defaults off.
- **Book hits carry no search verdict:** an engine-internal book reply (`depth 0`,
  `nodes 0`) is harvested like a Polyglot book move instead of as an engine score of 0,
  which used to anchor the experience book's variety cutoff on never-searched moves.
- A non-integer `Hash` option is now warned about instead of silently falling back to 64 MB.

### Time management
- **Clock branch for no-increment games:** instead of a fixed `movetime`, the embedded
  engine gets the real clock minus a reserve of `15 × move_overhead` and plans with its
  own time manager above that floor; the ceiling and the zero-budget point equal the
  previous fixed-time branch. Games with increment, the first move and correspondence
  keep the fixed `movetime`.

### Robustness & connectivity
- **Non-blocking move delivery:** `make_move` runs in a background task with its full
  retry budget, so a transport blip no longer blocks the game task while our clock runs
  for the next move. A stale delivery is aborted before the next search and at game end.
- **Game-stream resubscribe budget** raised from 12 s to 45 s (still capped at 10 % of the
  remaining clock) with exponential 1-2-4-8 s back-off until the budget is spent, so a
  multi-second Lichess outage no longer forfeits a game.
- **Cooperative shutdown:** a cancellation token ends resubscribe back-off immediately on
  Ctrl-C, and tasks aborted after the drain timeout are joined before the runtime is
  dropped (fixes a process panic during shutdown that took a running game with it).
- **Parallel-game guard:** our own pending outbound challenge is cancelled before an
  incoming challenge is accepted (or the incoming one is declined if the cancel fails);
  a second non-tournament game that would exceed `challenge.concurrency` is aborted
  instead of played (`PARALLEL_GAME_ABORTED`). Pending accepts are keyed by challenge id
  so `gameStart` releases exactly its own reservation, and the matchmaker counts them.
- **Aborted games are not games:** when a counted game ends `aborted` before any play,
  the daily bot-vs-bot tally point is handed back and the post-game challenge cooldown
  is skipped.
- **Non-blocking, lossless log writer** on a dedicated thread, so a slow stdout/file
  write can no longer stall the game task between "engine decided" and "move sent".
- **Post-search timeline** debug lines (`since_search_ms` on bookkeeping, delivery spawn
  and "move sent") to pin any future silent gap to one step.
- **Startup fingerprint line** `LIVE-FINGERPRINT bot=… net=… book=… config=…` (md5 of the
  binary, `EvalFile`, `ExpFile` and the config file) so a game's log maps to the exact
  configuration that played it.
- **Chess960 / fromPosition:** `searchmoves` are rendered in Chess960 notation where the
  variant needs it, and `fromPosition` games use the Chess960 castling mode; the effective
  ponder flag from `move_time` is what the search receives.

### Configuration
- `engine.embedded`, `engine.book_ponder` and `engine.opponent_brain` are shown in
  `config.yml.example` (all default `false`).

---

## v0.4.0 (2026-07-16) — coupled with clrsrc v1.3.0

### Engine
- **clrsrc v1.3.0** — a pure search/correctness release; the network is **unchanged**
  from v0.3.0. Notable: singular-extensions disabled (the SPRT-validated search-ordering
  carrier), a TT mate/TB-win band rework so tablebase wins get their own band and
  `mate N` scores mean genuine mates, mate-before-50-move-rule priority, an NNUE
  loader guard (returns an error instead of an out-of-bounds panic on a foreign-bucket
  net), and opening-book hygiene.
- **Conversion-floor time trigger (default on).** In a clearly winning, low-piece
  endgame with a healthy clock, the engine no longer banks time into near-instant
  moves — it floors the per-move time so won positions get converted instead of
  drifting toward a repetition draw. NNUE-only (needs no private data); tunable via the
  `ConvFloor` / `ConvThreshold` / `ConvExtend` / `ConvMaxPieces` / `ConvMinRemaining`
  UCI options on the subprocess backend.

### Robustness & connectivity hardening
Fixes from a full source audit, all of which had been running on the live bot:

- **Stream-idle watchdog:** NDJSON streams now surface a `StreamIdle` error if no data
  *or* keep-alive arrives for 60 s, so a half-open connection (NAT/conntrack drop with
  no FIN/RST) triggers a resubscribe in seconds instead of hanging ~12 min for the OS
  TCP keep-alive to notice.
- **Stream-open & connect timeouts:** a 10 s cap on the stream response-header wait and
  on TCP+TLS connect, so a server that accepts the socket but never responds can't wedge
  the reconnect loop. Neither limits an established stream's body.
- **Non-JSON error body tolerance:** a challenge endpoint returning a non-JSON body
  (nginx/LB HTML on 5xx/429) no longer aborts the request path — it falls back to
  HTTP-status-only rate-limit classification instead of hammering a throttled account.
- **Game-task panic isolation:** a panic inside a game task is now caught and returned as
  a normal per-game error the join arm cleans up, instead of leaking the concurrency slot
  and idling the bot until a manual restart.
- **Accept/gameStart race guard:** an accepted-but-not-yet-started challenge now reserves
  a concurrency slot (with a TTL) so two challenges processed before either game starts
  can't both be accepted into parallel games under `concurrency: 1`.
- **Threefold-repetition counting fix:** repetitions are counted per *move*, not per
  event — `gameState` re-sends (draw/takeback flips) and `gameFull` reconnect resyncs no
  longer inflate the counter and risk claiming a draw in a position that never repeated.
- **Experience-overlay self-heal:** a torn trailing record from an interrupted write is
  truncated to the last aligned boundary before appending, and the header count is derived
  from the complete records actually on disk.

### Configuration
- `token` is now optional in `config.yml` and can be supplied via the `LICHESS_BOT_TOKEN`
  environment variable; an empty token is rejected at validation with a clear message.
- A **relative** `matchmaking.daily_counter_path` is now anchored to the config file's
  directory rather than the process working directory, so starting the bot from a
  different directory no longer silently resets the day's game tally.

### Time management
- **Clock-aware first move:** the first move keeps its ≤10 s ceiling but is now also capped
  at the normal `remaining/15 + inc` budget, so a book miss in a short time control no
  longer sinks a third of the clock into move 1.

---

## v0.3.0 (2026-06-23)

### Engine
- **clrsrc v1.2.0** (KB16 network): late-IIR search-ordering improvement, time-management
  v1, TT mate-depth guard, embedded PV fix.

### Matchmaking & budget
- **Daily bot-vs-bot budget** tracked against a UTC day counter, mirroring Lichess'
  100-games/day cap, with a slice reserved for the bot's own matchmaking seeks.
- **Escalating 429 back-off** on the challenge endpoint, and **per-opponent rate-limit
  suppression** persisted across restarts so a post-restart burst can't re-trigger a 429
  cascade.
- **Content-decline skip:** an opponent that declines on content grounds (friends-only,
  variant/rating mismatch) is skipped for the day instead of being misclassified as an
  account rate-limit.

---

## v0.2.0 (2026-06-10)

### Engine
- **clrsrc v1.1.1** (matefix + repfix + IIR-late): ~+100 Elo over v0.1.0's clrsrc v1.1.0
  - `matefix`: fixes short-mate repetition shuffle (+48 Elo), closes game `uEn2qBri` class
  - `repfix`: fixes long-mate 3-fold draw-detection in `depth<=0` leaf, closes game `lPG7cqDO` class
  - `IIR-late`: moves the Internal Iterative Reduction block after pruning gates — effective pruning
    depth is no longer reduced, yielding stronger pruning and +56 Elo

### Connectivity hardening
Three fixes that prevent mid-game connection losses from forfeiting games:

- **Fix 1a — Game-stream resubscribe (clock-aware):** When the game stream drops (EOF or transport
  error), the bot now attempts to reopen it with an exponential backoff (1s / 2s / 4s, hard cap 8 s,
  total budget `min(12 s, remaining_clock / 10)`) rather than immediately abandoning the game.
  On reconnect Lichess resends a `gameFull` event; the new `gameFull` reconnect arm replays the full
  move list from that event so the board state is always canonical (idempotent — no double-move risk
  even if our last move was already delivered before the drop).

- **Fix 2a — Stream-open Storm-Killer:** `get_event_stream` / `get_game_stream` now use a single
  HTTP attempt instead of the old `with_backoff` loop (100 ms retry every 5 s for up to 60 s on
  failure). That old loop issued ~600 rapid requests on a transient blip, triggering Lichess's
  `/api/stream/event` 429 cascade which killed all game streams account-wide. A single failing
  attempt is now surfaced immediately; the reconnect cadence is owned by the outer loop.

- **Fix 2b — Event-stream 429 back-off:** When the event-stream reconnect receives an HTTP 429
  `RateLimited`, the bot now honors `retry-after` from the Lichess response (plus jitter) instead
  of pressing ahead at the fixed 5 s exponential cadence. This prevents repeated re-triggering of
  the 429 window.

### Matchmaking
- **Diversity brake:** Matchmaking now enforces `max_challenges_per_opponent_per_day` (default: 5)
  so a single online bot cannot monopolize the challenge queue. A soft weighting additionally
  down-weights already-played opponents in favour of fresh bots when several are available.
- Config key `matchmaking.max_challenges_per_opponent_per_day` (integer, `0` = unlimited).

### Timer / subprocess time management
- Added `movetime_cap_ms` on the subprocess path: instead of forwarding raw clock times to the
  engine via `go wtime/btime`, the bot now sends `go movetime N` where N = `remaining/30 + inc`.
  This bounds the rare soft-inflation overshoot class (clrsrc's `stability_factor` could inflate
  the soft limit on oscillating positions, causing single moves > 120 s → forfeit on slow hardware).

### Diagnostics
- Per-move engine eval is now logged at `DEBUG` level (`ply`, `bestmove`, `score`, `depth`,
  `nodes`, `pv`) when using the embedded engine backend. Useful for post-game analysis.

---

## v0.1.0 (2026-05-31) — initial public release
