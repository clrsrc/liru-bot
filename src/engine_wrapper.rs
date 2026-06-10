//! Engine integration — Rust port of `lib/engine_wrapper.py`.
//!
//! Status: **UCI-Minimum** ist drin. Subprocess starten, Handshake, `position`/
//! `go`/`bestmove`-Roundtrip, sauberes `quit`. Plus reine Parser-/Formatter-
//! Funktionen, die der Client nutzt und die isoliert getestet werden.
//!
//! Noch fehlt (für vollständige `EngineWrapper`-Parität):
//!
//! - XBoard-Protokoll
//! - Polyglot-Book-Reader
//! - Syzygy / Gaviota EGTB-Lookups
//! - Online-Books / -EGTBs (chessdb, lichess cloud, opening explorer)
//! - `play_move`-Orchestrierung (Book → EGTB → Online → Engine-Suche)
//! - Move-Comments, Opponent-Info, `send_game_result`, Ponder

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;
use tracing::{debug, trace, warn};

use shakmaty::Position;

use crate::config::DrawOrResignConfig;
use crate::lichess::MAX_CHAT_MESSAGE_LEN;
use crate::lichess_types::GameStateType;
use crate::model::{Game, Termination};
use crate::timer::Timer;

// ---------------------------------------------------------------------------
// Trait + Noop-Engine (kept for `crate::conversation` and tests)
// ---------------------------------------------------------------------------

/// Minimum interface a "running engine" has to expose so the chat handler
/// (and later the main loop) can interact with it without knowing which
/// protocol it speaks under the hood.
pub trait EngineLike: Send + Sync {
    /// Human-readable engine name, e.g. `"Stockfish 16"`.
    fn name(&self) -> &str;

    /// Lines describing the current search state. `for_chat == true` returns
    /// a redacted/short form suitable for posting into a Lichess game chat
    /// (Python: `engine.get_stats(for_chat=True)`).
    fn get_stats(&self, for_chat: bool) -> Vec<String>;

    /// Principal variation from the most recent `info` line, in raw UCI.
    /// Empty when no search has produced a PV yet. Used by chat `!pv`.
    fn last_pv(&self) -> &[String] {
        &[]
    }
}

/// No-op engine used in tests and as a placeholder until a real
/// `UciClient` is started.
#[derive(Debug, Clone)]
pub struct NoopEngine {
    pub display_name: String,
}

impl NoopEngine {
    pub fn new<S: Into<String>>(name: S) -> Self {
        Self { display_name: name.into() }
    }
}

impl EngineLike for NoopEngine {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn get_stats(&self, _for_chat: bool) -> Vec<String> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("engine handshake timed out after {0:?}")]
    HandshakeTimeout(Duration),

    #[error("engine closed its stdout before responding to {expected}")]
    UnexpectedEof { expected: &'static str },

    #[error("could not parse `bestmove` line: {0}")]
    BestmoveParse(String),

    #[error("engine reported error: {0}")]
    Reported(String),
}

pub type EngineResult<T> = Result<T, EngineError>;

// ---------------------------------------------------------------------------
// Parser / formatter helpers
// ---------------------------------------------------------------------------

/// One UCI option as announced by the engine in response to `uci`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UciOption {
    pub name: String,
    /// `check`, `spin`, `combo`, `button`, `string`, …
    pub kind: String,
    pub default: Option<String>,
    pub min: Option<String>,
    pub max: Option<String>,
    pub vars: Vec<String>,
}

/// Opponent info as published over `UCI_Opponent`. Mirrors python-chess's
/// `chess.engine.Opponent`. Constructed from `model::Player` for live
/// games; tests build it directly.
#[derive(Debug, Clone, Default)]
pub struct OpponentInfo {
    pub title: Option<String>,
    pub rating: Option<i64>,
    pub is_bot: bool,
    pub name: String,
}

/// One ply's worth of engine commentary — what the engine published in
/// the last `info` line of the search that produced this move. Stored
/// per-bot-move so PGN exporters can annotate each move with its eval /
/// depth / PV later. Mirrors Python's `InfoStrDict` entry pushed by
/// `EngineWrapper.add_comment`.
#[derive(Debug, Clone, Default)]
pub struct MoveCommentary {
    /// Engine score from the side-to-move's perspective. `None` when
    /// the move came from a book / EGTB without a score.
    pub score: Option<PovScore>,
    pub depth: Option<u32>,
    pub nodes: Option<u64>,
    pub time_ms: Option<u64>,
    pub nps: Option<u64>,
    /// Principal variation in UCI, as the engine reported it. Empty for
    /// non-engine sources or when the engine emitted no PV.
    pub pv_uci: Vec<String>,
    /// Principal variation rendered in SAN against the position at the
    /// time the move was played. Convenience field — equivalent to
    /// `pv_to_san(pos_before_move, &pv_uci)`.
    pub pv_san: String,
}

/// Translate a UCI principal variation into the equivalent SAN string,
/// playing through `start_pos`. Stops at the first illegal/unparseable
/// token (most often when the engine reports an over-long PV). Returns
/// a single space-separated string — matches Python's
/// `board.variation_san(pv)` output, without move-number prefixes.
pub fn pv_to_san(start_pos: &shakmaty::Chess, pv_uci: &[String]) -> String {
    let mut pos = start_pos.clone();
    let mut sans = Vec::with_capacity(pv_uci.len());
    for uci_str in pv_uci {
        let Ok(uci) = shakmaty::uci::UciMove::from_ascii(uci_str.as_bytes()) else { break };
        let Ok(mv) = uci.to_move(&pos) else { break };
        let san = shakmaty::san::San::from_move(&pos, &mv);
        sans.push(san.to_string());
        pos.play_unchecked(&mv);
    }
    sans.join(" ")
}

/// Render the `gameover` UCI line for a finished `Game`. Pure helper so
/// the result-mapping logic stays testable without spawning an engine.
/// The result token comes from [`Game::result`]; the reason follows
/// Python's `send_game_result` branches.
fn format_gameover_line(game: &Game) -> String {
    let result = game.result();
    let reason = gameover_reason(game);
    match reason {
        Some(r) => format!("gameover {result} reason \"{r}\""),
        None => format!("gameover {result}"),
    }
}

fn gameover_reason(game: &Game) -> Option<String> {
    let status = game.state.status.as_deref()?;
    let winner = game.state.winner.as_deref();
    match Termination::from_status(status) {
        Termination::Mate => None,
        Termination::Resign => {
            let resigner = if winner == Some("white") { "Black" } else { "White" };
            Some(format!("{resigner} resigned"))
        }
        Termination::Abort => Some("Game aborted".into()),
        Termination::Draw => Some("Draw by agreement".into()),
        Termination::Timeout => {
            if let Some(w) = winner {
                let timeouter = if w == "white" { "Black" } else { "White" };
                Some(format!("{timeouter} timeout"))
            } else {
                Some("Time draw / insufficient material".into())
            }
        }
        Termination::Other => None,
    }
}

impl OpponentInfo {
    pub fn from_player(p: &crate::model::Player) -> Self {
        Self {
            title: p.title.clone(),
            rating: p.rating,
            is_bot: p.is_bot,
            name: p.name.clone(),
        }
    }

    /// Render the `UCI_Opponent` value string. Format mirrors python-chess
    /// and UCI conventions: `<title> <rating> <computer|human> <name>`,
    /// with `none` standing in for an unknown title or rating.
    pub fn uci_opponent_value(&self) -> String {
        let title = self.title.as_deref().filter(|s| !s.is_empty()).unwrap_or("none");
        let rating = self
            .rating
            .map(|r| r.to_string())
            .unwrap_or_else(|| "none".into());
        let kind = if self.is_bot { "computer" } else { "human" };
        format!("{title} {rating} {kind} {}", self.name)
    }
}

/// Result of parsing an `info ...` line. Only the fields the bot actually
/// reads back are typed; the rest stays in `raw` for `get_stats` to ship.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InfoLine {
    pub depth: Option<u32>,
    pub score_cp: Option<i64>,
    pub score_mate: Option<i64>,
    pub nodes: Option<u64>,
    pub nps: Option<u64>,
    pub time_ms: Option<u64>,
    pub pv: Vec<String>,
    /// Other tokens (`hashfull`, `multipv`, etc.) collected verbatim.
    pub raw: Vec<(String, String)>,
}

/// Parse a single `bestmove` line. Returns `(best, ponder)`.
pub fn parse_bestmove(line: &str) -> EngineResult<(String, Option<String>)> {
    let mut it = line.split_whitespace();
    let Some(head) = it.next() else {
        return Err(EngineError::BestmoveParse(line.into()));
    };
    if head != "bestmove" {
        return Err(EngineError::BestmoveParse(line.into()));
    }
    let Some(best) = it.next() else {
        return Err(EngineError::BestmoveParse(line.into()));
    };
    let mut ponder = None;
    while let Some(tok) = it.next() {
        if tok == "ponder" {
            ponder = it.next().map(|s| s.to_string());
            break;
        }
    }
    Ok((best.to_string(), ponder))
}

/// Parse `id name <…>` / `id author <…>`. Returns `(key, value)`.
pub fn parse_id_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("id ")?;
    let (k, v) = rest.split_once(' ')?;
    Some((k.to_string(), v.to_string()))
}

/// Parse one `option name X type Y …` line into a [`UciOption`].
pub fn parse_option_line(line: &str) -> Option<UciOption> {
    let rest = line.strip_prefix("option ")?;
    // Format is `name <words> type <word> [default <words>] [min <word>]
    //            [max <word>] [var <words>]*`. Split on the keyword
    // boundaries so option names with spaces survive.
    let mut name: Vec<&str> = Vec::new();
    let mut kind: Option<&str> = None;
    let mut default: Vec<&str> = Vec::new();
    let mut has_default = false;
    let mut min: Option<&str> = None;
    let mut max: Option<&str> = None;
    let mut vars: Vec<String> = Vec::new();
    let mut current_var: Vec<&str> = Vec::new();
    let mut state = ParseState::None;

    for tok in rest.split_whitespace() {
        match tok {
            "name" => state = ParseState::Name,
            "type" => state = ParseState::Type,
            "default" => {
                has_default = true;
                state = ParseState::Default;
            }
            "min" => state = ParseState::Min,
            "max" => state = ParseState::Max,
            "var" => {
                if !current_var.is_empty() {
                    vars.push(current_var.join(" "));
                    current_var.clear();
                }
                state = ParseState::Var;
            }
            _ => match state {
                ParseState::Name => name.push(tok),
                ParseState::Type => kind = Some(tok),
                ParseState::Default => default.push(tok),
                ParseState::Min => min = Some(tok),
                ParseState::Max => max = Some(tok),
                ParseState::Var => current_var.push(tok),
                ParseState::None => {}
            },
        }
    }
    if !current_var.is_empty() {
        vars.push(current_var.join(" "));
    }
    if name.is_empty() || kind.is_none() {
        return None;
    }
    Some(UciOption {
        name: name.join(" "),
        kind: kind?.to_string(),
        default: if has_default {
            Some(default.join(" "))
        } else {
            None
        },
        min: min.map(str::to_string),
        max: max.map(str::to_string),
        vars,
    })
}

#[derive(Copy, Clone)]
enum ParseState {
    None,
    Name,
    Type,
    Default,
    Min,
    Max,
    Var,
}

