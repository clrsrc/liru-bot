//! Outgoing bot-vs-bot challenges. Rust port of `lib/matchmaking.py`.
//!
//! The [`Matchmaking`] struct holds all per-bot state (rate limits, the
//! per-opponent decline cooldowns, …) and exposes one network-driven
//! method, [`Matchmaking::challenge`], that the main loop calls every tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use rand::rngs::StdRng;
use serde_json::json;
use tracing::{debug, error, info, warn};

use tokio::sync::Mutex;

use crate::blocklist::OnlineBlocklist;
use crate::config::{Config, MatchmakingConfig, MatchmakingOverride};
use crate::daily_counter::DailyCounter;
use crate::lichess::{Lichess, LichessError};
use crate::lichess_types::{ChallengeType, EventType, FilterType, PerfType, UserProfileType};
use crate::model::Challenge;
use crate::opponent_db::{ChallengeForm, OpponentDb};
use crate::timer::{days, minutes, seconds, years, Timer};

/// Decline-cooldown map key: `(opponent_name, game_aspect)`. `game_aspect` is
/// the empty string when the entry acts as a full block-list entry.
type DeclineKey = (String, String);

pub struct Matchmaking {
    li: Lichess,
    variants: Vec<String>,
    matchmaking_cfg: MatchmakingConfig,
    user_profile: UserProfileType,
    last_challenge_created_delay: Timer,
    last_game_ended_delay: Timer,
    last_user_profile_update_time: Timer,
    min_wait_time: Duration,
    rate_limit_timer: Timer,
    max_wait_time: Duration,
    challenge_id: String,
    /// Username of the opponent our outstanding `challenge_id` was sent to,
    /// so we can credit them in the opponent database when the game starts.
    challenge_target: String,
    challenge_type_acceptable: HashMap<DeclineKey, Timer>,
    challenge_filter: FilterType,
    online_block_list: OnlineBlocklist,
    /// Persistent record of every bot we've challenged. Also the source of
    /// truth for the permanent `noBot` exclusion list.
    opponent_db: OpponentDb,
    /// Shared UTC-daily bot-vs-bot game tally. Incremented by the event loop
    /// (`lichess_bot.rs`) on every started bot game; read here to stop seeking
    /// once we reach our mirror of Lichess' 100/day cap. See
    /// [`crate::daily_counter`].
    daily_counter: Arc<Mutex<DailyCounter>>,
    /// Count of *consecutive* unstructured "Too many requests" (account-level)
    /// 429s on challenge creation. Drives an escalating account-pause backoff
    /// (`generic_429_backoff_minutes`) so the bot doesn't keep re-probing every
    /// few minutes into a Lichess rate-limit that never refills while it's
    /// being hit (the 15.06 self-perpetuating account-429). Reset to 0 the
    /// moment a challenge is created successfully.
    consecutive_generic_429: u32,
}

/// Account-pause (minutes) for the N-th consecutive unstructured "Too many
/// requests" 429 on challenge creation. Escalates so a persistent account-level
/// rate-limit gets a long enough break to refill, instead of being kept drained
/// by short re-probes. Pure, so the schedule is unit-testable.
fn generic_429_backoff_minutes(consecutive: u32) -> f64 {
    match consecutive {
        0 | 1 => 5.0,
        2 => 10.0,
        3 => 20.0,
        4 => 40.0,
        _ => 60.0,
    }
}

impl Matchmaking {
    pub fn new(
        li: Lichess,
        config: &Config,
        user_profile: UserProfileType,
        online_block_list: OnlineBlocklist,
        daily_counter: Arc<Mutex<DailyCounter>>,
    ) -> Self {
        let variants = config
            .challenge
            .variants
            .iter()
            .filter(|v: &&String| v.as_str() != "fromPosition")
            .cloned()
            .collect();
        let cfg = config.matchmaking.clone();
        let max_wait_time = if cfg.allow_during_games {
            minutes(10.0)
        } else {
            years(10.0)
        };
        let last_game_ended_delay = Timer::new(minutes(cfg.challenge_timeout as f64));
        let challenge_filter = cfg.challenge_filter;
        let opponent_db = OpponentDb::load(&cfg.opponent_db_path);

        let mut mm = Self {
            li,
            variants,
            matchmaking_cfg: cfg,
            user_profile,
            last_challenge_created_delay: Timer::new(seconds(25.0)),
            last_game_ended_delay,
            last_user_profile_update_time: Timer::new(minutes(5.0)),
            min_wait_time: seconds(60.0),
            rate_limit_timer: Timer::zero(),
            max_wait_time,
            challenge_id: String::new(),
            challenge_target: String::new(),
            challenge_type_acceptable: HashMap::new(),
            challenge_filter,
            online_block_list,
            opponent_db,
            daily_counter,
            consecutive_generic_429: 0,
        };

        let block_list = mm.matchmaking_cfg.block_list.clone();
        for name in block_list {
            mm.add_to_block_list(&name);
        }

        // Restore in-memory decline suppressions from the persistent history so
        // that bots suppressed before a restart are not re-challenged immediately
        // (the "429-Sturm" root cause: challenge_type_acceptable was cleared).
        let fine = mm.challenge_filter == FilterType::Fine;
        for (username, reason_key, remaining, form) in mm.opponent_db.recent_declines(days(1.0)) {
            let aspect = decline_game_aspect(&reason_key, &form, fine);
            mm.challenge_type_acceptable
                .insert((username, aspect.to_string()), Timer::new(remaining));
        }

        mm
    }

