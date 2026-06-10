# Changelog

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
