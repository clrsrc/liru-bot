//! Main event loop — Rust port of the central parts of
//! `lib/lichess_bot.py`. **Scope of this first cut:**
//!
//! - Accept incoming challenges that pass `Challenge::is_supported`.
//! - For every accepted game, spawn an async task that opens the game
//!   stream, starts a UCI engine subprocess, rebuilds the board on each
//!   state update, and calls [`engine_wrapper::play_move`].
//! - Forward `chatLine` events to a [`Conversation`], send configured
//!   hello/goodbye greetings to the player and spectator rooms.
//! - Honour Ctrl-C as a graceful shutdown signal.
//!
//! **Deliberately not yet wired up** (Python parity TBD):
//!
//! - Outbound matchmaking (the "send out our own challenges" loop)
//! - PGN recording
//! - Takeback handling (we let Lichess time out the offer)
//! - Correspondence pickup on restart
//! - Fake-think-time
//! - Auto-restart on disconnect (we exit; a process supervisor restarts us)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use shakmaty::fen::Fen;
use shakmaty::uci::UciMove;
use shakmaty::{CastlingMode, Chess, Color, Position};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use futures::{FutureExt, StreamExt};
use tracing::{debug, error, info, warn};

use crate::blocklist::OnlineBlocklist;
use crate::config::{Config, EngineConfig, GreetingConfig};
use crate::conversation::{ChallengeQueue, ChatLine, Conversation};
use crate::daily_counter::DailyCounter;
use crate::engine_wrapper::{
    check_for_draw_offer, play_move, DrawResignTracker, EngineBackend, UciClient,
};
use crate::exp_overlay::{GameWdl, Jbk2Entry};
use crate::lichess::{Lichess, LichessError};
use crate::lichess_types::{
    ChallengeType, EventType, GameEventType, GameStateType, JsonValue, UserProfileType,
};
use crate::matchmaking::Matchmaking;
use crate::model::{Challenge, Game};
use crate::timer::Timer;
use crate::VERSION;

/// Per-game info we keep in `BotState.active_games`. `is_tournament`
/// gates the outbound-matchmaking tick — Lichess pairs the next arena
/// or swiss game on its own, so the bot must not race that pairing with
/// its own challenge or two games end up running in parallel under
/// `concurrency: 1`.
struct ActiveGame {
    opponent: String,
    is_tournament: bool,
}

/// Arena/Swiss are the Lichess `source` values that mean Lichess will
/// pair the next game itself. Other sources (`lobby`, `friend`, `api`,
/// `pool`, …) don't have that auto-pairing behavior.
fn is_tournament_source(source: Option<&str>) -> bool {
    matches!(source, Some("arena") | Some("swiss"))
}

/// Pause window after a tournament game ends, before outbound
/// matchmaking is allowed again. Lichess can deliver the next arena
/// pairing as a `gameStart` event seconds after the previous game
/// finished; outbound challenges sent in that window collide with the
/// new pairing because Lichess does not honour the bot's
/// `challenge.concurrency` gate for tournament games.
const TOURNAMENT_PAIRING_LAG: Duration = Duration::from_secs(60);

/// How long an accepted-but-not-yet-started challenge counts against the
/// concurrency gate before it's assumed dead (challenge expired between accept
/// and `gameStart`). Comfortably longer than the accept→gameStart latency.
const ACCEPT_PENDING_TTL: Duration = Duration::from_secs(20);

/// Per-bot state shared across tasks: blocklists, recent-challenge
/// memory, and how many ongoing games each opponent has. Wrapped in a
/// single mutex so the read-modify-write paths around challenge
/// filtering stay obvious.
struct BotState {
    online_blocklist: OnlineBlocklist,
    recent_bot_challenges: HashMap<String, Vec<Timer>>,
    opponent_engagements: HashMap<String, u32>,
    /// game_id → per-game info, for cleaning up `opponent_engagements`
    /// when a game ends and for the arena/swiss outbound-pause gate.
    active_games: HashMap<String, ActiveGame>,
    /// Correspondence game IDs that were already running when the bot
    /// started. Lichess resends `gameStart` for each of them on the
    /// event stream, so we only need this set as a marker — Python uses
    /// it to decide whether to queue or play immediately. Today we just
    /// log the pickup; the actual pause/checkin-period logic lives in
    /// the correspondence-queue module that isn't ported yet.
    startup_correspondence_games: HashSet<String>,
    /// game_ids for which a `play_game` task is currently running. Guards
    /// against Lichess resending `gameStart` on every event-stream reopen:
    /// a resend for an id already in this set is a pure re-subscribe and must
    /// not spawn a second task (two engine subprocesses + two move posters on
    /// one game → OOM / double-play). Inserted just before the spawn, removed
    /// when the task completes. Distinct from `active_games`, which
    /// `prefill_ongoing_games` populates before any task exists.
    spawned_games: HashSet<String>,
    /// Challenge/game id → accept timestamp, for challenges we've accepted but
    /// whose `gameStart` (which is what populates `active_games`) hasn't arrived
    /// yet. The concurrency gate counts these alongside `active_games`: without
    /// it, two incoming challenges processed before either game starts both
    /// read `active == 0` and get accepted, yielding two parallel games (two
    /// engines → OOM) despite `concurrency: 1`. Keyed by id (Lichess's
    /// challenge_id equals the resulting game_id for an accepted incoming
    /// challenge) so `gameStart` removes exactly the reservation it satisfies,
    /// not an arbitrary one — an unkeyed `Vec::pop()` could otherwise release a
    /// different in-flight accept's slot (e.g. an outbound matchmaking
    /// `gameStart` arriving first). Entries also expire via `ACCEPT_PENDING_TTL`
    /// so a challenge that never starts (expired between accept and start)
    /// can't wedge the gate.
    pending_accepts: HashMap<String, Instant>,
    /// Time the most recent tournament game finished. Used by the
    /// outbound matchmaking tick to wait out Lichess's pairing-lag
    /// window before issuing the next challenge.
    last_tournament_game_ended: Option<Instant>,
    /// game_ids that were counted toward the daily bot-vs-bot tally (at
    /// accept time for incoming, at `gameStart` for outbound). When such a
    /// game ends `aborted` — the opponent (or Lichess) killed it before any
    /// real play, e.g. an opponent bot aborting instantly — the join arm
    /// gives the tally point back and skips the post-game challenge
    /// cooldown: an abort is not a game. Entries are removed when the game
    /// task finishes.
    tallied_games: HashSet<String>,
}

impl BotState {
    fn new(online_blocklist: OnlineBlocklist) -> Self {
        Self {
            online_blocklist,
            recent_bot_challenges: HashMap::new(),
            opponent_engagements: HashMap::new(),
            active_games: HashMap::new(),
            startup_correspondence_games: HashSet::new(),
            spawned_games: HashSet::new(),
            pending_accepts: HashMap::new(),
            last_tournament_game_ended: None,
            tallied_games: HashSet::new(),
        }
    }

    /// Drop expired pending-accept markers, then return the effective in-flight
    /// game count (`active_games` + still-pending accepts) for the concurrency
    /// gate. See [`BotState::pending_accepts`].
    fn effective_active_games(&mut self) -> usize {
        let now = Instant::now();
        self.pending_accepts
            .retain(|_, t| now.duration_since(*t) < ACCEPT_PENDING_TTL);
        self.active_games.len() + self.pending_accepts.len()
    }

    fn has_active_tournament_game(&self) -> bool {
        self.active_games.values().any(|g| g.is_tournament)
    }
}

/// Entry point invoked from `main.rs`. Owns the main event-stream loop.
pub async fn start_program(
    li: Lichess,
    profile: UserProfileType,
    config: Config,
) -> Result<()> {
    let li = Arc::new(li);
    let config = Arc::new(config);

    // Pre-fetch online blocklists once at startup. Empty list = no
    // online blocklist; bot still respects local `challenge.block_list`
    // via `is_supported`.
    let blocklist_urls = config.challenge.online_block_list.clone();
    let online_blocklist = if blocklist_urls.is_empty() {
        OnlineBlocklist::default()
    } else {
        OnlineBlocklist::new(blocklist_urls).await
    };
    let state = Arc::new(Mutex::new(BotState::new(online_blocklist)));

    // Shared challenge queue across all game tasks. Today we accept/decline
    // every challenge synchronously, so the queue stays empty in practice —
    // it's kept hot for the upcoming outbound-matchmaking work and so the
    // `!queue` chat command in a running game has something to read.
    let challengers: ChallengeQueue = Arc::new(StdMutex::new(Vec::new()));

    // Pre-fetch ongoing games so we know which `gameStart` events from the
    // upcoming event stream are actually pickups of correspondence games
    // we abandoned at the last restart. Lichess always resends `gameStart`
    // for each ongoing game when a bot opens the event stream, so we don't
    // need to spawn anything ourselves — the existing handler does it.
    prefill_ongoing_games(&li, &state, &profile).await;

    // Outbound matchmaking. The matchmaker holds its own copy of the
    // online block-list because its `should_accept_challenge` reads from
    // it on hot paths; the blocklist-refresh task (still TODO) will need
    // to keep both copies in sync. A separate mutex from `BotState`
    // because `Matchmaking::challenge` does HTTP work and would otherwise
    // block the challenge-acceptance hot path.
    // Shared UTC-daily bot-vs-bot game tally. Lives outside both `BotState`
    // and `Matchmaking` because it is written by the event loop (on every
    // started bot game) and read by both the incoming-challenge gate and the
    // matchmaker. See `daily_counter`.
    let daily_counter = Arc::new(Mutex::new(DailyCounter::load(
        &config.matchmaking.daily_counter_path,
    )));

    let blocklist_clone = state.lock().await.online_blocklist.clone();
    let matchmaker = Arc::new(Mutex::new(Matchmaking::new(
        (*li).clone(),
        &config,
        profile.clone(),
        blocklist_clone,
        daily_counter.clone(),
    )));
    let matchmaking_enabled = config.matchmaking.allow_matchmaking;
    if matchmaking_enabled {
        info!("matchmaking enabled; will issue outbound challenges periodically");
    } else {
        debug!("matchmaking disabled in config; outbound challenges off");
    }
    let mut matchmaking_rng = StdRng::from_entropy();

    let mut games: JoinSet<(String, Result<GameEnd>)> = JoinSet::new();
    let mut event_stream = Box::pin(
        li.get_event_stream()
            .await
            .context("opening event stream")?,
    );
    info!("event stream open — listening for challenges and game starts");

    // Cooperative shutdown signal for per-game tasks. Cloned into every
    // spawned `play_game` so long-lived backoff loops (stream resubscribe)
    // can bail out immediately on Ctrl-C instead of running their full
    // budget and racing the runtime teardown below (5bTILJsc, 2026-08-09:
    // an in-progress resubscribe `sleep` outlived `main()` returning and
    // the runtime being dropped, panicking the whole process mid-drain).
    let shutdown_token = CancellationToken::new();

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    // 30 s is a comfortable tick for outbound matchmaking — Python evaluates
    // this on every event-loop iteration, which is effectively every few
    // seconds, but `Matchmaking::challenge` has its own internal cool-down
    // timers (`last_challenge_created_delay`, `last_game_ended_delay`, …)
    // so we don't need a hot tick.
    let mut matchmaking_tick = tokio::time::interval(Duration::from_secs(30));
    matchmaking_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; discard it so we don't challenge before
    // the bot has settled.
    matchmaking_tick.tick().await;

    // Online-blocklist refresh. Python refreshes on every incoming
    // challenge; we run a 10-min interval instead so traffic-heavy
    // accounts don't pound the blocklist hosts. The refresh runs on a
    // clone so neither `BotState` nor `Matchmaking` holds its mutex
    // during HTTP — replacement of both copies happens after the HTTP
    // round-trip.
    let has_online_blocklist = !config.challenge.online_block_list.is_empty();
    let mut blocklist_tick = tokio::time::interval(Duration::from_secs(600));
    blocklist_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    blocklist_tick.tick().await;

    loop {
        tokio::select! {
            biased;

            _ = &mut shutdown => {
                warn!("Ctrl-C received, waiting for in-flight games to finish");
                shutdown_token.cancel();
                break;
            }

            event = event_stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Err(e) = handle_event(event, &li, &config, &profile, &state, &challengers, &matchmaker, &daily_counter, &mut games, &shutdown_token).await {
                            error!("event handling failed: {e:#}");
                        }
                    }
                    other => {
                        // Lichess closes the long-poll event stream after a
                        // quiet period (no events for a while). Without
                        // reconnect logic the bot would silently exit and
                        // stop playing. We reopen with capped backoff and
                        // let the main loop keep running — only Ctrl-C
                        // really terminates the bot.
                        match &other {
                            Some(Err(e)) => warn!("event stream error: {e}, reconnecting"),
                            None => warn!("event stream ended, reconnecting"),
                            Some(Ok(_)) => unreachable!(),
                        }
                        let mut backoff = Duration::from_secs(5);
                        loop {
                            let jitter = Duration::from_millis(matchmaking_rng.gen_range(0..1000));
                            tokio::time::sleep(backoff + jitter).await;
                            match li.get_event_stream().await {
                                Ok(s) => {
                                    event_stream = Box::pin(s);
                                    info!("event stream reopened");
                                    break;
                                }
                                Err(LichessError::RateLimited { timeout, .. }) => {
                                    warn!("event stream rate-limited, honoring retry-after {:?}", timeout);
                                    backoff = timeout.max(backoff);
                                }
                                Err(e) => {
                                    error!("event stream reconnect failed: {e}, retry in {:?}", backoff);
                                    backoff = (backoff * 2).min(Duration::from_secs(60));
                                }
                            }
                        }
                    }
                }
            }

            Some(joined) = games.join_next() => {
                match joined {
                    Ok((game_id, Ok(end))) => {
                        let mut s = state.lock().await;
                        s.spawned_games.remove(&game_id);
                        let was_tallied = s.tallied_games.remove(&game_id);
                        if let Some(game) = s.active_games.remove(&game_id) {
                            if let Some(n) = s.opponent_engagements.get_mut(&game.opponent) {
                                *n = n.saturating_sub(1);
                            }
                            if game.is_tournament {
                                s.last_tournament_game_ended = Some(Instant::now());
                            }
                        }
                        drop(s);
                        match end {
                            GameEnd::Played => {
                                matchmaker.lock().await.game_done();
                            }
                            GameEnd::Aborted => {
                                // An abort before real play is not a game: hand
                                // the daily-tally point back (if this game took
                                // one) and leave the post-game challenge cooldown
                                // untouched so the matchmaker may re-challenge
                                // right away instead of waiting out a game that
                                // never happened.
                                if was_tallied {
                                    let tally = daily_counter.lock().await.decrement();
                                    info!(game_id = %game_id, tally, "aborted before play: daily tally point returned, no post-game cooldown");
                                } else {
                                    info!(game_id = %game_id, "aborted before play: no post-game cooldown");
                                }
                            }
                        }
                        info!(game_id = %game_id, "game task finished");
                    }
                    Ok((game_id, Err(e))) => {
                        error!(game_id = %game_id, "game task failed: {e:#}");
                        let mut s = state.lock().await;
                        s.spawned_games.remove(&game_id);
                        s.tallied_games.remove(&game_id);
                        if let Some(game) = s.active_games.remove(&game_id) {
                            if let Some(n) = s.opponent_engagements.get_mut(&game.opponent) {
                                *n = n.saturating_sub(1);
                            }
                            if game.is_tournament {
                                s.last_tournament_game_ended = Some(Instant::now());
                            }
                        }
                        drop(s);
                        matchmaker.lock().await.game_done();
                    }
                    Err(e) => {
                        error!("game task panicked: {e}");
                    }
                }
            }

            _ = matchmaking_tick.tick(), if matchmaking_enabled => {
                run_matchmaking_tick(&state, &challengers, &matchmaker, &config, &mut matchmaking_rng).await;
            }

            _ = blocklist_tick.tick(), if has_online_blocklist => {
                refresh_online_blocklist(&state, &matchmaker).await;
            }
        }
    }

    // Wait for outstanding games (up to a generous timeout) before exiting.
    let drain = tokio::time::timeout(Duration::from_secs(120), async {
        while let Some(j) = games.join_next().await {
            if let Ok((id, Err(e))) = j {
                warn!(game_id = %id, "game task ended with: {e}");
            }
        }
    })
    .await;
    if drain.is_err() {
        warn!("drain timed out, aborting remaining game tasks");
        games.abort_all();
        // `abort_all()` only marks tasks for cancellation — per Tokio's docs
        // the actual drop happens on the task's next poll, not synchronously
        // here. Without this rejoin, that poll (and any panic it triggers,
        // e.g. a timer entry mid-drop) happens during `Runtime::drop` after
        // `main()` returns below, outside any catch_unwind boundary, and
        // takes the whole process down with it (5bTILJsc, 2026-08-09).
        // Joining drives every aborted task to completion now, while the
        // runtime is still fully alive to catch it.
        while games.join_next().await.is_some() {}
    }
    info!("liru-bot shut down cleanly");
    Ok(())
}

