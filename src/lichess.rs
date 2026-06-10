//! HTTP/streaming client for lichess.org.
//!
//! Rust port of `lib/lichess.py`. Same endpoint surface, rate-limit tracking,
//! and constant-interval backoff (`max_time = 60 s`, `interval = 100 ms`).
//!
//! Differences vs. Python:
//!
//! - All I/O is `async` (the Python version is synchronous on top of `requests`).
//!   Streaming endpoints return an `impl Stream` over typed events instead of
//!   leaking the underlying `Response`.
//! - The constructor [`Lichess::connect`] is fallible and validates the OAuth
//!   token before returning. Python does the same in `__init__` — we just make
//!   the failure explicit in the return type.
//! - No module-level `Stop` singleton. Shutdown is wired in by `lichess_bot.rs`
//!   later — for now in-flight retries simply respect the `max_time` budget.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream::{Stream, TryStreamExt};
use reqwest::{Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;
use tracing::{debug, warn};
use url::Url;

use crate::lichess_types::{
    ChallengeType, EventType, GameEventType, GameType, OnlineType, PublicDataType, TokenTests,
    UserProfileType,
};
use crate::timer::{sec_str, seconds, Timer};

/// Maximum number of characters in a chat message (Lichess hard limit).
pub const MAX_CHAT_MESSAGE_LEN: usize = 140;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const BACKOFF_MAX_TIME: Duration = Duration::from_secs(60);
const BACKOFF_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Endpoint catalogue
// ---------------------------------------------------------------------------

/// Path templates supported by the bot API. Values mirror `ENDPOINTS` in
/// `lib/lichess.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Profile,
    Playing,
    Stream,
    StreamEvent,
    Move,
    Takeback,
    Chat,
    Abort,
    Accept,
    Decline,
    Upgrade,
    Resign,
    ClaimVictory,
    ClaimDraw,
    Draw,
    Export,
    OnlineBots,
    Challenge,
    Cancel,
    Status,
    PublicData,
    TokenTest,
}

impl Endpoint {
    pub const fn template(self) -> &'static str {
        match self {
            Endpoint::Profile => "/api/account",
            Endpoint::Playing => "/api/account/playing",
            Endpoint::Stream => "/api/bot/game/stream/{}",
            Endpoint::StreamEvent => "/api/stream/event",
            Endpoint::Move => "/api/bot/game/{}/move/{}",
            Endpoint::Takeback => "/api/bot/game/{}/takeback/{}",
            Endpoint::Chat => "/api/bot/game/{}/chat",
            Endpoint::Abort => "/api/bot/game/{}/abort",
            Endpoint::Accept => "/api/challenge/{}/accept",
            Endpoint::Decline => "/api/challenge/{}/decline",
            Endpoint::Upgrade => "/api/bot/account/upgrade",
            Endpoint::Resign => "/api/bot/game/{}/resign",
            Endpoint::ClaimVictory => "/api/bot/game/{}/claim-victory",
            Endpoint::ClaimDraw => "/api/bot/game/{}/claim-draw",
            Endpoint::Draw => "/api/bot/game/{}/draw/{}",
            Endpoint::Export => "/game/export/{}",
            Endpoint::OnlineBots => "/api/bot/online",
            Endpoint::Challenge => "/api/challenge/{}",
            Endpoint::Cancel => "/api/challenge/{}/cancel",
            Endpoint::Status => "/api/users/status",
            Endpoint::PublicData => "/api/user/{}",
            Endpoint::TokenTest => "/api/token/test",
        }
    }

    /// Render a concrete path by substituting `{}` placeholders in the
    /// template with `args`. We use this in two places — for the path
    /// component of the URL and as a stable key in the rate-limit table.
    fn render(self, args: &[&str]) -> String {
        let mut out = String::with_capacity(self.template().len() + 16);
        let mut arg_iter = args.iter();
        let mut chars = self.template().chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' && chars.peek() == Some(&'}') {
                chars.next();
                if let Some(a) = arg_iter.next() {
                    out.push_str(a);
                } else {
                    out.push_str("{}");
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum LichessError {
    #[error("endpoint {path} is rate-limited, retry in {timeout:?}")]
    RateLimited {
        path: &'static str,
        timeout: Duration,
    },

    #[error("invalid OAuth token: {0}")]
    Token(String),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("backoff exhausted after {tries} attempt(s): {source}")]
    Backoff {
        tries: u32,
        #[source]
        source: Box<LichessError>,
    },

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json decode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("stream decode error: {0}")]
    LinesDecode(#[from] tokio_util::codec::LinesCodecError),
}