/// Parse a single `info …` line into a typed [`InfoLine`]. Unknown keys
/// land in `raw` so callers can still display them.
pub fn parse_info_line(line: &str) -> Option<InfoLine> {
    let rest = line.strip_prefix("info ")?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut info = InfoLine::default();
    let mut i = 0;
    while i < tokens.len() {
        let key = tokens[i];
        i += 1;
        match key {
            "depth" => {
                if let Some(v) = tokens.get(i) {
                    info.depth = v.parse().ok();
                    i += 1;
                }
            }
            "nodes" => {
                if let Some(v) = tokens.get(i) {
                    info.nodes = v.parse().ok();
                    i += 1;
                }
            }
            "nps" => {
                if let Some(v) = tokens.get(i) {
                    info.nps = v.parse().ok();
                    i += 1;
                }
            }
            "time" => {
                if let Some(v) = tokens.get(i) {
                    info.time_ms = v.parse().ok();
                    i += 1;
                }
            }
            "score" => {
                // `score cp 12` | `score mate 5`
                if let Some(kind) = tokens.get(i).copied() {
                    i += 1;
                    if let Some(v) = tokens.get(i) {
                        match kind {
                            "cp" => info.score_cp = v.parse().ok(),
                            "mate" => info.score_mate = v.parse().ok(),
                            _ => info
                                .raw
                                .push(("score".into(), format!("{kind} {v}"))),
                        }
                        i += 1;
                    }
                }
            }
            "pv" => {
                while let Some(tok) = tokens.get(i) {
                    info.pv.push((*tok).to_string());
                    i += 1;
                }
            }
            other => {
                if let Some(v) = tokens.get(i) {
                    info.raw.push((other.into(), (*v).to_string()));
                    i += 1;
                }
            }
        }
    }
    Some(info)
}

/// Render an [`InfoLine`] as a compact `key=value` summary for log output.
/// `None` fields are omitted so the line stays grep-friendly. PV joins with
/// spaces; empty PV is dropped entirely.
pub(crate) fn format_uci_info_log(info: &InfoLine) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(7);
    if let Some(d) = info.depth {
        parts.push(format!("depth={d}"));
    }
    if let Some(cp) = info.score_cp {
        parts.push(format!("cp={cp}"));
    }
    if let Some(m) = info.score_mate {
        parts.push(format!("mate={m}"));
    }
    if let Some(n) = info.nodes {
        parts.push(format!("nodes={n}"));
    }
    if let Some(nps) = info.nps {
        parts.push(format!("nps={nps}"));
    }
    if let Some(t) = info.time_ms {
        parts.push(format!("time_ms={t}"));
    }
    if !info.pv.is_empty() {
        parts.push(format!("pv={}", info.pv.join(" ")));
    }
    parts.join(" ")
}

/// Format the `position` command for the engine.
///
/// - `startpos` if `fen == None || fen == Some("startpos")`
/// - `fen <FEN>` otherwise
/// - `moves <uci ...>` appended if `moves` is non-empty
pub fn format_position(fen: Option<&str>, moves: &[&str]) -> String {
    let head = match fen {
        None | Some("startpos") | Some("") => "position startpos".to_string(),
        Some(f) => format!("position fen {f}"),
    };
    if moves.is_empty() {
        head
    } else {
        format!("{head} moves {}", moves.join(" "))
    }
}

/// Search budget for one `go` command. Mirrors the most common fields of
/// `python-chess`'s `Limit` — additional fields will be added if/when
/// `play_move` actually uses them.
#[derive(Debug, Clone, Default)]
pub struct GoLimits {
    pub wtime_ms: Option<u64>,
    pub btime_ms: Option<u64>,
    pub winc_ms: Option<u64>,
    pub binc_ms: Option<u64>,
    pub movetime_ms: Option<u64>,
    pub depth: Option<u32>,
    pub nodes: Option<u64>,
    pub movestogo: Option<u32>,
    pub infinite: bool,
    pub ponder: bool,
    /// UCI `searchmoves <uci>...` filter. When non-empty the engine is
    /// restricted to picking from these moves only. Used when an
    /// EGTB / online source has narrowed the candidate set but doesn't
    /// pick the single best move itself.
    pub searchmoves: Vec<String>,
}

impl GoLimits {
    pub fn movetime(ms: u64) -> Self {
        Self { movetime_ms: Some(ms), ..Default::default() }
    }
}

pub fn format_go(limits: &GoLimits) -> String {
    let mut out = String::from("go");
    if limits.ponder {
        out.push_str(" ponder");
    }
    if let Some(v) = limits.wtime_ms {
        out.push_str(&format!(" wtime {v}"));
    }
    if let Some(v) = limits.btime_ms {
        out.push_str(&format!(" btime {v}"));
    }
    if let Some(v) = limits.winc_ms {
        out.push_str(&format!(" winc {v}"));
    }
    if let Some(v) = limits.binc_ms {
        out.push_str(&format!(" binc {v}"));
    }
    if let Some(v) = limits.movestogo {
        out.push_str(&format!(" movestogo {v}"));
    }
    if let Some(v) = limits.depth {
        out.push_str(&format!(" depth {v}"));
    }
    if let Some(v) = limits.nodes {
        out.push_str(&format!(" nodes {v}"));
    }
    if let Some(v) = limits.movetime_ms {
        out.push_str(&format!(" movetime {v}"));
    }
    if limits.infinite {
        out.push_str(" infinite");
    }
    if !limits.searchmoves.is_empty() {
        out.push_str(" searchmoves");
        for mv in &limits.searchmoves {
            out.push(' ');
            out.push_str(mv);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// MoveDecision — unified output of book / EGTB / online / engine
// ---------------------------------------------------------------------------

/// Where a [`MoveDecision`] came from. The string form goes into the
/// `Source:` line of the in-chat stats; matches Python's
/// `"lichess-bot-source:..."` tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveSource {
    Engine,
    OpeningBook(String),
    SyzygyEgtb,
    GaviotaEgtb,
    LichessCloud,
    LichessExplorer(String),
    LichessEgtb,
    Chessdb,
}

impl MoveSource {
    pub fn as_label(&self) -> String {
        match self {
            Self::Engine => "Engine".into(),
            Self::OpeningBook(name) => format!("Opening Book ({name})"),
            Self::SyzygyEgtb => "Syzygy EGTB".into(),
            Self::GaviotaEgtb => "Gaviota EGTB".into(),
            Self::LichessCloud => "Lichess Cloud Analysis".into(),
            Self::LichessExplorer(variant) => {
                format!("Lichess Opening Explorer ({variant})")
            }
            Self::LichessEgtb => "Lichess EGTB".into(),
            Self::Chessdb => "ChessDB".into(),
        }
    }
}

/// A move the bot is about to play, together with optional draw / resign
/// flags and the source it came from. Used as the unified return type of
/// all decision steps (`get_book_move`, `get_egtb_move`,
/// `get_online_move`, engine `search`) so [`play_move`] can treat them
/// uniformly.
#[derive(Debug, Clone)]
pub struct MoveDecision {
    pub mv: shakmaty::Move,
    pub source: MoveSource,
    pub score: Option<PovScore>,
    pub draw_offered: bool,
    pub resigned: bool,
}

impl MoveDecision {
    pub fn new(mv: shakmaty::Move, source: MoveSource) -> Self {
        Self {
            mv,
            source,
            score: None,
            draw_offered: false,
            resigned: false,
        }
    }

    /// Format the move in UCI for `POST /api/bot/game/{id}/move/{uci}`.
    /// Castling is encoded as the king's final square (e1g1, e8c8, …),
    /// not the Polyglot-style king→rook coordinates.
    pub fn uci_standard(&self) -> String {
        shakmaty::uci::UciMove::from_standard(&self.mv).to_string()
    }

    /// Same as [`uci_standard`] but using Chess960's "from = king, to =
    /// rook" castling notation. Use this for Chess960 / From-Position
    /// games where Lichess expects the 960 format.
    pub fn uci_chess960(&self) -> String {
        shakmaty::uci::UciMove::from_chess960(&self.mv).to_string()
    }
}

/// Outcome of one pre-engine source. Either the source already picked a
/// single move ([`Decision`]), or it narrowed the candidate set to a
/// shortlist that the engine should search over via `searchmoves`
/// ([`Suggest`]). Python represents this with `isinstance(best_move,
/// list)`; we make it explicit.
#[derive(Debug, Clone)]
pub enum PreEngineResult {
    Decision(MoveDecision),
    Suggest(Vec<shakmaty::Move>),
}

// ---------------------------------------------------------------------------
// Clock / time-management helpers
// ---------------------------------------------------------------------------

/// Side to move. Kept board-free so the clock helpers stay pure functions —
/// the real caller in `play_move` will derive this from the actual `shakmaty`
/// position, tests just pass it in directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    White,
    Black,
}

/// First UCI move only gets a fixed 10 s budget — Lichess imposes a 30 s
/// "first move" timeout, and pondering does nothing because the engine
/// starts from scratch.
const FIRST_MOVE_TIME_MS: u64 = 10_000;
const MIN_CLOCK_MS: u64 = 1;

fn clamped_ms(value: Option<i64>) -> u64 {
    value.unwrap_or(0).max(0) as u64
}

/// Per-move overshoot cap in ms: `remaining / 30 + increment`.
///
/// BUS #3/#9/#10/#13 (Game EqGn5ie6): clrsrc's `stability_factor` can inflate
/// the soft limit in flat, best-move-oscillating positions; on slow hardware the
/// engine then runs a deep iteration toward the hard cap (126 s on a single move
/// → forfeit). The engine-side fix (`SOFT_INFLATION_CAP`) was SPRT-rejected as
/// ELO-neutral-negative, so this bot-side cap is the *only* overshoot protection.
/// In normal play it is ~the engine's own per-move budget, so it binds only in
/// the pathological case (clrsrc green-lit `min(soft, remaining/30 + inc)`).
fn movetime_cap_ms(remaining_ms: u64, inc_ms: u64) -> u64 {
    remaining_ms / 30 + inc_ms
}

/// Number of half-moves already played, derived from the UCI `moves`
/// string in [`GameStateType`]. Empty / missing → 0.
pub fn move_count(state: &GameStateType) -> usize {
    state
        .moves
        .as_deref()
        .map(|m| m.split_whitespace().count())
        .unwrap_or(0)
}

/// Has the opponent offered a draw on their previous move?
pub fn check_for_draw_offer(game: &Game) -> bool {
    if game.opponent_color == "white" {
        game.state.wdraw.unwrap_or(false)
    } else {
        game.state.bdraw.unwrap_or(false)
    }
}

/// Fixed-time budget for the very first move of a game.
pub fn first_move_time() -> GoLimits {
    GoLimits::movetime(FIRST_MOVE_TIME_MS)
}

/// Correspondence (or otherwise time-budgeted) move: spend at most
/// `search_time`, but never longer than the side-to-move's remaining
/// clock minus the communication overhead.
pub fn single_move_time(
    side: Side,
    state: &GameStateType,
    search_time: Duration,
    setup_timer: &Timer,
    move_overhead: Duration,
) -> GoLimits {
    let overhead = setup_timer.time_since_reset() + move_overhead;
    let remaining_ms = match side {
        Side::White => clamped_ms(state.wtime),
        Side::Black => clamped_ms(state.btime),
    };
    let raw = Duration::from_millis(remaining_ms).saturating_sub(overhead);
    let clock_time = raw.max(Duration::from_millis(MIN_CLOCK_MS));
    let search_time = search_time.min(clock_time);
    GoLimits::movetime(search_time.as_millis() as u64)
}

/// Realtime game: cap the search at `remaining/30 + inc` via `go movetime`.
///
/// We used to ship the raw clocks and let the engine budget itself, but that
/// left the overshoot class (see [`movetime_cap_ms`]) uncapped on the
/// subprocess path. clrsrc honors `go movetime` as `soft = hard = movetime`
/// (time.rs:133), so this bounds the rare soft-inflation overshoot; it also
/// bounds any generic UCI engine driven through the subprocess backend (the
/// trade-off: a fixed per-move budget instead of engine-side clock management).
/// The cap is further clamped to the real clock minus the comms overhead.
pub fn game_clock_time(
    side: Side,
    state: &GameStateType,
    setup_timer: &Timer,
    move_overhead: Duration,
) -> GoLimits {
    let overhead = setup_timer.time_since_reset() + move_overhead;
    let (remaining_ms, inc_ms) = match side {
        Side::White => (clamped_ms(state.wtime), clamped_ms(state.winc)),
        Side::Black => (clamped_ms(state.btime), clamped_ms(state.binc)),
    };
    let min = Duration::from_millis(MIN_CLOCK_MS);
    // Time actually left after the overhead we've already spent receiving the move.
    let avail = Duration::from_millis(remaining_ms).saturating_sub(overhead).max(min);
    let cap = Duration::from_millis(movetime_cap_ms(remaining_ms, inc_ms));
    let search = cap.min(avail).max(min);
    GoLimits::movetime(search.as_millis() as u64)
}

/// Top-level dispatch — replicates Python's `move_time(...)`. Returns
/// `(limits, can_ponder)`; the first move always disables pondering
/// because a fresh clock starts right after.
#[allow(clippy::too_many_arguments)]
pub fn move_time(
    side: Side,
    state: &GameStateType,
    can_ponder: bool,
    setup_timer: &Timer,
    move_overhead: Duration,
    is_correspondence: bool,
    correspondence_move_time: Duration,
) -> (GoLimits, bool) {
    if move_count(state) < 2 {
        return (first_move_time(), false);
    }
    if is_correspondence {
        (
            single_move_time(side, state, correspondence_move_time, setup_timer, move_overhead),
            can_ponder,
        )
    } else {
        (game_clock_time(side, state, setup_timer, move_overhead), can_ponder)
    }
}

// ---------------------------------------------------------------------------
// Draw / resign heuristic
// ---------------------------------------------------------------------------

/// Engine evaluation of the latest position, **from the side-to-move's
/// perspective**. `cp` = centipawns, `mate` = mate in N (positive = winning).
/// Exactly one of the two is set; `mate` wins when both are present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PovScore {
    pub cp: Option<i64>,
    pub mate: Option<i64>,
}