/// Query `/api/account/playing` once at startup. Logs each ongoing game,
/// pre-populates `BotState.active_games` (so the concurrency gate sees
/// them immediately), and marks correspondence games as "pickups" so the
/// `gameStart` resend that Lichess always sends right after the event
/// stream opens can be logged distinctly. Errors are downgraded to a
/// `warn!` — Python silently treats this call as best-effort, and
/// failing it must not block the main loop.
async fn prefill_ongoing_games(
    li: &Lichess,
    state: &Arc<Mutex<BotState>>,
    profile: &UserProfileType,
) {
    let Some(games) = li.get_ongoing_games().await else {
        info!("no /api/account/playing data available at startup (network or auth)");
        return;
    };
    if games.is_empty() {
        info!("no ongoing games at startup");
        return;
    }
    let mut s = state.lock().await;
    let bot_name = profile.username.clone().unwrap_or_default();
    for g in &games {
        let Some(id) = g.game_id.as_ref().or(g.id.as_ref()).cloned() else {
            continue;
        };
        let opponent_name = g
            .opponent
            .as_ref()
            .and_then(|p| p.username.clone().or_else(|| p.name.clone()))
            .unwrap_or_default();
        let speed = g.speed.as_deref().unwrap_or("?");
        let is_tournament = is_tournament_source(g.source.as_deref());
        info!(
            game_id = %id,
            speed = %speed,
            opponent = %opponent_name,
            source = ?g.source,
            "ongoing game at startup"
        );
        s.active_games.insert(
            id.clone(),
            ActiveGame {
                opponent: opponent_name.clone(),
                is_tournament,
            },
        );
        if !opponent_name.is_empty() && opponent_name != bot_name {
            *s.opponent_engagements.entry(opponent_name).or_insert(0) += 1;
        }
        if speed == "correspondence" {
            s.startup_correspondence_games.insert(id);
        }
    }
}

/// Refresh both the BotState and Matchmaking copies of the online
/// blocklist. The refresh itself runs on a local clone so no shared mutex
/// is held while waiting on the network. If the refresh hits a transient
/// error, `OnlineBlocklist::refresh` already logs a `warn!`; we still
/// publish the (partially-updated) copy because etags returned by other
/// URLs in the same batch are valuable.
async fn refresh_online_blocklist(
    state: &Arc<Mutex<BotState>>,
    matchmaker: &Arc<Mutex<Matchmaking>>,
) {
    let mut working = {
        let s = state.lock().await;
        s.online_blocklist.clone()
    };
    working.refresh().await;
    {
        let mut s = state.lock().await;
        s.online_blocklist = working.clone();
    }
    matchmaker.lock().await.replace_online_block_list(working);
}

/// One matchmaking tick — snapshot the active-games set + challenge-queue
/// length under the `BotState` lock, then call `Matchmaking::challenge`
/// while holding only the matchmaker lock. The two-step lock dance avoids
/// holding `BotState` while the matchmaker does HTTP work (potentially
/// seconds long).
async fn run_matchmaking_tick(
    state: &Arc<Mutex<BotState>>,
    challengers: &ChallengeQueue,
    matchmaker: &Arc<Mutex<Matchmaking>>,
    config: &Arc<Config>,
    rng: &mut StdRng,
) {
    let (active_games_set, tournament_gate) = {
        let mut s = state.lock().await;
        // Include in-flight accepted-but-not-yet-started incoming challenges
        // (see `BotState::pending_accepts`) so the matchmaker doesn't read
        // `active == 0` and send an outbound challenge that pushes us over
        // `challenge.concurrency` before the incoming game's `gameStart`
        // lands. Sentinel keys are prefixed to guarantee no collision with
        // real game ids; `Matchmaking::challenge` only reads `.len()`.
        s.effective_active_games();
        let mut set: HashSet<String> = s.active_games.keys().cloned().collect();
        set.extend(
            s.pending_accepts
                .keys()
                .map(|id| format!("pending:{id}")),
        );
        let gate = if s.has_active_tournament_game() {
            Some("tournament game in progress")
        } else if s
            .last_tournament_game_ended
            .is_some_and(|t| t.elapsed() < TOURNAMENT_PAIRING_LAG)
        {
            Some("waiting for tournament pairing lag")
        } else {
            None
        };
        (set, gate)
    };
    if let Some(reason) = tournament_gate {
        debug!(reason = %reason, "skipping outbound matchmaking tick");
        return;
    }
    let queue_len = challengers
        .lock()
        .map(|q| q.len())
        .unwrap_or(0);
    let max_games = config.challenge.concurrency as usize;
    let mut m = matchmaker.lock().await;
    m.challenge(&active_games_set, queue_len, max_games, rng).await;
}

async fn handle_event(
    event: EventType,
    li: &Arc<Lichess>,
    config: &Arc<Config>,
    profile: &UserProfileType,
    state: &Arc<Mutex<BotState>>,
    challengers: &ChallengeQueue,
    matchmaker: &Arc<Mutex<Matchmaking>>,
    daily_counter: &Arc<Mutex<DailyCounter>>,
    games: &mut JoinSet<(String, Result<GameEnd>)>,
    shutdown_token: &CancellationToken,
) -> Result<()> {
    match event.kind.as_deref() {
        Some("challenge") => {
            if let Some(challenge_info) = event.challenge {
                handle_challenge(challenge_info, li, config, profile, state, matchmaker, daily_counter).await;
            }
        }
        Some("challengeCanceled") => {
            debug!(kind = ?event.kind, "challenge lifecycle event");
        }
        Some("challengeDeclined") => {
            matchmaker.lock().await.declined_challenge(&event);
        }
        Some("gameStart") => {
            if let Some(game) = event.game.clone() {
                let Some(game_id) = game.id.clone() else {
                    warn!("gameStart event without id, ignoring");
                    return Ok(());
                };
                let opponent = game.opponent.as_ref().and_then(|p| p.username.clone());
                let is_tournament = is_tournament_source(game.source.as_deref());
                // Parallel-game guard, layer 2 (Bus #467/#476): a brand-new,
                // non-tournament game that would push us over
                // `challenge.concurrency` is aborted instead of played. Layer 1
                // in `handle_challenge` cancels our outbound challenge before
                // accepting an incoming one, but the two accepts can still
                // cross on the wire; this is the root guard for "never two
                // games in one process" (embedded engine: process-global STOP).
                // Games known from startup (`prefill_ongoing_games`) are
                // exempt — they are already running, not new.
                let abort_second = {
                    let s = state.lock().await;
                    if s.spawned_games.contains(&game_id) {
                        debug!(game_id = %game_id, "gameStart resend for running game, ignoring");
                        return Ok(());
                    }
                    second_game_must_abort(
                        s.active_games.len(),
                        config.challenge.concurrency,
                        is_tournament,
                        s.active_games.contains_key(&game_id),
                    )
                };
                if abort_second {
                    let running = state.lock().await.active_games.len();
                    match li.abort(&game_id).await {
                        Ok(()) => {
                            error!(
                                game_id = %game_id,
                                opponent = ?opponent,
                                running,
                                limit = config.challenge.concurrency,
                                "PARALLEL_GAME_ABORTED: second game started while another is running; aborted instead of played"
                            );
                            // Reconcile the matchmaker (clears its outbound id
                            // if this was our challenge) and free any reserved
                            // accept slot; nothing was registered as active.
                            let mut s = state.lock().await;
                            s.pending_accepts.remove(&game_id);
                            drop(s);
                            matchmaker.lock().await.accepted_challenge(&event);
                            return Ok(());
                        }
                        Err(e) => {
                            // Abort refused (a move was already played?) — play
                            // the game rather than forfeit it on time; this is
                            // the pre-guard behaviour.
                            warn!(
                                game_id = %game_id,
                                running,
                                "abort of second parallel game failed ({e}); playing it anyway"
                            );
                        }
                    }
                }
                let is_correspondence_pickup = {
                    let mut s = state.lock().await;
                    // Re-check after the abort round-trip above (a resend could
                    // have arrived meanwhile — same reasoning as in the guard).
                    if s.spawned_games.contains(&game_id) {
                        debug!(game_id = %game_id, "gameStart resend for running game, ignoring");
                        return Ok(());
                    }
                    let pickup = s.startup_correspondence_games.remove(&game_id);
                    // If `prefill_ongoing_games` already inserted this game,
                    // it has also bumped `opponent_engagements`. Lichess
                    // resends `gameStart` for every ongoing game when the
                    // event stream opens, so we'd otherwise double-count.
                    let already_known = s.active_games.contains_key(&game_id);
                    s.active_games.insert(
                        game_id.clone(),
                        ActiveGame {
                            opponent: opponent.clone().unwrap_or_default(),
                            is_tournament,
                        },
                    );
                    s.spawned_games.insert(game_id.clone());
                    if !already_known {
                        // This game is now counted in `active_games`; release the
                        // reservation for this specific id if any (no-op for
                        // outbound / matchmaking starts, which never reserve
                        // one — accepted-incoming challenge_id == game_id, so
                        // this can't accidentally release a different accept's
                        // slot).
                        s.pending_accepts.remove(&game_id);
                        if let Some(name) = opponent.as_ref() {
                            *s.opponent_engagements.entry(name.clone()).or_insert(0) += 1;
                        }
                    }
                    pickup
                };
                if is_correspondence_pickup {
                    info!(game_id = %game_id, "picking up correspondence game from previous run");
                }
                // Tell the matchmaker that this game start consumed the
                // challenge we sent. If it wasn't our challenge, the call
                // is a no-op. A `true` return means this was our outbound
                // matchmaking challenge — which only ever targets bots — so it
                // counts toward the daily bot-vs-bot tally. (Incoming bot games
                // are counted at accept time in `handle_challenge`; together the
                // two cover every bot-vs-bot game without double counting.)
                let was_our_outbound = matchmaker.lock().await.accepted_challenge(&event);
                if was_our_outbound {
                    let tally = daily_counter.lock().await.increment();
                    state.lock().await.tallied_games.insert(game_id.clone());
                    debug!(game_id = %game_id, tally, "counted outbound bot game toward daily tally");
                }
                let li = li.clone();
                let cfg = config.clone();
                let profile = profile.clone();
                let queue = challengers.clone();
                let game_id_for_task = game_id.clone();
                let cancel = shutdown_token.clone();
                games.spawn(async move {
                    // Catch panics *inside* the task so they come back as a
                    // normal `(game_id, Err)` result the join arm can clean up
                    // (active_games / spawned_games / opponent_engagements /
                    // matchmaker.game_done()). Without this, a panic surfaces as
                    // a `JoinError` with no game_id — nothing gets cleaned up, the
                    // concurrency slot leaks, and at concurrency:1 the bot idles
                    // (declining every challenge with "later") until a manual
                    // restart. AssertUnwindSafe is sound here: on panic we discard
                    // all captured state and return, observing no broken invariants.
                    let res = std::panic::AssertUnwindSafe(play_game(
                        li,
                        cfg,
                        profile,
                        queue,
                        game_id_for_task.clone(),
                        cancel,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|panic| {
                        let msg = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        Err(anyhow!("play_game panicked: {msg}"))
                    });
                    (game_id_for_task, res)
                });
                info!(game_id = %game_id, "spawned game task");
            }
        }
        Some("gameFinish") => {
            debug!(game_id = ?event.game.as_ref().and_then(|g| g.id.clone()), "gameFinish event");
        }
        _ => {
            debug!(kind = ?event.kind, "ignoring event");
        }
    }
    Ok(())
}

async fn handle_challenge(
    info: ChallengeType,
    li: &Arc<Lichess>,
    config: &Arc<Config>,
    profile: &UserProfileType,
    state: &Arc<Mutex<BotState>>,
    matchmaker: &Arc<Mutex<Matchmaking>>,
    daily_counter: &Arc<Mutex<DailyCounter>>,
) {
    let challenge = Challenge::from_info(&info, profile);

    // Lichess sends `challenge`-events for both directions: incoming
    // (someone challenges us) and outgoing (we challenged someone).
    // For the outgoing case, calling `accept_challenge` on our own
    // challenge id returns 404 — the matchmaker already tracks the
    // outbound challenge via `accepted_challenge` in the gameStart
    // handler, so the only thing to do here is skip the event silently.
    if challenge.from_self {
        debug!(challenge_id = %challenge.id, "own outgoing challenge event, ignoring");
        return;
    }

    // Reciprocity (communal engine development): a bot that has
    // accepted one of *our* outbound matchmaking challenges (a game started)
    // is welcomed back regardless of rating — we challenge bots down to
    // ~our_rating-1000, so it's inconsistent to turn the same bot away at the
    // 2200 incoming floor when it challenges us. Only the rating gate is
    // waived; every other filter (block-lists, variant, noBot, concurrency,
    // daily budget) still applies. Looked up in the matchmaker's persistent
    // opponent database; locked and released before the state lock below.
    let rating_exempt = if challenge.challenger.is_bot {
        matchmaker
            .lock()
            .await
            .bot_accepted_our_challenge(&challenge.challenger.name)
    } else {
        false
    };

    // Global concurrency gate: separate from per-opponent engagement
    // (which `Challenge::is_supported` already enforces). We honour
    // `challenge.concurrency` from the YAML; in-flight games are
    // counted by their `gameStart`-tracked entry, which is good
    // enough — a race between accepting two challenges in the same
    // millisecond is theoretically possible but doesn't happen with a
    // normal bot's pacing.
    let (active_count, supported_pair) = {
        let mut guard = state.lock().await;
        let s = &mut *guard;
        let active = s.effective_active_games();
        let supported = challenge.is_supported(
            &config.challenge,
            &mut s.recent_bot_challenges,
            &s.opponent_engagements,
            &s.online_blocklist,
            profile,
            rating_exempt,
        );
        (active, supported)
    };
    if !concurrency_allows(active_count, config.challenge.concurrency) {
        info!(
            challenge_id = %challenge.id,
            active = active_count,
            limit = config.challenge.concurrency,
            "declining challenge: concurrency limit reached"
        );
        li.decline_challenge(&challenge.id, "later").await;
        return;
    }
    let (supported, reason) = supported_pair;
    if !supported {
        info!(challenge_id = %challenge.id, %reason, "declining challenge");
        li.decline_challenge(&challenge.id, reason).await;
        return;
    }

    // Daily bot-vs-bot budget gate (incoming side). Our best-effort mirror of
    // Lichess' 100/day cap, declining *bot* challenges once the tally reaches
    // `daily_game_limit - reserved_for_matchmaking` so a flood of incoming
    // (often casual, weak) bots can't eat the whole quota and starve our own
    // rated seeks. Human challengers never count toward the Lichess cap, so
    // they bypass this entirely. `daily_game_limit == 0` disables the budget.
    if challenge.challenger.is_bot && config.matchmaking.daily_game_limit > 0 {
        let limit = config.matchmaking.daily_game_limit;
        let reserved = config.matchmaking.reserved_for_matchmaking;
        let count = daily_counter.lock().await.count();
        if over_incoming_bot_budget(count, limit, reserved) {
            info!(
                challenge_id = %challenge.id,
                count, limit, reserved,
                threshold = limit.saturating_sub(reserved),
                "declining incoming bot challenge: daily bot-game budget reached"
            );
            li.decline_challenge(&challenge.id, "later").await;
            return;
        }
    }

    // Parallel-game guard, layer 1 (Vorfall trvX6Pww/9iBcixas + dhchjrB9/
    // AVEU3CPf 30.09, Bus #467/#476): the concurrency gate above only counts
    // running games and pending incoming accepts — NOT our own outbound
    // matchmaking challenge that may still be waiting for the challenged bot's
    // answer. If that bot accepts *after* we accepted this incoming challenge,
    // Lichess starts a second game in the same process, and with the embedded
    // engine's process-global STOP one game's prefill reclaim kills the other
    // game's search (ILLEGAL_BESTMOVE → fallback moves, rated loss). So
    // withdraw the outbound challenge first; Lichess' cancel endpoint also
    // aborts the game if the accept has just landed and no move was played.
    // If the cancel fails we don't know whether a game will start from it, so
    // the incoming challenge is declined with "later" instead of accepted.
    let outstanding = matchmaker.lock().await.outstanding_challenge_id();
    if let Some(outbound_id) = outstanding {
        match li.cancel(&outbound_id).await {
            Ok(()) => {
                info!(
                    challenge_id = %challenge.id,
                    outbound_id = %outbound_id,
                    "cancelled our pending outbound challenge before accepting incoming one"
                );
                matchmaker.lock().await.discard_challenge(&outbound_id);
            }
            Err(e) => {
                warn!(
                    challenge_id = %challenge.id,
                    outbound_id = %outbound_id,
                    "cancel of pending outbound challenge failed ({e}); declining incoming challenge"
                );
                li.decline_challenge(&challenge.id, "later").await;
                return;
            }
        }
    }

    info!(challenge_id = %challenge.id, "accepting challenge");
    // Reserve a concurrency slot for this accept until its `gameStart` lands
    // (see `BotState::pending_accepts`), so a second incoming challenge
    // processed before then is gated instead of also being accepted.
    state
        .lock()
        .await
        .pending_accepts
        .insert(challenge.id.clone(), Instant::now());
    if let Err(e) = li.accept_challenge(&challenge.id).await {
        warn!(challenge_id = %challenge.id, "accept failed: {e}");
        // No game will start from a failed accept — release the reserved slot.
        state.lock().await.pending_accepts.remove(&challenge.id);
    } else if challenge.challenger.is_bot {
        // Count the accepted incoming bot game toward the daily tally. Counted
        // here at accept time (the gameStart opponent object omits the title,
        // so bot-ness can't be re-derived there). The id is remembered in
        // `tallied_games` so an `aborted` end hands the point back (join arm).
        let tally = daily_counter.lock().await.increment();
        state.lock().await.tallied_games.insert(challenge.id.clone());
        debug!(challenge_id = %challenge.id, tally, "counted incoming bot game toward daily tally");
    }
}

/// Whether one more game would still fit under the configured
/// `challenge.concurrency` limit. `concurrency == 0` means "decline
/// everything", matching Python's behaviour and the warning emitted
/// during config validation.
/// Whether a `gameStart` for a game not yet in `active_games` must be aborted
/// because `running_games` other games already fill `challenge.concurrency`
/// (parallel-game guard, layer 2). Tournament pairings are never aborted
/// (arena no-shows are penalised and the tournament gate already keeps
/// matchmaking quiet), and games already known from the startup pickup are
/// running, not new. `concurrency == 0` never aborts here either — such a
/// bot accepts nothing, so any game that starts is a deliberate outside
/// action (e.g. a startup pickup) and not a second parallel game.
pub fn second_game_must_abort(
    running_games: usize,
    concurrency: u32,
    is_tournament: bool,
    already_known: bool,
) -> bool {
    if is_tournament || already_known || concurrency == 0 {
        return false;
    }
    running_games >= concurrency as usize
}

pub fn concurrency_allows(active: usize, concurrency: u32) -> bool {
    if concurrency == 0 {
        return false;
    }
    active < concurrency as usize
}

/// Whether an incoming **bot** challenge must be declined to protect the daily
/// bot-vs-bot budget. `count` is the current tally, `limit` our mirror of
/// Lichess' 100/day cap, `reserved` the slice held back for self-sought
/// matchmaking. Incoming bots are turned away once the tally reaches
/// `limit - reserved`, leaving `reserved` games of headroom for our own seeks.
/// Pure (no I/O) so the budget policy is unit-testable. Callers gate on
/// `is_bot` and `limit > 0` before consulting this.
pub fn over_incoming_bot_budget(count: u32, limit: u32, reserved: u32) -> bool {
    count >= limit.saturating_sub(reserved)
}

// ---------------------------------------------------------------------------
// Per-game task
// ---------------------------------------------------------------------------

/// How a game task ended, as far as the main loop's bookkeeping cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GameEnd {
    /// The game was actually played (any terminal status but `aborted`), or
    /// the task left the loop without a terminal status (stream give-up,
    /// shutdown). Both get the normal post-game cooldown.
    Played,
    /// Lichess reported `aborted`: ended before real play. Not a game.
    Aborted,
}

