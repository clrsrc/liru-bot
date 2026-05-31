//! Domain objects: `Game`, `Challenge`, `Player`. Mirrors `lib/model.py`.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shakmaty::fen::Fen;
use shakmaty::{CastlingMode, Chess, Position};

use crate::blocklist::OnlineBlocklist;
use crate::config::ChallengeConfig;
use crate::lichess_types::{ChallengeType, GameEventType, PlayerType, UserProfileType};
use crate::timer::{msec, sec_str, to_msec, years, Timer};

/// Detect whether a FEN already implies a Chess960 starting position.
pub fn is_chess_960(fen: &str) -> bool {
    // Compare a strict-FEN parse against a Chess960-relaxed one. If the strict
    // parse rejects but the chess960 parse accepts (or piece placement
    // differs), the position is non-standard.
    let standard = fen
        .parse::<Fen>()
        .ok()
        .and_then(|f| f.into_position::<Chess>(CastlingMode::Standard).ok());
    let chess960 = fen
        .parse::<Fen>()
        .ok()
        .and_then(|f| f.into_position::<Chess>(CastlingMode::Chess960).ok());
    match (standard, chess960) {
        (Some(a), Some(b)) => a.board() != b.board(),
        (None, Some(_)) => true,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub struct Player {
    pub title: Option<String>,
    pub rating: Option<i64>,
    pub provisional: Option<bool>,
    pub ai_level: Option<i64>,
    pub is_bot: bool,
    pub name: String,
}

impl Player {
    pub fn new(info: &PlayerType) -> Self {
        let title = info.title.clone();
        let ai_level = info.ai_level;
        let is_bot = title.as_deref() == Some("BOT") || ai_level.is_some();
        let name = if let Some(level) = ai_level {
            format!("AI level {level}")
        } else {
            info.name.clone().unwrap_or_default()
        };
        Self {
            title,
            rating: info.rating,
            provisional: info.provisional,
            ai_level,
            is_bot,
            name,
        }
    }
}

impl fmt::Display for Player {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ai_level.is_some() {
            return f.write_str(&self.name);
        }
        let rating = match (self.rating, self.provisional) {
            (Some(r), Some(true)) => format!("{r}?"),
            (Some(r), _) => r.to_string(),
            (None, _) => String::new(),
        };
        let title = self.title.as_deref().unwrap_or("");
        let parts = [title, &self.name, &format!("({rating})")];
        let joined = parts
            .iter()
            .filter(|s| !s.is_empty() && **s != "()")
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        f.write_str(joined.trim())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Termination {
    Mate,
    Timeout,
    Resign,
    Abort,
    Draw,
    Other,
}

impl Termination {
    pub fn from_status(s: &str) -> Self {
        match s {
            "mate" => Self::Mate,
            "outoftime" => Self::Timeout,
            "resign" => Self::Resign,
            "aborted" => Self::Abort,
            "draw" => Self::Draw,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Challenge {
    pub id: String,
    pub rated: bool,
    pub variant: String,
    pub perf_name: String,
    pub speed: String,
    pub increment: Option<i64>,
    pub base: Option<i64>,
    pub days: Option<i64>,
    pub challenger: Player,
    pub challenge_target: Player,
    pub from_self: bool,
    pub initial_fen: String,
    pub color: String,
    pub time_control: Option<crate::lichess_types::TimeControlType>,
}

impl Challenge {
    pub fn from_info(info: &ChallengeType, user_profile: &UserProfileType) -> Self {
        let challenger = Player::new(info.challenger.as_ref().unwrap_or(&PlayerType::default()));
        let target = Player::new(info.dest_user.as_ref().unwrap_or(&PlayerType::default()));
        let from_self = challenger.name == user_profile.username();
        let variant = info
            .variant
            .as_ref()
            .and_then(|v| v.key.clone())
            .unwrap_or_default();
        let perf_name = info
            .perf
            .as_ref()
            .and_then(|p| p.get("name").and_then(|v| v.as_str().map(|s| s.to_string())))
            .unwrap_or_default();
        let initial_fen = info
            .initial_fen
            .clone()
            .unwrap_or_else(|| "startpos".to_string());
        let color = match info.color.as_deref() {
            Some("random") => info.final_color.clone().unwrap_or_default(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let tc = info.time_control.clone();
        Self {
            id: info.id.clone().unwrap_or_default(),
            rated: info.rated.unwrap_or(false),
            variant,
            perf_name,
            speed: info.speed.clone().unwrap_or_default(),
            increment: tc.as_ref().and_then(|t| t.increment),
            base: tc.as_ref().and_then(|t| t.limit),
            days: tc.as_ref().and_then(|t| t.days_per_turn),
            challenger,
            challenge_target: target,
            from_self,
            initial_fen,
            color,
            time_control: tc,
        }
    }

    pub fn is_supported_variant(&self, cfg: &ChallengeConfig) -> bool {
        if !cfg.variants.iter().any(|v| v == &self.variant) {
            return false;
        }
        if self.initial_fen == "startpos" {
            return true;
        }
        if is_chess_960(&self.initial_fen) {
            return cfg.variants.iter().any(|v| v == "chess960");
        }
        true
    }

    pub fn is_supported_time_control(&self, cfg: &ChallengeConfig) -> bool {
        if !cfg.time_controls.iter().any(|s| s == &self.speed) {
            return false;
        }
        let require_non_zero_increment =
            self.challenger.is_bot && self.speed == "bullet" && cfg.bullet_requires_increment;
        let increment_min = cfg.min_increment.max(if require_non_zero_increment { 1 } else { 0 });

        match (self.base, self.increment, self.days) {
            (Some(base), Some(inc), _) => {
                let inc_ok = (inc as u32) >= increment_min && (inc as u32) <= cfg.max_increment;
                let base_ok = (base as u32) >= cfg.min_base
                    && cfg.max_base.map_or(true, |m| base <= m);
                inc_ok && base_ok
            }
            (_, _, Some(days)) => {
                let lo = cfg.min_days as i64;
                let hi = cfg.max_days.unwrap_or(i64::MAX);
                lo <= days && days <= hi
            }
            _ => cfg.max_days.is_none(), // unlimited only when max_days is .inf
        }
    }

    pub fn is_supported_mode(&self, cfg: &ChallengeConfig) -> bool {
        let want = if self.rated { "rated" } else { "casual" };
        cfg.modes.iter().any(|m| m == want)
    }

    pub fn is_supported_rating(
        &self,
        cfg: &ChallengeConfig,
        user_profile: &UserProfileType,
    ) -> bool {
        let challenger_rating = match self.challenger.rating {
            Some(r) => r,
            None => return true,
        };
        let mut min_rating = cfg.min_rating;
        let mut max_rating = cfg.max_rating;
        if let Some(diff) = cfg.rating_difference {
            if let Some(bot_rating) = user_profile.rating_for(&self.perf_name) {
                min_rating = min_rating.max(bot_rating - diff);
                max_rating = max_rating.min(bot_rating + diff);
            }
        }
        min_rating <= challenger_rating && challenger_rating <= max_rating
    }

    pub fn is_supported_recent(
        &self,
        cfg: &ChallengeConfig,
        recent: &mut HashMap<String, Vec<Timer>>,
    ) -> bool {
        // Check the cheap exits *before* touching the map: otherwise every
        // challenger (non-bots, and even when the limit is off) would leave a
        // permanent key behind — a slow but unbounded leak in 24/7 operation.
        let max = match cfg.max_recent_bot_challenges {
            Some(v) => v,
            None => return true,
        };
        if !self.challenger.is_bot {
            return true;
        }
        let entry = recent.entry(self.challenger.name.clone()).or_default();
        entry.retain(|t| !t.is_expired());
        let allowed = entry.len() < max as usize;
        if entry.is_empty() {
            recent.remove(&self.challenger.name);
        }
        allowed
    }

    /// Returns `("", "")` if all checks pass, otherwise `(false, decline_reason)`.
    pub fn is_supported(
        &self,
        cfg: &ChallengeConfig,
        recent_bot_challenges: &mut HashMap<String, Vec<Timer>>,
        opponent_engagements: &HashMap<String, u32>,
        online_block_list: &OnlineBlocklist,
        user_profile: &UserProfileType,
    ) -> (bool, &'static str) {
        if self.from_self {
            return (true, "");
        }

        let allowed: Vec<&str> = cfg
            .allow_list
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.as_str())
            .collect();
        let allowed_opponents = if allowed.is_empty() {
            vec![self.challenger.name.as_str()]
        } else {
            allowed
        };

        let reject = |cond: bool, reason: &'static str| -> Option<&'static str> {
            if !cond { Some(reason) } else { None }
        };

        // Run identical to Python's chained `decline_due_to(...)`.
        let mode_label: &'static str = if self.rated { "casual" } else { "rated" };
        let engaged = opponent_engagements
            .get(&self.challenger.name)
            .copied()
            .unwrap_or(0);

        let chain = reject(cfg.accept_bot || !self.challenger.is_bot, "noBot")
            .or_else(|| reject(!cfg.only_bot || self.challenger.is_bot, "onlyBot"))
            .or_else(|| reject(self.is_supported_time_control(cfg), "timeControl"))
            .or_else(|| reject(self.is_supported_variant(cfg), "variant"))
            .or_else(|| reject(self.is_supported_mode(cfg), mode_label))
            .or_else(|| reject(self.is_supported_rating(cfg, user_profile), "generic"))
            .or_else(|| reject(
                !cfg.block_list.iter().any(|b| b == &self.challenger.name),
                "generic",
            ))
            .or_else(|| reject(!online_block_list.contains(&self.challenger.name), "generic"))
            .or_else(|| reject(
                allowed_opponents.iter().any(|o| *o == self.challenger.name),
                "generic",
            ))
            .or_else(|| reject(self.is_supported_recent(cfg, recent_bot_challenges), "later"))
            .or_else(|| reject(engaged < cfg.max_simultaneous_games_per_user, "later"));

        if let Some(reason) = chain {
            return (false, reason);
        }

        (true, "")
    }

    pub fn score(&self) -> i64 {
        let rated_bonus = if self.rated { 200 } else { 0 };
        let titled_bonus = match (&self.challenger.title, self.challenger.is_bot) {
            (Some(_), false) => 200,
            _ => 0,
        };
        self.challenger.rating.unwrap_or(0) + rated_bonus + titled_bonus
    }

    pub fn mode(&self) -> &'static str {
        if self.rated { "rated" } else { "casual" }
    }
}

impl fmt::Display for Challenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} challenge from {} ({})",
            self.perf_name,
            self.mode(),
            self.challenger,
            self.id
        )
    }
}

/// Stable `chess.Board` equivalent for now is just an FEN string.
#[derive(Debug, Clone)]
pub struct Game {
    pub username: String,
    pub id: String,
    pub speed: Option<String>,
    pub clock_initial: Duration,
    pub clock_increment: Duration,
    pub perf_name: String,
    pub variant_name: String,
    /// Lichess's camelCase variant identifier (`"standard"`, `"chess960"`,
    /// `"atomic"`, …). Used as the key for `engine.polyglot.book` and
    /// other per-variant lookups. Falls back to a lower-cased variant
    /// name when the API didn't provide one.
    pub variant_key: String,
    pub mode: String,
    pub white: Player,
    pub black: Player,
    pub initial_fen: Option<String>,
    pub state: crate::lichess_types::GameStateType,
    pub is_white: bool,
    pub my_color: &'static str,
    pub opponent_color: &'static str,
    pub me: Player,
    pub opponent: Player,
    pub base_url: String,
    pub game_start: SystemTime,
    pub abort_time: Timer,
    pub terminate_time: Timer,
    pub disconnect_time: Timer,
}

impl Game {
    pub fn new(
        info: &GameEventType,
        username: &str,
        base_url: &str,
        abort_time: Duration,
    ) -> Self {
        let id = info.id.clone().unwrap_or_default();
        let speed = info.speed.clone();
        let clock = info.clock.clone();
        let ten_years_in_ms = to_msec(years(10.0));
        let clock_initial = msec(clock
            .as_ref()
            .and_then(|c| c.initial)
            .map(|n| n as f64)
            .unwrap_or(ten_years_in_ms));
        let clock_increment = msec(clock
            .as_ref()
            .and_then(|c| c.increment)
            .map(|n| n as f64)
            .unwrap_or(0.0));
        let perf_name = info
            .perf
            .as_ref()
            .and_then(|p| p.get("name").and_then(|v| v.as_str().map(|s| s.to_string())))
            .unwrap_or_else(|| "{perf?}".to_string());
        let variant_name = info
            .variant
            .as_ref()
            .and_then(|v| v.name.clone())
            .unwrap_or_default();
        let variant_key = info
            .variant
            .as_ref()
            .and_then(|v| v.key.clone())
            .unwrap_or_else(|| variant_name.to_lowercase());
        let mode = if info.rated.unwrap_or(false) { "rated" } else { "casual" }.to_string();
        let white = Player::new(info.white.as_ref().unwrap_or(&PlayerType::default()));
        let black = Player::new(info.black.as_ref().unwrap_or(&PlayerType::default()));
        let is_white = white.name.to_lowercase() == username.to_lowercase();
        let (my_color, opponent_color) = if is_white { ("white", "black") } else { ("black", "white") };
        let (me, opponent) = if is_white {
            (white.clone(), black.clone())
        } else {
            (black.clone(), white.clone())
        };
        let game_start = info
            .created_at
            .and_then(|ms| u64::try_from(ms).ok())
            .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
            .unwrap_or_else(SystemTime::now);
        let state = info.state.clone().unwrap_or_default();
        let terminate_in = clock_initial + clock_increment + abort_time + Duration::from_secs(60);

        Self {
            username: username.to_string(),
            id,
            speed,
            clock_initial,
            clock_increment,
            perf_name,
            variant_name,
            variant_key,
            mode,
            white,
            black,
            initial_fen: info.initial_fen.clone(),
            state,
            is_white,
            my_color,
            opponent_color,
            me,
            opponent,
            base_url: base_url.to_string(),
            game_start,
            abort_time: Timer::new(abort_time),
            terminate_time: Timer::new(terminate_in),
            disconnect_time: Timer::zero(),
        }
    }

    pub fn url(&self) -> String {
        format!("{}/{}", self.short_url(), self.my_color)
    }

    pub fn short_url(&self) -> String {
        match url::Url::parse(&self.base_url).and_then(|b| b.join(&self.id)) {
            Ok(u) => u.to_string(),
            Err(_) => format!("{}{}", self.base_url, self.id),
        }
    }

    pub fn pgn_event(&self) -> String {
        if matches!(self.variant_name.as_str(), "Standard" | "From Position") {
            format!("{} {} game", title_case(&self.mode), title_case(&self.perf_name))
        } else {
            format!("{} {} game", title_case(&self.mode), self.variant_name)
        }
    }

    pub fn time_control(&self) -> String {
        format!("{}+{}", sec_str(self.clock_initial), sec_str(self.clock_increment))
    }

    pub fn is_abortable(&self) -> bool {
        self.state.moves.as_deref().map_or(true, |m| !m.contains(' '))
    }

    pub fn ping(&mut self, abort_in: Duration, terminate_in: Duration, disconnect_in: Duration) {
        if self.is_abortable() {
            self.abort_time = Timer::new(abort_in);
        }
        self.terminate_time = Timer::new(terminate_in);
        self.disconnect_time = Timer::new(disconnect_in);
    }

    pub fn should_abort_now(&self) -> bool {
        self.is_abortable() && self.abort_time.is_expired()
    }

    pub fn should_terminate_now(&self) -> bool {
        self.terminate_time.is_expired()
    }

    pub fn should_disconnect_now(&self) -> bool {
        self.disconnect_time.is_expired()
    }

    pub fn my_remaining_time(&self) -> Duration {
        let wtime = msec(self.state.wtime.map(|n| n as f64).unwrap_or(0.0));
        let btime = msec(self.state.btime.map(|n| n as f64).unwrap_or(0.0));
        if self.is_white { wtime } else { btime }
    }

    pub fn result(&self) -> &'static str {
        let winner = self.state.winner.as_deref();
        let termination = self
            .state
            .status
            .as_deref()
            .map(Termination::from_status);
        match (winner, termination) {
            (Some("white"), _) => "1-0",
            (Some("black"), _) => "0-1",
            (_, Some(Termination::Draw)) | (_, Some(Termination::Timeout)) => "1/2-1/2",
            _ => "*",
        }
    }
}

impl fmt::Display for Game {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} vs {} ({})",
            self.url(),
            self.perf_name,
            self.opponent,
            self.id
        )
    }
}