pub type LichessResult<T> = Result<T, LichessError>;

impl LichessError {
    /// Is this an error that retrying cannot fix? (4xx, token errors, URL
    /// errors, …)
    fn is_final(&self) -> bool {
        match self {
            LichessError::Http(e) => match e.status() {
                Some(status) => status.as_u16() < 500,
                None => false,
            },
            LichessError::RateLimited { .. }
            | LichessError::Token(_)
            | LichessError::Url(_)
            | LichessError::Json(_) => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Body kind for POST requests
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PostBody<'a> {
    Empty,
    Raw(String),
    Form(&'a [(&'a str, &'a str)]),
    Json(&'a serde_json::Value),
}

// ---------------------------------------------------------------------------
// Lichess client
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct State {
    /// Per-endpoint rate-limit timers, keyed by the *template* string so all
    /// instances of `/api/bot/game/{}/move/{}` share a single timer.
    rate_limit_timers: HashMap<&'static str, Timer>,
}

#[derive(Clone)]
pub struct Lichess {
    client: Client,
    other_client: Client,
    base_url: Url,
    token: String,
    version: String,
    max_retries: u32,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Lichess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lichess")
            .field("base_url", &self.base_url.as_str())
            .field("version", &self.version)
            .field("max_retries", &self.max_retries)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Lichess {
    /// Build a client without contacting Lichess. Used by tests in other
    /// modules (hence `pub(crate)`) and by [`connect`](Self::connect) below.
    pub(crate) fn new_raw(
        token: String,
        url: Url,
        version: String,
        max_retries: u32,
    ) -> LichessResult<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        let auth_value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                LichessError::Token("token contains characters illegal in an HTTP header".into())
            })?;
        headers.insert(reqwest::header::AUTHORIZATION, auth_value);

        let mut bot = Self {
            client: build_client(headers.clone(), &version, "?")?,
            other_client: build_client(reqwest::header::HeaderMap::new(), &version, "?")?,
            base_url: url,
            token,
            version,
            max_retries,
            state: Arc::new(Mutex::new(State::default())),
        };
        bot.set_user_agent("?")?;
        Ok(bot)
    }

    /// Build a client *and* validate the OAuth token (`/api/token/test`).
    ///
    /// Returns `LichessError::Token` if the token is unknown to Lichess or
    /// doesn't have the `bot:play` scope.
    pub async fn connect(
        token: String,
        url: Url,
        version: String,
        max_retries: u32,
    ) -> LichessResult<Self> {
        let bot = Self::new_raw(token.clone(), url, version, max_retries)?;
        bot.validate_token().await?;
        Ok(bot)
    }

    async fn validate_token(&self) -> LichessResult<()> {
        let response = self
            .send_post(Endpoint::TokenTest, &[], PostBody::Raw(self.token.clone()), &[], true)
            .await?;
        let body: TokenTests = response.json().await?;
        let info = body.get(&self.token).and_then(|opt| opt.as_ref()).ok_or_else(|| {
            LichessError::Token(
                "Could not retrieve token information. Check the bot token in your config."
                    .into(),
            )
        })?;
        let scopes = info.scopes.as_deref().unwrap_or("");
        if !scopes.split(',').any(|s| s.trim() == "bot:play") {
            return Err(LichessError::Token(format!(
                "Token is missing the bot:play scope. Current scopes: {scopes}"
            )));
        }
        Ok(())
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Update the `User-Agent` to include the bot's username (called once the
    /// profile is known).
    pub fn set_user_agent(&mut self, username: &str) -> LichessResult<()> {
        let mut headers = reqwest::header::HeaderMap::new();
        let auth_value =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token)).map_err(
                |_| LichessError::Token("token has illegal HTTP header characters".into()),
            )?;
        headers.insert(reqwest::header::AUTHORIZATION, auth_value);

        self.client = build_client(headers, &self.version, username)?;
        self.other_client =
            build_client(reqwest::header::HeaderMap::new(), &self.version, username)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Rate-limit helpers
    // -----------------------------------------------------------------------

    fn set_rate_limit_delay(&self, path_template: &'static str, delay: Duration) {
        warn!(
            endpoint = path_template,
            delay = sec_str(delay),
            "endpoint is rate-limited, pausing"
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .rate_limit_timers
            .insert(path_template, Timer::new(delay));
    }

    fn check_rate_limit(&self, endpoint: Endpoint) -> LichessResult<()> {
        let template = endpoint.template();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(timer) = state.rate_limit_timers.get(template) {
            if !timer.is_expired() {
                return Err(LichessError::RateLimited {
                    path: template,
                    timeout: timer.time_until_expiration(),
                });
            }
        }
        Ok(())
    }

    /// Delay to apply when an endpoint returns HTTP 429. The move endpoint
    /// gets a short 1 s pause — a 60 s block would forfeit every live game on
    /// time — while all other endpoints follow Lichess's recommended ~1 min
    /// cool-down. Kept in one place so the GET and POST paths can't diverge.
    fn rate_limit_delay_for(endpoint: Endpoint) -> Duration {
        if endpoint == Endpoint::Move {
            seconds(1.0)
        } else {
            seconds(60.0)
        }
    }

    // -----------------------------------------------------------------------
    // HTTP core — used by every high-level method below
    // -----------------------------------------------------------------------

    async fn send_get(
        &self,
        endpoint: Endpoint,
        args: &[&str],
        params: &[(&str, &str)],
        timeout: Option<Duration>,
    ) -> LichessResult<Response> {
        self.check_rate_limit(endpoint)?;
        let path = endpoint.render(args);
        let url = self.base_url.join(&path)?;

        self.with_backoff(|| async {
            let mut req = self.client.get(url.clone());
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            if !params.is_empty() {
                req = req.query(params);
            }
            let response = req.send().await?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.set_rate_limit_delay(endpoint.template(), Self::rate_limit_delay_for(endpoint));
            }

            Ok(response.error_for_status()?)
        })
        .await
    }

    /// Single-attempt GET for streaming endpoints — no backoff retry loop.
    ///
    /// `get_event_stream` and `get_game_stream` use this instead of
    /// `send_get`. The outer reconnect loops in `lichess_bot.rs` own the
    /// retry cadence (exponential 5 s → 60 s). Using `with_backoff` here
    /// would retry at 100 ms intervals for up to 60 s on a transient
    /// network blip — ≈ 600 rapid requests that trigger Lichess 429 and
    /// can cascade to kill game streams that are still alive.
    async fn try_get_stream(
        &self,
        endpoint: Endpoint,
        args: &[&str],
    ) -> LichessResult<Response> {
        self.check_rate_limit(endpoint)?;
        let path = endpoint.render(args);
        let url = self.base_url.join(&path)?;
        let response = self.client.get(url).send().await?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            self.set_rate_limit_delay(endpoint.template(), Self::rate_limit_delay_for(endpoint));
        }
        Ok(response.error_for_status()?)
    }

    async fn send_post(
        &self,
        endpoint: Endpoint,
        args: &[&str],
        body: PostBody<'_>,
        params: &[(&str, &str)],
        raise_for_status: bool,
    ) -> LichessResult<Response> {
        self.check_rate_limit(endpoint)?;
        let path = endpoint.render(args);
        let url = self.base_url.join(&path)?;

        self.with_backoff(|| async {
            let mut req = self.client.post(url.clone()).timeout(REQUEST_TIMEOUT);
            if !params.is_empty() {
                req = req.query(params);
            }
            req = match &body {
                PostBody::Empty => req,
                PostBody::Raw(s) => req.body(s.clone()),
                PostBody::Form(pairs) => req.form(*pairs),
                PostBody::Json(v) => req.json(*v),
            };

            let response = req.send().await?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                self.set_rate_limit_delay(endpoint.template(), Self::rate_limit_delay_for(endpoint));
            }

            if raise_for_status {
                Ok(response.error_for_status()?)
            } else {
                Ok(response)
            }
        })
        .await
    }

