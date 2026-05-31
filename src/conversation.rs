//! Chat / `!command` handler. Rust port of `lib/conversation.py`.
//!
//! Listens to messages from a game's player and spectator rooms, logs them,
//! and answers a fixed set of `!`-prefixed commands. The implementation is
//! deliberately a thin façade over [`crate::lichess::Lichess`] (for sending
//! replies) and [`crate::engine_wrapper::EngineLike`] (for `!eval` / `!name`).
//!
//! **API-Abweichung vom Python-Original:** `engine` ist kein Feld, sondern
//! Parameter von [`Conversation::react`]. Damit darf `play_game` parallel
//! mit `&mut UciClient` arbeiten und reicht beim `chatLine`-Event einfach
//! `&engine` rein — Python hat das via `Arc`-äquivalentes Objekt-Reference
//! gelöst, was im Rust-Borrow-Modell unhandlich wäre.

use std::sync::{Arc, Mutex};

use shakmaty::fen::Fen;
use shakmaty::{Chess, EnPassantMode};
use tracing::info;

use crate::engine_wrapper::{pv_to_san, EngineLike};
use crate::lichess::{Lichess, LichessResult};
use crate::lichess_types::{GameEventType, UserProfileType};
use crate::model::{Challenge, Game};
use crate::timer::seconds;

/// Shared challenge queue. Python uses a `multiprocessing.Manager().list()`
/// here so the matchmaking process and the game worker can both see new
/// challenges; in Rust we stay single-process and share via `Arc<Mutex<…>>`.
pub type ChallengeQueue = Arc<Mutex<Vec<Challenge>>>;

/// Prefix that turns a chat message into a command.
pub const COMMAND_PREFIX: char = '!';

/// One parsed chat line. Wraps the relevant fields of a `chatLine` game event
/// (`type: "chatLine"`) so the rest of the module can stay typed.
#[derive(Debug, Clone)]
pub struct ChatLine {
    /// `"player"` or `"spectator"`.
    pub room: String,
    /// Sender's Lichess username (case as Lichess returns it).
    pub username: String,
    /// Message body.
    pub text: String,
}

impl ChatLine {
    /// Build a [`ChatLine`] from a streamed game event. Missing fields fall
    /// back to empty strings — Lichess always populates them for real
    /// `chatLine` events.
    pub fn from_event(info: &GameEventType) -> Self {
        Self {
            room: info.room.clone().unwrap_or_default(),
            username: info.username.clone().unwrap_or_default(),
            text: info.text.clone().unwrap_or_default(),
        }
    }

    /// Convenience constructor for outgoing messages that need to look like
    /// a chat line internally (Python's `send_message` does the same trick).
    fn outgoing(room: &str) -> Self {
        Self {
            room: room.to_string(),
            username: String::new(),
            text: String::new(),
        }
    }
}

/// One game's chat state. Lives for the duration of a single Lichess game.
pub struct Conversation {
    game: Game,
    li: Lichess,
    version: String,
    challengers: ChallengeQueue,
    messages: Vec<ChatLine>,
    profile: UserProfileType,
    source_url: Option<String>,
}

impl Conversation {
    pub fn new(
        game: Game,
        li: Lichess,
        version: impl Into<String>,
        challengers: ChallengeQueue,
        profile: UserProfileType,
        source_url: Option<String>,
    ) -> Self {
        Self {
            game,
            li,
            version: version.into(),
            challengers,
            messages: Vec::new(),
            profile,
            source_url,
        }
    }

    /// React to an incoming chat message. Logs it, stores it, and dispatches
    /// to [`Self::command`] if the message starts with `COMMAND_PREFIX`. The
    /// engine reference is borrowed only for the duration of the call so the
    /// game loop can keep a `&mut UciClient` between chat events.
    pub async fn react(
        &mut self,
        line: ChatLine,
        engine: &dyn EngineLike,
        board: &Chess,
    ) -> LichessResult<()> {
        info!(
            url = %self.game.url(),
            room = %line.room,
            username = %line.username,
            text = %line.text,
            "chat"
        );
        let is_command = line.text.starts_with(COMMAND_PREFIX);
        self.messages.push(line.clone());
        // Bound the per-game chat history so a long correspondence game or
        // chat spam from the opponent / spectators can't grow it unbounded.
        const MAX_MESSAGES: usize = 500;
        if self.messages.len() > MAX_MESSAGES {
            let overflow = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(0..overflow);
        }
        if is_command {
            // Strip leading '!' and lowercase the rest; we keep the original
            // line around so replies still address the right room.
            let cmd: String = line
                .text
                .chars()
                .skip(1)
                .flat_map(|c| c.to_lowercase())
                .collect();
            self.command(&line, &cmd, engine, board).await?;
        }
        Ok(())
    }