impl PovScore {
    pub fn from_cp(cp: i64) -> Self {
        Self { cp: Some(cp), mate: None }
    }

    pub fn from_mate(mate: i64) -> Self {
        Self { cp: None, mate: Some(mate) }
    }

    /// Mirror `chess.engine.Score.score(mate_score=...)`: collapse the
    /// mate-in-N into a clamped centipawn equivalent so the comparison
    /// against `resign_score` / `offer_draw_score` works uniformly.
    pub fn to_cp(&self, mate_score: i64) -> i64 {
        if let Some(mate) = self.mate {
            if mate > 0 {
                mate_score - mate
            } else if mate < 0 {
                -mate_score - mate
            } else {
                // mate 0 == "I'm already mated"
                -mate_score
            }
        } else {
            self.cp.unwrap_or(0)
        }
    }
}

/// Decision returned by [`DrawResignTracker::decide`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawResignDecision {
    pub offer_draw: bool,
    pub resign: bool,
}

/// Rolling window of recent engine scores, used to decide when to offer
/// a draw or resign. Mirrors Python's `EngineWrapper.offer_draw_or_resign`.
#[derive(Debug, Clone, Default)]
pub struct DrawResignTracker {
    scores: Vec<PovScore>,
}

impl DrawResignTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, score: PovScore) {
        self.scores.push(score);
    }

    pub fn scores(&self) -> &[PovScore] {
        &self.scores
    }

    /// Most recent recorded score in centipawns from our point of view
    /// (positive = good for us); `None` before the first evaluation. Used to
    /// gate draw acceptance / claims so we never give up a winning position.
    pub fn last_score_cp(&self) -> Option<i64> {
        self.scores.last().map(|s| s.to_cp(40_000))
    }

    /// Apply the draw/resign heuristic for the current position.
    ///
    /// `piece_count` is the number of pieces on the board (Python:
    /// `chess.popcount(board.occupied)`). The draw offer requires that
    /// `piece_count <= cfg.offer_draw_pieces` *and* the last
    /// `cfg.offer_draw_moves` scores all sit within `±cfg.offer_draw_score`.
    pub fn decide(&self, cfg: &DrawOrResignConfig, piece_count: u32) -> DrawResignDecision {
        const MATE_SCORE: i64 = 40_000;
        let mut decision = DrawResignDecision::default();

        if cfg.offer_draw_enabled
            && self.scores.len() >= cfg.offer_draw_moves as usize
            && piece_count <= cfg.offer_draw_pieces
        {
            let window = &self.scores[self.scores.len() - cfg.offer_draw_moves as usize..];
            let near_draw = window
                .iter()
                .all(|s| s.to_cp(MATE_SCORE).abs() <= cfg.offer_draw_score);
            if near_draw {
                decision.offer_draw = true;
            }
        }

        if cfg.resign_enabled && self.scores.len() >= cfg.resign_moves as usize {
            let window = &self.scores[self.scores.len() - cfg.resign_moves as usize..];
            let near_loss = window
                .iter()
                .all(|s| s.to_cp(MATE_SCORE) <= cfg.resign_score);
            if near_loss {
                decision.resign = true;
            }
        }

        decision
    }
}

// ---------------------------------------------------------------------------
// Readable formatters (chat / log output)
// ---------------------------------------------------------------------------

/// Human-readable centipawn score: `-1.23` or `#5` for mate-in-N.
pub fn readable_score(score: &PovScore) -> String {
    if let Some(mate) = score.mate {
        format!("#{mate}")
    } else {
        let cp = score.cp.unwrap_or(0);
        let v = (cp as f64) / 100.0;
        format!("{v:.2}")
    }
}

/// Lichess sends WDL as a `(win, draw, loss)` permille triple from the
/// side-to-move's perspective. We convert to a "win expectation" % the
/// same way python-chess does: `(win + draw/2) / total`.
pub fn readable_wdl(win: u32, draw: u32, loss: u32) -> String {
    // Sum in f64, not u32: the WDL counts come from the network and a
    // malformed frame near u32::MAX would otherwise overflow in debug builds.
    let total = win as f64 + draw as f64 + loss as f64;
    if total <= 0.0 {
        return "0.0%".to_string();
    }
    let pct = ((win as f64) + (draw as f64) * 0.5) * 100.0 / total;
    format!("{:.1}%", round_to(pct, 1))
}

/// `seconds` formatted as `Xm Ys` (or just `Ys` for < 1 min). Matches
/// Python's `readable_time`. Input is seconds (Lichess reports
/// `movetime` in ms, callers divide by 1000 first).
pub fn readable_time(seconds: f64) -> String {
    let minutes = (seconds / 60.0).trunc();
    let rem = seconds - minutes * 60.0;
    if minutes >= 1.0 {
        format!("{minutes:.0}m {rem:.1}s")
    } else {
        format!("{rem:.1}s")
    }
}

/// `123_456_789 → "123.5M"`. Used for `nodes` / `nps`.
pub fn readable_number(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{}B", round_to(n as f64 / 1e9, 1))
    } else if n >= 1_000_000 {
        format!("{}M", round_to(n as f64 / 1e6, 1))
    } else if n >= 1_000 {
        format!("{}K", round_to(n as f64 / 1e3, 1))
    } else {
        n.to_string()
    }
}

fn round_to(v: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (v * f).round() / f
}

fn score_from_info(info: &InfoLine) -> Option<PovScore> {
    if let Some(m) = info.score_mate {
        return Some(PovScore::from_mate(m));
    }
    info.score_cp.map(PovScore::from_cp)
}

// ---------------------------------------------------------------------------
// UCI subprocess client
// ---------------------------------------------------------------------------

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct UciClient {
    name: String,
    /// `id author <…>` (informational only).
    pub author: Option<String>,
    pub options: Vec<UciOption>,
    last_info: Vec<InfoLine>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
    /// `Some(...)` while the engine is in a `go ponder` background
    /// search. The next user-facing `search()` call MUST resolve this
    /// (via `ponderhit` or `stop`) before sending any other UCI
    /// command. Tracked here rather than in the caller so the contract
    /// is local to the protocol layer.
    ponder_state: Option<PonderState>,
    /// Per-bot-move commentary, in playback order. Indexed by Python's
    /// `comment_for_board_index` semantics — see [`Self::commentary_for_half_move`].
    move_commentary: Vec<MoveCommentary>,
    /// Half-move count when the bot played its first move in the
    /// current game. `None` until the first `record_move_commentary`.
    comment_start_index: Option<usize>,
}

/// Outstanding ponder search the engine is running in the background.
#[derive(Debug, Clone)]
struct PonderState {
    /// The opponent move we asked the engine to assume.
    expected_opponent_move: String,
}

impl std::fmt::Debug for UciClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UciClient")
            .field("name", &self.name)
            .field("author", &self.author)
            .field("options", &self.options.len())
            .field("pid", &self.child.id())
            .finish()
    }
}

impl UciClient {
    /// Spawn the engine binary and run the UCI handshake (`uci` → `uciok`,
    /// `isready` → `readyok`). Optional `options` are sent via `setoption`
    /// between the two phases.
    pub async fn spawn(
        binary: &str,
        extra_args: &[String],
        cwd: Option<&str>,
        options: &HashMap<String, String>,
        silence_stderr: bool,
    ) -> EngineResult<Self> {
        let mut cmd = Command::new(binary);
        cmd.args(extra_args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if silence_stderr {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));

        let mut client = Self {
            name: String::from("unknown"),
            author: None,
            options: Vec::new(),
            last_info: Vec::new(),
            stdin,
            stdout,
            child,
            ponder_state: None,
            move_commentary: Vec::new(),
            comment_start_index: None,
        };

        client.handshake().await?;
        for (k, v) in options {
            client.set_option(k, v).await?;
        }
        client.is_ready().await?;
        Ok(client)
    }

    async fn handshake(&mut self) -> EngineResult<()> {
        self.send_line("uci").await?;
        let res = timeout(HANDSHAKE_TIMEOUT, async {
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = self.stdout.read_line(&mut buf).await?;
                if n == 0 {
                    return Err(EngineError::UnexpectedEof { expected: "uciok" });
                }
                let line = buf.trim();
                if line == "uciok" {
                    return Ok(());
                }
                if let Some((k, v)) = parse_id_line(line) {
                    match k.as_str() {
                        "name" => self.name = v,
                        "author" => self.author = Some(v),
                        _ => {}
                    }
                } else if let Some(opt) = parse_option_line(line) {
                    self.options.push(opt);
                } else {
                    debug!(uci_line = %line, "unparsed line during handshake");
                }
            }
        })
        .await;