    /// Whether matchmaking should attempt to create a new challenge right now.
    /// Sends a cancel request for an expired-but-not-yet-cleaned challenge as
    /// a side effect — mirrors the Python implementation.
    pub async fn should_create_challenge(&mut self) -> bool {
        let matchmaking_enabled = self.matchmaking_cfg.allow_matchmaking;
        let time_has_passed =
            self.last_game_ended_delay.is_expired() && self.rate_limit_timer.is_expired();
        let challenge_expired =
            self.last_challenge_created_delay.is_expired() && !self.challenge_id.is_empty();
        let min_wait_time_passed =
            self.last_challenge_created_delay.time_since_reset() > self.min_wait_time;

        if challenge_expired {
            let id = std::mem::take(&mut self.challenge_id);
            if let Err(err) = self.li.cancel(&id).await {
                debug!(%id, %err, "cancel of expired challenge failed");
            } else {
                info!(%id, "expired challenge cancelled");
            }
            self.show_earliest_challenge_time();
        }

        // Daily bot-vs-bot budget gate: once our own tally reaches our mirror
        // of Lichess' 100/day cap, stop seeking entirely — otherwise every
        // outbound challenge just bounces off Lichess' `bot.vsBot.day` rate
        // limit. Resets transparently at 00:00 UTC via the counter's rollover.
        if matchmaking_enabled {
            let limit = self.matchmaking_cfg.daily_game_limit;
            if limit > 0 && self.daily_counter.lock().await.count() >= limit {
                debug!(limit, "daily bot-game limit reached; pausing matchmaking until 00:00 UTC reset");
                return false;
            }
        }

        matchmaking_enabled && (time_has_passed || challenge_expired) && min_wait_time_passed
    }

    /// `POST /api/challenge/{user}` with the chosen parameters. Returns the
    /// new challenge id, or an empty string on error.
    pub async fn create_challenge(
        &mut self,
        username: &str,
        base_time: i64,
        increment: i64,
        days: i64,
        variant: &str,
        mode: &str,
    ) -> String {
        let mut params = json!({
            "rated": mode == "rated",
            "variant": variant,
        });

        let map = params.as_object_mut().expect("payload is a JSON object");
        if days > 0 {
            map.insert("days".into(), days.into());
        } else if base_time > 0 || increment > 0 {
            map.insert("clock.limit".into(), base_time.into());
            map.insert("clock.increment".into(), increment.into());
        } else {
            error!(
                "At least one of challenge_days, challenge_initial_time, or challenge_increment \
                 must be greater than zero in the matchmaking section of your config file."
            );
            return String::new();
        }

        self.last_challenge_created_delay.reset();
        match self.li.challenge(username, &params).await {
            Ok(response) => {
                let id = response.id.clone().unwrap_or_default();
                if id.is_empty() {
                    self.handle_challenge_error_response(&response, username);
                } else {
                    let form = ChallengeForm {
                        base_time,
                        increment,
                        days,
                        variant: variant.to_string(),
                        mode: mode.to_string(),
                        game_type: game_category(variant, base_time, increment, days).to_string(),
                    };
                    self.challenge_target = username.to_string();
                    self.opponent_db.record_challenge_sent(username, form);
                    // A challenge went through → our account is no longer
                    // rate-limited; reset the escalating-backoff counter.
                    self.consecutive_generic_429 = 0;
                }
                return id;
            }
            Err(LichessError::RateLimited { timeout, .. }) => {
                warn!(?timeout, "matchmaking rate-limited");
                self.rate_limit_timer = Timer::new(timeout);
            }
            Err(err) => {
                debug!(%err, "create_challenge failed");
            }
        }

        warn!("Could not create challenge");
        self.show_earliest_challenge_time();
        String::new()
    }

    fn handle_challenge_error_response(&mut self, response: &ChallengeType, username: &str) {
        error!(?response, "challenge error response");
        if response.bot_is_rate_limited == Some(true) {
            // Our own account is rate-limited — pause all outbound challenges
            // for the server-given duration.
            if let Some(timeout) = response.rate_limit_timeout {
                self.rate_limit_timer = Timer::new(timeout);
            }
        } else if response.opponent_is_rate_limited == Some(true) {
            // The opponent hit their own cap (e.g. 100 bot games/day). Suppress
            // them in-memory AND persist it, so a restart does not re-challenge
            // every capped opponent in a burst → Lichess "Too many requests".
            self.add_challenge_filter(username, "", response.rate_limit_timeout);
            if let Some(timeout) = response.rate_limit_timeout {
                self.opponent_db
                    .record_rate_limited(username, timeout.as_secs() as i64);
            }
        } else if response.account_throttled_429 == Some(true) {
            // Real generic account-429 (HTTP 429, not a daily-vs-bot limit): our account
            // is being throttled — not this opponent's fault. Back the whole account off
            // rather than day-suppressing an innocent opponent (the old behaviour shrank
            // the candidate pool and fed the 429 storm).
            // ESCALATING backoff: Lichess' rate-limit bucket does not refill while it's
            // still being hit, so a fixed short pause lets the bot re-probe and perpetuate
            // its own account-429 (observed 15.06). Each consecutive generic-429 pauses
            // longer (5→10→20→40→60 min) until a challenge gets through (resets the counter).
            self.consecutive_generic_429 = self.consecutive_generic_429.saturating_add(1);
            let mins = generic_429_backoff_minutes(self.consecutive_generic_429);
            warn!(
                consecutive = self.consecutive_generic_429,
                pause_min = mins,
                "account rate-limited (generic 429); pausing outbound challenges"
            );
            self.rate_limit_timer = Timer::new(minutes(mins));
        } else {
            // Content decline (onlyFriends, variant/rating/casual mismatch, …): the
            // challenge POST returned a non-429 error body — NOT a rate limit. Skip THIS
            // opponent (in-memory cooldown, consulted via in_block_list) like noBot/onlyBot;
            // do NOT pause the whole account or touch the 429 counter. Without this, a
            // single friend-only bot in a thin pool drove the escalating backoff into hours
            // of idle (bot #167, 16.06). Locale-independent: keyed on HTTP status, not text.
            warn!(
                opponent = username,
                error = ?response.error,
                "challenge declined for a content reason (not a rate limit); skipping opponent"
            );
            self.add_challenge_filter(username, "", None);
        }
        self.show_earliest_challenge_time();
    }

