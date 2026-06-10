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
use futures::StreamExt;
use tracing::{debug, error, info, warn};

use crate::blocklist::OnlineBlocklist;
use crate::config::{Config, EngineConfig, GreetingConfig};
use crate::conversation::{ChallengeQueue, ChatLine, Conversation};
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
    /// Time the most recent tournament game finished. Used by the
    /// outbound matchmaking tick to wait out Lichess's pairing-lag
    /// window before issuing the next challenge.
    last_tournament_game_ended: Option<Instant>,
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
            last_tournament_game_ended: None,
        }
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
    let blocklist_clone = state.lock().await.online_blocklist.clone();
    let matchmaker = Arc::new(Mutex::new(Matchmaking::new(
        (*li).clone(),
        &config,
        profile.clone(),
        blocklist_clone,
    )));
    let matchmaking_enabled = config.matchmaking.allow_matchmaking;
    if matchmaking_enabled {
        info!("matchmaking enabled; will issue outbound challenges periodically");
    } else {
        debug!("matchmaking disabled in config; outbound challenges off");
    }
    let mut matchmaking_rng = StdRng::from_entropy();

    let mut games: JoinSet<(String, Result<()>)> = JoinSet::new();
    let mut event_stream = Box::pin(
        li.get_event_stream()
            .await
            .context("opening event stream")?,
    );
    info!("event stream open — listening for challenges and game starts");

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
                break;
            }

            event = event_stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        if let Err(e) = handle_event(event, &li, &config, &profile, &state, &challengers, &matchmaker, &mut games).await {
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
                    Ok((game_id, Ok(()))) => {
                        let mut s = state.lock().await;
                        s.spawned_games.remove(&game_id);
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
                        info!(game_id = %game_id, "game task finished");
                    }
                    Ok((game_id, Err(e))) => {
                        error!(game_id = %game_id, "game task failed: {e:#}");
                        let mut s = state.lock().await;
                        s.spawned_games.remove(&game_id);
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
        let s = state.lock().await;
        let set: HashSet<String> = s.active_games.keys().cloned().collect();
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
    games: &mut JoinSet<(String, Result<()>)>,
) -> Result<()> {
    match event.kind.as_deref() {
        Some("challenge") => {
            if let Some(challenge_info) = event.challenge {
                handle_challenge(challenge_info, li, config, profile, state).await;
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
                let is_correspondence_pickup = {
                    let mut s = state.lock().await;
                    // A play_game task already running for this id means this
                    // gameStart is a re-subscribe from an event-stream reopen,
                    // not a new game. Spawning again would run two engine
                    // subprocesses + two move posters on one game (OOM /
                    // double-play). Skip the resend entirely.
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
                // is a no-op.
                matchmaker.lock().await.accepted_challenge(&event);
                let li = li.clone();
                let cfg = config.clone();
                let profile = profile.clone();
                let queue = challengers.clone();
                let game_id_for_task = game_id.clone();
                games.spawn(async move {
                    let res = play_game(li, cfg, profile, queue, game_id_for_task.clone()).await;
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
        let active = s.active_games.len();
        let supported = challenge.is_supported(
            &config.challenge,
            &mut s.recent_bot_challenges,
            &s.opponent_engagements,
            &s.online_blocklist,
            profile,
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
    info!(challenge_id = %challenge.id, "accepting challenge");
    if let Err(e) = li.accept_challenge(&challenge.id).await {
        warn!(challenge_id = %challenge.id, "accept failed: {e}");
    }
}

/// Whether one more game would still fit under the configured
/// `challenge.concurrency` limit. `concurrency == 0` means "decline
/// everything", matching Python's behaviour and the warning emitted
/// during config validation.
pub fn concurrency_allows(active: usize, concurrency: u32) -> bool {
    if concurrency == 0 {
        return false;
    }
    active < concurrency as usize
}

// ---------------------------------------------------------------------------
// Per-game task
// ---------------------------------------------------------------------------

async fn play_game(
    li: Arc<Lichess>,
    config: Arc<Config>,
    profile: UserProfileType,
    challengers: ChallengeQueue,
    game_id: String,
) -> Result<()> {
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

    let castling_mode = if game.variant_key == "chess960" {
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
                    // Budget: fast early retries, capped at min(12 s, 10% of
                    // remaining clock) to avoid burning time in Blitz/Bullet.
                    // Correspondence (no clock) and timed-out clocks use 12 s.
                    let remaining = game.my_remaining_time();
                    let budget = if remaining.is_zero() {
                        Duration::from_secs(12)
                    } else {
                        Duration::from_secs(12).min(remaining / 10).max(Duration::from_secs(1))
                    };
                    let delays = [
                        Duration::from_secs(1),
                        Duration::from_secs(2),
                        Duration::from_secs(4),
                        Duration::from_secs(8),
                    ];
                    let mut total_waited = Duration::ZERO;
                    let mut resubscribed = false;
                    for delay in delays {
                        if total_waited + delay > budget {
                            break;
                        }
                        tokio::time::sleep(delay).await;
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
                    }
                    if resubscribed {
                        continue;
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

        match evt.kind.as_deref() {
            Some("gameState") => {
                let new_state = state_from_event(&evt);
                apply_new_moves(&game.state, &new_state, &mut board)?;
                *position_counts
                    .entry(crate::polyglot::polyglot_hash(&board))
                    .or_insert(0) += 1;
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
                    if let Err(e) = engine.send_game_result(&game).await {
                        warn!(game_id = %game.id, "send_game_result failed: {e}");
                    }
                    harvest_experience_overlay(
                        &game,
                        castling_mode,
                        &engine,
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
                        )
                        .await?;
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
                *position_counts
                    .entry(crate::polyglot::polyglot_hash(&board))
                    .or_insert(0) += 1;

                if is_game_over(&game.state) {
                    info!(game_id = %game.id, status = ?game.state.status, "game ended during reconnect");
                    if let Err(e) = engine.send_game_result(&game).await {
                        warn!(game_id = %game.id, "send_game_result failed: {e}");
                    }
                    harvest_experience_overlay(&game, castling_mode, &engine, &config.engine.experience);
                    send_goodbye(&conversation, &game, &config.greeting).await;
                    save_pgn_if_configured(&li, &config, &game, &username).await;
                    break;
                }

                let terminate_in = compute_terminate_in(&game, &board);
                game.ping(abort_deadline(&game, abort_time), terminate_in, Duration::from_secs(0));

                if is_engine_move(&game, None, &board) {
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
                    )
                    .await?;
                }
            }
            Some("chatLine") => {
                let line = ChatLine::from_event(&evt);
                if let Err(e) = conversation.react(line, &engine, &board).await {
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

    // Drop this game's out-of-book counter so the global map doesn't grow
    // unbounded over a 24/7 run (no-op if online sources were never used).
    crate::online_book::reset_out_of_book_counter(&game.id);
    if let Err(e) = engine.quit().await {
        warn!(game_id = %game.id, error = %e, "engine quit failed (process may need a manual reap)");
    }
    Ok(())
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
    game.state
        .moves
        .as_deref()
        .map(|s| s.split_whitespace().count())
        .unwrap_or(0)
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
) -> Result<()> {
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
    )
    .await
    .with_context(|| format!("play_move failed for game {}", game.id))?;
    debug!(game_id = %game.id, mv = %decision.uci_standard(), source = ?decision.source, "move sent");
    Ok(())
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