        match res {
            Ok(inner) => inner,
            Err(_) => Err(EngineError::HandshakeTimeout(HANDSHAKE_TIMEOUT)),
        }
    }

    /// `isready` → `readyok`. Blocks until the engine has caught up, but with
    /// a timeout: a hung engine (e.g. a stuck `setoption` allocating a huge
    /// hash) must not pin the bot's startup/restart forever in async context.
    pub async fn is_ready(&mut self) -> EngineResult<()> {
        self.send_line("isready").await?;
        let wait = async {
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = self.stdout.read_line(&mut buf).await?;
                if n == 0 {
                    return Err(EngineError::UnexpectedEof { expected: "readyok" });
                }
                if buf.trim() == "readyok" {
                    return Ok(());
                }
            }
        };
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, wait).await {
            Ok(inner) => inner,
            Err(_) => Err(EngineError::HandshakeTimeout(HANDSHAKE_TIMEOUT)),
        }
    }

    /// `setoption name X value Y` (omit `value` for buttons).
    pub async fn set_option(&mut self, name: &str, value: &str) -> EngineResult<()> {
        let line = if value.is_empty() {
            format!("setoption name {name}")
        } else {
            format!("setoption name {name} value {value}")
        };
        self.send_line(&line).await
    }

    /// Whether the engine declared a UCI option with this exact name in
    /// its handshake.
    pub fn has_option(&self, name: &str) -> bool {
        self.options.iter().any(|o| o.name == name)
    }

    /// Send `UCI_Opponent` (and `UCI_RatingAdv` for our own rating) only
    /// if the engine declared the option in its handshake. Python's
    /// `engine.send_opponent_information` does the same gate via
    /// python-chess's automatic option-presence check.
    ///
    /// `our_rating` is the bot's own rating in the speed being played;
    /// most engines don't expose `UCI_RatingAdv`, so the second send
    /// is usually a no-op.
    pub async fn send_opponent_info(
        &mut self,
        opp: &OpponentInfo,
        our_rating: Option<i64>,
    ) -> EngineResult<()> {
        if self.has_option("UCI_Opponent") {
            let value = opp.uci_opponent_value();
            self.set_option("UCI_Opponent", &value).await?;
        }
        if let (Some(r), true) = (our_rating, self.has_option("UCI_RatingAdv")) {
            self.set_option("UCI_RatingAdv", &r.to_string()).await?;
        }
        Ok(())
    }

    /// Inform the engine that the game has ended via the UCI v2
    /// `gameover` extension. Not all engines understand the line;
    /// those that don't simply ignore it. The result token (`1-0` /
    /// `0-1` / `1/2-1/2` / `*`) comes from [`Game::result`]; the reason
    /// is derived from `state.status`+`winner` exactly like
    /// `engine_wrapper.send_game_result` in Python.
    pub async fn send_game_result(&mut self, game: &Game) -> EngineResult<()> {
        let line = format_gameover_line(game);
        self.send_line(&line).await
    }

    /// Read-only access to all commentary the engine has emitted so far
    /// in this game. PGN exporters merge this into the lichess PGN to
    /// produce annotated game records.
    pub fn move_commentary(&self) -> &[MoveCommentary] {
        &self.move_commentary
    }

    /// Principal variation from the last `info` line, in raw UCI. Empty
    /// when the last search produced no PV or no info at all.
    pub fn last_info_pv(&self) -> &[String] {
        self.last_info.last().map(|i| i.pv.as_slice()).unwrap_or(&[])
    }

    /// Append commentary for the move the bot is about to play. `pos`
    /// is the position **before** the move so SAN rendering of the PV
    /// is correct. `history_len` is the number of half-moves played so
    /// far in the game — used to track where the bot's commentary
    /// starts (so `commentary_for_half_move` can map a board index back
    /// to a commentary entry). For engine moves, `pv_uci` should be
    /// whatever the engine returned; for book/EGTB/online moves, pass
    /// an empty `Vec`.
    pub fn record_move_commentary(
        &mut self,
        pos: &shakmaty::Chess,
        history_len: usize,
        score: Option<PovScore>,
        pv_uci: Vec<String>,
    ) {
        if self.comment_start_index.is_none() {
            self.comment_start_index = Some(history_len);
        }
        let info = self.last_info.last();
        let depth = info.and_then(|i| i.depth);
        let nodes = info.and_then(|i| i.nodes);
        let time_ms = info.and_then(|i| i.time_ms);
        let nps = info.and_then(|i| i.nps);
        let pv_san = pv_to_san(pos, &pv_uci);
        self.move_commentary.push(MoveCommentary {
            score,
            depth,
            nodes,
            time_ms,
            nps,
            pv_uci,
            pv_san,
        });
    }

    /// Drop the most recent commentary entry. Mirrors Python's
    /// `discard_last_move_commentary` and is meant for the takeback
    /// case (Lichess lets the opponent retract their reply, so the
    /// commentary we recorded against that reply is no longer relevant).
    pub fn discard_last_commentary(&mut self) {
        self.move_commentary.pop();
    }

    /// Look up commentary by **half-move index** (i.e. how many plies
    /// have been played before this one). Returns `None` for opponent
    /// moves and for moves played before our first commentary entry.
    /// Mirrors Python's `comment_for_board_index`.
    pub fn commentary_for_half_move(&self, half_move_index: usize) -> Option<&MoveCommentary> {
        let start = self.comment_start_index?;
        if half_move_index < start {
            return None;
        }
        let rel = half_move_index - start;
        if rel % 2 != 0 {
            return None;
        }
        self.move_commentary.get(rel / 2)
    }

    /// One full search round: send `position` + `go`, read `info` lines, return
    /// `(best_move, ponder_move)` from `bestmove`.
    pub async fn play(
        &mut self,
        fen: Option<&str>,
        moves: &[&str],
        limits: &GoLimits,
    ) -> EngineResult<(String, Option<String>)> {
        self.send_line(&format_position(fen, moves)).await?;
        self.send_line(&format_go(limits)).await?;
        self.read_until_bestmove().await
    }

    /// Read engine output until a `bestmove` line arrives, collecting
    /// `info` lines into `last_info` along the way. Used by [`play`] and
    /// by the ponder-resolution path.
    async fn read_until_bestmove(&mut self) -> EngineResult<(String, Option<String>)> {
        // Clear here (not only in play/search) so the ponder-resolution and
        // quit paths also start fresh — otherwise last_info accumulates info
        // lines across consecutive ponderhits over a long game.
        self.last_info.clear();
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self.stdout.read_line(&mut buf).await?;
            if n == 0 {
                return Err(EngineError::UnexpectedEof { expected: "bestmove" });
            }
            let line = buf.trim();
            if line.starts_with("info ") {
                if let Some(info) = parse_info_line(line) {
                    trace!(target: "engine_uci", "uci info {}", format_uci_info_log(&info));
                    self.last_info.push(info);
                }
            } else if line.starts_with("bestmove") {
                if let Some(last) = self.last_info.last() {
                    debug!(target: "engine_uci", "search complete {}", format_uci_info_log(last));
                }
                return parse_bestmove(line);
            } else {
                debug!(uci_line = %line, "unparsed line during search");
            }
        }
    }

    /// High-level search: send `position` + `go`, collect `info` lines,
    /// parse the `bestmove`, convert it to a shakmaty move against
    /// `pos`, and package everything into a [`MoveDecision`].
    ///
    /// If a prior call started a `go ponder` search, resolve it first
    /// (`ponderhit` if the opponent played the predicted move,
    /// otherwise `stop` and discard the result). When `can_ponder` is
    /// true and the engine returned a `ponder` hint, kick off a new
    /// background search after this move so the next call has a head
    /// start.
    ///
    /// `fen` / `moves` describe the position+history *up to but not
    /// including* this side's move. `pos` is the same position as a
    /// shakmaty value used only for the UCI→shakmaty `Move` conversion.
    pub async fn search<P>(
        &mut self,
        pos: &P,
        fen: Option<&str>,
        moves: &[&str],
        limits: &GoLimits,
        can_ponder: bool,
    ) -> EngineResult<MoveDecision>
    where
        P: shakmaty::Position,
    {
        // (1) Resolve any outstanding ponder search.
        let bestmove_from_ponder = self.resolve_ponder(moves).await?;

        // (2) Either reuse the bestmove the engine produced under
        // ponderhit, or run a fresh search.
        let (best_uci, ponder_uci) = if let Some(result) = bestmove_from_ponder {
            result
        } else {
            self.send_line(&format_position(fen, moves)).await?;
            self.send_line(&format_go(limits)).await?;
            self.read_until_bestmove().await?
        };

        // (3) Convert UCI bestmove → shakmaty Move while we still have
        // the borrowed `pos`.
        let uci = shakmaty::uci::UciMove::from_ascii(best_uci.as_bytes())
            .map_err(|_| EngineError::BestmoveParse(best_uci.clone()))?;
        let mv = uci
            .to_move(pos)
            .map_err(|_| EngineError::BestmoveParse(best_uci.clone()))?;
        let score = self.last_info.last().and_then(score_from_info);

        // (4) Start the next ponder search if we can.
        if can_ponder {
            if let Some(ponder) = ponder_uci.as_deref() {
                if let Err(e) = self.start_ponder(fen, moves, &best_uci, ponder, limits).await {
                    warn!(error = %e, "starting ponder search failed");
                }
            }
        }

        Ok(MoveDecision {
            mv,
            source: MoveSource::Engine,
            score,
            draw_offered: false,
            resigned: false,
        })
    }

    /// If a ponder search is in progress, send `ponderhit` or `stop`
    /// based on what move the opponent actually played. Returns the
    /// `bestmove`+`ponder` pair the engine produces in response — caller
    /// only uses it on `ponderhit`; on miss the result is discarded.
    async fn resolve_ponder(
        &mut self,
        moves: &[&str],
    ) -> EngineResult<Option<(String, Option<String>)>> {
        let Some(state) = self.ponder_state.take() else {
            return Ok(None);
        };
        let last_opp = moves.last().copied();
        if last_opp == Some(state.expected_opponent_move.as_str()) {
            debug!(predicted = %state.expected_opponent_move, "ponder hit");
            self.send_line("ponderhit").await?;
            Ok(Some(self.read_until_bestmove().await?))
        } else {
            debug!(
                predicted = %state.expected_opponent_move,
                actual = ?last_opp,
                "ponder miss"
            );
            self.send_line("stop").await?;
            // Engine emits a bestmove in response to `stop`; discard it.
            let _ = self.read_until_bestmove().await?;
            Ok(None)
        }
    }

    /// Abort an in-flight `go ponder` search without using its result. Called
    /// when this ply's move comes from a pre-engine source (book / EGTB /
    /// online): otherwise the `go ponder` started after our previous move
    /// keeps running on a position we'll never reach, burning a core and
    /// polluting the hash until the next real search finally sends `stop`.
    /// Mirrors the stop+drain in [`quit`].
    pub async fn cancel_ponder(&mut self) -> EngineResult<()> {
        if self.ponder_state.take().is_some() {
            self.send_line("stop").await?;
            // Engine emits a bestmove in response to `stop`; discard it.
            let _ = self.read_until_bestmove().await?;
            debug!("ponder cancelled for pre-engine move");
        }
        Ok(())
    }

    /// Send `position … moves bestmove ponder_move` + `go ponder …` so
    /// the engine can search in the background while it's the
    /// opponent's turn.
    async fn start_ponder(
        &mut self,
        fen: Option<&str>,
        moves: &[&str],
        best: &str,
        ponder: &str,
        limits: &GoLimits,
    ) -> EngineResult<()> {
        let mut history: Vec<&str> = moves.to_vec();
        history.push(best);
        history.push(ponder);
        self.send_line(&format_position(fen, &history)).await?;
        let mut ponder_limits = limits.clone();
        ponder_limits.ponder = true;
        self.send_line(&format_go(&ponder_limits)).await?;
        self.ponder_state = Some(PonderState {
            expected_opponent_move: ponder.to_string(),
        });
        debug!(prediction = %ponder, "ponder started");
        Ok(())
    }

    /// Tell the engine to shut down (`quit`) and wait briefly for it to exit.
    /// If a ponder search is still in flight, stop it first so the engine
    /// is in a state where it expects further commands.
    pub async fn quit(mut self) -> EngineResult<()> {
        if self.ponder_state.take().is_some() {
            let _ = self.send_line("stop").await;
            // Engine should emit a final bestmove in response to stop.
            // Best-effort: drain it but don't fail quit on a slow engine.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.read_until_bestmove(),
            )
            .await;
        }
        let _ = self.send_line("quit").await;
        // Best-effort wait — `kill_on_drop` covers the worst case.
        match timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(EngineError::Io(e)),
            Err(_) => {
                warn!("engine did not exit within 5 s, killing");
                let _ = self.child.start_kill();
                Ok(())
            }
        }
    }

    async fn send_line(&mut self, line: &str) -> EngineResult<()> {
        debug!(send = %line, "uci ->");
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

impl EngineLike for UciClient {
    fn name(&self) -> &str {
        &self.name
    }

    fn get_stats(&self, for_chat: bool) -> Vec<String> {
        let Some(latest) = self.last_info.last() else {
            return Vec::new();
        };
        format_info_stats(latest, "Engine", for_chat)
    }

    fn last_pv(&self) -> &[String] {
        self.last_info_pv()
    }
}

// ---------------------------------------------------------------------------
// B3 — bot-computed absolute wall-clock deadline for the embedded engine
// ---------------------------------------------------------------------------

/// The time inputs the in-process embedded engine needs for one move
/// (BOT_ENGINE_INTEGRATION_PLAN.md B3). The subprocess backend ignores this and
/// keeps using `game_clock_time`.
///
/// The whole point: today the overhead is paid **twice** — `game_clock_time`
/// subtracts `setup_timer.elapsed() + move_overhead` from the side's clock, and
/// then clrsrc's `TimeManager::new` subtracts *another* hardcoded 30 ms. Here
/// the raw clocks are passed through untouched (so clrsrc still does its own
/// form-aware budgeting) and the overhead is applied exactly **once**, as the
/// gap between "now" and `max_deadline`. clrsrc's form-aware hard limit is then
/// clamped by this absolute ceiling: one time model, one truth.
///
/// `max_deadline` uses only **half** the round-trip as the network term (folded
/// into the `move_overhead` buffer): Lichess timestamps the clock at move
/// receipt and compensates lag server-side, so subtracting the full RTT would
/// throw away real thinking time (verified against the official API spec, see
/// the lichess-spec-audit). Live per-game RTT adaptation is B6 (deferred); until
/// then the fixed `move_overhead` buffer is the single, conservative source.
#[derive(Debug, Clone)]
pub struct EmbeddedTiming {
    pub wtime_ms: i64,
    pub btime_ms: i64,
    pub winc_ms: i64,
    pub binc_ms: i64,
    pub movestogo: u32,
    /// Fixed time per move (first move / correspondence); 0 = clock-based.
    pub movetime_ms: i64,
    /// Fixed-depth override; 0 = unlimited.
    pub depth: i32,
    /// Fixed-node override; 0 = unlimited.
    pub nodes: u64,
    /// Absolute instant by which the move must be chosen. Hard wall ceiling.
    pub max_deadline: std::time::Instant,
}

impl EmbeddedTiming {
    /// Derive the raw clock + absolute deadline from the live game state. `limits`
    /// carries the *mode* already decided by [`move_time`] (fixed movetime for the
    /// first move / correspondence, or a fixed depth/nodes override); the clocks
    /// are taken **raw** from `state` rather than from `limits` (which has the
    /// overhead pre-subtracted for the subprocess path).
    pub fn compute(
        side: Side,
        state: &GameStateType,
        setup_timer: &Timer,
        move_overhead: Duration,
        limits: &GoLimits,
    ) -> Self {
        let elapsed = setup_timer.time_since_reset();
        let now = std::time::Instant::now();
        // Single overhead source = move_overhead (covers ½RTT + margin). B6 will
        // shrink this with measured live RTT.
        let buffer = elapsed + move_overhead;
        let min_budget = Duration::from_millis(MIN_CLOCK_MS);

        let (max_deadline, movetime_ms) = match limits.movetime_ms {
            Some(mt) => {
                // Fixed-time mode: spend at most `mt`, minus the one overhead.
                let budget = Duration::from_millis(mt).saturating_sub(buffer).max(min_budget);
                (now + budget, mt as i64)
            }
            None => {
                // Clock mode: hard ceiling = flag-fall − overhead
                //            = now + remaining − elapsed − move_overhead,
                // then tightened to the per-move overshoot cap (see
                // `movetime_cap_ms`). clrsrc clamps its form-aware hard limit to
                // `max_deadline`, so the cap binds only when clrsrc would
                // otherwise overshoot — normal moves keep clrsrc's soft
                // adaptivity (unlike the subprocess movetime collapse).
                let (remaining, inc) = match side {
                    Side::White => (clamped_ms(state.wtime), clamped_ms(state.winc)),
                    Side::Black => (clamped_ms(state.btime), clamped_ms(state.binc)),
                };
                let hard = Duration::from_millis(remaining).saturating_sub(buffer);
                let cap = Duration::from_millis(movetime_cap_ms(remaining, inc));
                let budget = hard.min(cap).max(min_budget);
                (now + budget, 0)
            }
        };

        EmbeddedTiming {
            wtime_ms: clamped_ms(state.wtime) as i64,
            btime_ms: clamped_ms(state.btime) as i64,
            winc_ms: clamped_ms(state.winc) as i64,
            binc_ms: clamped_ms(state.binc) as i64,
            movestogo: limits.movestogo.unwrap_or(0),
            movetime_ms,
            depth: limits.depth.map(|d| d as i32).unwrap_or(0),
            nodes: limits.nodes.unwrap_or(0),
            max_deadline,
        }
    }
}

// ---------------------------------------------------------------------------
// B1 — the engine backend the bot loop drives for one game
// ---------------------------------------------------------------------------

/// The engine the bot drives for one game. The subprocess UCI client is the
/// default; the in-process embedded clrsrc engine (BOT_ENGINE_INTEGRATION_PLAN.md
/// B1) is opt-in behind the `embedded` feature and only selected for standard
/// chess (clrsrc's FEN parser has no Chess960 castling). The bot loop holds one
/// of these per game and forwards the same calls to whichever backend is active.
pub enum EngineBackend {
    Subprocess(UciClient),
    #[cfg(feature = "embedded")]
    Embedded(Box<crate::embedded_engine::EmbeddedEngine>),
}

impl EngineBackend {
    pub async fn send_opponent_info(
        &mut self,
        opp: &OpponentInfo,
        our_rating: Option<i64>,
    ) -> EngineResult<()> {
        match self {
            EngineBackend::Subprocess(c) => c.send_opponent_info(opp, our_rating).await,
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.send_opponent_info(opp, our_rating).await,
        }
    }

    pub async fn send_game_result(&mut self, game: &Game) -> EngineResult<()> {
        match self {
            EngineBackend::Subprocess(c) => c.send_game_result(game).await,
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.send_game_result(game).await,
        }
    }

    pub async fn cancel_ponder(&mut self) -> EngineResult<()> {
        match self {
            EngineBackend::Subprocess(c) => c.cancel_ponder().await,
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.cancel_ponder().await,
        }
    }

    /// Run a search. `embedded_timing` is the B3 raw-clock/deadline bundle; the
    /// subprocess backend ignores it (it already got an overhead-subtracted
    /// `GoLimits` from [`move_time`]).
    #[cfg_attr(not(feature = "embedded"), allow(unused_variables))]
    pub async fn search<P>(
        &mut self,
        pos: &P,
        fen: Option<&str>,
        moves: &[&str],
        limits: &GoLimits,
        can_ponder: bool,
        embedded_timing: EmbeddedTiming,
    ) -> EngineResult<MoveDecision>
    where
        P: shakmaty::Position,
    {
        match self {
            EngineBackend::Subprocess(c) => {
                c.search(pos, fen, moves, limits, can_ponder).await
            }
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => {
                e.search(pos, fen, moves, embedded_timing, can_ponder).await
            }
        }
    }

    pub fn last_info_pv(&self) -> &[String] {
        match self {
            EngineBackend::Subprocess(c) => c.last_info_pv(),
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.last_info_pv(),
        }
    }

    pub fn record_move_commentary(
        &mut self,
        pos: &shakmaty::Chess,
        history_len: usize,
        score: Option<PovScore>,
        pv_uci: Vec<String>,
    ) {
        match self {
            EngineBackend::Subprocess(c) => {
                c.record_move_commentary(pos, history_len, score, pv_uci)
            }
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => {
                e.record_move_commentary(pos, history_len, score, pv_uci)
            }
        }
    }

    pub fn commentary_for_half_move(&self, half_move_index: usize) -> Option<&MoveCommentary> {
        match self {
            EngineBackend::Subprocess(c) => c.commentary_for_half_move(half_move_index),
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.commentary_for_half_move(half_move_index),
        }
    }

    pub async fn quit(self) -> EngineResult<()> {
        match self {
            EngineBackend::Subprocess(c) => c.quit().await,
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.quit().await,
        }
    }
}

impl EngineLike for EngineBackend {
    fn name(&self) -> &str {
        match self {
            EngineBackend::Subprocess(c) => c.name(),
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.name(),
        }
    }

    fn get_stats(&self, for_chat: bool) -> Vec<String> {
        match self {
            EngineBackend::Subprocess(c) => c.get_stats(for_chat),
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.get_stats(for_chat),
        }
    }

    fn last_pv(&self) -> &[String] {
        match self {
            EngineBackend::Subprocess(c) => c.last_pv(),
            #[cfg(feature = "embedded")]
            EngineBackend::Embedded(e) => e.last_pv(),
        }
    }
}

/// Build the `"Key: value"` lines reported in the game log and (optionally)
/// the game chat. Mirrors the Python `EngineWrapper.get_stats` order:
/// Source / Evaluation / Depth / Nodes / Speed / Pv. If `for_chat == true`,
/// the PV is clipped move-by-move so the entire joined message stays
/// under [`MAX_CHAT_MESSAGE_LEN`].
pub fn format_info_stats(info: &InfoLine, source: &str, for_chat: bool) -> Vec<String> {
    let mut entries: Vec<(&'static str, String)> = Vec::new();
    entries.push(("Source", source.to_string()));

    let score = if info.score_mate.is_some() {
        Some(PovScore::from_mate(info.score_mate.unwrap()))
    } else {
        info.score_cp.map(PovScore::from_cp)
    };
    if let Some(s) = score.as_ref() {
        entries.push(("Evaluation", readable_score(s)));
    }
    if let Some(d) = info.depth {
        entries.push(("Depth", d.to_string()));
    }
    if let Some(n) = info.nodes {
        entries.push(("Nodes", readable_number(n)));
    }
    if let Some(n) = info.nps {
        entries.push(("Speed", format!("{}nps", readable_number(n))));
    }

    let pv_string = clip_pv_for_chat(&info.pv, for_chat, &entries);
    if !pv_string.is_empty() {
        entries.push(("Pv", pv_string));
    }

    entries
        .into_iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect()
}

fn clip_pv_for_chat(
    pv: &[String],
    for_chat: bool,
    other_entries: &[(&'static str, String)],
) -> String {
    if pv.is_empty() {
        return String::new();
    }
    if !for_chat {
        return pv.join(" ");
    }
    // Python: `len(", ".join(bot_stats)) + PONDERPV_CHARACTERS` where
    // PONDERPV_CHARACTERS = len(", Pv: ") = 6. We replicate that by
    // pre-joining the non-PV entries.
    let bot_stats: Vec<String> = other_entries
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect();
    let header_len = bot_stats.join(", ").chars().count() + ", Pv: ".chars().count();
    let mut moves: Vec<&str> = pv.iter().map(String::as_str).collect();
    while !moves.is_empty()
        && moves.join(" ").chars().count() + header_len > MAX_CHAT_MESSAGE_LEN
    {
        moves.pop();
    }
    if let Some(last) = moves.last() {
        if last.ends_with('.') {
            moves.pop();
        }
    }
    moves.join(" ")
}

// ---------------------------------------------------------------------------
// play_move orchestration
// ---------------------------------------------------------------------------

/// Convert shakmaty's `Color` into our board-free [`Side`] enum.
pub fn side_from_color(c: shakmaty::Color) -> Side {
    match c {
        shakmaty::Color::White => Side::White,
        shakmaty::Color::Black => Side::Black,
    }
}

/// Try the non-engine move sources in Python's order: opening book →
/// local EGTB → online book / online EGTB. Returns the first hit, or
/// `None` when the engine should pick the move itself.
///
/// EGTB and online sources are still stubs (always `None`), but the
/// call sites are in place so [`play_move`] keeps its top-down shape.
pub async fn choose_pre_engine_move<R>(
    pos: &shakmaty::Chess,
    half_move_count: usize,
    game: &Game,
    li: &crate::lichess::Lichess,
    cfg: &crate::config::EngineConfig,
    rng: &mut R,
) -> Option<PreEngineResult>
where
    R: rand::Rng + ?Sized,
{
    if let Some(book) = crate::polyglot::get_book_move(
        pos,
        half_move_count,
        &cfg.polyglot,
        &game.variant_key,
        rng,
    ) {
        return Some(PreEngineResult::Decision(MoveDecision::new(
            book.mv,
            MoveSource::OpeningBook(book.book_label),
        )));
    }
    if let Some(d) = crate::egtb::get_egtb_move(pos, game, &cfg.lichess_bot_tbs, &cfg.draw_or_resign) {
        return Some(d);
    }
    if let Some(d) =
        crate::online_book::get_online_move(li, pos, game, &cfg.online_moves, &cfg.draw_or_resign, rng).await
    {
        return Some(d);
    }
    None
}

/// Full move pipeline for one ply, mirroring Python's
/// `EngineWrapper.play_move`:
///
/// 1. Try opening book / local EGTB / online sources via
///    [`choose_pre_engine_move`].
/// 2. If no source picked a final move, compute a UCI search budget
///    with [`move_time`] (restricting `searchmoves` to a suggestion list
///    if the source returned one) and call `engine.search(...)`.
/// 3. Record the resulting `score` in `draw_resign` and let it decide
///    whether to flip `draw_offered` / `resigned`.
/// 4. Honour `min_time` (`tokio::time::sleep` if we got here too fast).
/// 5. Either `li.resign(game.id)` or `li.make_move(game.id, uci, draw)`.
///
/// Returns the final `MoveDecision` for callers that want to add a
/// commentary entry or update game state.
#[allow(clippy::too_many_arguments)]
pub async fn play_move<R>(
    engine: &mut EngineBackend,
    pos: &shakmaty::Chess,
    initial_fen: Option<&str>,
    history_moves: &[&str],
    game: &Game,
    li: &crate::lichess::Lichess,
    setup_timer: &Timer,
    move_overhead: Duration,
    can_ponder: bool,
    is_correspondence: bool,
    correspondence_move_time: Duration,
    cfg: &crate::config::EngineConfig,
    min_time: Duration,
    draw_resign: &mut DrawResignTracker,
    rng: &mut R,
) -> EngineResult<MoveDecision>
where
    R: rand::Rng + ?Sized,
{
    let pre =
        choose_pre_engine_move(pos, history_moves.len(), game, li, cfg, rng).await;

    let side = side_from_color(pos.turn());
    let mut decision = match pre {
        Some(PreEngineResult::Decision(d)) => {
            // Playing a pre-engine move (book / EGTB / online): the engine's
            // `search()` — and with it `resolve_ponder` — is skipped on this
            // path, so a ponder search started after our previous move would
            // keep running on a line we won't reach. Stop it explicitly.
            if let Err(e) = engine.cancel_ponder().await {
                warn!(error = %e, "cancelling stale ponder before pre-engine move failed");
            }
            d
        }
        Some(PreEngineResult::Suggest(moves)) => {
            let (mut limits, _) = move_time(
                side,
                &game.state,
                can_ponder,
                setup_timer,
                move_overhead,
                is_correspondence,
                correspondence_move_time,
            );
            limits.searchmoves = moves
                .iter()
                .map(|m| shakmaty::uci::UciMove::from_standard(m).to_string())
                .collect();
            let timing = EmbeddedTiming::compute(side, &game.state, setup_timer, move_overhead, &limits);
            engine.search(pos, initial_fen, history_moves, &limits, can_ponder, timing).await?
        }
        None => {
            let (limits, _) = move_time(
                side,
                &game.state,
                can_ponder,
                setup_timer,
                move_overhead,
                is_correspondence,
                correspondence_move_time,
            );
            let timing = EmbeddedTiming::compute(side, &game.state, setup_timer, move_overhead, &limits);
            engine.search(pos, initial_fen, history_moves, &limits, can_ponder, timing).await?
        }
    };

    // The draw/resign tracker only considers engine evaluations — book
    // and tablebase moves get their own draw/resign flags directly from
    // their sources (Python does this implicitly by only appending to
    // `self.scores` from within `search`).
    if matches!(decision.source, MoveSource::Engine) {
        if let Some(score) = decision.score {
            draw_resign.record(score);
        }
        let piece_count = pos.board().occupied().count() as u32;
        let d = draw_resign.decide(&cfg.draw_or_resign, piece_count);
        decision.draw_offered = decision.draw_offered || d.offer_draw;
        decision.resigned = decision.resigned || d.resign;
    }

    // Capture commentary for this ply. Engine moves get the PV from
    // the last info line; book / EGTB / online moves get an empty PV
    // (their MoveDecision doesn't currently carry one — fine for the
    // PGN merger, which only needs score + depth there).
    let pv_uci: Vec<String> = if matches!(decision.source, MoveSource::Engine) {
        engine.last_info_pv().to_vec()
    } else {
        Vec::new()
    };
    engine.record_move_commentary(pos, history_moves.len(), decision.score, pv_uci);

    // Heed min_time: don't fire a move back at Lichess sooner than this
    // (mostly relevant for very fast book/EGTB responses on bullet).
    let elapsed = setup_timer.time_since_reset();
    if elapsed < min_time {
        tokio::time::sleep(min_time - elapsed).await;
    }

    if decision.resigned && history_moves.len() >= 2 {
        if let Err(e) = li.resign(&game.id).await {
            warn!(game_id = %game.id, error = %e, "resign request failed");
        }
    } else {
        let uci = if game.variant_key == "chess960" || game.variant_key == "fromPosition" {
            decision.uci_chess960()
        } else {
            decision.uci_standard()
        };
        // `make_move` already retries transient failures via `with_backoff`;
        // an error here means it exhausted them, so the move never reached
        // Lichess — log it loudly rather than silently flagging on time.
        if let Err(e) = li.make_move(&game.id, &uci, decision.draw_offered).await {
            warn!(game_id = %game.id, %uci, error = %e, "make_move failed; move not delivered to Lichess");
        }
    }

    Ok(decision)
}

// ---------------------------------------------------------------------------
// Tests (pure parser / formatter logic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bestmove_with_ponder() {
        let (best, ponder) = parse_bestmove("bestmove e2e4 ponder e7e5").unwrap();
        assert_eq!(best, "e2e4");
        assert_eq!(ponder.as_deref(), Some("e7e5"));
    }

    #[test]
    fn bestmove_without_ponder() {
        let (best, ponder) = parse_bestmove("bestmove a7a8q").unwrap();
        assert_eq!(best, "a7a8q");
        assert!(ponder.is_none());
    }

    #[test]
    fn bestmove_garbage_rejected() {
        assert!(parse_bestmove("info depth 1").is_err());
        assert!(parse_bestmove("bestmove").is_err());
    }

    #[test]
    fn id_line_splits_key_and_value() {
        assert_eq!(
            parse_id_line("id name Stockfish 16"),
            Some(("name".into(), "Stockfish 16".into()))
        );
        assert_eq!(
            parse_id_line("id author T. Romstad"),
            Some(("author".into(), "T. Romstad".into()))
        );
        assert!(parse_id_line("uciok").is_none());
    }

    #[test]
    fn option_line_with_spaces_in_name_and_default() {
        let opt = parse_option_line(
            "option name Move Overhead type spin default 30 min 0 max 5000",
        )
        .unwrap();
        assert_eq!(opt.name, "Move Overhead");
        assert_eq!(opt.kind, "spin");
        assert_eq!(opt.default.as_deref(), Some("30"));
        assert_eq!(opt.min.as_deref(), Some("0"));
        assert_eq!(opt.max.as_deref(), Some("5000"));
    }

    #[test]
    fn option_line_with_combo_vars() {
        let opt = parse_option_line(
            "option name Style type combo default Normal var Solid var Normal var Risky",
        )
        .unwrap();
        assert_eq!(opt.kind, "combo");
        assert_eq!(opt.vars, vec!["Solid", "Normal", "Risky"]);
    }

    #[test]
    fn info_line_basic_fields() {
        let info = parse_info_line(
            "info depth 12 score cp 24 nodes 1500 nps 75000 time 20 pv e2e4 e7e5",
        )
        .unwrap();
        assert_eq!(info.depth, Some(12));
        assert_eq!(info.score_cp, Some(24));
        assert_eq!(info.nodes, Some(1500));
        assert_eq!(info.nps, Some(75000));
        assert_eq!(info.time_ms, Some(20));
        assert_eq!(info.pv, vec!["e2e4", "e7e5"]);
    }

    #[test]
    fn info_line_mate_score() {
        let info = parse_info_line("info depth 5 score mate 3 pv e2e4 e7e5 d1h5").unwrap();
        assert_eq!(info.score_mate, Some(3));
        assert!(info.score_cp.is_none());
        assert_eq!(info.pv.len(), 3);
    }

    #[test]
    fn info_line_unknown_keys_land_in_raw() {
        let info = parse_info_line("info depth 1 hashfull 423 multipv 1").unwrap();
        assert!(info.raw.iter().any(|(k, v)| k == "hashfull" && v == "423"));
        assert!(info.raw.iter().any(|(k, v)| k == "multipv" && v == "1"));
    }

    #[test]
    fn format_position_uses_startpos_by_default() {
        assert_eq!(format_position(None, &[]), "position startpos");
        assert_eq!(format_position(Some("startpos"), &[]), "position startpos");
        assert_eq!(
            format_position(None, &["e2e4", "e7e5"]),
            "position startpos moves e2e4 e7e5"
        );
    }

    #[test]
    fn format_position_uses_fen_when_provided() {
        let fen = "r1bqkbnr/pppppppp/2n5/8/8/2N5/PPPPPPPP/R1BQKBNR w KQkq - 2 2";
        assert_eq!(
            format_position(Some(fen), &["d2d4"]),
            format!("position fen {fen} moves d2d4")
        );
    }

    #[test]
    fn format_go_writes_clock_and_increment() {
        let limits = GoLimits {
            wtime_ms: Some(300_000),
            btime_ms: Some(295_000),
            winc_ms: Some(2_000),
            binc_ms: Some(2_000),
            ..Default::default()
        };
        assert_eq!(
            format_go(&limits),
            "go wtime 300000 btime 295000 winc 2000 binc 2000"
        );
    }

    #[test]
    fn format_go_movetime() {
        let limits = GoLimits::movetime(1500);
        assert_eq!(format_go(&limits), "go movetime 1500");
    }

    #[test]
    fn format_go_ponder_and_infinite() {
        let limits = GoLimits {
            ponder: true,
            infinite: true,
            ..Default::default()
        };
        assert_eq!(format_go(&limits), "go ponder infinite");
    }

    #[test]
    fn format_go_appends_searchmoves_at_the_end() {
        let limits = GoLimits {
            movetime_ms: Some(2000),
            searchmoves: vec!["e2e4".into(), "d2d4".into()],
            ..Default::default()
        };
        assert_eq!(
            format_go(&limits),
            "go movetime 2000 searchmoves e2e4 d2d4"
        );
    }

    #[test]
    fn readable_number_formats_thousands_and_millions() {
        assert_eq!(readable_number(999), "999");
        assert_eq!(readable_number(1_500), "1.5K");
        assert_eq!(readable_number(2_300_000), "2.3M");
        assert_eq!(readable_number(7_500_000_000), "7.5B");
    }

    // -----------------------------------------------------------------
    // Clock helpers
    // -----------------------------------------------------------------

    fn state_with_moves(moves: &str, w: i64, b: i64, winc: i64, binc: i64) -> GameStateType {
        GameStateType {
            moves: Some(moves.to_string()),
            wtime: Some(w),
            btime: Some(b),
            winc: Some(winc),
            binc: Some(binc),
            ..Default::default()
        }
    }

    #[test]
    fn move_count_handles_empty_and_missing() {
        let mut s = GameStateType::default();
        assert_eq!(move_count(&s), 0);
        s.moves = Some(String::new());
        assert_eq!(move_count(&s), 0);
        s.moves = Some("e2e4".into());
        assert_eq!(move_count(&s), 1);
        s.moves = Some("e2e4 e7e5 d2d4".into());
        assert_eq!(move_count(&s), 3);
    }

    #[test]
    fn first_move_time_is_ten_seconds_no_pondering() {
        let state = state_with_moves("", 60_000, 60_000, 0, 0);
        let (limits, can_ponder) =
            move_time(Side::White, &state, true, &Timer::zero(), Duration::ZERO, false, Duration::ZERO);
        assert_eq!(limits.movetime_ms, Some(10_000));
        assert!(!can_ponder);
    }

    /// `Timer::zero()` starts at `Instant::now()`, so by the time the
    /// helpers read `time_since_reset()` we've already accumulated a few
    /// microseconds. Allow a small slack on exact-millisecond asserts.
    fn close_to(actual: Option<u64>, expected: u64, slack_ms: u64) -> bool {
        match actual {
            Some(a) if a <= expected && expected - a <= slack_ms => true,
            Some(a) if a > expected => false,
            _ => false,
        }
    }

    #[test]
    fn second_move_uses_game_clock_in_realtime() {
        // 2 half-moves already played → next move is the 3rd, no longer
        // covered by the fixed "first move" budget. The realtime path now caps
        // the search at remaining/30 + inc via `go movetime`.
        let state = state_with_moves("e2e4 e7e5", 300_000, 295_000, 2_000, 2_000);
        let (limits, can_ponder) = move_time(
            Side::White,
            &state,
            true,
            &Timer::zero(),
            Duration::ZERO,
            false,
            Duration::ZERO,
        );
        // 300000/30 + 2000 = 12000. The cap is derived from the raw clock, so
        // the tiny Timer drift doesn't move it (it only shrinks `avail`, which
        // stays far above the cap here).
        assert_eq!(limits.movetime_ms, Some(12_000));
        assert!(limits.wtime_ms.is_none());
        assert!(can_ponder);
    }

    #[test]
    fn game_clock_caps_movetime_per_side() {
        let state = state_with_moves("e2e4 e7e5", 300_000, 295_000, 2_000, 2_000);
        // White: 300000/30 + 2000 = 12000.
        let w = game_clock_time(Side::White, &state, &Timer::zero(), Duration::from_millis(500));
        assert_eq!(w.movetime_ms, Some(12_000));
        assert!(w.wtime_ms.is_none());
        // Black: 295000/30 + 2000 = 11833 — the cap follows the side to move.
        let b = game_clock_time(Side::Black, &state, &Timer::zero(), Duration::from_millis(500));
        assert_eq!(b.movetime_ms, Some(11_833));
    }

    #[test]
    fn game_clock_clamps_to_one_ms_when_overhead_exceeds_remaining() {
        // 50 ms left, 100 ms overhead → `avail` clamps to 1 ms, below the
        // cap (50/30 = 1), so the movetime clamps to the 1 ms floor.
        let state = state_with_moves("e2e4 e7e5", 50, 60_000, 0, 0);
        let limits = game_clock_time(
            Side::White,
            &state,
            &Timer::zero(),
            Duration::from_millis(100),
        );
        assert_eq!(limits.movetime_ms, Some(1));
    }

    #[test]
    fn embedded_clock_mode_tightens_deadline_to_overshoot_cap() {
        // Plenty of clock → the per-move cap (300000/30 + 2000 = 12000 ms), not
        // the flag-fall ceiling, is what bounds `max_deadline`. Raw clocks still
        // ride along untouched for clrsrc's own soft computation.
        let state = state_with_moves("e2e4 e7e5", 300_000, 295_000, 2_000, 2_000);
        let t = EmbeddedTiming::compute(
            Side::White,
            &state,
            &Timer::zero(),
            Duration::from_millis(300),
            &GoLimits::default(),
        );
        let budget_ms = t
            .max_deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64;
        assert!(
            (11_900..=12_000).contains(&budget_ms),
            "expected ~12000 ms cap, got {budget_ms}"
        );
        assert_eq!(t.wtime_ms, 300_000);
        assert_eq!(t.movetime_ms, 0);
    }

    #[test]
    fn embedded_clock_mode_falls_back_to_flag_fall_in_scramble() {
        // 5 s left, 300 ms overhead → cap would be 5000/30 = 166 ms, but the
        // flag-fall ceiling (5000 − 300 = 4700) is larger, so the cap binds.
        // Conversely with no increment and a near-flat clock the hard ceiling
        // is the smaller term and protects the flag.
        let state = state_with_moves("e2e4 e7e5", 400, 60_000, 0, 0);
        let t = EmbeddedTiming::compute(
            Side::White,
            &state,
            &Timer::zero(),
            Duration::from_millis(300),
            &GoLimits::default(),
        );
        let budget_ms = t
            .max_deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64;
        // cap = 400/30 = 13 ms; hard = 400 − 300 = 100 ms → min = 13 ms.
        assert!(budget_ms <= 13, "expected cap ≤13 ms, got {budget_ms}");
    }

    #[test]
    fn correspondence_caps_at_remaining_clock() {
        let state = state_with_moves("e2e4 e7e5", 5_000, 60_000, 0, 0);
        let (limits, _) = move_time(
            Side::White,
            &state,
            false,
            &Timer::zero(),
            Duration::ZERO,
            true,
            Duration::from_secs(30),
        );
        // 30 s search requested, but only ~5 s on the clock — must cap.
        assert!(close_to(limits.movetime_ms, 5_000, 5));
    }

    #[test]
    fn correspondence_uses_full_search_when_clock_is_plenty() {
        let state = state_with_moves("e2e4 e7e5", 600_000, 600_000, 0, 0);
        let (limits, _) = move_time(
            Side::Black,
            &state,
            false,
            &Timer::zero(),
            Duration::ZERO,
            true,
            Duration::from_secs(30),
        );
        assert_eq!(limits.movetime_ms, Some(30_000));
    }

    #[test]
    fn check_for_draw_offer_reads_opponent_color() {
        use crate::lichess_types::GameEventType;
        let mut info = GameEventType::default();
        info.id = Some("abcd1234".into());
        info.state = Some(GameStateType {
            wdraw: Some(true),
            ..Default::default()
        });
        // Player is white, opponent is "black"-flag side. Setting `wdraw`
        // means white offered → opponent_color != white → no offer.
        let mut game = Game::new(&info, "white_player", "https://lichess.org/", Duration::ZERO);
        game.is_white = true;
        game.opponent_color = "black";
        assert!(!check_for_draw_offer(&game));
        // Now flip: opponent is white, draw flag on white side = real offer.
        game.opponent_color = "white";
        assert!(check_for_draw_offer(&game));
    }

    // -----------------------------------------------------------------
    // Draw / resign tracker
    // -----------------------------------------------------------------

    fn dor(
        draw: bool,
        draw_score: i64,
        draw_moves: u32,
        draw_pieces: u32,
        resign: bool,
        resign_score: i64,
        resign_moves: u32,
    ) -> DrawOrResignConfig {
        DrawOrResignConfig {
            resign_enabled: resign,
            resign_score,
            resign_for_egtb_minus_two: true,
            resign_moves,
            offer_draw_enabled: draw,
            offer_draw_score: draw_score,
            offer_draw_for_egtb_zero: true,
            offer_draw_moves: draw_moves,
            offer_draw_pieces: draw_pieces,
        }
    }

    #[test]
    fn tracker_offers_draw_when_window_is_flat() {
        let cfg = dor(true, 10, 3, 32, false, 0, 0);
        let mut t = DrawResignTracker::new();
        t.record(PovScore::from_cp(5));
        t.record(PovScore::from_cp(-3));
        t.record(PovScore::from_cp(0));
        let d = t.decide(&cfg, 12);
        assert!(d.offer_draw);
        assert!(!d.resign);
    }

    #[test]
    fn tracker_does_not_offer_draw_with_pieces_above_threshold() {
        let cfg = dor(true, 10, 3, 10, false, 0, 0);
        let mut t = DrawResignTracker::new();
        for _ in 0..3 {
            t.record(PovScore::from_cp(0));
        }
        let d = t.decide(&cfg, 24);
        assert!(!d.offer_draw);
    }

    #[test]
    fn tracker_does_not_offer_draw_when_one_score_is_winning() {
        let cfg = dor(true, 10, 3, 32, false, 0, 0);
        let mut t = DrawResignTracker::new();
        t.record(PovScore::from_cp(0));
        t.record(PovScore::from_cp(500));
        t.record(PovScore::from_cp(0));
        assert!(!t.decide(&cfg, 8).offer_draw);
    }

    #[test]
    fn tracker_resigns_after_consecutive_losing_scores() {
        let cfg = dor(false, 0, 0, 0, true, -1000, 3);
        let mut t = DrawResignTracker::new();
        t.record(PovScore::from_cp(-1100));
        t.record(PovScore::from_cp(-1200));
        t.record(PovScore::from_cp(-1500));
        let d = t.decide(&cfg, 20);
        assert!(d.resign);
    }

    #[test]
    fn tracker_does_not_resign_if_one_score_above_threshold() {
        let cfg = dor(false, 0, 0, 0, true, -1000, 3);
        let mut t = DrawResignTracker::new();
        t.record(PovScore::from_cp(-1100));
        t.record(PovScore::from_cp(-200));
        t.record(PovScore::from_cp(-1500));
        assert!(!t.decide(&cfg, 20).resign);
    }

    #[test]
    fn pov_score_collapses_mate_to_clamped_cp() {
        // mate in 5 from our perspective: huge positive.
        assert_eq!(PovScore::from_mate(5).to_cp(40_000), 39_995);
        // mate in -3 (we get mated in 3): huge negative.
        assert_eq!(PovScore::from_mate(-3).to_cp(40_000), -39_997);
        // plain cp passes through.
        assert_eq!(PovScore::from_cp(-123).to_cp(40_000), -123);
    }

    // -----------------------------------------------------------------
    // Readable formatters
    // -----------------------------------------------------------------

    #[test]
    fn readable_score_handles_cp_and_mate() {
        assert_eq!(readable_score(&PovScore::from_cp(34)), "0.34");
        assert_eq!(readable_score(&PovScore::from_cp(-156)), "-1.56");
        assert_eq!(readable_score(&PovScore::from_mate(5)), "#5");
        assert_eq!(readable_score(&PovScore::from_mate(-2)), "#-2");
    }

    #[test]
    fn readable_wdl_matches_python_expectation() {
        assert_eq!(readable_wdl(500, 500, 0), "75.0%");
        assert_eq!(readable_wdl(1000, 0, 0), "100.0%");
        assert_eq!(readable_wdl(0, 0, 1000), "0.0%");
        assert_eq!(readable_wdl(0, 0, 0), "0.0%");
    }

    #[test]
    fn readable_time_minutes_and_seconds() {
        assert_eq!(readable_time(45.0), "45.0s");
        assert_eq!(readable_time(123.5), "2m 3.5s");
        assert_eq!(readable_time(0.7), "0.7s");
    }

    // -----------------------------------------------------------------
    // Opponent info
    // -----------------------------------------------------------------

    #[test]
    fn opponent_info_value_includes_title_rating_kind_name() {
        let opp = OpponentInfo {
            title: Some("GM".into()),
            rating: Some(2900),
            is_bot: false,
            name: "Magnus".into(),
        };
        assert_eq!(opp.uci_opponent_value(), "GM 2900 human Magnus");
    }

    #[test]
    fn opponent_info_value_marks_bots_as_computer() {
        let opp = OpponentInfo {
            title: Some("BOT".into()),
            rating: Some(3500),
            is_bot: true,
            name: "Stockfish".into(),
        };
        assert_eq!(opp.uci_opponent_value(), "BOT 3500 computer Stockfish");
    }

    #[test]
    fn opponent_info_value_substitutes_none_for_missing_title_and_rating() {
        let opp = OpponentInfo {
            title: None,
            rating: None,
            is_bot: false,
            name: "Anonymous".into(),
        };
        assert_eq!(opp.uci_opponent_value(), "none none human Anonymous");
    }

    #[test]
    fn opponent_info_value_treats_empty_title_as_none() {
        let opp = OpponentInfo {
            title: Some(String::new()),
            rating: Some(1500),
            is_bot: false,
            name: "Beginner".into(),
        };
        assert_eq!(opp.uci_opponent_value(), "none 1500 human Beginner");
    }

    #[test]
    fn opponent_info_from_player_copies_fields() {
        use crate::lichess_types::PlayerType;
        let raw = PlayerType {
            name: Some("Bot1".into()),
            title: Some("BOT".into()),
            rating: Some(2400),
            ..Default::default()
        };
        let p = crate::model::Player::new(&raw);
        let opp = OpponentInfo::from_player(&p);
        assert_eq!(opp.name, "Bot1");
        assert_eq!(opp.title.as_deref(), Some("BOT"));
        assert_eq!(opp.rating, Some(2400));
        assert!(opp.is_bot, "BOT title should mark this as a bot");
    }

    // -----------------------------------------------------------------
    // format_gameover_line
    // -----------------------------------------------------------------

    fn fixture_game_for_result(status: &str, winner: Option<&str>) -> Game {
        use crate::lichess_types::{GameEventType, GameStateType, PlayerType, VariantInfo};
        let mut info = GameEventType::default();
        info.id = Some("gx".into());
        info.speed = Some("blitz".into());
        let mut state = GameStateType::default();
        state.status = Some(status.into());
        state.winner = winner.map(String::from);
        info.state = Some(state);
        let mut variant = VariantInfo::default();
        variant.key = Some("standard".into());
        info.variant = Some(variant);
        info.white = Some(PlayerType { name: Some("us".into()), ..Default::default() });
        info.black = Some(PlayerType { name: Some("them".into()), ..Default::default() });
        Game::new(&info, "us", "https://lichess.org/", std::time::Duration::ZERO)
    }

    #[test]
    fn gameover_line_for_mate_has_no_reason() {
        // Lichess sends winner explicitly even on mate; Python skips the
        // reason field anyway.
        let game = fixture_game_for_result("mate", Some("white"));
        assert_eq!(format_gameover_line(&game), "gameover 1-0");
    }

    #[test]
    fn gameover_line_for_resign_names_resigning_side() {
        let game = fixture_game_for_result("resign", Some("black"));
        // black is the winner → white resigned.
        assert_eq!(format_gameover_line(&game), "gameover 0-1 reason \"White resigned\"");
        let game = fixture_game_for_result("resign", Some("white"));
        assert_eq!(format_gameover_line(&game), "gameover 1-0 reason \"Black resigned\"");
    }

    #[test]
    fn gameover_line_for_abort_uses_star_result() {
        let game = fixture_game_for_result("aborted", None);
        assert_eq!(format_gameover_line(&game), "gameover * reason \"Game aborted\"");
    }

    #[test]
    fn gameover_line_for_draw_uses_default_reason() {
        let game = fixture_game_for_result("draw", None);
        assert_eq!(
            format_gameover_line(&game),
            "gameover 1/2-1/2 reason \"Draw by agreement\""
        );
    }

    #[test]
    fn gameover_line_for_timeout_with_winner_names_timeouter() {
        let game = fixture_game_for_result("outoftime", Some("white"));
        assert_eq!(format_gameover_line(&game), "gameover 1-0 reason \"Black timeout\"");
    }

    #[test]
    fn gameover_line_for_timeout_without_winner_is_drawish() {
        let game = fixture_game_for_result("outoftime", None);
        assert_eq!(
            format_gameover_line(&game),
            "gameover 1/2-1/2 reason \"Time draw / insufficient material\""
        );
    }

    // -----------------------------------------------------------------
    // pv_to_san
    // -----------------------------------------------------------------

    #[test]
    fn pv_to_san_renders_opening_moves() {
        let pos = shakmaty::Chess::default();
        let pv = vec!["e2e4".into(), "e7e5".into(), "g1f3".into(), "b8c6".into()];
        let san = pv_to_san(&pos, &pv);
        assert_eq!(san, "e4 e5 Nf3 Nc6");
    }

    #[test]
    fn pv_to_san_stops_at_illegal_move() {
        let pos = shakmaty::Chess::default();
        let pv = vec!["e2e4".into(), "rotten-token".into(), "e7e5".into()];
        let san = pv_to_san(&pos, &pv);
        // Stops cleanly at "rotten-token"; first move still renders.
        assert_eq!(san, "e4");
    }

    #[test]
    fn pv_to_san_renders_captures_and_checks_without_plus_suffix() {
        // After 1.e4 e5 2.Bc4 Nc6 3.Qh5 Nf6?? 4.Qxf7#  — make sure
        // captures render as `Qxf7` etc. We use San (not SanPlus), so
        // no `#` suffix.
        let pos = shakmaty::Chess::default();
        let pv = vec![
            "e2e4".into(), "e7e5".into(),
            "f1c4".into(), "b8c6".into(),
            "d1h5".into(), "g8f6".into(),
            "h5f7".into(),
        ];
        let san = pv_to_san(&pos, &pv);
        assert!(san.ends_with("Qxf7"), "got: {san}");
    }

    #[test]
    fn pv_to_san_empty_pv_returns_empty_string() {
        let pos = shakmaty::Chess::default();
        assert_eq!(pv_to_san(&pos, &[]), "");
    }

    // -----------------------------------------------------------------
    // get_stats / format_info_stats
    // -----------------------------------------------------------------

    #[test]
    fn format_info_stats_uses_python_ordering() {
        let info = InfoLine {
            depth: Some(12),
            score_cp: Some(34),
            nodes: Some(1_500_000),
            nps: Some(75_000),
            pv: vec!["e2e4".into(), "e7e5".into()],
            ..Default::default()
        };
        let lines = format_info_stats(&info, "Engine", false);
        assert_eq!(
            lines,
            vec![
                "Source: Engine",
                "Evaluation: 0.34",
                "Depth: 12",
                "Nodes: 1.5M",
                "Speed: 75Knps",
                "Pv: e2e4 e7e5",
            ]
        );
    }

    // -----------------------------------------------------------------
    // MoveDecision + MoveSource
    // -----------------------------------------------------------------

    #[test]
    fn move_source_labels_match_python_format() {
        assert_eq!(MoveSource::Engine.as_label(), "Engine");
        assert_eq!(MoveSource::SyzygyEgtb.as_label(), "Syzygy EGTB");
        assert_eq!(MoveSource::LichessCloud.as_label(), "Lichess Cloud Analysis");
        assert_eq!(
            MoveSource::OpeningBook("book123".into()).as_label(),
            "Opening Book (book123)"
        );
        assert_eq!(
            MoveSource::LichessExplorer("Lichess".into()).as_label(),
            "Lichess Opening Explorer (Lichess)"
        );
    }

    #[test]
    fn move_decision_uci_standard_encodes_castling_as_king_target() {
        use shakmaty::fen::Fen;
        use shakmaty::{CastlingMode, Chess};
        let pos: Chess =
            "r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1"
                .parse::<Fen>()
                .unwrap()
                .into_position(CastlingMode::Standard)
                .unwrap();
        // Pick the legal short-castle move out of the move list.
        let castle = pos
            .legal_moves()
            .into_iter()
            .find(|m| m.is_castle() && m.to().file() == shakmaty::File::H)
            .expect("short castle");
        let decision = MoveDecision::new(castle, MoveSource::Engine);
        assert_eq!(decision.uci_standard(), "e1g1");
        assert_eq!(decision.uci_chess960(), "e1h1");
    }

    #[test]
    fn side_from_color_roundtrips_both_colors() {
        assert_eq!(side_from_color(shakmaty::Color::White), Side::White);
        assert_eq!(side_from_color(shakmaty::Color::Black), Side::Black);
    }

    #[test]
    fn score_from_info_prefers_mate_over_cp() {
        let info = InfoLine {
            score_cp: Some(42),
            score_mate: Some(5),
            ..Default::default()
        };
        assert_eq!(score_from_info(&info), Some(PovScore::from_mate(5)));
        let cp_only = InfoLine {
            score_cp: Some(-30),
            ..Default::default()
        };
        assert_eq!(score_from_info(&cp_only), Some(PovScore::from_cp(-30)));
        assert!(score_from_info(&InfoLine::default()).is_none());
    }

    // -----------------------------------------------------------------
    // choose_pre_engine_move (Book → EGTB-stub → Online-stub)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn choose_pre_engine_returns_none_when_book_disabled_and_stubs_active() {
        use crate::lichess::Lichess;
        use crate::lichess_types::GameEventType;
        use rand::rngs::StdRng;
        use rand::SeedableRng;
        use reqwest::Url;
        use shakmaty::Chess;

        let url = Url::parse("https://lichess.org/").unwrap();
        let li = Lichess::new_raw("tok".into(), url, "0".into(), 1).unwrap();
        let mut info = GameEventType::default();
        info.id = Some("g1".into());
        let game = Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO);
        let cfg = crate::config::EngineConfig {
            dir: ".".into(),
            name: "noop".into(),
            ..Default::default()
        };
        let pos = Chess::default();
        let mut rng = StdRng::seed_from_u64(0);
        let res = choose_pre_engine_move(&pos, 0, &game, &li, &cfg, &mut rng).await;
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn choose_pre_engine_returns_book_hit_when_polyglot_finds_one() {
        use crate::lichess::Lichess;
        use crate::lichess_types::GameEventType;
        use crate::polyglot::PolyglotEntry;
        use rand::rngs::StdRng;
        use rand::SeedableRng;
        use reqwest::Url;
        use shakmaty::{Chess, Square};
        use std::io::Write;

        // Build a tiny one-entry book for the start position that returns e2e4.
        fn polyglot_move(from: Square, to: Square) -> u16 {
            let to_bits = u32::from(to.file()) | (u32::from(to.rank()) << 3);
            let from_bits = u32::from(from.file()) | (u32::from(from.rank()) << 3);
            (to_bits | (from_bits << 6)) as u16
        }
        let raw = polyglot_move(Square::E2, Square::E4);
        let entry = PolyglotEntry {
            key: 0x463b_9618_1691_fc9c,
            raw_move: raw,
            weight: 100,
            learn: 0,
        };
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&entry.to_bytes()).unwrap();
        file.flush().unwrap();
        let book_path = file.path().to_string_lossy().to_string();

        let url = Url::parse("https://lichess.org/").unwrap();
        let li = Lichess::new_raw("tok".into(), url, "0".into(), 1).unwrap();

        let mut info = GameEventType::default();
        info.id = Some("g2".into());
        let mut variant = crate::lichess_types::VariantInfo::default();
        variant.key = Some("standard".into());
        info.variant = Some(variant);
        let game = Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO);

        let mut cfg = crate::config::EngineConfig {
            dir: ".".into(),
            name: "noop".into(),
            ..Default::default()
        };
        cfg.polyglot.enabled = true;
        cfg.polyglot.selection = "best_move".into();
        cfg.polyglot.normalization = "none".into();
        cfg.polyglot.min_weight = 0;
        cfg.polyglot.max_depth = 8;
        cfg.polyglot.book.insert("standard".into(), vec![book_path]);

        let pos = Chess::default();
        let mut rng = StdRng::seed_from_u64(0);
        let res = choose_pre_engine_move(&pos, 0, &game, &li, &cfg, &mut rng).await;
        match res {
            Some(PreEngineResult::Decision(d)) => {
                assert_eq!(d.uci_standard(), "e2e4");
                assert!(matches!(d.source, MoveSource::OpeningBook(_)));
            }
            other => panic!("expected book decision, got {other:?}"),
        }
    }

    #[test]
    fn format_info_stats_clips_pv_for_chat() {
        let info = InfoLine {
            depth: Some(40),
            score_cp: Some(0),
            nodes: Some(1_000_000_000),
            nps: Some(50_000_000),
            // 20-move PV → joined string is huge, must be clipped to fit
            // 140-char chat budget.
            pv: (0..20).map(|i| format!("e2e{i}")).collect(),
            ..Default::default()
        };
        let lines = format_info_stats(&info, "Engine", true);
        let joined = lines.join(", ");
        assert!(
            joined.chars().count() <= MAX_CHAT_MESSAGE_LEN,
            "joined stats `{joined}` exceeds {MAX_CHAT_MESSAGE_LEN} chars"
        );
        // PV must still be present, just shorter.
        let pv_line = lines.iter().find(|l| l.starts_with("Pv:")).expect("pv line");
        assert!(pv_line.split_whitespace().count() >= 2);
    }

    // -----------------------------------------------------------------
    // format_uci_info_log
    // -----------------------------------------------------------------

    #[test]
    fn format_uci_info_log_full_line() {
        let info = parse_info_line(
            "info depth 12 score cp 24 nodes 1500 nps 75000 time 20 pv e2e4 e7e5",
        )
        .unwrap();
        let s = format_uci_info_log(&info);
        assert_eq!(s, "depth=12 cp=24 nodes=1500 nps=75000 time_ms=20 pv=e2e4 e7e5");
    }

    #[test]
    fn format_uci_info_log_mate_score_omits_cp() {
        let info = parse_info_line("info depth 5 score mate 3 pv e2e4 e7e5 d1h5").unwrap();
        let s = format_uci_info_log(&info);
        assert!(s.contains("depth=5"));
        assert!(s.contains("mate=3"));
        assert!(!s.contains("cp="));
        assert!(s.contains("pv=e2e4 e7e5 d1h5"));
    }

    #[test]
    fn format_uci_info_log_skips_none_fields() {
        let s = format_uci_info_log(&InfoLine::default());
        assert_eq!(s, "");
    }
}