    async fn with_backoff<F, Fut>(&self, mut op: F) -> LichessResult<Response>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = LichessResult<Response>>,
    {
        let deadline = Instant::now() + BACKOFF_MAX_TIME;
        let mut tries: u32 = 0;
        loop {
            tries += 1;
            match op().await {
                Ok(r) => return Ok(r),
                Err(e) if e.is_final() => return Err(e),
                Err(e) if Instant::now() >= deadline => {
                    return Err(LichessError::Backoff {
                        tries,
                        source: Box::new(e),
                    });
                }
                Err(e) => {
                    debug!(tries, error = %e, "backing off");
                    tokio::time::sleep(BACKOFF_INTERVAL).await;
                }
            }
        }
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        endpoint: Endpoint,
        args: &[&str],
        params: &[(&str, &str)],
    ) -> LichessResult<T> {
        let response = self
            .send_get(endpoint, args, params, Some(REQUEST_TIMEOUT))
            .await?;
        Ok(response.json().await?)
    }

    async fn get_text(
        &self,
        endpoint: Endpoint,
        args: &[&str],
        params: &[(&str, &str)],
    ) -> LichessResult<String> {
        let response = self
            .send_get(endpoint, args, params, Some(REQUEST_TIMEOUT))
            .await?;
        Ok(response.text().await?)
    }