/// Classify a finished game's Lichess status for the join arm.
fn game_end_for(state: &GameStateType) -> GameEnd {
    if state.status.as_deref() == Some("aborted") {
        GameEnd::Aborted
    } else {
        GameEnd::Played
    }
}

async fn play_game(
    li: Arc<Lichess>,
    config: Arc<Config>,
    profile: UserProfileType,
    challengers: ChallengeQueue,
    game_id: String,
    cancel: CancellationToken,
) -> Result<GameEnd> {
    let username = profile.username.clone().unwrap_or_default();
    let abort_time = Duration::from_secs(config.abort_time);
    let move_overhead = Duration::from_millis(config.move_overhead);
    let min_time = Duration::from_millis(config.rate_limiting_delay);
    let base_url = li.base_url().to_string();

    let mut stream = Box::pin(
        li.get_game_stream(&game_id)
            .await
            .with_context(|| format!("opening game stream for {game_id}"))?,
    );

    // The first event on a bot game stream is always gameFull.
    let first = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("game stream {game_id} ended before any event"))??;
    if first.kind.as_deref() != Some("gameFull") {
        bail!("expected gameFull as first event, got {:?}", first.kind);
    }

    let mut game = Game::new(&first, &username, &base_url, abort_time);
    info!(game_id = %game.id, opponent = %game.opponent, "game stream open");

    let mut engine = spawn_engine_for(&config.engine, &game)
        .await
        .with_context(|| format!("starting engine for game {game_id}"))?;

    // Tell the engine who it's playing. No-op when the engine doesn't
    // expose `UCI_Opponent` (most don't outside Stockfish-style builds).
    let opponent_info = crate::engine_wrapper::OpponentInfo::from_player(&game.opponent);
    if let Err(e) = engine.send_opponent_info(&opponent_info, game.me.rating).await {
        warn!(game_id = %game.id, "send_opponent_info failed: {e}");
    }
    #[cfg(feature = "embedded")]
    if matches!(engine, EngineBackend::Embedded(_)) {
        let rated = game.mode == "rated";
        let c = crate::embedded_engine::contempt_for(
            rated,
            game.opponent.rating,
            game.opponent.provisional.unwrap_or(false),
            game.opponent.is_bot,
            game.me.rating,
        );
        crate::embedded_engine::set_game_contempt(c, game.is_white);
        let opp_band = crate::embedded_engine::opp_band_for(game.opponent.rating, game.me.rating);
        info!(game_id = %game.id, contempt = c, rated, is_white = game.is_white, opp_band, opp_rating = ?game.opponent.rating, our_rating = ?game.me.rating, "embedded contempt set");
    }

    let castling_mode = if game.variant_key == "chess960" || game.variant_key == "fromPosition" {
        CastlingMode::Chess960
    } else {
        CastlingMode::Standard
    };
    let mut board = setup_board(&game, castling_mode)?;

    let initial_fen = game.initial_fen.clone();
    let is_correspondence = matches!(game.speed.as_deref(), Some("correspondence"));
    let correspondence_move_time = Duration::from_secs(config.correspondence.move_time);
    let correspondence_disconnect = Duration::from_secs(config.correspondence.disconnect_time);
    // Python's `lichess_bot.py` uses `uci_ponder or ponder` — either
    // flag turns pondering on.
    let can_ponder = config.engine.uci_ponder || config.engine.ponder;

    let mut draw_resign = DrawResignTracker::new();
    // Position-repetition counter (Polyglot Zobrist hash → occurrences) for the
    // claim-draw heuristic. One entry per gameState; a count ≥ 3 is a threefold
    // repetition. (Reconnects that replay several moves at once only stamp the
    // resulting position — fine, Lichess still auto-draws at five-fold.)
    let mut position_counts: HashMap<u64, u8> = HashMap::new();
    let mut rng = StdRng::from_entropy();
    let mut prior_game: Option<Game> = None;
    let mut setup_timer = Timer::zero();
    // Background delivery task for the most recently played move (see
    // `play_move`): the game task must never block on make_move retries.
    // `play_move` aborts a stale handle before each new search; the post-loop
    // teardown below aborts whatever is left at game end.
    let mut pending_delivery: Option<tokio::task::JoinHandle<()>> = None;

    // Chat: one Conversation per game, mirrors Python's `Conversation(game, …)`.
    // Greeting templates are rendered once with `{me}` / `{opponent}` filled
    // from the current game; we re-render at the goodbye stage too because
    // Python does the same (templates are stable strings, but rendering twice
    // is cheap and keeps the helper signature consistent).
    let mut conversation = Conversation::new(
        game.clone(),
        (*li).clone(),
        VERSION,
        challengers.clone(),
        profile.clone(),
        config.source_url.clone(),
    );
    let mut greeted = false;

    // Python parity (`lichess_bot.py:708`): arm the abort / terminate /
    // disconnect timers at game start. Without this the timers from
    // `Game::new` get evaluated but never refreshed once a `gameState`
    // arrives — and worse, they were never *checked* at all in the old
    // `while let` loop. Disconnect only applies to correspondence games
    // that haven't seen a move yet.
    let initial_disconnect = if is_correspondence
        && game.state.moves.as_deref().map_or(true, str::is_empty)
    {
        correspondence_disconnect
    } else {
        Duration::from_secs(0)
    };
    let initial_terminate = compute_terminate_in(&game, &board);
    game.ping(abort_deadline(&game, abort_time), initial_terminate, initial_disconnect);

    if is_engine_move(&game, prior_game.as_ref(), &board) {
        maybe_send_greeting(&conversation, &game, &config.greeting, &mut greeted).await;
        play_one(
            &mut engine,
            &board,
            initial_fen.as_deref(),
            &game,
            &li,
            &config,
            &mut draw_resign,
            &mut rng,
            &setup_timer,
            move_overhead,
            can_ponder,
            is_correspondence,
            correspondence_move_time,
            min_time,
            &mut pending_delivery,
        )
        .await?;
    }

    // Periodic tick so the inactivity timers (`should_abort_now`,
    // `should_terminate_now`, `should_disconnect_now`) actually get
    // evaluated — Lichess's NDJSON keep-alives are dropped by our stream
    // filter, so without this `select!` branch a silent opponent would
    // pin a game forever (cf. sp4ifq8J).
    let mut inactivity_tick = tokio::time::interval(Duration::from_secs(1));
    inactivity_tick.tick().await; // consume immediate first tick

    // H2 book-ponder / TT-prefill (clrsrc Bus #283–#293). `engine` becomes an
    // Option so the prefill task can *take* it during the opponent's time and
    // hand it back (move-in/return, clrsrc #292 — EmbeddedEngine is Send). Gated
    // on the config flag AND concurrency==1: the prefill abort raises clrsrc's
    // process-global STOP, which at concurrency>1 would also stop a neighbouring
    // game's live search. Embedded-only (checked per-checkout via is_embedded).
    let mut engine = Some(engine);
    // The `JoinHandle` is paired with an `Arc` clone of the engine's
    // `prefill_cancel` flag so the loop can raise it at reclaim before the engine
    // returns from the task (clrsrc #298 a/b).
    let mut prefill: Option<(tokio::task::JoinHandle<EngineBackend>, Arc<AtomicBool>)> = None;
    let book_ponder = config.engine.book_ponder && config.challenge.concurrency == 1;
    // opponent_brain (Bus #299): whole-game generalisation — prefill after every
    // move, not just book moves. Supersedes book_ponder's book-only gate.
    let opponent_brain = config.engine.opponent_brain && config.challenge.concurrency == 1;

    loop {
        let evt = tokio::select! {
            biased;
            maybe_evt = stream.next() => match maybe_evt {
                Some(Ok(evt)) => evt,
                other => {
                    // Both stream EOF (None) and transport errors trigger a
                    // clock-aware resubscribe rather than abandoning the game.
                    // Lichess resends gameFull on the new connection; the
                    // gameFull arm below resyncs board state and replays moves.
                    match &other {
                        None => warn!(game_id = %game.id, "game stream ended; resubscribing"),
                        Some(Err(e)) => warn!(game_id = %game.id, "game stream error: {e}; resubscribing"),
                        Some(Ok(_)) => unreachable!(),
                    }
                    // Budget: giving up = certain forfeit, so ride out a
                    // transient Lichess outage (e.g. a multi-second 502 burst)
                    // as patiently as the clock allows — the event-stream
                    // reconnect already retries indefinitely and survives these.
                    // Cap at min(45 s, 10% of remaining clock) so Blitz/Bullet
                    // still can't burn their clock; the 10%-clause is what keeps
                    // short TC safe. Correspondence / timed-out clocks use 45 s.
                    // (nf2HyarU, 2026-07-01: a ~10 s 502 outlasted the old 12 s /
                    //  3-retry budget by ~3 s and forfeited a +0.37 rapid game.)
                    let remaining = game.my_remaining_time();
                    let budget = if remaining.is_zero() {
                        Duration::from_secs(45)
                    } else {
                        Duration::from_secs(45).min(remaining / 10).max(Duration::from_secs(1))
                    };
                    // Exponential backoff 1,2,4,8,8,8,… (per-attempt delay capped
                    // at 8 s) that keeps retrying until the budget is spent,
                    // rather than stopping after a fixed 4 steps.
                    let mut total_waited = Duration::ZERO;
                    let mut delay = Duration::from_secs(1);
                    let mut resubscribed = false;
                    let mut cancelled = false;
                    loop {
                        if total_waited + delay > budget {
                            break;
                        }
                        // Race the backoff sleep against shutdown so a Ctrl-C
                        // during this retry loop ends the game task right
                        // away instead of running the full budget and being
                        // torn down later by `abort_all()` — see the
                        // `shutdown_token` comment in `start_program` (5bTILJsc).
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                cancelled = true;
                                break;
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                        total_waited += delay;
                        match li.get_game_stream(&game.id).await {
                            Ok(s) => {
                                stream = Box::pin(s);
                                info!(game_id = %game.id, "game stream resubscribed");
                                resubscribed = true;
                                break;
                            }
                            Err(e2) => {
                                error!(game_id = %game.id, "game stream resubscription failed: {e2}");
                            }
                        }
                        delay = (delay * 2).min(Duration::from_secs(8));
                    }
                    if resubscribed {
                        continue;
                    } else if cancelled {
                        warn!(game_id = %game.id, "shutdown requested during stream resubscribe backoff, ending game task");
                        break;
                    } else {
                        warn!(game_id = %game.id, "game stream resubscribe budget exhausted, giving up");
                        break;
                    }
                }
            },
            _ = inactivity_tick.tick() => {
                if game.should_abort_now() {
                    info!(game_id = %game.id, "aborting game (first-move inactivity)");
                    if let Err(e) = li.abort(&game.id).await {
                        warn!(game_id = %game.id, "abort failed: {e}");
                    }
                    break;
                }
                if game.should_terminate_now() {
                    info!(game_id = %game.id, "terminating game (prolonged inactivity)");
                    if game.is_abortable() {
                        if let Err(e) = li.abort(&game.id).await {
                            warn!(game_id = %game.id, "abort failed: {e}");
                        }
                    }
                    break;
                }
                if is_correspondence
                    && !is_engine_move(&game, prior_game.as_ref(), &board)
                    && game.should_disconnect_now()
                {
                    info!(game_id = %game.id, "disconnecting correspondence game (lack of activity)");
                    break;
                }
                continue;
            }
        };
        setup_timer = Timer::zero();

        // H2: reclaim the engine from any in-flight prefill before handling the
        // event. Every branch below that needs the engine (play_one,
        // send_game_result, harvest) requires it back, and no two searches may
        // overlap (shared process-global STOP). The abort is prompt (per-node),
        // so the join is fast; the prefill's own max_deadline is a runaway guard.
        // Events that never touch the engine leave the prefill running: the
        // first event after our move is the gameState echo of that very move,
        // and reclaiming there ended every prefill after one network round
        // trip (clrsrc #467 / bot #458: median 18 ms over 491k prefills).
        let reclaim = if prefill.is_some() && event_needs_engine(&evt, &game, &board) {
            prefill.take()
        } else {
            None
        };
        if let Some((mut handle, prefill_cancel)) = reclaim {
            let reclaim_start = Instant::now();
            // Race-proof abort (clrsrc #297/#298, Vorfall kAozh2eG 07.07): if the
            // prefill task had not yet reached the search entry when the stop was
            // raised, the entry OVERWRITES the global STOP with its own cancel
            // value and the prefill runs its full deadline — billed to OUR clock
            // via this join. (b) Raise the shared per-call cancel FIRST so an
            // entry that runs after this point reads `true` and stops itself at
            // the first node; (c) then re-raise the global STOP until the join
            // completes (the entry overwrite can happen at most once, so this
            // converges within a few ms — the search checks the global STOP at
            // practically every node).
            prefill_cancel.store(true, Ordering::Relaxed);
            let joined = loop {
                EngineBackend::request_prefill_stop();
                tokio::select! {
                    r = &mut handle => break r,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {}
                }
            };
            // Reset for the next cycle so the regular-search-entry invariant
            // (prefill flag false) holds; the next prefill also clears it at spawn.
            prefill_cancel.store(false, Ordering::Relaxed);
            debug!(
                game_id = %game.id,
                reclaim_ms = reclaim_start.elapsed().as_millis() as u64,
                "H2 prefill reclaimed"
            );
            match joined {
                Ok(e) => engine = Some(e),
                Err(join_err) => {
                    error!(game_id = %game.id, error = %join_err, "H2 prefill task panicked; engine lost, ending game task");
                    break;
                }
            }
        }

        match evt.kind.as_deref() {
            Some("gameState") => {
                let new_state = state_from_event(&evt);
                // Count repetitions per *move*, not per event: Lichess also
                // re-sends `gameState` for draw/takeback-offer flips (and every
                // resubscribe restamps via `gameFull`), which would inflate the
                // threefold counter and risk claiming a draw in a position that
                // never actually repeated. Only stamp when the move list grew.
                let old_ply = game
                    .state
                    .moves
                    .as_deref()
                    .map_or(0, |m| m.split_whitespace().count());
                let new_ply = new_state
                    .moves
                    .as_deref()
                    .map_or(0, |m| m.split_whitespace().count());
                apply_new_moves(&game.state, &new_state, &mut board)?;
                if new_ply > old_ply {
                    *position_counts
                        .entry(crate::polyglot::polyglot_hash(&board))
                        .or_insert(0) += 1;
                }
                prior_game = Some(game.clone());
                game.state = new_state;

                // Refresh the inactivity deadlines whenever the position
                // moves forward. Python: `lichess_bot.py:757-758`.
                let terminate_in = compute_terminate_in(&game, &board);
                let disconnect_in = if is_correspondence
                    && is_engine_move(&game, prior_game.as_ref(), &board)
                {
                    correspondence_disconnect
                } else {
                    Duration::from_secs(0)
                };
                game.ping(abort_deadline(&game, abort_time), terminate_in, disconnect_in);

                if is_game_over(&game.state) {
                    info!(game_id = %game.id, status = ?game.state.status, "game over");
                    let engine = engine.as_mut().expect("engine present after prefill reclaim");
                    if let Err(e) = engine.send_game_result(&game).await {
                        warn!(game_id = %game.id, "send_game_result failed: {e}");
                    }
                    harvest_experience_overlay(
                        &game,
                        castling_mode,
                        engine,
                        &config.engine.experience,
                    );
                    send_goodbye(&conversation, &game, &config.greeting).await;
                    save_pgn_if_configured(&li, &config, &game, &username).await;
                    break;
                }
                if is_engine_move(&game, prior_game.as_ref(), &board) {
                    // Draw shortcuts, both gated on a non-winning evaluation so
                    // we never give up a winning position. On any failure we
                    // fall through to a normal move so the game never stalls.
                    let dr = &config.engine.draw_or_resign;
                    let not_winning = dr.offer_draw_enabled
                        && draw_resign
                            .last_score_cp()
                            .is_some_and(|cp| cp <= dr.offer_draw_score);
                    // (2) Accept a standing opponent draw offer.
                    let accept_draw = not_winning && check_for_draw_offer(&game);
                    // (3) Claim a draw by threefold repetition / 50-move rule
                    // rather than play on until Lichess auto-draws at five-fold
                    // / 75 moves (lets us avoid a stronger opponent fishing on
                    // in a dead-drawn position).
                    let claim_draw = not_winning
                        && (position_counts
                            .get(&crate::polyglot::polyglot_hash(&board))
                            .copied()
                            .unwrap_or(0)
                            >= 3
                            || board.halfmoves() >= 100);
                    if accept_draw && li.handle_draw_offer(&game.id, true).await.is_ok() {
                        info!(game_id = %game.id, "accepted opponent's draw offer");
                    } else if claim_draw && li.claim_draw(&game.id).await.is_ok() {
                        info!(game_id = %game.id, "claimed draw (repetition / 50-move rule)");
                    } else {
                        maybe_send_greeting(&conversation, &game, &config.greeting, &mut greeted).await;
                        let played = play_one(
                            engine.as_mut().expect("engine present after prefill reclaim"),
                            &board,
                            initial_fen.as_deref(),
                            &game,
                            &li,
                            &config,
                            &mut draw_resign,
                            &mut rng,
                            &setup_timer,
                            move_overhead,
                            can_ponder,
                            is_correspondence,
                            correspondence_move_time,
                            min_time,
                            &mut pending_delivery,
                        )
                        .await?;
                        if let Some(played) = played {
                            maybe_start_prefill(
                                book_ponder,
                                opponent_brain,
                                &mut engine,
                                &mut prefill,
                                initial_fen.as_deref(),
                                &game,
                                &board,
                                &played,
                            );
                        }
                    }
                }
            }
            Some("gameFull") => {
                // Received on reconnect. Resync board and state from the
                // embedded gameState; the full move list covers any moves
                // missed during disconnect. Pass prior=None to is_engine_move
                // so we play even when the move list didn't change (e.g. we
                // disconnected exactly on our turn and no move was delivered).
                let new_state = match evt.state {
                    Some(s) => s,
                    None => {
                        warn!(game_id = %game.id, "gameFull on reconnect has no embedded state, exiting game task");
                        break;
                    }
                };
                game.state = new_state;
                match setup_board(&game, castling_mode) {
                    Ok(b) => board = b,
                    Err(e) => {
                        error!(game_id = %game.id, "gameFull reconnect board rebuild failed: {e}");
                        break;
                    }
                }
                // No repetition stamp here: `gameFull` is a reconnect resync of
                // an already-counted position, so restamping would double-count.
                // A repetition spanning the disconnect is at worst under-counted,
                // and Lichess still auto-draws at fivefold.

                if is_game_over(&game.state) {
                    info!(game_id = %game.id, status = ?game.state.status, "game ended during reconnect");
                    let engine = engine.as_mut().expect("engine present after prefill reclaim");
                    if let Err(e) = engine.send_game_result(&game).await {
                        warn!(game_id = %game.id, "send_game_result failed: {e}");
                    }
                    harvest_experience_overlay(&game, castling_mode, engine, &config.engine.experience);
                    send_goodbye(&conversation, &game, &config.greeting).await;
                    save_pgn_if_configured(&li, &config, &game, &username).await;
                    break;
                }

                let terminate_in = compute_terminate_in(&game, &board);
                game.ping(abort_deadline(&game, abort_time), terminate_in, Duration::from_secs(0));

                if is_engine_move(&game, None, &board) {
                    maybe_send_greeting(&conversation, &game, &config.greeting, &mut greeted).await;
                    play_one(
                        engine.as_mut().expect("engine present after prefill reclaim"),
                        &board,
                        initial_fen.as_deref(),
                        &game,
                        &li,
                        &config,
                        &mut draw_resign,
                        &mut rng,
                        &setup_timer,
                        move_overhead,
                        can_ponder,
                        is_correspondence,
                        correspondence_move_time,
                        min_time,
                        &mut pending_delivery,
                    )
                    .await?;
                }
            }
            Some("chatLine") => {
                let line = ChatLine::from_event(&evt);
                let engine_ref = engine.as_ref().expect("engine present after prefill reclaim");
                if let Err(e) = conversation.react(line, engine_ref, &board).await {
                    warn!(game_id = %game.id, "chat reaction failed: {e}");
                }
            }
            Some("opponentGone") => {
                debug!(
                    game_id = %game.id,
                    gone = ?evt.gone,
                    claim_win_in = ?evt.claim_win_in_seconds,
                    "opponent gone update"
                );
                // Lichess sends `claimWinInSeconds: 0` once the opponent has
                // been disconnected long enough for us to claim the win. We
                // act on it automatically so a stuck stream doesn't pin the
                // concurrency slot forever (cf. uB73iI7W incident).
                if evt.gone == Some(true) && evt.claim_win_in_seconds == Some(0) {
                    match li.claim_victory(&game.id).await {
                        Ok(()) => info!(game_id = %game.id, "claimed victory (opponent gone)"),
                        Err(e) => warn!(game_id = %game.id, "claim_victory failed: {e}"),
                    }
                }
            }
            other => {
                debug!(game_id = %game.id, kind = ?other, "ignoring game event");
            }
        }
    }

    // A delivery task still retrying at game end has nothing useful left to
    // deliver — abort it instead of letting it POST into a finished game for
    // the rest of its backoff budget.
    if let Some(h) = pending_delivery.take() {
        h.abort();
    }
    // Drop this game's out-of-book counter so the global map doesn't grow
    // unbounded over a 24/7 run (no-op if online sources were never used).
    crate::online_book::reset_out_of_book_counter(&game.id);
    // Reclaim the engine if we broke out of the loop with a prefill still in
    // flight (e.g. an inactivity abort or a stream-resubscribe give-up), so it
    // isn't leaked before quit. A panicked prefill leaves `engine` None → quit
    // is simply skipped (there's nothing to reap in-process).
    if let Some((mut handle, prefill_cancel)) = prefill.take() {
        // Same cancel-first + re-raise pattern as the loop-top reclaim (entry may
        // overwrite a single stop raise — see clrsrc #297/#298 a/b/c).
        let reclaim_start = Instant::now();
        prefill_cancel.store(true, Ordering::Relaxed);
        let joined = loop {
            EngineBackend::request_prefill_stop();
            tokio::select! {
                r = &mut handle => break r,
                _ = tokio::time::sleep(std::time::Duration::from_millis(2)) => {}
            }
        };
        prefill_cancel.store(false, Ordering::Relaxed);
        debug!(
            game_id = %game.id,
            reclaim_ms = reclaim_start.elapsed().as_millis() as u64,
            "H2 prefill reclaimed (post-loop)"
        );
        if let Ok(e) = joined {
            engine = Some(e);
        }
    }
    if let Some(engine) = engine.take() {
        if let Err(e) = engine.quit().await {
            warn!(game_id = %game.id, error = %e, "engine quit failed (process may need a manual reap)");
        }
    }
    Ok(game_end_for(&game.state))
}