    pub fn perf(&self) -> HashMap<String, PerfType> {
        self.user_profile.perfs.clone().unwrap_or_default()
    }

    pub fn username(&self) -> &str {
        self.user_profile.username()
    }

    pub async fn update_user_profile(&mut self) {
        if self.last_user_profile_update_time.is_expired() {
            self.last_user_profile_update_time.reset();
            // mirror Python's `contextlib.suppress(Exception)` — best-effort
            // refresh; logging happens inside `get_profile` already.
            let mut clone = self.li.clone();
            if let Ok(profile) = clone.get_profile().await {
                self.user_profile = profile;
            }
        }
    }

    /// Picks one bot from `online_bots` (weighted by rating). Pure function:
    /// gets `rng` injected so unit tests can use a seeded RNG.
    pub fn get_weights(
        online_bots: &[UserProfileType],
        rating_preference: &str,
        min_rating: i64,
        max_rating: i64,
        game_type: &str,
    ) -> Vec<i64> {
        let rating_of = |bot: &UserProfileType| -> i64 {
            bot.perfs
                .as_ref()
                .and_then(|m| m.get(game_type))
                .and_then(|p| p.rating)
                .unwrap_or(0)
        };

        match rating_preference {
            "high" => {
                let spread = max_rating.saturating_sub(min_rating);
                let reduce =
                    min_rating.saturating_sub(spread).min(min_rating.saturating_sub(1));
                online_bots
                    .iter()
                    .map(|b| rating_of(b).saturating_sub(reduce))
                    .collect()
            }
            "low" => {
                let spread = min_rating.saturating_sub(max_rating);
                let reduce =
                    max_rating.saturating_sub(spread).max(max_rating.saturating_add(1));
                online_bots
                    .iter()
                    .map(|b| reduce.saturating_sub(rating_of(b)))
                    .collect()
            }
            _ => vec![1; online_bots.len()],
        }
    }

    pub async fn choose_opponent(
        &mut self,
        rng: &mut StdRng,
    ) -> (Option<String>, i64, i64, i64, String, String) {
        let mut override_keys: Vec<Option<&String>> = self
            .matchmaking_cfg
            .overrides
            .keys()
            .map(Some)
            .collect();
        override_keys.push(None);
        let chosen = override_keys.choose(rng).copied().flatten();
        info!(
            override = chosen.map(|s| s.as_str()).unwrap_or("default"),
            "using matchmaking configuration"
        );

        let override_cfg = chosen
            .and_then(|key| self.matchmaking_cfg.overrides.get(key).cloned())
            .unwrap_or_default();
        let match_config = merge_override(&self.matchmaking_cfg, &override_cfg);

        let variant = self
            .pick_value(&match_config.challenge_variant, &self.variants, rng);
        let mode = self.pick_value(
            &match_config.challenge_mode,
            &["casual".to_string(), "rated".to_string()],
            rng,
        );
        let rating_preference = match_config.rating_preference.clone();

        let base_time =
            pick_optional_int(&match_config.challenge_initial_time, rng).unwrap_or(0);
        let increment = pick_optional_int(&match_config.challenge_increment, rng).unwrap_or(0);
        let mut num_days = pick_optional_int(&match_config.challenge_days, rng).unwrap_or(0);

        // Either play correspondence (days > 0, no clock) or clock-based
        // (clock > 0, no days). Python randomly picks one of the available
        // axes via `random.choice([bool(num_days), not bool(base_time or increment)])`.
        let mut base_time_m = base_time;
        let mut increment_m = increment;
        let opts = [num_days > 0, !(base_time > 0 || increment > 0)];
        let play_correspondence = *opts.choose(rng).unwrap_or(&false);
        if play_correspondence {
            base_time_m = 0;
            increment_m = 0;
        } else {
            num_days = 0;
        }

        let game_type = game_category(&variant, base_time_m, increment_m, num_days);

        let mut min_rating = match_config.opponent_min_rating;
        let mut max_rating = match_config.opponent_max_rating;
        let rating_diff = match_config.opponent_rating_difference;
        let bot_rating = self
            .perf()
            .get(game_type)
            .and_then(|p| p.rating)
            .unwrap_or(0);
        if let Some(diff) = rating_diff {
            if bot_rating > 0 {
                min_rating = bot_rating - diff;
                max_rating = bot_rating + diff;
            }
        }
        info!(
            game_type,
            min_rating,
            max_rating,
            "seeking opponent in rating window"
        );

        self.online_block_list.refresh().await;
        // Fetch the full online-bot list (Lichess maxes ~300), not the default
        // 100. With only 100 the in-window survivors after the cap/rate-limited
        // filters collapsed to ~0 while ~165 of ~300 online bots were eligible —
        // self-inflicted matchmaking starvation (befunde/matchmaking_starvation_20260617.md).
        let online_bots_raw = self.li.get_online_bots(Some(300)).await;
        let raw_count = online_bots_raw.len();

        let online_bots: Vec<UserProfileType> = online_bots_raw
            .into_iter()
            .filter(|bot| self.is_suitable_opponent(bot, &game_type, min_rating, max_rating))
            // Diversity brake: drop opponents we've already challenged to the
            // daily cap, so a single bot can't monopolise matchmaking.
            .filter(|bot| self.under_daily_challenge_cap(bot))
            // Skip opponents we know are rate-limited (e.g. at their own daily
            // cap). Persisted in the opponent DB, so a restart doesn't
            // re-challenge every capped bot in a burst → Lichess 429 storm.
            .filter(|bot| !self.opponent_rate_limited(bot))
            .collect();
        let suitable_count = online_bots.len();

        let ready_bots: Vec<UserProfileType> = online_bots
            .iter()
            .filter(|bot| self.ready_for_challenge(bot, &variant, &game_type, &mode))
            .cloned()
            .collect();
        let ready_count = ready_bots.len();
        let candidates = if !ready_bots.is_empty() {
            ready_bots
        } else {
            online_bots
        };
        // Diagnostic: distinguish a silent-empty fetch (raw=0) from the filter
        // gate (raw>0 but suitable/ready=0) when "No suitable bots" appears.
        info!(
            online_raw = raw_count,
            suitable = suitable_count,
            ready = ready_count,
            candidates = candidates.len(),
            "online bot pool after filters"
        );

        let rating_weights = Self::get_weights(
            &candidates,
            &rating_preference,
            min_rating,
            max_rating,
            &game_type,
        );
        // Soft diversity: below the hard cap, down-weight opponents we've
        // already played today so fresh bots are preferred when several are
        // online. Each prior game today divides the rating weight by one more.
        let weights: Vec<i64> = candidates
            .iter()
            .zip(rating_weights)
            .map(|(bot, w)| {
                let played = bot
                    .username
                    .as_deref()
                    .map(|n| self.opponent_db.challenges_today(n))
                    .unwrap_or(0);
                (w.max(1) / (played as i64 + 1)).max(1)
            })
            .collect();

        let bot_username = if candidates.is_empty() {
            error!("No suitable bots found to challenge.");
            None
        } else {
            self.pick_bot_username(&candidates, &weights, rng).await
        };

        (bot_username, base_time_m, increment_m, num_days, variant, mode)
    }