    // -----------------------------------------------------------------------
    // High-level API surface — one method per ENDPOINTS entry in Python
    // -----------------------------------------------------------------------

    /// `POST /api/bot/account/upgrade` — upgrade the current account to BOT.
    pub async fn upgrade_to_bot_account(&self) -> LichessResult<()> {
        self.send_post(Endpoint::Upgrade, &[], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/move/{uci}?offeringDraw=…`
    pub async fn make_move(
        &self,
        game_id: &str,
        uci_move: &str,
        offering_draw: bool,
    ) -> LichessResult<()> {
        let draw = if offering_draw { "true" } else { "false" };
        self.send_post(
            Endpoint::Move,
            &[game_id, uci_move],
            PostBody::Empty,
            &[("offeringDraw", draw)],
            true,
        )
        .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/takeback/{yes|no}` — returns `true` if the
    /// request went through (so the caller knows whether the takeback was
    /// accepted).
    pub async fn accept_takeback(&self, game_id: &str, accept: bool) -> bool {
        let arg = if accept { "yes" } else { "no" };
        match self
            .send_post(Endpoint::Takeback, &[game_id, arg], PostBody::Empty, &[], true)
            .await
        {
            Ok(_) => accept,
            Err(_) => false,
        }
    }

    /// `POST /api/bot/game/{id}/chat`
    pub async fn chat(&self, game_id: &str, room: &str, text: &str) -> LichessResult<()> {
        if text.chars().count() > MAX_CHAT_MESSAGE_LEN {
            warn!(
                len = text.chars().count(),
                "chat message exceeds {MAX_CHAT_MESSAGE_LEN} characters, dropping"
            );
            return Ok(());
        }
        let body = [("room", room), ("text", text)];
        self.send_post(Endpoint::Chat, &[game_id], PostBody::Form(&body), &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/abort`
    pub async fn abort(&self, game_id: &str) -> LichessResult<()> {
        self.send_post(Endpoint::Abort, &[game_id], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/challenge/{id}/accept`
    pub async fn accept_challenge(&self, challenge_id: &str) -> LichessResult<()> {
        self.send_post(Endpoint::Accept, &[challenge_id], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/challenge/{id}/decline` — never raises, mirrors Python's
    /// `contextlib.suppress(Exception)`.
    pub async fn decline_challenge(&self, challenge_id: &str, reason: &str) {
        let body = [("reason", reason)];
        let _ = self
            .send_post(
                Endpoint::Decline,
                &[challenge_id],
                PostBody::Form(&body),
                &[],
                false,
            )
            .await;
    }

    /// `GET /api/account` — returns the bot's profile and updates the
    /// user agent accordingly.
    pub async fn get_profile(&mut self) -> LichessResult<UserProfileType> {
        let profile: UserProfileType = self.get_json(Endpoint::Profile, &[], &[]).await?;
        if let Some(name) = profile.username.as_deref() {
            self.set_user_agent(name)?;
        }
        Ok(profile)
    }

    /// `GET /api/account/playing` — returns `None` on any error (Python
    /// suppresses exceptions here).
    pub async fn get_ongoing_games(&self) -> Option<Vec<GameType>> {
        #[derive(serde::Deserialize)]
        struct Playing {
            #[serde(rename = "nowPlaying")]
            now_playing: Vec<GameType>,
        }
        let r: LichessResult<Playing> = self.get_json(Endpoint::Playing, &[], &[]).await;
        r.ok().map(|p| p.now_playing)
    }

    /// `POST /api/bot/game/{id}/resign`
    pub async fn resign(&self, game_id: &str) -> LichessResult<()> {
        self.send_post(Endpoint::Resign, &[game_id], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/claim-victory` — claim the win when the
    /// opponent's disconnection timer has run out (the `opponentGone` event
    /// reports `claimWinInSeconds: 0`).
    pub async fn claim_victory(&self, game_id: &str) -> LichessResult<()> {
        self.send_post(Endpoint::ClaimVictory, &[game_id], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/draw/{yes|no}` — accept (`yes`) or decline
    /// (`no`) the opponent's standing draw offer. (Sending `yes` with no
    /// pending offer instead *offers* a draw; we only call it to accept.)
    pub async fn handle_draw_offer(&self, game_id: &str, accept: bool) -> LichessResult<()> {
        let arg = if accept { "yes" } else { "no" };
        self.send_post(Endpoint::Draw, &[game_id, arg], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `POST /api/bot/game/{id}/claim-draw` — claim a draw by the fifty-move
    /// rule or threefold repetition once the position qualifies.
    pub async fn claim_draw(&self, game_id: &str) -> LichessResult<()> {
        self.send_post(Endpoint::ClaimDraw, &[game_id], PostBody::Empty, &[], true)
            .await?;
        Ok(())
    }

    /// `GET /game/export/{id}` — returns the PGN text. Empty string on error.
    pub async fn get_game_pgn(&self, game_id: &str) -> String {
        self.get_text(Endpoint::Export, &[game_id], &[])
            .await
            .unwrap_or_default()
    }

    /// `GET /api/bot/online` — returns the list of online bots (empty on
    /// error). Parses the NDJSON body in one shot.
    ///
    /// `nb` caps how many bots Lichess returns; pass `None` for the
    /// server default (currently 100). Lichess maxes out around 300.
    pub async fn get_online_bots(&self, nb: Option<u32>) -> Vec<UserProfileType> {
        let nb_str = nb.map(|n| n.to_string());
        let params: Vec<(&str, &str)> = match &nb_str {
            Some(s) => vec![("nb", s.as_str())],
            None => Vec::new(),
        };
        let Ok(text) = self.get_text(Endpoint::OnlineBots, &[], &params).await else {
            return Vec::new();
        };
        text.lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// `POST /api/challenge/{user}` — returns the challenge response with
    /// rate-limit fields populated where applicable.
    pub async fn challenge(
        &self,
        username: &str,
        payload: &serde_json::Value,
    ) -> LichessResult<ChallengeType> {
        let response = self
            .send_post(
                Endpoint::Challenge,
                &[username],
                PostBody::Json(payload),
                &[],
                false,
            )
            .await?;

        let status = response.status();
        let bytes = response.bytes().await?;
        let mut challenge: ChallengeType = serde_json::from_slice(&bytes)?;

        let bot_rate_limited =
            status == StatusCode::TOO_MANY_REQUESTS && is_daily_game_rate_limit(&challenge);
        let opp_rate_limited =
            status == StatusCode::BAD_REQUEST && is_daily_game_rate_limit(&challenge);

        if bot_rate_limited || opp_rate_limited {
            let delay = challenge
                .ratelimit
                .as_ref()
                .and_then(|m| m.get("seconds"))
                .and_then(|v| v.as_f64())
                .map(seconds)
                .unwrap_or_else(|| seconds(60.0));
            if bot_rate_limited {
                self.set_rate_limit_delay(Endpoint::Challenge.template(), delay);
            }
            challenge.bot_is_rate_limited = Some(bot_rate_limited);
            challenge.opponent_is_rate_limited = Some(opp_rate_limited);
            challenge.rate_limit_timeout = Some(delay);
        }
        Ok(challenge)
    }

    /// `POST /api/challenge/{id}/cancel`
    pub async fn cancel(&self, challenge_id: &str) -> LichessResult<()> {
        let _ = self
            .send_post(Endpoint::Cancel, &[challenge_id], PostBody::Empty, &[], false)
            .await?;
        Ok(())
    }

    /// `GET /api/users/status?ids=<user>` — is the bot's account online?
    pub async fn is_online(&self, user_id: &str) -> bool {
        let list: LichessResult<Vec<UserProfileType>> = self
            .get_json(Endpoint::Status, &[], &[("ids", user_id)])
            .await;
        list.ok()
            .and_then(|v| v.into_iter().next())
            .and_then(|u| u.online)
            .unwrap_or(false)
    }

    /// `GET /api/user/{name}` — public profile data.
    pub async fn get_public_data(&self, user_name: &str) -> LichessResult<PublicDataType> {
        self.get_json(Endpoint::PublicData, &[user_name], &[]).await
    }

    /// `GET <external-url>?<params>` — used for chessdb / lichess online
    /// books / EGTBs. Retries up to `max_retries` times with the same
    /// constant backoff as the rest of the client.
    pub async fn online_book_get(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> LichessResult<OnlineType> {
        let mut tries: u32 = 0;
        let deadline = Instant::now() + BACKOFF_MAX_TIME;
        loop {
            tries += 1;
            let mut req = self.other_client.get(url).timeout(REQUEST_TIMEOUT);
            if !params.is_empty() {
                req = req.query(params);
            }
            match req.send().await {
                Ok(response) => match response.error_for_status() {
                    Ok(r) => return Ok(r.json().await?),
                    Err(e) => {
                        let status = e.status().map(|s| s.as_u16()).unwrap_or(0);
                        if status < 500 || tries >= self.max_retries || Instant::now() >= deadline
                        {
                            return Err(LichessError::Http(e));
                        }
                    }
                },
                Err(e) => {
                    if tries >= self.max_retries || Instant::now() >= deadline {
                        return Err(LichessError::Http(e));
                    }
                }
            }
            tokio::time::sleep(BACKOFF_INTERVAL).await;
        }
    }

    // -----------------------------------------------------------------------
    // Streaming endpoints (NDJSON)
    // -----------------------------------------------------------------------

    /// `GET /api/stream/event` — stream of incoming challenges / game starts.
    pub async fn get_event_stream(
        &self,
    ) -> LichessResult<impl Stream<Item = LichessResult<EventType>> + Send + 'static> {
        let response = self.try_get_stream(Endpoint::StreamEvent, &[]).await?;
        Ok(ndjson_stream::<EventType>(response))
    }

    /// `GET /api/bot/game/stream/{id}` — stream of moves / chats for one game.
    pub async fn get_game_stream(
        &self,
        game_id: &str,
    ) -> LichessResult<impl Stream<Item = LichessResult<GameEventType>> + Send + 'static> {
        let response = self.try_get_stream(Endpoint::Stream, &[game_id]).await?;
        Ok(ndjson_stream::<GameEventType>(response))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_client(
    mut headers: reqwest::header::HeaderMap,
    version: &str,
    username: &str,
) -> LichessResult<Client> {
    let ua = format!("liru-bot/{version} user:{username}");
    let ua_value = reqwest::header::HeaderValue::from_str(&ua).map_err(|_| {
        LichessError::Token("username produces a non-ASCII User-Agent header".into())
    })?;
    headers.insert(reqwest::header::USER_AGENT, ua_value);

    Client::builder()
        .default_headers(headers)
        // Force HTTP/1.1: Lichess streams stay alive for hours over h1, but
        // we observed reqwest's default h2 dropping the event stream every
        // ~5 s with no real events — Python lichess-bot uses urllib3 (h1
        // only) and doesn't see this. Stream pipelining is a non-feature
        // here anyway since each endpoint is its own request.
        .http1_only()
        // Send a TCP keep-alive every 30 s so dead-but-not-FIN'd connections
        // surface as read errors instead of silently hanging.
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(LichessError::from)
}

fn is_daily_game_rate_limit(challenge: &ChallengeType) -> bool {
    let Some(rl) = challenge.ratelimit.as_ref() else {
        return false;
    };
    challenge.error.is_some()
        && rl.get("key").and_then(|v| v.as_str()) == Some("bot.vsBot.day")
}

/// Convert a streaming `Response` into a `Stream` of decoded NDJSON items.
fn ndjson_stream<T>(response: Response) -> impl Stream<Item = LichessResult<T>> + Send + 'static
where
    T: DeserializeOwned + Send + 'static,
{
    let byte_stream = response
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    let reader = StreamReader::new(byte_stream);
    FramedRead::new(reader, LinesCodec::new())
        .map_err(LichessError::from)
        .try_filter(|line| futures::future::ready(!line.is_empty()))
        .and_then(|line| async move {
            // A single line that doesn't match the expected type (an unknown
            // event or a keep-alive fragment) must not tear down the whole
            // stream — map a parse failure to `None` (skip) rather than an
            // error. Genuine IO/stream errors still propagate below.
            Ok(serde_json::from_str::<T>(&line)
                .map_err(|e| debug!(error = %e, %line, "skipping unparseable NDJSON line"))
                .ok())
        })
        .try_filter_map(|opt| futures::future::ready(Ok(opt)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_render_substitutes_positional_args() {
        assert_eq!(Endpoint::Profile.render(&[]), "/api/account");
        assert_eq!(
            Endpoint::Move.render(&["abc123", "e2e4"]),
            "/api/bot/game/abc123/move/e2e4"
        );
        assert_eq!(
            Endpoint::PublicData.render(&["StefanBot"]),
            "/api/user/StefanBot"
        );
    }

    #[test]
    fn endpoint_render_keeps_braces_for_missing_args() {
        // Defensive: a caller that forgets an argument should still produce
        // something we can spot in a log instead of panicking.
        assert_eq!(Endpoint::Stream.render(&[]), "/api/bot/game/stream/{}");
    }

    #[test]
    fn lichess_error_is_final_classification() {
        let url_err = url::Url::parse("not a url").unwrap_err();
        assert!(LichessError::Url(url_err).is_final());

        assert!(LichessError::Token("missing scope".into()).is_final());
        assert!(LichessError::RateLimited {
            path: Endpoint::Move.template(),
            timeout: Duration::from_secs(1)
        }
        .is_final());
    }

    #[test]
    fn rate_limit_check_blocks_until_timer_expires() {
        let url = Url::parse("https://lichess.org/").unwrap();
        let bot = Lichess::new_raw("dummy".into(), url, "0".into(), 3).unwrap();
        bot.set_rate_limit_delay(Endpoint::Move.template(), Duration::from_secs(10));
        let err = bot.check_rate_limit(Endpoint::Move).unwrap_err();
        match err {
            LichessError::RateLimited { path, timeout } => {
                assert_eq!(path, Endpoint::Move.template());
                assert!(timeout <= Duration::from_secs(10));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_check_passes_after_zero_timer() {
        let url = Url::parse("https://lichess.org/").unwrap();
        let bot = Lichess::new_raw("dummy".into(), url, "0".into(), 3).unwrap();
        bot.set_rate_limit_delay(Endpoint::Profile.template(), Duration::ZERO);
        assert!(bot.check_rate_limit(Endpoint::Profile).is_ok());
    }

    // -----------------------------------------------------------------------
    // wiremock integration tests
    // -----------------------------------------------------------------------

    use futures::StreamExt;
    use wiremock::matchers::{body_string, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// Build a Lichess client pointing at the wiremock server (no token check).
    fn make_client(server: &MockServer) -> Lichess {
        let url = Url::parse(&server.uri()).unwrap();
        Lichess::new_raw("test-token".into(), url, "0.1.0".into(), 3).unwrap()
    }

    #[tokio::test]
    async fn make_move_posts_uci_with_draw_offer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/move/e2e4"))
            .and(query_param("offeringDraw", "true"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        bot.make_move("gameid", "e2e4", true).await.unwrap();
    }

    #[tokio::test]
    async fn claim_victory_posts_to_correct_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/g42/claim-victory"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
            .expect(1)
            .mount(&server)
            .await;

        let bot = make_client(&server);
        bot.claim_victory("g42").await.unwrap();
    }

    #[tokio::test]
    async fn chat_sends_form_encoded_room_and_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/g1/chat"))
            .and(body_string("room=player&text=gg+wp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        bot.chat("g1", "player", "gg wp").await.unwrap();
    }

    #[tokio::test]
    async fn get_profile_decodes_and_updates_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id":"bot1","username":"BotOne"})),
            )
            .mount(&server)
            .await;

        let mut bot = make_client(&server);
        let profile = bot.get_profile().await.unwrap();
        assert_eq!(profile.username.as_deref(), Some("BotOne"));
        // user-agent rebuilt; we can't read it back from the client directly,
        // but we can verify the call didn't error and the field is set above.
    }

    #[tokio::test]
    async fn rate_limit_response_sets_timer_and_blocks_next_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        // First call: gets 429 + records the rate-limit timer. With a 60 s
        // delay and 100 ms backoff interval the retry loop will exhaust its
        // budget — but that's fine, we only care that the timer was set.
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            let _ = bot.get_json::<UserProfileType>(Endpoint::Profile, &[], &[]).await;
        })
        .await;

        // Second call: should be short-circuited by check_rate_limit.
        let err = bot
            .get_json::<UserProfileType>(Endpoint::Profile, &[], &[])
            .await
            .unwrap_err();
        assert!(matches!(err, LichessError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn validate_token_rejects_missing_bot_play_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token/test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "test-token": { "scopes": "preference:read", "userId": "stefan" }
            })))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let err = Lichess::connect("test-token".into(), url, "0".into(), 3)
            .await
            .unwrap_err();
        assert!(matches!(err, LichessError::Token(_)));
    }

    #[tokio::test]
    async fn validate_token_accepts_bot_play_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token/test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "test-token": { "scopes": "bot:play,preference:read", "userId": "stefan" }
            })))
            .mount(&server)
            .await;

        let url = Url::parse(&server.uri()).unwrap();
        let bot = Lichess::connect("test-token".into(), url, "0".into(), 3)
            .await
            .unwrap();
        assert_eq!(bot.version(), "0");
    }

    #[tokio::test]
    async fn get_online_bots_parses_ndjson_body() {
        let body = "{\"id\":\"a\",\"username\":\"Alpha\"}\n\
                    {\"id\":\"b\",\"username\":\"Beta\"}\n\
                    \n";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/bot/online"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        let bots = bot.get_online_bots(None).await;
        assert_eq!(bots.len(), 2);
        assert_eq!(bots[0].username.as_deref(), Some("Alpha"));
        assert_eq!(bots[1].id.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn event_stream_yields_typed_events_and_skips_blank_lines() {
        let body = "{\"type\":\"challenge\",\"challenge\":{\"id\":\"c1\"}}\n\
                    \n\
                    {\"type\":\"gameStart\",\"game\":{\"id\":\"g1\"}}\n";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stream/event"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        let mut stream = Box::pin(bot.get_event_stream().await.unwrap());

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.kind.as_deref(), Some("challenge"));
        assert_eq!(first.challenge.unwrap().id.as_deref(), Some("c1"));

        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(second.kind.as_deref(), Some("gameStart"));
        assert_eq!(second.game.unwrap().id.as_deref(), Some("g1"));

        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn stream_open_failure_is_single_attempt_no_storm() {
        // Connectivity-Hardening Fix 2a: a failing stream open must hit the
        // endpoint exactly ONCE. The old `send_get` path retried every 100 ms
        // for up to 60 s (~600 requests) on a transient blip — that burst is
        // what triggered Lichess's /api/stream/event 429 cascade. `try_get_stream`
        // does a single attempt; the reconnect loop in lichess_bot paces retries.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stream/event"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        let res = tokio::time::timeout(Duration::from_secs(2), bot.get_event_stream()).await;
        assert!(res.is_ok(), "stream open must return promptly, not spin in a 60 s backoff loop");
        assert!(res.unwrap().is_err(), "503 stream open should surface an error");

        let hits = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/api/stream/event")
            .count();
        assert_eq!(hits, 1, "stream open must be a single attempt, got {hits}");
    }

    #[tokio::test]
    async fn event_stream_429_surfaces_rate_limited_for_reconnect() {
        // Connectivity-Hardening Fix 2b relies on get_event_stream surfacing
        // LichessError::RateLimited after a 429, so the reconnect loop can honor
        // the server's retry-after window instead of retrying too early.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/stream/event"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        // First call: receives 429 and records the per-endpoint rate-limit timer.
        let _ = bot.get_event_stream().await;
        // Second call: short-circuited by check_rate_limit → RateLimited variant.
        let err = bot
            .get_event_stream()
            .await
            .err()
            .expect("rate-limited stream open should error");
        assert!(
            matches!(err, LichessError::RateLimited { .. }),
            "expected RateLimited so the reconnect loop honors retry-after, got {err:?}"
        );
    }

    #[tokio::test]
    async fn decline_challenge_never_panics_on_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/challenge/c1/decline"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let bot = make_client(&server);
        // Should not return / panic — Python suppresses every exception here.
        bot.decline_challenge("c1", "generic").await;
    }

    // Silence the unused-import warning when the [cfg(test)] block isn't
    // compiled (e.g. with `cargo check --release`).
    #[allow(dead_code)]
    fn _req_marker(_: Request) {}
}