    /// Dispatch one `!command`. Mirrors the if/elif chain in Python's
    /// `Conversation.command`.
    async fn command(
        &mut self,
        line: &ChatLine,
        cmd: &str,
        engine: &dyn EngineLike,
        board: &Chess,
    ) -> LichessResult<()> {
        // Lichess usernames are case-insensitive; the chatLine event may use a
        // different casing than game.username, so compare case-insensitively or
        // we'd refuse !eval / !pv to ourselves.
        let from_self = line.username.eq_ignore_ascii_case(&self.game.username);
        let is_eval = cmd.starts_with("eval");

        if cmd == "commands" || cmd == "help" {
            return self
                .send_reply(
                    line,
                    "Supported commands: !wait (wait a minute for my first move), !name, \
                     !eval, !queue, !ratings, !fen, !pv, !source",
                )
                .await;
        }

        if cmd == "wait" && self.game.is_abortable() {
            self.game.ping(seconds(60.0), seconds(120.0), seconds(120.0));
            return self.send_reply(line, "Waiting 60 seconds...").await;
        }

        if cmd == "name" {
            let reply = format!(
                "{} running {} (liru-bot v{})",
                self.game.me.name,
                engine.name(),
                self.version
            );
            return self.send_reply(line, &reply).await;
        }

        if is_eval && (from_self || line.room == "spectator") {
            let stats = engine.get_stats(true).join(", ");
            return self.send_reply(line, &stats).await;
        }

        if is_eval {
            return self
                .send_reply(line, "I don't tell that to my opponent, sorry.")
                .await;
        }

        if cmd == "queue" {
            // Tolerate a poisoned lock (a panic in another task that held the
            // shared queue must not turn every later !queue command into a
            // crash of the game loop): the queue is just a Vec, safe to read.
            let queued = self
                .challengers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let reply = if queued.is_empty() {
                "No challenges queued.".to_string()
            } else {
                let names = queued
                    .iter()
                    .rev()
                    .map(|c| format!("@{}", c.challenger.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Challenge queue: {names}")
            };
            return self.send_reply(line, &reply).await;
        }

        if cmd == "ratings" {
            return self.send_reply(line, &self.format_ratings()).await;
        }

        if cmd == "fen" {
            let fen = Fen::from_position(board.clone(), EnPassantMode::Legal).to_string();
            return self.send_reply(line, &fen).await;
        }

        if cmd == "pv" {
            if !(from_self || line.room == "spectator") {
                return self
                    .send_reply(line, "I don't tell that to my opponent, sorry.")
                    .await;
            }
            let pv = engine.last_pv();
            let reply = if pv.is_empty() {
                "No PV available yet.".to_string()
            } else {
                let san = pv_to_san(board, pv);
                if san.is_empty() { pv.join(" ") } else { san }
            };
            return self.send_reply(line, &reply).await;
        }

        if cmd == "source" {
            let reply = match &self.source_url {
                Some(url) if !url.is_empty() => url.clone(),
                _ => "Source not yet published.".to_string(),
            };
            return self.send_reply(line, &reply).await;
        }

        // Unknown command — Python silently ignores it.
        Ok(())
    }

    /// Render the bot's ratings as a comma-separated list (`bullet 2950,
    /// blitz 3000, …`). Skips perfs without a rating. Used by `!ratings`.
    fn format_ratings(&self) -> String {
        const ORDER: &[&str] = &[
            "bullet",
            "blitz",
            "rapid",
            "classical",
            "ultraBullet",
            "correspondence",
        ];
        let mut parts: Vec<String> = Vec::new();
        for perf in ORDER {
            if let Some(rating) = self.profile.rating_for(perf) {
                parts.push(format!("{perf} {rating}"));
            }
        }
        if parts.is_empty() {
            "No ratings available.".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Send a reply that targets the same room as the original message.
    pub async fn send_reply(&self, line: &ChatLine, reply: &str) -> LichessResult<()> {
        info!(
            url = %self.game.url(),
            room = %line.room,
            speaker = %self.game.username,
            text = %reply,
            "chat-reply"
        );
        self.li.chat(&self.game.id, &line.room, reply).await
    }

    /// Send a free-standing message into `room` (e.g. on game start).
    /// Python's `send_message` is a no-op for empty strings; we mirror that.
    pub async fn send_message(&self, room: &str, message: &str) -> LichessResult<()> {
        if message.is_empty() {
            return Ok(());
        }
        self.send_reply(&ChatLine::outgoing(room), message).await
    }

    /// Read-only access to the messages this conversation has seen so far
    /// (the Python version exposes `self.messages` as a plain attribute).
    pub fn messages(&self) -> &[ChatLine] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use url::Url;
    use wiremock::matchers::{body_string, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::engine_wrapper::{EngineLike, NoopEngine};
    use crate::lichess::Lichess;
    use crate::lichess_types::{GameEventType, PlayerType, TimeControlType, VariantInfo};
    use crate::model::Game;

    /// Engine that returns scripted stats for `!eval`.
    #[derive(Debug)]
    struct StatsEngine {
        name: String,
        stats: Vec<String>,
    }

    impl EngineLike for StatsEngine {
        fn name(&self) -> &str {
            &self.name
        }
        fn get_stats(&self, _for_chat: bool) -> Vec<String> {
            self.stats.clone()
        }
    }

    async fn make_lichess_via_connect(server: &MockServer) -> Lichess {
        Mock::given(method("POST"))
            .and(path("/api/token/test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "test-token": { "scopes": "bot:play", "userId": "BotOne" }
            })))
            .mount(server)
            .await;
        let url = Url::parse(&server.uri()).unwrap();
        Lichess::connect("test-token".into(), url, "0.1.0".into(), 3)
            .await
            .expect("token mock should accept")
    }

    fn dummy_game(username: &str) -> Game {
        let info = GameEventType {
            id: Some("gameid".into()),
            speed: Some("blitz".into()),
            clock: Some(TimeControlType { initial: Some(300_000), increment: Some(2_000), ..Default::default() }),
            white: Some(PlayerType { name: Some(username.into()), ..Default::default() }),
            black: Some(PlayerType { name: Some("Other".into()), ..Default::default() }),
            variant: Some(VariantInfo { name: Some("Standard".into()), ..Default::default() }),
            state: Some(crate::lichess_types::GameStateType::default()),
            rated: Some(true),
            created_at: Some(0),
            ..Default::default()
        };
        Game::new(&info, username, "https://lichess.org/", std::time::Duration::from_secs(30))
    }

    #[tokio::test]
    async fn help_command_lists_supported_commands() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!help".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn name_command_includes_engine_and_version() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(
                "room=player&text=BotOne+running+Stockfish+%28liru-bot+v0.1.0%29",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!name".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn eval_from_opponent_is_refused() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(
                "room=player&text=I+don%27t+tell+that+to+my+opponent%2C+sorry.",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = StatsEngine {
            name: "Stockfish".into(),
            stats: vec!["depth 20".into(), "score 0.4".into()],
        };
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!eval".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn eval_from_self_reveals_stats() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string("room=player&text=depth+20%2C+score+0.4"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = StatsEngine {
            name: "Stockfish".into(),
            stats: vec!["depth 20".into(), "score 0.4".into()],
        };
        conv.react(
            ChatLine {
                room: "player".into(),
                username: "BotOne".into(), // == game.username → from_self
                text: "!eval".into(),
            },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn queue_empty_reports_no_challenges() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string("room=player&text=No+challenges+queued."))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!queue".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    fn profile_with_ratings(perfs: &[(&str, i64)]) -> UserProfileType {
        use std::collections::HashMap;
        use crate::lichess_types::PerfType;
        let mut map = HashMap::new();
        for (perf, rating) in perfs {
            map.insert(
                (*perf).to_lowercase(),
                PerfType { rating: Some(*rating), ..Default::default() },
            );
        }
        UserProfileType { perfs: Some(map), ..Default::default() }
    }

    #[tokio::test]
    async fn ratings_command_lists_known_perfs_in_order() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(
                "room=player&text=bullet+2950%2C+blitz+3000%2C+rapid+3050",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            profile_with_ratings(&[("blitz", 3000), ("bullet", 2950), ("rapid", 3050)]),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!ratings".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fen_command_replies_with_current_position() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        // Default Chess board → standard FEN, URL-encoded as the chat body.
        let expected = "room=player&text=rnbqkbnr%2Fpppppppp%2F8%2F8%2F8%2F8%2FPPPPPPPP%2FRNBQKBNR+w+KQkq+-+0+1";
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(expected))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!fen".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    /// Engine that returns a scripted PV.
    #[derive(Debug)]
    struct PvEngine(Vec<String>);
    impl EngineLike for PvEngine {
        fn name(&self) -> &str { "PvEngine" }
        fn get_stats(&self, _for_chat: bool) -> Vec<String> { Vec::new() }
        fn last_pv(&self) -> &[String] { &self.0 }
    }

    #[tokio::test]
    async fn pv_command_renders_san_for_spectator() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string("room=spectator&text=e4+e5"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = PvEngine(vec!["e2e4".into(), "e7e5".into()]);
        conv.react(
            ChatLine { room: "spectator".into(), username: "Other".into(), text: "!pv".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn pv_command_refuses_opponent_in_player_room() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(
                "room=player&text=I+don%27t+tell+that+to+my+opponent%2C+sorry.",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = PvEngine(vec!["e2e4".into()]);
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!pv".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn source_command_returns_configured_url() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string(
                "room=player&text=https%3A%2F%2Fexample.com%2Frepo",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            Some("https://example.com/repo".into()),
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!source".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn source_command_falls_back_when_unset() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/bot/game/gameid/chat"))
            .and(body_string("room=player&text=Source+not+yet+published."))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        let engine = NoopEngine::new("Stockfish");
        conv.react(
            ChatLine { room: "player".into(), username: "Other".into(), text: "!source".into() },
            &engine,
            &Chess::default(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn send_message_skips_empty_strings() {
        let server = MockServer::start().await;
        let li = make_lichess_via_connect(&server).await;
        // No `Mock` for /chat — if we hit it the test panics via wiremock.

        let conv = Conversation::new(
            dummy_game("BotOne"),
            li,
            "0.1.0",
            Arc::new(Mutex::new(Vec::new())),
            UserProfileType::default(),
            None,
        );
        conv.send_message("player", "").await.unwrap();
    }

}