// ---------------------------------------------------------------------------
// Greetings
// ---------------------------------------------------------------------------

/// Substitute `{me}` and `{opponent}` in a greeting template. Python uses
/// `defaultdict(str)` + `str.format_map`, so unknown braces stay literal in
/// principle; we only support the two keys the YAML schema documents and
/// leave any other `{...}` untouched.
fn render_greeting(template: &str, game: &Game) -> String {
    template
        .replace("{me}", &game.me.name)
        .replace("{opponent}", &game.opponent.name)
}

/// Count the half-moves recorded in `game.state.moves`. Mirrors Python's
/// `len(board.move_stack)`.
fn move_count(game: &Game) -> usize {
    crate::engine_wrapper::move_count(&game.state)
}

/// Send `hello` / `hello_spectators` once, in the first two plies of the
/// game — matches Python's `say_hello` guard `len(board.move_stack) < 2`.
async fn maybe_send_greeting(
    conv: &Conversation,
    game: &Game,
    greeting: &GreetingConfig,
    greeted: &mut bool,
) {
    if *greeted || move_count(game) >= 2 {
        return;
    }
    *greeted = true;
    let hello = render_greeting(&greeting.hello, game);
    let hello_spectators = render_greeting(&greeting.hello_spectators, game);
    if let Err(e) = conv.send_message("player", &hello).await {
        warn!(game_id = %game.id, "hello to player room failed: {e}");
    }
    if let Err(e) = conv.send_message("spectator", &hello_spectators).await {
        warn!(game_id = %game.id, "hello to spectator room failed: {e}");
    }
}