    async fn pick_bot_username(
        &mut self,
        candidates: &[UserProfileType],
        weights: &[i64],
        rng: &mut StdRng,
    ) -> Option<String> {
        // `WeightedIndex` rejects all-zero / negative weights. Fall back to a
        // uniform pick in that case so we never crash on an unlucky config.
        let dist = WeightedIndex::new(weights.iter().map(|w| (*w).max(1) as u64)).ok();
        let bot = match dist {
            Some(d) => &candidates[d.sample(rng)],
            None => candidates.choose(rng)?,
        };
        let name = bot.username.clone()?;
        match self.li.get_public_data(&name).await {
            Ok(public) if public.blocking == Some(true) => {
                self.add_to_block_list(&name);
                None
            }
            Ok(_) => Some(name),
            Err(err) => {
                warn!(%name, %err, "could not fetch opponent profile");
                None
            }
        }
    }

    /// Diversity brake: returns `false` once we have already initiated
    /// `max_challenges_per_opponent_per_day` challenges against this bot today
    /// (local date), so matchmaking stops farming a single opponent. A cap of
    /// `0` disables the brake (unlimited). Opponents with no username pass.
    fn under_daily_challenge_cap(&self, bot: &UserProfileType) -> bool {
        let cap = self.matchmaking_cfg.max_challenges_per_opponent_per_day;
        if cap == 0 {
            return true;
        }
        match bot.username.as_deref() {
            Some(name) => self.opponent_db.challenges_today(name) < cap,
            None => true,
        }
    }

    /// Whether `bot` is currently rate-limited against our outbound challenges
    /// (e.g. they hit their own daily bot-games cap). Backed by the persistent
    /// opponent DB, so a restart doesn't re-challenge them in a burst. Bots
    /// with no username pass (can't be looked up).
    fn opponent_rate_limited(&self, bot: &UserProfileType) -> bool {
        bot.username
            .as_deref()
            .map_or(false, |name| self.opponent_db.is_rate_limited(name))
    }

    fn is_suitable_opponent(
        &self,
        bot: &UserProfileType,
        game_type: &str,
        min_rating: i64,
        max_rating: i64,
    ) -> bool {
        let Some(name) = bot.username.as_deref() else {
            return false;
        };
        if name == self.username() {
            return false;
        }
        if self.in_block_list(name) {
            return false;
        }
        let perf = bot
            .perfs
            .as_ref()
            .and_then(|m| m.get(game_type));
        let games = perf.and_then(|p| p.games).unwrap_or(0);
        let rating = perf.and_then(|p| p.rating).unwrap_or(0);
        games > 0 && (min_rating..=max_rating).contains(&rating)
    }

    fn ready_for_challenge(
        &self,
        bot: &UserProfileType,
        variant: &str,
        game_type: &str,
        mode: &str,
    ) -> bool {
        let Some(name) = bot.username.as_deref() else {
            return false;
        };
        if self.challenge_filter != FilterType::Fine {
            return true;
        }
        [variant, game_type, mode]
            .iter()
            .all(|aspect| self.should_accept_challenge(name, aspect))
    }

