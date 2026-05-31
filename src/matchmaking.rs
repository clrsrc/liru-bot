//! Outgoing bot-vs-bot challenges. Rust port of `lib/matchmaking.py`.
//!
//! The [`Matchmaking`] struct holds all per-bot state (rate limits, the
//! per-opponent decline cooldowns, …) and exposes one network-driven
//! method, [`Matchmaking::challenge`], that the main loop calls every tick.

use std::collections::HashMap;
use std::time::Duration;

use chrono::Local;
use rand::distributions::WeightedIndex;
use rand::prelude::*;
use rand::rngs::StdRng;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::blocklist::OnlineBlocklist;
use crate::config::{Config, MatchmakingConfig, MatchmakingOverride};
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
}

impl Matchmaking {
    pub fn new(
        li: Lichess,
        config: &Config,
        user_profile: UserProfileType,
        online_block_list: OnlineBlocklist,
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
        };

        let block_list = mm.matchmaking_cfg.block_list.clone();
        for name in block_list {
            mm.add_to_block_list(&name);
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
            if let Some(timeout) = response.rate_limit_timeout {
                self.rate_limit_timer = Timer::new(timeout);
            }
        } else if response.opponent_is_rate_limited == Some(true) {
            self.add_challenge_filter(username, "", response.rate_limit_timeout);
        } else {
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
        let online_bots_raw = self.li.get_online_bots(None).await;

        let online_bots: Vec<UserProfileType> = online_bots_raw
            .into_iter()
            .filter(|bot| self.is_suitable_opponent(bot, &game_type, min_rating, max_rating))
            .collect();

        let ready_bots: Vec<UserProfileType> = online_bots
            .iter()
            .filter(|bot| self.ready_for_challenge(bot, &variant, &game_type, &mode))
            .cloned()
            .collect();
        let candidates = if !ready_bots.is_empty() {
            ready_bots
        } else {
            online_bots
        };

        let weights = Self::get_weights(
            &candidates,
            &rating_preference,
            min_rating,
            max_rating,
            &game_type,
        );

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

    pub fn accepted_challenge(&mut self, event: &EventType) {
        if let Some(id) = event.game.as_ref().and_then(|g| g.id.clone()) {
            // When an opponent accepts our outbound challenge the resulting
            // game id equals the challenge id. Only then do we credit the
            // game to the opponent we last challenged.
            if id == self.challenge_id && !self.challenge_target.is_empty() {
                let target = std::mem::take(&mut self.challenge_target);
                self.opponent_db.record_accepted(&target);
            }
            self.discard_challenge(&id);
        }
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
        }
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
}