/// Send `goodbye` / `goodbye_spectators` after the game has ended. Empty
/// templates are dropped by [`Conversation::send_message`].
async fn send_goodbye(conv: &Conversation, game: &Game, greeting: &GreetingConfig) {
    let goodbye = render_greeting(&greeting.goodbye, game);
    let goodbye_spectators = render_greeting(&greeting.goodbye_spectators, game);
    if let Err(e) = conv.send_message("player", &goodbye).await {
        warn!(game_id = %game.id, "goodbye to player room failed: {e}");
    }
    if let Err(e) = conv.send_message("spectator", &goodbye_spectators).await {
        warn!(game_id = %game.id, "goodbye to spectator room failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// PGN recording
// ---------------------------------------------------------------------------
//
// Mirrors Python's `save_pgn_record` for the finished-game case. We only
// fetch and write the canonical Lichess PGN — engine-side annotations
// (PV, depth, eval per move) are deliberately skipped: the Rust UCI
// wrapper does not yet keep per-ply commentary, and adding a chess.pgn-
// equivalent round-trip is more scope than this task warrants. Once
// `EngineWrapper::comment_for_board_index` exists, this is the place to
// merge engine evals into the lichess PGN before writing.

const PGN_ILLEGAL_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Strip filename-illegal characters. Matches Python's
/// `"".join(c for c in s if c not in '<>:"/\\|?*')`.
fn sanitize_filename(s: &str) -> String {
    s.chars().filter(|c| !PGN_ILLEGAL_FILENAME_CHARS.contains(c)).collect()
}

/// Pick the file the PGN should land in. Mirrors Python's
/// `get_game_file_path`. `complete=false` always uses the per-game name
/// (Python's `force_single` / "still running" path).
fn pgn_target_path(
    dir: &Path,
    grouping: &str,
    game_id: &str,
    white: &str,
    black: &str,
    user: &str,
    complete: bool,
) -> PathBuf {
    let name = if grouping == "game" || !complete {
        format!("{white} vs {black} - {game_id}.pgn")
    } else if grouping == "opponent" {
        let opponent = if user == black { white } else { black };
        format!("{user} games vs. {opponent}.pgn")
    } else {
        // "all" — also the fallback for unknown values (config validates
        // the enum, so this branch is the documented "all" case).
        format!("{user} games.pgn")
    };
    dir.join(sanitize_filename(&name))
}

/// Fetch the canonical PGN from Lichess and append/write it to
/// `pgn_directory`. No-op when `pgn_directory` is unset. Errors are
/// downgraded to a `warn!` so a failing PGN write never aborts the bot.
async fn save_pgn_if_configured(li: &Lichess, config: &Config, game: &Game, username: &str) {
    let Some(dir_str) = config.pgn_directory.as_deref() else {
        return;
    };
    let dir = PathBuf::from(dir_str);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn!(game_id = %game.id, "could not create pgn_directory {dir_str}: {e}");
        return;
    }
    let pgn = li.get_game_pgn(&game.id).await;
    if pgn.trim().is_empty() {
        warn!(game_id = %game.id, "lichess returned empty PGN, skipping write");
        return;
    }
    let target = pgn_target_path(
        &dir,
        &config.pgn_file_grouping,
        &game.id,
        &game.white.name,
        &game.black.name,
        username,
        true,
    );
    let single = pgn_target_path(
        &dir,
        "game",
        &game.id,
        &game.white.name,
        &game.black.name,
        username,
        true,
    );
    // "game" grouping: overwrite (one file per game). Otherwise: append,
    // so all matches against the same opponent / all of our matches end up
    // in a shared PGN.
    let write_res = if target == single {
        tokio::fs::write(&target, format!("{}\n\n", pgn.trim_end())).await
    } else {
        append_pgn(&target, &pgn).await
    };
    if let Err(e) = write_res {
        warn!(game_id = %game.id, path = %target.display(), "writing PGN failed: {e}");
        return;
    }
    info!(game_id = %game.id, path = %target.display(), "wrote PGN");
    // For groupings that share a file, also clean up any leftover
    // single-game file (Python does the same) — the single-game file is
    // written only while the game is still running, so finishing the game
    // makes it obsolete.
    if target != single && tokio::fs::metadata(&single).await.is_ok() {
        if let Err(e) = tokio::fs::remove_file(&single).await {
            debug!(game_id = %game.id, path = %single.display(), "could not remove intermediate single-game PGN: {e}");
        }
    }
}

async fn append_pgn(path: &Path, pgn: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(pgn.trim_end().as_bytes()).await?;
    f.write_all(b"\n\n").await?;
    Ok(())
}

/// Whether handling `evt` needs the engine back from an in-flight prefill.
/// Mirrors the engine uses in the game loop's event match: a `gameState` needs
/// it when the game is over (send_game_result, harvest) or when the new state
/// puts us on move (play_one); `gameFull` and `chatLine` always use it.
/// Everything else — above all the `gameState` echo of our own move, draw-offer
/// flips on the opponent's turn and `opponentGone` — leaves the prefill alone.
fn event_needs_engine(evt: &GameEventType, game: &Game, board: &Chess) -> bool {
    match evt.kind.as_deref() {
        Some("gameState") => {
            let new_state = state_from_event(evt);
            if is_game_over(&new_state) {
                return true;
            }
            let mut after = board.clone();
            if apply_new_moves(&game.state, &new_state, &mut after).is_err() {
                return true;
            }
            (after.turn() == Color::White) == game.is_white
        }
        Some("gameFull") | Some("chatLine") => true,
        _ => false,
    }
}

/// Move list for a prefill: the game's moves so far plus the move we just
/// played (`game.state.moves` only grows from stream events, so our own move
/// is not in it yet). `None` when that move ended the game on the board
/// (mate, stalemate, insufficient material) — nothing to search then.
#[cfg_attr(not(feature = "embedded"), allow(dead_code))]
fn prefill_moves(game: &Game, board: &Chess, played: &shakmaty::Move) -> Option<Vec<String>> {
    let mut after = board.clone();
    after.play_unchecked(played);
    if after.is_game_over() {
        return None;
    }
    let mut moves: Vec<String> = game
        .state
        .moves
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect();
    moves.push(UciMove::from_standard(played).to_string());
    Some(moves)
}

/// Ponder-lite prefill: right after we played a move, start a background search
/// on the resulting position (opponent to move) during their think, so our next
/// search starts with a warm TT/history instead of cold (clrsrc Bus #283–#299).
/// Two gates share this path:
///   - `book_ponder`: only while still in the opening book (last move was a
///     book hit, `depth == 0`) — warms the cold book-exit search (H2).
///   - `opponent_brain`: after EVERY move, whole game — supersedes the book gate.
/// Requires the embedded backend. The engine is moved into the task and handed
/// back on join (reclaimed race-safely at the loop top on the next event that
/// needs it, see [`event_needs_engine`]). No-op if neither gate is armed, a
/// prefill is already in flight, or `played` ended the game. The deadline is
/// only a runaway guard (`min(opp_time·0.5, 10 s)`); the real stop is the
/// loop-top abort+join when the opponent moves.
#[cfg(feature = "embedded")]
#[allow(clippy::too_many_arguments)]
fn maybe_start_prefill(
    book_ponder: bool,
    opponent_brain: bool,
    engine: &mut Option<EngineBackend>,
    prefill: &mut Option<(tokio::task::JoinHandle<EngineBackend>, Arc<AtomicBool>)>,
    initial_fen: Option<&str>,
    game: &Game,
    board: &Chess,
    played: &shakmaty::Move,
) {
    if (!book_ponder && !opponent_brain) || prefill.is_some() {
        return;
    }
    let Some(eng) = engine.as_ref() else { return };
    if !eng.is_embedded() {
        return;
    }
    // opponent_brain prefills after every move; book_ponder only while still in
    // the opening book (last move a book hit). opponent_brain wins if both set.
    if !opponent_brain && eng.last_depth() != Some(0) {
        return;
    }
    let Some(moves) = prefill_moves(game, board, played) else {
        return;
    };
    // Grab an Arc clone of the cancel flag before the engine moves into the task,
    // so the loop can raise it at reclaim (clrsrc #298 a/b). The `is_embedded`
    // gate above guarantees `Some`.
    let prefill_cancel = eng
        .prefill_cancel_handle()
        .expect("embedded engine has a prefill cancel handle");
    // (a) Reset the shared flag to false at the spawn site (moved out of
    // prefill_blocking): a clean slate for this prefill, and the loop's reset
    // after the previous reclaim already left it false.
    prefill_cancel.store(false, Ordering::Relaxed);
    let fen = initial_fen.map(String::from);
    let game_ply = moves.len() as u32;
    // Backstop tightened 0.9/30s -> 0.5/10s (clrsrc #297): if any future path
    // ever loses the abort again, it costs 10s once instead of 30s per move.
    // Warmth is unaffected — real prefill slices are 0-2s of opponent time.
    let deadline = std::time::Instant::now()
        + game
            .opponent_remaining_time()
            .mul_f64(0.5)
            .min(Duration::from_secs(10));
    debug!(
        game_id = %game.id,
        game_ply,
        after_move = %moves.last().map(String::as_str).unwrap_or(""),
        kind = if opponent_brain { "opponent_brain" } else { "book_ponder" },
        "prefill started"
    );
    let mut eng = engine.take().expect("engine present for prefill checkout");
    let handle = tokio::task::spawn_blocking(move || {
        let move_refs: Vec<&str> = moves.iter().map(String::as_str).collect();
        eng.prefill_blocking(fen.as_deref(), &move_refs, deadline, game_ply);
        eng
    });
    *prefill = Some((handle, prefill_cancel));
}

/// Without the embedded backend there is no in-process engine to prefill.
#[cfg(not(feature = "embedded"))]
fn maybe_start_prefill(
    _book_ponder: bool,
    _opponent_brain: bool,
    _engine: &mut Option<EngineBackend>,
    _prefill: &mut Option<(tokio::task::JoinHandle<EngineBackend>, Arc<AtomicBool>)>,
    _initial_fen: Option<&str>,
    _game: &Game,
    _board: &Chess,
    _played: &shakmaty::Move,
) {
}

#[allow(clippy::too_many_arguments)]
async fn play_one(
    engine: &mut EngineBackend,
    board: &Chess,
    initial_fen: Option<&str>,
    game: &Game,
    li: &Lichess,
    config: &Config,
    draw_resign: &mut DrawResignTracker,
    rng: &mut StdRng,
    setup_timer: &Timer,
    move_overhead: Duration,
    can_ponder: bool,
    is_correspondence: bool,
    correspondence_move_time: Duration,
    min_time: Duration,
    pending_delivery: &mut Option<tokio::task::JoinHandle<()>>,
) -> Result<Option<shakmaty::Move>> {
    let history: Vec<&str> = game
        .state
        .moves
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .collect();
    let decision = play_move(
        engine,
        board,
        initial_fen,
        &history,
        game,
        li,
        setup_timer,
        move_overhead,
        can_ponder,
        is_correspondence,
        correspondence_move_time,
        &config.engine,
        min_time,
        draw_resign,
        rng,
        pending_delivery,
    )
    .await
    .with_context(|| format!("play_move failed for game {}", game.id))?;
    let since_search_ms = engine
        .last_search_returned()
        .map(|t| t.elapsed().as_millis() as u64)
        .unwrap_or(0);
    debug!(game_id = %game.id, mv = %decision.uci_standard(), source = ?decision.source, since_search_ms, "move sent");
    // `play_move` resigns instead of delivering a move under exactly this
    // condition; no move was played then, so there is nothing to prefill.
    if decision.resigned && history.len() >= 2 {
        return Ok(None);
    }
    Ok(Some(decision.mv))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `shakmaty::Chess` position from `game.initial_fen` plus the
/// UCI move history in `game.state.moves`.
pub fn setup_board(game: &Game, castling_mode: CastlingMode) -> Result<Chess> {
    let mut board: Chess = match game.initial_fen.as_deref() {
        Some(fen) if !fen.is_empty() && fen != "startpos" => Fen::from_ascii(fen.as_bytes())
            .with_context(|| format!("parsing initial_fen `{fen}`"))?
            .into_position(castling_mode)
            .with_context(|| format!("position from `{fen}`"))?,
        _ => Chess::default(),
    };
    if let Some(moves) = game.state.moves.as_deref() {
        for uci in moves.split_whitespace() {
            let uci_move = UciMove::from_ascii(uci.as_bytes())
                .with_context(|| format!("parsing uci `{uci}`"))?;
            let mv = uci_move
                .to_move(&board)
                .with_context(|| format!("illegal uci `{uci}` in game {}", game.id))?;
            board.play_unchecked(&mv);
        }
    }
    Ok(board)
}

/// After a finished game, append the WDL outcome of the bot's own opening
/// moves to the JBK2 experience overlay (see [`crate::exp_overlay`]). No-op
/// when harvesting is disabled, the game wasn't actually decided
/// (abort / no-start), or there are no own moves to record. clrsrc reads
/// only `clrsrc.exp` at runtime, so this append is contention-free.
fn harvest_experience_overlay(
    game: &Game,
    castling_mode: CastlingMode,
    engine: &EngineBackend,
    cfg: &crate::config::ExperienceConfig,
) {
    if !cfg.harvest_enabled || cfg.overlay_path.is_empty() {
        return;
    }
    let Some(wdl) = game_wdl_for_harvest(game) else {
        return;
    };

    let commentary = |ply: usize| {
        engine
            .commentary_for_half_move(ply)
            .map(|c| (c.score.and_then(|s| s.cp), c.depth))
    };
    let entries = match build_harvest_entries(game, castling_mode, wdl, cfg.harvest_depth, commentary)
    {
        Ok(entries) => entries,
        Err(e) => {
            warn!(game_id = %game.id, "experience harvest skipped: {e:#}");
            return;
        }
    };
    if entries.is_empty() {
        return;
    }
    match crate::exp_overlay::append_entries(&cfg.overlay_path, &entries) {
        Ok(()) => info!(
            game_id = %game.id,
            count = entries.len(),
            result = ?wdl,
            overlay = %cfg.overlay_path,
            "harvested WDL entries to experience overlay"
        ),
        Err(e) => warn!(game_id = %game.id, "writing experience overlay failed: {e}"),
    }
}

/// The game's WDL outcome from the bot's point of view, or `None` when the
/// game has no meaningful result to harvest (still running, aborted, or a
/// no-start). Decisive games with `winner` set map to Win/Loss; finished
/// games without a winner (draw/stalemate) map to Draw.
fn game_wdl_for_harvest(game: &Game) -> Option<GameWdl> {
    let status = game.state.status.as_deref().unwrap_or("");
    if !matches!(
        status,
        "mate" | "resign" | "stalemate" | "timeout" | "outoftime" | "draw" | "variantEnd"
    ) {
        return None;
    }
    Some(match game.state.winner.as_deref() {
        Some(c) if c == game.my_color => GameWdl::Win,
        Some(_) => GameWdl::Loss,
        None => GameWdl::Draw,
    })
}

/// Replay the game from its initial position and build a [`Jbk2Entry`] for
/// each of the bot's own moves (up to `max_own_moves`). The key is the
/// Polyglot hash of the position **before** the move; score/depth come
/// from the engine commentary recorded for that ply, falling back to the
/// "unset" sentinels for book / tablebase moves.
fn build_harvest_entries(
    game: &Game,
    castling_mode: CastlingMode,
    wdl: GameWdl,
    max_own_moves: usize,
    commentary: impl Fn(usize) -> Option<(Option<i64>, Option<u32>)>,
) -> Result<Vec<Jbk2Entry>> {
    // Start from the initial position — NOT `setup_board`, which would have
    // already replayed the whole game.
    let mut pos: Chess = match game.initial_fen.as_deref() {
        Some(fen) if !fen.is_empty() && fen != "startpos" => Fen::from_ascii(fen.as_bytes())
            .with_context(|| format!("parsing initial_fen `{fen}`"))?
            .into_position(castling_mode)
            .with_context(|| format!("position from `{fen}`"))?,
        _ => Chess::default(),
    };
    let moves_str = game.state.moves.as_deref().unwrap_or("");
    let mut entries = Vec::new();
    let mut own_moves = 0usize;
    for (ply, uci) in moves_str.split_whitespace().enumerate() {
        if own_moves >= max_own_moves {
            break;
        }
        let uci_move =
            UciMove::from_ascii(uci.as_bytes()).with_context(|| format!("parsing uci `{uci}`"))?;
        let mv = uci_move
            .to_move(&pos)
            .with_context(|| format!("illegal uci `{uci}` during harvest of game {}", game.id))?;
        let is_our_move = (pos.turn() == Color::White) == game.is_white;
        if is_our_move {
            if let Some(packed) = crate::polyglot::encode_move(&mv) {
                let key = crate::polyglot::polyglot_hash(&pos);
                let (score_cp, depth) = commentary(ply).unwrap_or((None, None));
                entries.push(Jbk2Entry::selfplay(key, packed, score_cp, depth, wdl));
            }
            own_moves += 1;
        }
        pos.play_unchecked(&mv);
    }
    Ok(entries)
}

/// Whether the bot is the side to move *and* the position has changed
/// since we last looked. Mirrors Python's `is_engine_move`.
pub fn is_engine_move(game: &Game, prior: Option<&Game>, board: &Chess) -> bool {
    let changed = match prior {
        None => true,
        Some(p) => game_changed(game, p),
    };
    let bot_turn = (board.turn() == Color::White) == game.is_white;
    changed && bot_turn && !is_game_over(&game.state)
}

fn game_changed(curr: &Game, prior: &Game) -> bool {
    let cm = curr.state.moves.as_deref().unwrap_or("");
    let pm = prior.state.moves.as_deref().unwrap_or("");
    cm.len() != pm.len() || curr.state.status != prior.state.status
}

fn is_game_over(state: &GameStateType) -> bool {
    matches!(
        state.status.as_deref(),
        Some(s) if s != "created" && s != "started"
    )
}

/// Python parity (`lichess_bot.py:757`): the terminate deadline is the
/// remaining clock + increment of the side to move plus a 60 s grace
/// window. Lets us bail out of a stuck stream once the opponent has
/// burnt all their time *and* a minute extra.
fn compute_terminate_in(game: &Game, board: &Chess) -> Duration {
    let is_white_to_move = board.turn() == Color::White;
    let time_ms = if is_white_to_move { game.state.wtime } else { game.state.btime }.unwrap_or(0);
    let inc_ms = if is_white_to_move { game.state.winc } else { game.state.binc }.unwrap_or(0);
    let time = Duration::from_millis(time_ms.max(0) as u64);
    let inc = Duration::from_millis(inc_ms.max(0) as u64);
    time + inc + Duration::from_secs(60)
}

fn state_from_event(evt: &GameEventType) -> GameStateType {
    GameStateType {
        kind: Some("gameState".into()),
        moves: evt.moves.clone(),
        wtime: evt.wtime,
        btime: evt.btime,
        winc: evt.winc,
        binc: evt.binc,
        wdraw: evt.wdraw,
        bdraw: evt.bdraw,
        status: evt.status.clone(),
        winner: evt.winner.clone(),
        wtakeback: evt.wtakeback,
        btakeback: evt.btakeback,
        expiration: evt.expiration.clone(),
    }
}

/// First-move abort deadline. Lichess reports the authoritative window in
/// `gameState.expiration` (`millisToMove` minus already-elapsed `idleMillis`);
/// we fall back to the configured `abort_time` when the field is absent
/// (e.g. the game is no longer abortable, or an older API response).
fn abort_deadline(game: &Game, fallback: Duration) -> Duration {
    game.state
        .expiration
        .as_ref()
        .and_then(|e| {
            e.millis_to_move
                .map(|to_move| (to_move - e.idle_millis.unwrap_or(0)).max(0) as u64)
        })
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

/// Push any UCI moves that are new in `new_state` (vs. the move list
/// already reflected in `board`).
fn apply_new_moves(old: &GameStateType, new: &GameStateType, board: &mut Chess) -> Result<()> {
    let old_count = old
        .moves
        .as_deref()
        .map(|s| s.split_whitespace().count())
        .unwrap_or(0);
    let new_moves_str = new.moves.as_deref().unwrap_or("");
    let new_moves: Vec<&str> = new_moves_str.split_whitespace().collect();
    if new_moves.len() <= old_count {
        return Ok(());
    }
    for uci in &new_moves[old_count..] {
        let uci_move = UciMove::from_ascii(uci.as_bytes())
            .with_context(|| format!("parsing new uci `{uci}`"))?;
        let mv = uci_move
            .to_move(board)
            .with_context(|| format!("illegal new uci `{uci}`"))?;
        board.play_unchecked(&mv);
    }
    Ok(())
}

/// Pick and start the engine backend for `game`. Returns the subprocess UCI
/// client described in `engine.dir`/`engine.name` by default; with
/// `--features embedded` and `engine.embedded: true` it returns the in-process
/// clrsrc backend instead, but only for standard chess (clrsrc's FEN parser has
/// no Chess960 castling) — every other variant falls back to the subprocess.
#[cfg_attr(not(feature = "embedded"), allow(unused_variables))]
async fn spawn_engine_for(cfg: &EngineConfig, game: &Game) -> Result<EngineBackend> {
    let uci_options: HashMap<String, String> = cfg
        .uci_options
        .iter()
        .map(|(k, v)| (k.clone(), json_value_to_string(v)))
        .collect();

    #[cfg(feature = "embedded")]
    if cfg.embedded {
        if game.variant_key == "standard" {
            // Rating-Band-Repertoire (clrsrc 373e127): per-game `OppBand` only for the
            // in-process engine; the subprocess path keeps the config options untouched.
            let mut uci_options = uci_options.clone();
            uci_options.insert(
                "OppBand".to_string(),
                crate::embedded_engine::opp_band_for(game.opponent.rating, game.me.rating).to_string(),
            );
            let engine = crate::embedded_engine::EmbeddedEngine::new(cfg, &uci_options)
                .map_err(|e| anyhow!("embedded engine init failed: {e}"))?;
            info!(game_id = %game.id, "using in-process embedded clrsrc backend");
            return Ok(EngineBackend::Embedded(Box::new(engine)));
        }
        info!(
            game_id = %game.id, variant = %game.variant_key,
            "embedded backend requested but variant is not standard; falling back to subprocess"
        );
    }

    let binary = PathBuf::from(&cfg.dir).join(&cfg.name);
    let binary_str = binary.to_string_lossy().to_string();
    let mut extra_args: Vec<String> = Vec::new();
    if let Some(opts) = &cfg.engine_options {
        for (k, v) in opts {
            extra_args.push(match v {
                JsonValue::Null => format!("--{k}"),
                _ => format!("--{k}={}", json_value_to_string(v)),
            });
        }
    }
    let cwd_opt = if cfg.working_dir.is_empty() {
        None
    } else {
        Some(cfg.working_dir.as_str())
    };
    let client =
        UciClient::spawn(&binary_str, &extra_args, cwd_opt, &uci_options, cfg.silence_stderr)
            .await
            .map_err(|e| anyhow!("UCI spawn failed: {e}"))?;
    Ok(EngineBackend::Subprocess(client))
}

fn json_value_to_string(v: &JsonValue) -> String {
    match v {
        JsonValue::Null => String::new(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lichess_types::{GameEventType, PlayerType, VariantInfo};

    fn fixture_game(initial_fen: Option<&str>, moves: Option<&str>, is_white: bool) -> Game {
        let mut info = GameEventType::default();
        info.id = Some("abcd1234".into());
        info.initial_fen = initial_fen.map(String::from);
        let mut state = GameStateType::default();
        state.moves = moves.map(String::from);
        info.state = Some(state);
        let mut white = PlayerType::default();
        white.name = Some("us".into());
        let mut black = PlayerType::default();
        black.name = Some("them".into());
        if is_white {
            info.white = Some(white);
            info.black = Some(black);
        } else {
            info.white = Some(black);
            info.black = Some(white);
        }
        let mut variant = VariantInfo::default();
        variant.key = Some("standard".into());
        info.variant = Some(variant);
        Game::new(&info, "us", "https://lichess.org/", Duration::ZERO)
    }

    #[test]
    fn setup_board_returns_start_position_when_no_fen_no_moves() {
        let game = fixture_game(None, None, true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        assert_eq!(board.turn(), Color::White);
        assert_eq!(board.board().occupied().count(), 32);
    }

    #[test]
    fn setup_board_applies_move_history() {
        let game = fixture_game(None, Some("e2e4 e7e5 g1f3"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        assert_eq!(board.turn(), Color::Black);
        // White knight on f3 now → 32 pieces still (no captures).
        assert_eq!(board.board().occupied().count(), 32);
    }

    #[test]
    fn setup_board_uses_provided_initial_fen() {
        // King + Pawn vs King — 3 pieces total.
        let fen = "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1";
        let game = fixture_game(Some(fen), None, true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        assert_eq!(board.board().occupied().count(), 3);
    }

    fn finished_game(
        moves: &str,
        is_white: bool,
        status: &str,
        winner: Option<&str>,
    ) -> Game {
        let mut game = fixture_game(None, Some(moves), is_white);
        game.state.status = Some(status.into());
        game.state.winner = winner.map(String::from);
        game
    }

    #[test]
    fn harvest_collects_white_bots_own_moves_in_order() {
        // 5 plies played; as White our moves are plies 0, 2, 4.
        let game = finished_game("e2e4 e7e5 g1f3 b8c6 f1b5", true, "resign", Some("white"));
        let entries =
            build_harvest_entries(&game, CastlingMode::Standard, GameWdl::Win, 16, |_| None)
                .unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.wdl == GameWdl::Win));
        // First entry's key is the start position's Polyglot hash.
        assert_eq!(entries[0].key, 0x463B_9618_1691_FC9C);
        // No commentary → unset score, zero depth.
        assert_eq!(entries[0].score, crate::exp_overlay::UNSET_SCORE);
        assert_eq!(entries[0].depth, 0);
    }

    #[test]
    fn harvest_collects_black_bots_own_moves_at_odd_plies() {
        // As Black our moves are plies 1, 3.
        let game = finished_game("e2e4 e7e5 g1f3 b8c6", false, "mate", Some("white"));
        let entries =
            build_harvest_entries(&game, CastlingMode::Standard, GameWdl::Loss, 16, |_| None)
                .unwrap();
        assert_eq!(entries.len(), 2);
        // Position before our first move (after 1.e4) — not the start hash.
        assert_ne!(entries[0].key, 0x463B_9618_1691_FC9C);
        assert!(entries.iter().all(|e| e.wdl == GameWdl::Loss));
    }

    #[test]
    fn harvest_respects_max_own_moves_limit() {
        let game = finished_game("e2e4 e7e5 g1f3 b8c6 f1b5", true, "resign", Some("white"));
        let entries =
            build_harvest_entries(&game, CastlingMode::Standard, GameWdl::Win, 1, |_| None).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn harvest_threads_commentary_score_and_depth() {
        let game = finished_game("e2e4 e7e5 g1f3", true, "resign", Some("white"));
        // Ply 0 has a score+depth; ply 2 has none (e.g. book move).
        let entries = build_harvest_entries(
            &game,
            CastlingMode::Standard,
            GameWdl::Win,
            16,
            |ply| if ply == 0 { Some((Some(37), Some(20))) } else { None },
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].score, 37);
        assert_eq!(entries[0].depth, 20);
        assert_eq!(entries[1].score, crate::exp_overlay::UNSET_SCORE);
        assert_eq!(entries[1].depth, 0);
    }

    #[test]
    fn harvest_book_replay_ply_is_written_without_engine_verdict() {
        // A clrsrc-internal book hit reaches the harvest as commentary
        // (score=None, depth=Some(0)) since score_from_outcome (embedded_engine.rs)
        // maps depth=0/nodes=0 to None. The overlay entry must then look exactly
        // like a Polyglot book move: source=SELFPLAY only (byte 16 == 0x08),
        // clrsrc_score unset (bytes 28..30 == 00 80), count/WDL still counted.
        let game = finished_game("e2e4 e7e5 g1f3", true, "resign", Some("white"));
        let entries = build_harvest_entries(
            &game,
            CastlingMode::Standard,
            GameWdl::Win,
            16,
            |ply| if ply == 0 { Some((None, Some(0))) } else { Some((Some(-188), Some(24))) },
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        let book = entries[0].to_bytes();
        assert_eq!(book[16], crate::exp_overlay::SOURCE_SELFPLAY, "book replay: no ENGINE bit");
        assert_eq!(&book[28..30], &[0x00, 0x80], "book replay: clrsrc_score unset");
        assert_eq!(&book[10..12], &[0x00, 0x80], "book replay: score unset");
        assert_eq!(&book[12..14], &0i16.to_le_bytes());
        assert_eq!(&book[14..16], &1u16.to_le_bytes(), "count still accrues");
        assert_eq!(&book[18..20], &1u16.to_le_bytes(), "wdl_w still accrues");
        let searched = entries[1].to_bytes();
        assert_eq!(searched[16], 0x28, "searched move keeps SELFPLAY|ENGINE");
        assert_eq!(&searched[28..30], &(-188i16).to_le_bytes());
    }

    #[test]
    fn game_wdl_maps_winner_status_and_skips_aborts() {
        // White bot.
        let win = finished_game("e2e4", true, "resign", Some("white"));
        assert_eq!(game_wdl_for_harvest(&win), Some(GameWdl::Win));
        let loss = finished_game("e2e4", true, "mate", Some("black"));
        assert_eq!(game_wdl_for_harvest(&loss), Some(GameWdl::Loss));
        let draw = finished_game("e2e4", true, "stalemate", None);
        assert_eq!(game_wdl_for_harvest(&draw), Some(GameWdl::Draw));
        // Aborted / non-terminal → nothing to harvest.
        let aborted = finished_game("e2e4", true, "aborted", None);
        assert_eq!(game_wdl_for_harvest(&aborted), None);
        let running = finished_game("e2e4", true, "started", None);
        assert_eq!(game_wdl_for_harvest(&running), None);
    }

    #[test]
    fn is_engine_move_true_when_bot_white_and_white_to_move() {
        let game = fixture_game(None, None, true);
        let board = Chess::default();
        assert!(is_engine_move(&game, None, &board));
    }

    #[test]
    fn is_engine_move_false_when_bot_black_and_white_to_move() {
        let game = fixture_game(None, None, false);
        let board = Chess::default();
        assert!(!is_engine_move(&game, None, &board));
    }

    #[test]
    fn is_engine_move_false_when_game_unchanged() {
        let game = fixture_game(None, Some("e2e4"), true);
        let prior = game.clone();
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        // Game state identical → "no change" → don't replay.
        assert!(!is_engine_move(&game, Some(&prior), &board));
    }

    fn game_state_event(moves: &str, status: &str) -> GameEventType {
        let mut evt = GameEventType::default();
        evt.kind = Some("gameState".into());
        evt.moves = Some(moves.into());
        evt.status = Some(status.into());
        evt
    }

    fn event_of_kind(kind: &str) -> GameEventType {
        let mut evt = GameEventType::default();
        evt.kind = Some(kind.into());
        evt
    }

    #[test]
    fn event_needs_engine_skips_own_move_echo_and_opponent_turn_flips() {
        // White bot, opponent just played e7e5, we answered g1f3: the stream
        // has not echoed our move yet, so game state and board end at e7e5.
        let game = fixture_game(None, Some("e2e4 e7e5"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        // Echo of our own move: opponent to move afterwards.
        let echo = game_state_event("e2e4 e7e5 g1f3", "started");
        assert!(!event_needs_engine(&echo, &game, &board));

        // After the echo was applied, a draw-offer flip repeats the move list.
        let game = fixture_game(None, Some("e2e4 e7e5 g1f3"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let flip = game_state_event("e2e4 e7e5 g1f3", "started");
        assert!(!event_needs_engine(&flip, &game, &board));
        assert!(!event_needs_engine(&event_of_kind("opponentGone"), &game, &board));
        assert!(!event_needs_engine(&event_of_kind("somethingNew"), &game, &board));
    }

    #[test]
    fn event_needs_engine_reclaims_for_our_turn_game_end_chat_and_resync() {
        let game = fixture_game(None, Some("e2e4 e7e5 g1f3"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        // Opponent replied: we are on move.
        let reply = game_state_event("e2e4 e7e5 g1f3 b8c6", "started");
        assert!(event_needs_engine(&reply, &game, &board));
        // Echo and reply arriving as one state while the board still lacks both.
        let behind = fixture_game(None, Some("e2e4 e7e5"), true);
        let behind_board = setup_board(&behind, CastlingMode::Standard).unwrap();
        assert!(event_needs_engine(&reply, &behind, &behind_board));
        // Game over on the opponent's turn (resignation, flag, abort).
        let over = game_state_event("e2e4 e7e5 g1f3", "resign");
        assert!(event_needs_engine(&over, &game, &board));
        // A move list that does not fit the board goes down the normal path.
        let broken = game_state_event("e2e4 e7e5 g1f3 e2e4", "started");
        assert!(event_needs_engine(&broken, &game, &board));
        assert!(event_needs_engine(&event_of_kind("chatLine"), &game, &board));
        assert!(event_needs_engine(&event_of_kind("gameFull"), &game, &board));
    }

    #[test]
    fn event_needs_engine_matches_black_bot_turns() {
        let game = fixture_game(None, Some("e2e4"), false);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let echo = game_state_event("e2e4 e7e5", "started");
        assert!(!event_needs_engine(&echo, &game, &board));
        let game = fixture_game(None, Some("e2e4 e7e5"), false);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let reply = game_state_event("e2e4 e7e5 g1f3", "started");
        assert!(event_needs_engine(&reply, &game, &board));
    }

    fn uci_to_move(board: &Chess, uci: &str) -> shakmaty::Move {
        UciMove::from_ascii(uci.as_bytes()).unwrap().to_move(board).unwrap()
    }

    #[test]
    fn prefill_moves_appends_the_move_we_just_played() {
        let game = fixture_game(None, Some("e2e4 e7e5"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let played = uci_to_move(&board, "g1f3");
        let moves = prefill_moves(&game, &board, &played).unwrap();
        assert_eq!(moves, ["e2e4", "e7e5", "g1f3"]);

        // First move of the game as White: no moves in the state yet.
        let game = fixture_game(None, None, true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let played = uci_to_move(&board, "e2e4");
        assert_eq!(prefill_moves(&game, &board, &played).unwrap(), ["e2e4"]);
    }

    #[test]
    fn prefill_moves_writes_castling_as_king_target_square() {
        let game = fixture_game(None, Some("e2e4 e7e5 g1f3 b8c6 f1c4 f8c5"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let played = uci_to_move(&board, "e1g1");
        let moves = prefill_moves(&game, &board, &played).unwrap();
        assert_eq!(moves.last().map(String::as_str), Some("e1g1"));
    }

    #[test]
    fn prefill_moves_is_none_when_our_move_ends_the_game() {
        // Scholar's mate: Qxf7# leaves Black without a move to search.
        let game = fixture_game(None, Some("e2e4 e7e5 d1h5 b8c6 f1c4 g8f6"), true);
        let board = setup_board(&game, CastlingMode::Standard).unwrap();
        let played = uci_to_move(&board, "h5f7");
        assert!(prefill_moves(&game, &board, &played).is_none());
    }

    #[test]
    fn is_game_over_detects_terminal_status() {
        let mut s = GameStateType::default();
        s.status = Some("started".into());
        assert!(!is_game_over(&s));
        s.status = Some("mate".into());
        assert!(is_game_over(&s));
        s.status = Some("resign".into());
        assert!(is_game_over(&s));
    }

    #[test]
    fn game_end_only_treats_lichess_aborted_as_not_a_game() {
        // Only the literal `aborted` status returns the tally point and skips
        // the post-game cooldown; every real result — and a task that left
        // the loop with the game still running — counts as played.
        let mut s = GameStateType::default();
        s.status = Some("aborted".into());
        assert_eq!(game_end_for(&s), GameEnd::Aborted);
        for played in ["mate", "resign", "draw", "outoftime", "stalemate", "started", "created"] {
            s.status = Some(played.into());
            assert_eq!(game_end_for(&s), GameEnd::Played, "{played}");
        }
        s.status = None;
        assert_eq!(game_end_for(&s), GameEnd::Played);
    }

    #[test]
    fn apply_new_moves_pushes_only_new_uci_tokens() {
        let old = GameStateType { moves: Some("e2e4 e7e5".into()), ..Default::default() };
        let new = GameStateType { moves: Some("e2e4 e7e5 g1f3 b8c6".into()), ..Default::default() };
        let game = fixture_game(None, Some("e2e4 e7e5"), true);
        let mut board = setup_board(&game, CastlingMode::Standard).unwrap();
        apply_new_moves(&old, &new, &mut board).unwrap();
        assert_eq!(board.turn(), Color::White);
        // 32 pieces still on the board (no captures so far).
        assert_eq!(board.board().occupied().count(), 32);
    }

    #[test]
    fn concurrency_allows_basic_thresholds() {
        assert!(concurrency_allows(0, 1));
        assert!(!concurrency_allows(1, 1));
        assert!(concurrency_allows(1, 2));
        assert!(!concurrency_allows(2, 2));
        // concurrency=0 means "accept nothing"
        assert!(!concurrency_allows(0, 0));
        assert!(!concurrency_allows(5, 0));
    }

    #[test]
    fn second_game_guard_aborts_only_new_extra_casual_games() {
        // Vorfall 30.09 (trvX6Pww/9iBcixas): concurrency 1, one game running,
        // a late-accepted outbound challenge starts a second one → abort.
        assert!(second_game_must_abort(1, 1, false, false));
        assert!(second_game_must_abort(2, 2, false, false));
        // Nothing running / still below the limit → play.
        assert!(!second_game_must_abort(0, 1, false, false));
        assert!(!second_game_must_abort(1, 2, false, false));
        // Tournament pairings are never aborted.
        assert!(!second_game_must_abort(1, 1, true, false));
        // Startup pickups (already in active_games) are running, not new.
        assert!(!second_game_must_abort(1, 1, false, true));
        assert!(!second_game_must_abort(2, 1, false, true));
        // concurrency 0 accepts nothing, so never abort what does start.
        assert!(!second_game_must_abort(1, 0, false, false));
    }

    #[test]
    fn pending_accepts_removal_is_keyed_not_positional() {
        // Regression for the unkeyed-Vec::pop() race (merged_bug_002): a
        // gameStart for a game that never reserved a pending-accept slot
        // (e.g. an outbound matchmaking start) must not release someone
        // else's in-flight incoming-accept reservation.
        let mut state = BotState::new(OnlineBlocklist::default());
        state
            .pending_accepts
            .insert("challengeA".to_string(), Instant::now());
        state
            .pending_accepts
            .insert("challengeC".to_string(), Instant::now());
        assert_eq!(state.effective_active_games(), 2);

        // gameStart(B) arrives for an outbound/matchmaking game that never
        // reserved a slot — must be a no-op, not drop an unrelated entry.
        state.pending_accepts.remove("gameB");
        assert_eq!(state.effective_active_games(), 2);

        // gameStart(A) arrives — releases exactly A's reservation.
        state.pending_accepts.remove("challengeA");
        assert_eq!(state.effective_active_games(), 1);
        assert!(state.pending_accepts.contains_key("challengeC"));
    }

    #[test]
    fn incoming_bot_budget_thresholds() {
        // No reservation: incoming bots accepted right up to the limit, then
        // turned away.
        assert!(!over_incoming_bot_budget(0, 100, 0));
        assert!(!over_incoming_bot_budget(99, 100, 0));
        assert!(over_incoming_bot_budget(100, 100, 0));
        assert!(over_incoming_bot_budget(101, 100, 0));

        // Reserve 30 for matchmaking: incoming bots stop at 70, leaving 30
        // headroom for our own seeks (which run up to the full 100).
        assert!(!over_incoming_bot_budget(69, 100, 30));
        assert!(over_incoming_bot_budget(70, 100, 30));

        // reserved >= limit saturates to a zero threshold (decline all
        // incoming bots) rather than underflowing.
        assert!(over_incoming_bot_budget(0, 100, 100));
        assert!(over_incoming_bot_budget(0, 100, 250));
    }

    #[test]
    fn json_value_to_string_renders_primitives() {
        assert_eq!(json_value_to_string(&JsonValue::Null), "");
        assert_eq!(json_value_to_string(&JsonValue::Bool(true)), "true");
        assert_eq!(
            json_value_to_string(&JsonValue::String("Hash".into())),
            "Hash"
        );
        let n: JsonValue = serde_json::from_str("1024").unwrap();
        assert_eq!(json_value_to_string(&n), "1024");
    }

    #[test]
    fn render_greeting_substitutes_me_and_opponent() {
        let game = fixture_game(None, None, true);
        assert_eq!(
            render_greeting("Hi {opponent}, I'm {me}!", &game),
            "Hi them, I'm us!"
        );
    }

    #[test]
    fn render_greeting_leaves_unknown_braces_literal() {
        let game = fixture_game(None, None, true);
        assert_eq!(render_greeting("foo {bar}", &game), "foo {bar}");
        assert_eq!(render_greeting("", &game), "");
    }

    #[test]
    fn sanitize_filename_strips_illegal_chars() {
        assert_eq!(sanitize_filename("a<b>c:d\"e/f\\g|h?i*j"), "abcdefghij");
        assert_eq!(sanitize_filename("clean name.pgn"), "clean name.pgn");
    }

    #[test]
    fn pgn_target_path_picks_grouping_correctly() {
        let dir = Path::new("/tmp/pgn");
        // "game" → one file per game, sanitized.
        let p = pgn_target_path(dir, "game", "abcd", "We", "Them", "We", true);
        assert_eq!(p, Path::new("/tmp/pgn/We vs Them - abcd.pgn"));
        // "opponent" + complete → shared file per opponent. When user
        // played black, opponent is white.
        let p = pgn_target_path(dir, "opponent", "abcd", "Other", "Me", "Me", true);
        assert_eq!(p, Path::new("/tmp/pgn/Me games vs. Other.pgn"));
        // "opponent" + still running → falls back to single file.
        let p = pgn_target_path(dir, "opponent", "abcd", "Other", "Me", "Me", false);
        assert_eq!(p, Path::new("/tmp/pgn/Other vs Me - abcd.pgn"));
        // "all" → one file for all our games.
        let p = pgn_target_path(dir, "all", "abcd", "Other", "Me", "Me", true);
        assert_eq!(p, Path::new("/tmp/pgn/Me games.pgn"));
    }

    #[test]
    fn move_count_handles_none_empty_and_populated() {
        let mut game = fixture_game(None, None, true);
        assert_eq!(move_count(&game), 0);
        game.state.moves = Some("".into());
        assert_eq!(move_count(&game), 0);
        game.state.moves = Some("e2e4".into());
        assert_eq!(move_count(&game), 1);
        game.state.moves = Some("e2e4 e7e5 g1f3".into());
        assert_eq!(move_count(&game), 3);
    }

    // -----------------------------------------------------------------------
    // Tournament outbound-matchmaking gate
    // -----------------------------------------------------------------------

    #[test]
    fn is_tournament_source_recognizes_arena_and_swiss() {
        assert!(is_tournament_source(Some("arena")));
        assert!(is_tournament_source(Some("swiss")));
        assert!(!is_tournament_source(Some("lobby")));
        assert!(!is_tournament_source(Some("friend")));
        assert!(!is_tournament_source(Some("pool")));
        assert!(!is_tournament_source(None));
    }

    #[test]
    fn tournament_gate_open_when_no_tournament_game() {
        let mut s = BotState::new(OnlineBlocklist::default());
        assert!(!s.has_active_tournament_game());
        // Non-tournament game should not flip the gate.
        s.active_games.insert(
            "g1".into(),
            ActiveGame { opponent: "Alice".into(), is_tournament: false },
        );
        assert!(!s.has_active_tournament_game());
    }

    #[test]
    fn tournament_gate_blocks_during_active_arena_game() {
        let mut s = BotState::new(OnlineBlocklist::default());
        s.active_games.insert(
            "g1".into(),
            ActiveGame { opponent: "Alice".into(), is_tournament: true },
        );
        s.active_games.insert(
            "g2".into(),
            ActiveGame { opponent: "Bob".into(), is_tournament: false },
        );
        assert!(s.has_active_tournament_game());
    }

    #[test]
    fn tournament_pairing_lag_expires_after_window() {
        let now = Instant::now();
        let within = now
            .checked_sub(TOURNAMENT_PAIRING_LAG / 2)
            .expect("clock must support recent past");
        assert!(within.elapsed() < TOURNAMENT_PAIRING_LAG);
        let past = now
            .checked_sub(TOURNAMENT_PAIRING_LAG * 2)
            .expect("clock must support recent past");
        assert!(past.elapsed() >= TOURNAMENT_PAIRING_LAG);
    }

    // -----------------------------------------------------------------------
    // Integration tests for greeting/goodbye plumbing through Conversation
    // -----------------------------------------------------------------------

    mod greeting_integration {
        use super::*;
        use crate::config::GreetingConfig;
        use crate::conversation::Conversation;
        use crate::lichess::Lichess;
        use serde_json::json;
        use std::sync::{Arc, Mutex as StdMutex};
        use url::Url;
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn make_lichess(server: &MockServer) -> Lichess {
            Mock::given(method("POST"))
                .and(path("/api/token/test"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "test-token": { "scopes": "bot:play", "userId": "us" }
                })))
                .mount(server)
                .await;
            let url = Url::parse(&server.uri()).unwrap();
            Lichess::connect("test-token".into(), url, "0.1.0".into(), 3)
                .await
                .expect("token mock should accept")
        }

        #[tokio::test]
        async fn maybe_send_greeting_sends_once_in_opening() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("POST"))
                .and(path("/api/bot/game/abcd1234/chat"))
                .and(body_string_contains("room=player"))
                .and(body_string_contains("text=Hi+them"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/api/bot/game/abcd1234/chat"))
                .and(body_string_contains("room=spectator"))
                .and(body_string_contains("text=Watch+us+beat+them"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;

            let game = fixture_game(None, None, true);
            let conv =
                Conversation::new(
                    game.clone(),
                    li,
                    "0.1.0",
                    Arc::new(StdMutex::new(Vec::new())),
                    UserProfileType::default(),
                    None,
                );
            let cfg = GreetingConfig {
                hello: "Hi {opponent}, GLHF".into(),
                hello_spectators: "Watch {me} beat {opponent}".into(),
                goodbye: String::new(),
                goodbye_spectators: String::new(),
            };
            let mut greeted = false;
            maybe_send_greeting(&conv, &game, &cfg, &mut greeted).await;
            assert!(greeted, "greeted flag must flip after first hello");
            // Second call must NOT hit the API again — the `.expect(1)` mocks
            // would panic on Drop if they saw a second request.
            maybe_send_greeting(&conv, &game, &cfg, &mut greeted).await;
        }

        #[tokio::test]
        async fn maybe_send_greeting_skips_when_two_plies_already_played() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            // No chat mock — any POST to /chat would fail the test.

            let game = fixture_game(None, Some("e2e4 e7e5"), true);
            let conv =
                Conversation::new(
                    game.clone(),
                    li,
                    "0.1.0",
                    Arc::new(StdMutex::new(Vec::new())),
                    UserProfileType::default(),
                    None,
                );
            let cfg = GreetingConfig {
                hello: "Hi".into(),
                hello_spectators: "Hi".into(),
                ..Default::default()
            };
            let mut greeted = false;
            maybe_send_greeting(&conv, &game, &cfg, &mut greeted).await;
            assert!(!greeted, "must not flip greeted after the opening window");
        }

        #[tokio::test]
        async fn send_goodbye_drops_empty_templates() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            // No chat mock at all — Conversation::send_message returns Ok(())
            // without hitting the network for empty strings.

            let game = fixture_game(None, Some("e2e4 e7e5"), true);
            let conv =
                Conversation::new(
                    game.clone(),
                    li,
                    "0.1.0",
                    Arc::new(StdMutex::new(Vec::new())),
                    UserProfileType::default(),
                    None,
                );
            let cfg = GreetingConfig::default();
            send_goodbye(&conv, &game, &cfg).await;
        }

        #[tokio::test]
        async fn prefill_ongoing_games_separates_correspondence_pickups() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("GET"))
                .and(path("/api/account/playing"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "nowPlaying": [
                        { "gameId": "blitzabcd", "speed": "blitz",
                          "opponent": { "username": "Alice" } },
                        { "gameId": "corrxyzz",   "speed": "correspondence",
                          "opponent": { "username": "Bob"   } },
                        { "gameId": "selfgame",   "speed": "blitz",
                          "opponent": { "username": "us"    } }
                    ]
                })))
                .expect(1)
                .mount(&server)
                .await;

            let online_blocklist = OnlineBlocklist::default();
            let state = Arc::new(tokio::sync::Mutex::new(BotState::new(online_blocklist)));
            let profile = UserProfileType {
                username: Some("us".into()),
                ..Default::default()
            };
            prefill_ongoing_games(&li, &state, &profile).await;

            let s = state.lock().await;
            assert_eq!(s.active_games.len(), 3, "all three games should be tracked");
            assert!(s.active_games.contains_key("blitzabcd"));
            assert!(s.active_games.contains_key("corrxyzz"));
            assert!(s.active_games.contains_key("selfgame"));
            assert_eq!(
                s.startup_correspondence_games.iter().collect::<Vec<_>>(),
                vec![&"corrxyzz".to_string()]
            );
            // Alice and Bob each have one engagement, "us" does NOT — we
            // never count ourselves as an opponent (game vs. our 2nd account
            // would otherwise inflate the per-opponent gate).
            assert_eq!(s.opponent_engagements.get("Alice"), Some(&1));
            assert_eq!(s.opponent_engagements.get("Bob"), Some(&1));
            assert!(!s.opponent_engagements.contains_key("us"));
        }

        #[tokio::test]
        async fn prefill_ongoing_games_handles_empty_and_error() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("GET"))
                .and(path("/api/account/playing"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "nowPlaying": []
                })))
                .mount(&server)
                .await;

            let state = Arc::new(tokio::sync::Mutex::new(BotState::new(OnlineBlocklist::default())));
            let profile = UserProfileType { username: Some("us".into()), ..Default::default() };
            prefill_ongoing_games(&li, &state, &profile).await;
            let s = state.lock().await;
            assert!(s.active_games.is_empty());
            assert!(s.startup_correspondence_games.is_empty());
        }

        #[tokio::test]
        async fn send_goodbye_renders_and_sends_when_set() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("POST"))
                .and(path("/api/bot/game/abcd1234/chat"))
                .and(body_string_contains("room=player"))
                .and(body_string_contains("gg+them"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            // No spectator mock — spectator template stays empty in this test
            // and must not produce a request.

            let game = fixture_game(None, Some("e2e4 e7e5"), true);
            let conv =
                Conversation::new(
                    game.clone(),
                    li,
                    "0.1.0",
                    Arc::new(StdMutex::new(Vec::new())),
                    UserProfileType::default(),
                    None,
                );
            let cfg = GreetingConfig {
                goodbye: "gg {opponent}".into(),
                ..Default::default()
            };
            send_goodbye(&conv, &game, &cfg).await;
        }
    }

    mod pgn_integration {
        use super::*;
        use crate::config::Config;
        use crate::lichess::Lichess;
        use crate::matchmaking::Matchmaking;
        use serde_json::json;
        use url::Url;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn make_lichess(server: &MockServer) -> Lichess {
            Mock::given(method("POST"))
                .and(path("/api/token/test"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "test-token": { "scopes": "bot:play", "userId": "us" }
                })))
                .mount(server)
                .await;
            let url = Url::parse(&server.uri()).unwrap();
            Lichess::connect("test-token".into(), url, "0.1.0".into(), 3)
                .await
                .expect("token mock should accept")
        }

        /// Build a minimal `Config` for PGN tests via the regular YAML
        /// path — much shorter than handcrafting one and exercises the
        /// same parser the real bot uses. `dir` lives inside a TempDir
        /// so YAML escaping is irrelevant.
        fn config_with_pgn(dir: &Path, grouping: &str) -> Config {
            let dir_lit = dir.to_string_lossy().replace('\\', "/");
            let yaml = format!(
                "token: t\nurl: https://example.invalid/\n\
                 pgn_directory: \"{dir_lit}\"\npgn_file_grouping: {grouping}\n\
                 engine:\n  dir: ''\n  name: ''\n\
                 challenge:\n  concurrency: 1\n"
            );
            serde_yaml_ng::from_str::<Config>(&yaml).expect("test yaml parses")
        }

        fn config_without_pgn() -> Config {
            let yaml = "token: t\nurl: https://example.invalid/\n\
                        engine:\n  dir: ''\n  name: ''\n\
                        challenge:\n  concurrency: 1\n";
            serde_yaml_ng::from_str::<Config>(yaml).expect("test yaml parses")
        }

        #[tokio::test]
        async fn save_pgn_per_game_writes_one_file() {
            let tmp = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("GET"))
                .and(path("/game/export/abcd1234"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    "[Event \"Test\"]\n[Site \"lichess.org/abcd1234\"]\n\n1. e4 e5 *\n",
                ))
                .expect(1)
                .mount(&server)
                .await;

            let game = fixture_game(None, Some("e2e4 e7e5"), true);
            let cfg = config_with_pgn(tmp.path(), "game");
            save_pgn_if_configured(&li, &cfg, &game, "us").await;

            let written = tokio::fs::read_to_string(tmp.path().join("us vs them - abcd1234.pgn"))
                .await
                .expect("pgn file should exist");
            assert!(written.contains("[Event \"Test\"]"));
            assert!(written.ends_with("\n\n"));
        }

        #[tokio::test]
        async fn save_pgn_opponent_grouping_appends_and_removes_single_file() {
            let tmp = tempfile::tempdir().unwrap();
            // Pretend a previous in-progress write left the single-game file
            // behind. After the game finishes with "opponent" grouping, we
            // append to the shared file and delete the single-game file.
            let single_path = tmp.path().join("us vs them - abcd1234.pgn");
            tokio::fs::write(&single_path, "stale single-game stub\n\n").await.unwrap();

            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("GET"))
                .and(path("/game/export/abcd1234"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    "[Event \"x\"]\n\n1. e4 e5 *\n",
                ))
                .mount(&server)
                .await;

            let game = fixture_game(None, Some("e2e4 e7e5"), true);
            let cfg = config_with_pgn(tmp.path(), "opponent");
            // Pre-populate the shared opponent file so we can prove we *append*.
            let shared = tmp.path().join("us games vs. them.pgn");
            tokio::fs::write(&shared, "previous-game-pgn\n\n").await.unwrap();
            save_pgn_if_configured(&li, &cfg, &game, "us").await;

            let written = tokio::fs::read_to_string(&shared).await.expect("shared pgn");
            assert!(written.starts_with("previous-game-pgn"));
            assert!(written.contains("[Event \"x\"]"));
            assert!(!single_path.exists(), "stale single-game file must be cleaned up");
        }

        #[tokio::test]
        async fn save_pgn_skips_when_directory_unset() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            // No /game/export mock — if save_pgn_if_configured hit it, the
            // request would 502 (default wiremock behaviour) but we still
            // wouldn't open a file. We only check that no panic happens.
            let cfg = config_without_pgn();
            let game = fixture_game(None, Some("e2e4"), true);
            save_pgn_if_configured(&li, &cfg, &game, "us").await;
        }

        #[tokio::test]
        async fn refresh_online_blocklist_updates_state_and_matchmaker() {
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            let blocklist_url = format!("{}/bl.txt", server.uri());
            // First serve an empty list so the initial OnlineBlocklist::new
            // call doesn't populate anything; then swap in the real one.
            Mock::given(method("GET"))
                .and(path("/bl.txt"))
                .respond_with(ResponseTemplate::new(200).set_body_string("alice\nbob\n"))
                .mount(&server)
                .await;

            // Build the BotState's blocklist via the real constructor —
            // this performs the initial refresh against the mock above, so
            // we already have alice+bob inside before the periodic refresh.
            let bl = crate::blocklist::OnlineBlocklist::new(vec![blocklist_url]).await;
            let state =
                Arc::new(tokio::sync::Mutex::new(BotState::new(bl)));

            // Hand-roll a Matchmaking with the *empty* default blocklist so
            // we can prove the refresh path copies the new entries over.
            let mut cfg_yaml = String::new();
            cfg_yaml.push_str("token: t\nurl: https://example.invalid/\n");
            cfg_yaml.push_str("engine:\n  dir: ''\n  name: ''\n");
            cfg_yaml.push_str("challenge:\n  concurrency: 1\n");
            let cfg: Config = serde_yaml_ng::from_str(&cfg_yaml).unwrap();
            let mm = Matchmaking::new(
                li.clone(),
                &cfg,
                UserProfileType { username: Some("us".into()), ..Default::default() },
                crate::blocklist::OnlineBlocklist::default(),
                Arc::new(tokio::sync::Mutex::new(crate::daily_counter::DailyCounter::load(""))),
            );
            let matchmaker = Arc::new(tokio::sync::Mutex::new(mm));

            // Sanity: matchmaker doesn't see alice yet.
            assert!(!matchmaker.lock().await.in_block_list("alice"));

            // Run the refresh helper — both copies should now hold alice.
            refresh_online_blocklist(&state, &matchmaker).await;
            assert!(state.lock().await.online_blocklist.contains("alice"));
            assert!(state.lock().await.online_blocklist.contains("bob"));
            assert!(matchmaker.lock().await.in_block_list("alice"));
            assert!(matchmaker.lock().await.in_block_list("bob"));
        }

        #[tokio::test]
        async fn save_pgn_skips_when_lichess_returns_empty() {
            let tmp = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            let li = make_lichess(&server).await;
            Mock::given(method("GET"))
                .and(path("/game/export/abcd1234"))
                .respond_with(ResponseTemplate::new(200).set_body_string(""))
                .mount(&server)
                .await;

            let game = fixture_game(None, Some("e2e4"), true);
            let cfg = config_with_pgn(tmp.path(), "game");
            save_pgn_if_configured(&li, &cfg, &game, "us").await;

            let written = std::fs::read_dir(tmp.path()).unwrap().count();
            assert_eq!(written, 0, "no pgn file should be created for empty body");
        }
    }
}