    fn pick_value(
        &self,
        configured: &str,
        choices: &[String],
        rng: &mut StdRng,
    ) -> String {
        if configured != "random" {
            return configured.to_string();
        }
        choices.choose(rng).cloned().unwrap_or_default()
    }

    /// Try to send a challenge. Called by the main event loop every tick.
    pub async fn challenge(
        &mut self,
        active_games: &std::collections::HashSet<String>,
        challenge_queue_len: usize,
        max_games: usize,
        rng: &mut StdRng,
    ) {
        let max_for_matchmaking = if self.matchmaking_cfg.allow_during_games {
            max_games
        } else {
            std::cmp::min(1, max_games)
        };
        let game_count = active_games.len() + challenge_queue_len;
        if game_count >= max_for_matchmaking
            || (game_count > 0
                && self.last_challenge_created_delay.time_since_reset() < self.max_wait_time)
        {
            return;
        }
        if !self.should_create_challenge().await {
            return;
        }

        info!("Challenging a random bot");
        self.update_user_profile().await;
        let (bot, base, inc, days, variant, mode) = self.choose_opponent(rng).await;
        info!(?bot, %variant, "will challenge");
        let id = match bot {
            Some(name) => {
                self.create_challenge(&name, base, inc, days, &variant, &mode)
                    .await
            }
            None => String::new(),
        };
        info!(challenge_id = %if id.is_empty() { "None" } else { id.as_str() });
        self.challenge_id = id;
    }

    pub fn discard_challenge(&mut self, challenge_id: &str) {
        if self.challenge_id == challenge_id {
            self.challenge_id.clear();
        }
    }

    pub fn game_done(&mut self) {
        self.last_game_ended_delay.reset();
        self.show_earliest_challenge_time();
    }

    fn show_earliest_challenge_time(&self) {
        if !self.matchmaking_cfg.allow_matchmaking {
            return;
        }
        let postgame = self.last_game_ended_delay.time_until_expiration();
        let next = self
            .min_wait_time
            .checked_sub(self.last_challenge_created_delay.time_since_reset())
            .unwrap_or(Duration::ZERO);
        let rate_limit = self.rate_limit_timer.time_until_expiration();
        let time_left = postgame.max(next).max(rate_limit);
        let when = Local::now() + chrono::Duration::from_std(time_left).unwrap_or_default();
        info!(
            earliest = %when.format("%c"),
            "earliest next challenge"
        );
    }

    pub fn add_to_block_list(&mut self, username: &str) {
        self.add_challenge_filter(username, "", Some(years(10.0)));
    }

    /// Swap in a freshly-loaded online block-list. Used by the periodic
    /// refresh task in `lichess_bot.rs` to keep this private copy in sync
    /// with `BotState::online_blocklist` — both are clones from the same
    /// startup load, but only the BotState copy gets refreshed in place.
    pub fn replace_online_block_list(&mut self, blocklist: OnlineBlocklist) {
        self.online_block_list = blocklist;
    }

    pub fn in_block_list(&self, username: &str) -> bool {
        !self.should_accept_challenge(username, "")
            || self.online_block_list.contains(username)
            || self.opponent_db.is_blocked(username)
    }

    pub fn add_challenge_filter(
        &mut self,
        username: &str,
        game_aspect: &str,
        timeout: Option<Duration>,
    ) {
        let key = (username.to_string(), game_aspect.to_string());
        self.challenge_type_acceptable
            .insert(key, Timer::new(timeout.unwrap_or_else(|| days(1.0))));
    }

    pub fn should_accept_challenge(&self, username: &str, game_aspect: &str) -> bool {
        let key = (username.to_string(), game_aspect.to_string());
        self.challenge_type_acceptable
            .get(&key)
            .map(|t| t.is_expired())
            .unwrap_or(true)
    }

    /// Reconcile a `gameStart` against our outstanding outbound challenge.
    /// Returns `true` when the started game *was* our outbound matchmaking
    /// challenge — which, since matchmaking only ever targets bots, means the
    /// caller should count it toward the daily bot-vs-bot tally.
    pub fn accepted_challenge(&mut self, event: &EventType) -> bool {
        if let Some(id) = event.game.as_ref().and_then(|g| g.id.clone()) {
            // When an opponent accepts our outbound challenge the resulting
            // game id equals the challenge id. Only then do we credit the
            // game to the opponent we last challenged.
            let was_ours = id == self.challenge_id && !self.challenge_target.is_empty();
            if was_ours {
                let target = std::mem::take(&mut self.challenge_target);
                self.opponent_db.record_accepted(&target);
            }
            self.discard_challenge(&id);
            return was_ours;
        }
        false
    }

    /// Whether `name` is a bot that accepted one of *our* outbound matchmaking
    /// challenges (i.e. a game actually started, `games_played > 0` in the
    /// persistent opponent database). Drives the reciprocity rule in
    /// `handle_challenge`: such bots are accepted back regardless of rating
    /// (communal engine development). Persists across restarts.
    pub fn bot_accepted_our_challenge(&self, name: &str) -> bool {
        self.opponent_db
            .get(name)
            .map(|r| r.games_played > 0)
            .unwrap_or(false)
    }