/// Python `str.title()` behavior: capitalize each whitespace-delimited word.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, word) in s.split_whitespace().enumerate() {
        if i > 0 { out.push(' '); }
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            for ch in c.to_uppercase() { out.push(ch); }
            for ch in chars {
                for low in ch.to_lowercase() { out.push(low); }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termination_mapping() {
        assert_eq!(Termination::from_status("mate"), Termination::Mate);
        assert_eq!(Termination::from_status("draw"), Termination::Draw);
        assert_eq!(Termination::from_status("???"), Termination::Other);
    }

    #[test]
    fn title_case_works() {
        assert_eq!(title_case("rated bullet"), "Rated Bullet");
        assert_eq!(title_case("CASUAL chess"), "Casual Chess");
    }

    #[test]
    fn player_with_ai_level() {
        let p = Player::new(&PlayerType {
            ai_level: Some(5),
            ..Default::default()
        });
        assert_eq!(p.name, "AI level 5");
        assert!(p.is_bot);
    }

    #[test]
    fn player_with_bot_title() {
        let p = Player::new(&PlayerType {
            title: Some("BOT".into()),
            name: Some("foo".into()),
            rating: Some(2400),
            ..Default::default()
        });
        assert!(p.is_bot);
        assert_eq!(p.name, "foo");
    }

    #[test]
    fn standard_fen_is_not_chess_960() {
        assert!(!is_chess_960("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"));
    }

    // ----------------------------------------------------------------------
    // Ports of `test_bot/test_model.py` — selective cross-validation.
    // ----------------------------------------------------------------------

    #[test]
    fn is_chess_960_detects_shuffled_back_ranks() {
        // Two of the 960 starting positions Python tests against.
        // Their back-ranks are non-standard, so `is_chess_960` must
        // flag them as 960.
        assert!(is_chess_960(
            "brnkrqnb/pppppppp/8/8/8/8/PPPPPPPP/BRNKRQNB w KQkq - 0 1"
        ));
        assert!(is_chess_960(
            "nrbbnkqr/pppppppp/8/8/8/8/PPPPPPPP/NRBBNKQR w KQkq - 0 1"
        ));
    }

    #[test]
    fn player_display_for_bot_with_rating_matches_python() {
        // Python's `str(Player)` for a BOT named "b" rated 3000 returns
        // "BOT b (3000)" — same shape we render via `fmt::Display`.
        let p = Player::new(&PlayerType {
            title: Some("BOT".into()),
            name: Some("b".into()),
            rating: Some(3000),
            ..Default::default()
        });
        assert_eq!(format!("{p}"), "BOT b (3000)");
    }

    #[test]
    fn player_display_for_ai_level_is_just_name() {
        let p = Player::new(&PlayerType {
            ai_level: Some(5),
            ..Default::default()
        });
        assert_eq!(format!("{p}"), "AI level 5");
    }

    /// Build a Bullet `Game` matching the fixture in
    /// `test_model.py::test_game` so our derived fields (`mode`,
    /// `my_color`, `url`, `pgn_event`, `time_control`, `is_abortable`)
    /// match Python value-for-value.
    fn fixture_bullet_game() -> Game {
        use crate::lichess_types::{
            GameEventType, GameStateType, JsonValue, PlayerType, TimeControlType, VariantInfo,
        };
        let mut info = GameEventType::default();
        info.id = Some("zzzzzzzz".into());
        info.speed = Some("bullet".into());
        info.rated = Some(false);
        info.created_at = Some(1_700_000_000_000);
        info.initial_fen = Some("startpos".into());
        info.white = Some(PlayerType {
            name: Some("c".into()),
            rating: Some(2000),
            ..Default::default()
        });
        info.black = Some(PlayerType {
            title: Some("BOT".into()),
            name: Some("b".into()),
            rating: Some(3000),
            ..Default::default()
        });
        info.variant = Some(VariantInfo {
            key: Some("standard".into()),
            name: Some("Standard".into()),
            ..Default::default()
        });
        let mut perf = std::collections::HashMap::new();
        perf.insert("name".to_string(), JsonValue::String("Bullet".into()));
        info.perf = Some(perf);
        info.clock = Some(TimeControlType {
            initial: Some(90_000),
            increment: Some(1_000),
            ..Default::default()
        });
        let mut state = GameStateType::default();
        state.moves = Some(String::new());
        state.wtime = Some(90_000);
        state.btime = Some(90_000);
        state.winc = Some(1_000);
        state.binc = Some(1_000);
        state.status = Some("started".into());
        info.state = Some(state);
        Game::new(&info, "b", "https://lichess.org/", Duration::from_secs(30))
    }

    #[test]
    fn game_id_mode_color_match_python_fixture() {
        let g = fixture_bullet_game();
        assert_eq!(g.id, "zzzzzzzz");
        assert_eq!(g.mode, "casual");
        assert!(!g.is_white);
        assert_eq!(g.my_color, "black");
    }

    #[test]
    fn game_urls_match_python_fixture() {
        let g = fixture_bullet_game();
        assert_eq!(g.url(), "https://lichess.org/zzzzzzzz/black");
        assert_eq!(g.short_url(), "https://lichess.org/zzzzzzzz");
    }

    #[test]
    fn pgn_event_and_time_control_match_python_fixture() {
        let g = fixture_bullet_game();
        assert_eq!(g.pgn_event(), "Casual Bullet game");
        assert_eq!(g.time_control(), "90+1");
    }

    #[test]
    fn is_abortable_true_with_zero_moves() {
        let g = fixture_bullet_game();
        assert!(g.is_abortable());
    }

    #[test]
    fn is_abortable_false_once_both_sides_have_moved() {
        let mut g = fixture_bullet_game();
        // Two plies recorded → contains a space → not abortable anymore.
        g.state.moves = Some("e2e4 e7e5".into());
        assert!(!g.is_abortable());
    }

    #[test]
    fn should_abort_now_fires_after_abort_window_elapses() {
        // Reproduces the sp4ifq8J failure mode in miniature: a fresh
        // bot game with the abort timer set to a sub-millisecond window
        // must flip `should_abort_now` once it expires.
        let mut g = fixture_bullet_game();
        g.ping(Duration::from_millis(1), Duration::from_secs(3600), Duration::from_secs(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(g.should_abort_now(), "abortable game with expired abort_time must abort");
    }

    #[test]
    fn should_abort_now_false_once_both_sides_have_moved() {
        let mut g = fixture_bullet_game();
        g.state.moves = Some("e2e4 e7e5".into());
        g.ping(Duration::from_millis(1), Duration::from_secs(3600), Duration::from_secs(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!g.should_abort_now(), "non-abortable game must never abort even past abort_time");
    }

    #[test]
    fn should_terminate_now_fires_after_terminate_window_elapses() {
        let mut g = fixture_bullet_game();
        g.ping(Duration::from_secs(3600), Duration::from_millis(1), Duration::from_secs(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(g.should_terminate_now());
    }
}