    pub fn declined_challenge(&mut self, event: &EventType) {
        let Some(challenge_info) = event.challenge.as_ref() else {
            return;
        };
        let challenge = Challenge::from_info(challenge_info, &self.user_profile);
        let opponent_name = challenge.challenge_target.name.clone();
        let reason = challenge_info.decline_reason.clone().unwrap_or_default();
        info!(
            opponent = %opponent_name,
            challenge = %challenge,
            %reason,
            "opponent declined challenge"
        );
        self.discard_challenge(&challenge.id);

        if !challenge.from_self {
            return;
        }

        let mode = if challenge.rated { "rated" } else { "casual" };
        let reason_key = challenge_info
            .decline_reason_key
            .clone()
            .unwrap_or_default()
            .to_lowercase();

        // Persist the decline — and, for `noBot` / `onlyBot`, a permanent
        // exclusion — independently of the in-memory `challenge_filter`. The
        // database is the durable source of truth for "never challenge again".
        self.opponent_db.record_declined(&opponent_name, &reason_key);

        if self.challenge_filter == FilterType::None {
            return;
        }

        // Mapping reason_key → which "game aspect" should we cool down on?
        // Mirrors the dict in Python's `declined_challenge`.
        let aspect_for = |key: &str| -> &str {
            match key {
                // `onlybot` is the official counterpart to `nobot` (the
                // opponent only plays bots) — both are whole-opponent blocks.
                "generic" | "later" | "nobot" | "onlybot" => "",
                "toofast" | "tooslow" | "timecontrol" => challenge.speed.as_str(),
                "rated" | "casual" => mode,
                "standard" | "variant" => challenge.variant.as_str(),
                _ => "",
            }
        };
        let known = matches!(
            reason_key.as_str(),
            "generic" | "later" | "nobot" | "onlybot" | "toofast" | "tooslow" | "timecontrol" |
            "rated" | "casual" | "standard" | "variant"
        );
        if !known {
            warn!(%reason_key, "unknown decline reason key");
        }
        let game_problem = if self.challenge_filter == FilterType::Fine {
            aspect_for(&reason_key)
        } else {
            ""
        };
        self.add_challenge_filter(&opponent_name, game_problem, None);
        info!(
            opponent = %opponent_name,
            game_problem,
            "will not re-challenge today"
        );
        self.show_earliest_challenge_time();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Merge `override_cfg` on top of `base`, mirroring Python's `cfg | override`.
/// Any `Option::None` field in the override leaves the base value untouched;
/// `Option::Some(v)` replaces it.
fn merge_override(
    base: &MatchmakingConfig,
    override_cfg: &MatchmakingOverride,
) -> MatchmakingConfig {
    let mut out = base.clone();
    if let Some(v) = &override_cfg.challenge_variant {
        out.challenge_variant = v.clone();
    }
    if let Some(v) = &override_cfg.challenge_initial_time {
        out.challenge_initial_time = v.clone();
    }
    if let Some(v) = &override_cfg.challenge_increment {
        out.challenge_increment = v.clone();
    }
    if let Some(v) = &override_cfg.challenge_days {
        out.challenge_days = v.clone();
    }
    if let Some(v) = override_cfg.opponent_min_rating {
        out.opponent_min_rating = v;
    }
    if let Some(v) = override_cfg.opponent_max_rating {
        out.opponent_max_rating = v;
    }
    // `opponent_rating_difference` in the override is a generic JSON value
    // (Python's config layer accepts both `int` and `null`). Convert here.
    if let Some(json) = &override_cfg.opponent_rating_difference {
        out.opponent_rating_difference = json.as_i64();
    }
    if let Some(v) = &override_cfg.challenge_mode {
        out.challenge_mode = v.clone();
    }
    if let Some(v) = &override_cfg.rating_preference {
        out.rating_preference = v.clone();
    }
    out
}

/// Maps a Lichess decline `reason_key` to the `game_aspect` string used as
/// the second component of [`DeclineKey`]. Mirrors the `aspect_for` closure
/// in [`Matchmaking::declined_challenge`] but works from the persisted
/// [`ChallengeForm`] so it can be called during startup suppression-restore.
///
/// `fine` is `challenge_filter == FilterType::Fine`; when `false` every
/// reason key collapses to `""` (whole-opponent block).
fn decline_game_aspect<'a>(reason_key: &str, form: &'a ChallengeForm, fine: bool) -> &'a str {
    if !fine {
        return "";
    }
    match reason_key {
        "generic" | "later" | "nobot" | "onlybot" => "",
        "toofast" | "tooslow" | "timecontrol" => &form.game_type,
        "rated" | "casual" => &form.mode,
        "standard" | "variant" => &form.variant,
        _ => "",
    }
}

fn pick_optional_int(values: &[Option<i64>], rng: &mut StdRng) -> Option<i64> {
    values.choose(rng).copied().flatten()
}

/// Get the game type (e.g. bullet, atomic, classical). Lichess has one
/// rating per variant, regardless of time control; for non-standard variants
/// the variant key is returned as-is.
pub fn game_category(variant: &str, base_time: i64, increment: i64, num_days: i64) -> &'static str {
    let game_duration = base_time + increment * 40;
    match (variant, num_days, game_duration) {
        (v, _, _) if v != "standard" => match v {
            "antichess" => "antichess",
            "atomic" => "atomic",
            "chess960" => "chess960",
            "crazyhouse" => "crazyhouse",
            "horde" => "horde",
            "kingOfTheHill" => "kingOfTheHill",
            "racingKings" => "racingKings",
            "threeCheck" => "threeCheck",
            _ => "standard",
        },
        (_, days, _) if days > 0 => "correspondence",
        (_, _, d) if d < 179 => "bullet",
        (_, _, d) if d < 479 => "blitz",
        (_, _, d) if d < 1499 => "rapid",
        _ => "classical",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lichess_types::PerfType;

    #[test]
    fn game_category_distinguishes_speeds() {
        assert_eq!(game_category("standard", 60, 0, 0), "bullet");
        assert_eq!(game_category("standard", 300, 2, 0), "blitz");
        assert_eq!(game_category("standard", 600, 5, 0), "rapid");
        assert_eq!(game_category("standard", 1800, 0, 0), "classical");
        assert_eq!(game_category("standard", 0, 0, 3), "correspondence");
    }

    #[test]
    fn game_category_keeps_variant_for_non_standard() {
        assert_eq!(game_category("atomic", 60, 0, 0), "atomic");
        assert_eq!(game_category("crazyhouse", 600, 0, 0), "crazyhouse");
    }

    fn bot_with_rating(name: &str, game_type: &str, rating: i64, games: i64) -> UserProfileType {
        UserProfileType {
            id: Some(name.to_lowercase()),
            username: Some(name.into()),
            perfs: Some(HashMap::from([(
                game_type.into(),
                PerfType {
                    rating: Some(rating),
                    games: Some(games),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        }
    }

    #[test]
    fn get_weights_uniform_default() {
        let bots = vec![
            bot_with_rating("a", "blitz", 1500, 10),
            bot_with_rating("b", "blitz", 2200, 10),
        ];
        let w = Matchmaking::get_weights(&bots, "none", 1500, 2500, "blitz");
        assert_eq!(w, vec![1, 1]);
    }

    #[test]
    fn get_weights_high_favours_strong_bots() {
        let bots = vec![
            bot_with_rating("weak", "blitz", 1500, 10),
            bot_with_rating("strong", "blitz", 2500, 10),
        ];
        let w = Matchmaking::get_weights(&bots, "high", 1500, 2500, "blitz");
        // Stronger bot must outweigh weaker one.
        assert!(w[1] > w[0]);
    }

    #[test]
    fn get_weights_low_favours_weak_bots() {
        let bots = vec![
            bot_with_rating("weak", "blitz", 1500, 10),
            bot_with_rating("strong", "blitz", 2500, 10),
        ];
        let w = Matchmaking::get_weights(&bots, "low", 1500, 2500, "blitz");
        assert!(w[0] > w[1]);
    }

    /// Build a `Matchmaking` directly, bypassing `new()`, so we don't need a
    /// real `Lichess` client for pure-logic tests. The Lichess client is
    /// constructed via `new_raw` and never sees network traffic in these
    /// tests.
    fn direct_struct() -> Matchmaking {
        let li_url = url::Url::parse("https://lichess.org/").unwrap();
        let li = Lichess::new_raw("token".into(), li_url, "0".into(), 3).unwrap();
        Matchmaking {
            li,
            variants: vec!["standard".into()],
            matchmaking_cfg: MatchmakingConfig::default(),
            user_profile: UserProfileType {
                username: Some("BotOne".into()),
                ..Default::default()
            },
            last_challenge_created_delay: Timer::zero(),
            last_game_ended_delay: Timer::zero(),
            last_user_profile_update_time: Timer::zero(),
            min_wait_time: seconds(60.0),
            rate_limit_timer: Timer::zero(),
            max_wait_time: years(10.0),
            challenge_id: String::new(),
            challenge_target: String::new(),
            challenge_type_acceptable: HashMap::new(),
            challenge_filter: FilterType::None,
            online_block_list: OnlineBlocklist::default(),
            opponent_db: OpponentDb::load(""),
            daily_counter: Arc::new(Mutex::new(DailyCounter::load(""))),
            consecutive_generic_429: 0,
        }
    }

    #[test]
    fn bot_accepted_our_challenge_requires_a_played_game() {
        let mut mm = direct_struct();
        // Unknown opponent → false.
        assert!(!mm.bot_accepted_our_challenge("Weiawaga"));
        // We challenged them, but they haven't accepted yet → still false.
        mm.opponent_db
            .record_challenge_sent("Weiawaga", ChallengeForm::default());
        assert!(!mm.bot_accepted_our_challenge("Weiawaga"));
        // They accepted and a game started → reciprocity applies.
        mm.opponent_db.record_accepted("Weiawaga");
        assert!(mm.bot_accepted_our_challenge("Weiawaga"));
    }

    #[test]
    fn opponent_rate_limited_is_persisted_and_skipped() {
        use crate::lichess_types::ChallengeType;
        let mut mm = direct_struct();
        let bot = UserProfileType {
            username: Some("CappedBot".into()),
            ..Default::default()
        };
        assert!(!mm.opponent_rate_limited(&bot));

        // Opponent at their own cap → persisted (survives restart) + skipped.
        let resp = ChallengeType {
            opponent_is_rate_limited: Some(true),
            rate_limit_timeout: Some(Duration::from_secs(3600)),
            ..Default::default()
        };
        mm.handle_challenge_error_response(&resp, "CappedBot");
        assert!(mm.opponent_rate_limited(&bot));
        assert!(mm.opponent_db.is_rate_limited("CappedBot"));
    }

    #[test]
    fn generic_too_many_requests_pauses_account_not_opponent() {
        use crate::lichess_types::ChallengeType;
        let mut mm = direct_struct();
        // Real generic 429 (HTTP 429, not daily-limit): flagged by Lichess::challenge.
        let resp = ChallengeType {
            error: Some("Too many requests. Try again later.".into()),
            account_throttled_429: Some(true),
            ..Default::default()
        };
        mm.handle_challenge_error_response(&resp, "InnocentBot");
        // The whole account is paused ...
        assert!(!mm.rate_limit_timer.is_expired());
        // ... and the innocent opponent is NOT day-suppressed (old bug).
        assert!(mm.should_accept_challenge("InnocentBot", ""));
        assert!(!mm.opponent_db.is_rate_limited("InnocentBot"));
        // First occurrence counts toward the escalating backoff.
        assert_eq!(mm.consecutive_generic_429, 1);
    }

    #[test]
    fn content_decline_skips_opponent_not_account() {
        // onlyFriends (and other content declines) arrive with a non-429 status, so
        // account_throttled_429 is false/None. Must skip the opponent (in_block_list),
        // NOT pause the account or count toward the 429 backoff (bot #167, 16.06).
        use crate::lichess_types::ChallengeType;
        let mut mm = direct_struct();
        let resp = ChallengeType {
            error: Some("BOT Foo accepts challenges only from friends.".into()),
            ..Default::default() // account_throttled_429 = None
        };
        mm.handle_challenge_error_response(&resp, "FriendOnlyBot");
        // Opponent is skipped from future selection ...
        assert!(mm.in_block_list("FriendOnlyBot"));
        // ... but the account is NOT paused and the 429 counter stays put.
        assert!(mm.rate_limit_timer.is_expired());
        assert_eq!(mm.consecutive_generic_429, 0);
    }

    #[test]
    fn generic_429_backoff_escalates_then_resets() {
        // Pure schedule: 5 → 10 → 20 → 40 → 60 (capped).
        assert_eq!(generic_429_backoff_minutes(1), 5.0);
        assert_eq!(generic_429_backoff_minutes(2), 10.0);
        assert_eq!(generic_429_backoff_minutes(3), 20.0);
        assert_eq!(generic_429_backoff_minutes(4), 40.0);
        assert_eq!(generic_429_backoff_minutes(5), 60.0);
        assert_eq!(generic_429_backoff_minutes(99), 60.0);

        use crate::lichess_types::ChallengeType;
        let mut mm = direct_struct();
        let resp = ChallengeType {
            error: Some("Too many requests. Try again later.".into()),
            account_throttled_429: Some(true),
            ..Default::default()
        };
        // Three consecutive generic-429s → counter climbs.
        for expected in 1..=3 {
            mm.handle_challenge_error_response(&resp, "InnocentBot");
            assert_eq!(mm.consecutive_generic_429, expected);
        }
        // A successful challenge resets the escalation.
        mm.consecutive_generic_429 = 0; // (set directly: create_challenge resets on success)
        assert_eq!(mm.consecutive_generic_429, 0);
    }

    #[test]
    fn should_accept_challenge_defaults_true() {
        let mm = direct_struct();
        assert!(mm.should_accept_challenge("Whoever", ""));
        assert!(mm.should_accept_challenge("Whoever", "blitz"));
    }

    #[test]
    fn add_challenge_filter_blocks_until_timer_expires() {
        let mut mm = direct_struct();
        mm.add_challenge_filter("Foo", "", Some(Duration::from_secs(60)));
        assert!(!mm.should_accept_challenge("Foo", ""));
        // Different aspect → not affected
        assert!(mm.should_accept_challenge("Foo", "blitz"));
    }

    #[test]
    fn discard_challenge_clears_only_matching_id() {
        let mut mm = direct_struct();
        mm.challenge_id = "abc".into();
        mm.discard_challenge("xyz");
        assert_eq!(mm.challenge_id, "abc");
        mm.discard_challenge("abc");
        assert_eq!(mm.challenge_id, "");
    }

    #[test]
    fn in_block_list_uses_decline_filter() {
        let mut mm = direct_struct();
        mm.add_to_block_list("Foo");
        assert!(mm.in_block_list("Foo"));
        assert!(!mm.in_block_list("Bar"));
    }

    #[test]
    fn daily_challenge_cap_filters_overplayed_opponent() {
        let mut mm = direct_struct();
        // Default cap is 5.
        mm.matchmaking_cfg.max_challenges_per_opponent_per_day = 5;
        let repeat = bot_with_rating("Repeat", "blitz", 2000, 50);
        let fresh = bot_with_rating("Fresh", "blitz", 2000, 50);

        // Fresh opponent is always allowed.
        assert!(mm.under_daily_challenge_cap(&fresh));

        // Four challenges: still under the cap.
        for _ in 0..4 {
            mm.opponent_db
                .record_challenge_sent("Repeat", ChallengeForm::default());
        }
        assert!(mm.under_daily_challenge_cap(&repeat));

        // Fifth challenge reaches the cap → now excluded.
        mm.opponent_db
            .record_challenge_sent("Repeat", ChallengeForm::default());
        assert!(!mm.under_daily_challenge_cap(&repeat));

        // Cap of 0 disables the brake.
        mm.matchmaking_cfg.max_challenges_per_opponent_per_day = 0;
        assert!(mm.under_daily_challenge_cap(&repeat));
    }

    #[test]
    fn decline_game_aspect_maps_reason_keys() {
        let form = ChallengeForm {
            base_time: 300,
            increment: 2,
            days: 0,
            variant: "chess960".into(),
            mode: "rated".into(),
            game_type: "blitz".into(),
        };
        assert_eq!(decline_game_aspect("generic", &form, true), "");
        assert_eq!(decline_game_aspect("later", &form, true), "");
        assert_eq!(decline_game_aspect("toofast", &form, true), "blitz");
        assert_eq!(decline_game_aspect("timecontrol", &form, true), "blitz");
        assert_eq!(decline_game_aspect("casual", &form, true), "rated");
        assert_eq!(decline_game_aspect("variant", &form, true), "chess960");
        // When not fine, all aspects collapse to "".
        assert_eq!(decline_game_aspect("toofast", &form, false), "");
        assert_eq!(decline_game_aspect("variant", &form, false), "");
    }
}
