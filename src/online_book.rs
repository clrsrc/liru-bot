//! Online move sources — Rust port of `get_online_move` /
//! `get_chessdb_move` / `get_lichess_cloud_move` /
//! `get_opening_explorer_move` / `get_online_egtb_move` from
//! `lib/engine_wrapper.py`.
//!
//! Three opening books and one EGTB endpoint share a thin orchestrator
//! ([`get_online_move`]). Each individual probe is its own `async fn`
//! against a Lichess / chessdb URL, returning a [`MoveDecision`] (or
//! [`PreEngineResult`] for the EGTB shortlist case).
//!
//! **Per-game "out of book" counter**: Python keeps a module-level
//! `Counter[str]` that increments every time the orchestrator falls
//! through all opening sources, and stops querying once it crosses
//! `max_out_of_book_moves`. We mirror that here as a `LazyLock<Mutex<
//! HashMap<String, u32>>>` keyed by `game.id`. Tests use unique
//! `game.id`s to avoid cross-talk.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use rand::seq::SliceRandom;
use rand::Rng;
use shakmaty::fen::Fen;
use shakmaty::{Chess, Color, EnPassantMode, Position};
use tracing::{debug, info};

use crate::config::{
    ChessdbBookConfig, DrawOrResignConfig, LichessCloudAnalysisConfig,
    LichessOpeningExplorerConfig, OnlineEgtbConfig, OnlineMovesConfig,
};
use crate::egtb::wdl_to_score;
use crate::engine_wrapper::{MoveDecision, MoveSource, PovScore, PreEngineResult};
use crate::lichess::Lichess;
use crate::lichess_types::{OnlineMoveType, OnlineType};
use crate::model::Game;

// ---------------------------------------------------------------------------
// Out-of-book counter
// ---------------------------------------------------------------------------

static OUT_OF_BOOK: LazyLock<Mutex<HashMap<String, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How many consecutive plies in this game had no online-book hit. Public
/// so tests and the chat handler could read it; Python exposes the
/// underlying `Counter` directly.
pub fn out_of_book_count(game_id: &str) -> u32 {
    OUT_OF_BOOK.lock().map(|m| *m.get(game_id).unwrap_or(&0)).unwrap_or(0)
}

/// Manually reset the counter for one game. Useful in tests and when a
/// game ends and we don't want the entry to linger.
pub fn reset_out_of_book_counter(game_id: &str) {
    if let Ok(mut m) = OUT_OF_BOOK.lock() {
        m.remove(game_id);
    }
}

fn bump_out_of_book(game_id: &str) -> u32 {
    if let Ok(mut m) = OUT_OF_BOOK.lock() {
        let n = m.entry(game_id.to_string()).or_insert(0);
        *n += 1;
        return *n;
    }
    0
}

// ---------------------------------------------------------------------------
// Filter helpers
// ---------------------------------------------------------------------------

/// Milliseconds left on our clock as published in the latest game state.
/// `0` when neither side's time is known (e.g. brand-new game, no state
/// update yet). Mirrors Python's `msec(game.state[wbtime(board)])`.
fn time_left_ms(pos: &Chess, game: &Game) -> i64 {
    let v = match pos.turn() {
        Color::White => game.state.wtime,
        Color::Black => game.state.btime,
    };
    v.unwrap_or(0)
}

/// `true` when the game is plain chess (no Antichess/Atomic/...). The
/// chessdb-book endpoint and the masters-explorer endpoint reject
/// everything else, matching Python's `board.uci_variant != "chess"`.
fn is_chess_variant(variant_key: &str) -> bool {
    matches!(variant_key, "" | "standard" | "fromPosition" | "chess960")
}

/// Convert Lichess' `variant_key` ("standard", "antichess", …) into the
/// `variant` query parameter the Lichess cloud / explorer / EGTB
/// endpoints expect. Python uses `str(board.uci_variant)` with an
/// `if uci_variant == "chess": "standard"` override.
fn variant_param(variant_key: &str) -> &str {
    match variant_key {
        "" | "standard" | "fromPosition" | "chess960" => "standard",
        v => v,
    }
}

/// Time-window check shared by every source: must be enabled, our side
/// must have at least `min_time` seconds left, and the game's initial
/// clock must not exceed `max_time`. Mirrors the opening
/// `if not use_X or time_left < min_time or clock_initial > max_time`
/// guard in Python.
fn time_window_open(
    pos: &Chess,
    game: &Game,
    enabled: bool,
    min_time_s: u32,
    max_time_s: u32,
) -> bool {
    if !enabled {
        return false;
    }
    let time_left_ms = time_left_ms(pos, game);
    let clock_initial_ms = game.clock_initial.as_millis() as i64;
    let min_ms = (min_time_s as i64) * 1000;
    let max_ms = (max_time_s as i64) * 1000;
    time_left_ms >= min_ms && clock_initial_ms <= max_ms
}

/// Build the FEN string the online APIs expect. shakmaty's
/// `Fen::from_position` defaults to standard FEN; we use
/// `EnPassantMode::Legal` for parity with python-chess `board.fen()`.
fn fen_of(pos: &Chess) -> String {
    Fen::from_position(pos.clone(), EnPassantMode::Legal).to_string()
}

/// Resolve a UCI string against the current position into a `shakmaty::Move`.
/// Online sources return UCI in standard notation; for Chess960 castling
/// they also use the standard king-target square form. Returns `None`
/// for anything illegal in the current position.
fn uci_to_move(uci: &str, pos: &Chess) -> Option<shakmaty::Move> {
    let uci_move = shakmaty::uci::UciMove::from_ascii(uci.as_bytes()).ok()?;
    uci_move.to_move(pos).ok()
}

// ---------------------------------------------------------------------------
// ChessDB opening book
// ---------------------------------------------------------------------------

const CHESSDB_URL: &str = "https://www.chessdb.cn/cdb.php";

/// Probe chessdb.cn's opening book. Returns a single move plus a
/// `score`/`depth` annotation. `move_quality` selects which API action
/// to use:
///
/// - `"best"` → `action=querypv`, with `min_depth` enforced and a PV
///   echoed back; we keep the full PV for chat / PGN annotation.
/// - `"good"` → `action=querybest`, just a single move.
/// - `"all"`  → `action=query`, single move from the entire book entry.
pub async fn get_chessdb_move(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &ChessdbBookConfig,
) -> Option<MoveDecision> {
    get_chessdb_move_at(CHESSDB_URL, li, pos, game, cfg).await
}

async fn get_chessdb_move_at(
    url: &str,
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &ChessdbBookConfig,
) -> Option<MoveDecision> {
    if !time_window_open(pos, game, cfg.enabled, cfg.min_time, cfg.max_time) {
        return None;
    }
    if !is_chess_variant(&game.variant_key) {
        return None;
    }

    let action = match cfg.move_quality.as_str() {
        "best" => "querypv",
        "good" => "querybest",
        "all" => "query",
        other => {
            debug!(quality = %other, "unknown chessdb move_quality, skipping");
            return None;
        }
    };
    let fen = fen_of(pos);
    let params = [("action", action), ("board", fen.as_str()), ("json", "1")];
    let data = li.online_book_get(url, &params).await.ok()?;
    if data.status.as_deref() != Some("ok") {
        return None;
    }

    if cfg.move_quality == "best" {
        let depth = data.depth?;
        if (depth as u32) < cfg.min_depth {
            return None;
        }
        let pv = data.pv.as_ref()?;
        let first_uci = pv.first()?;
        let mv = uci_to_move(first_uci, pos)?;
        let score = data.score.map(PovScore::from_cp);
        info!(
            game_id = %game.id,
            mv = %first_uci,
            depth,
            score = ?data.score,
            "got move from chessdb.cn (best)"
        );
        Some(MoveDecision {
            mv,
            source: MoveSource::Chessdb,
            score,
            draw_offered: false,
            resigned: false,
        })
    } else {
        // "good" / "all" — single move, no score.
        let uci = data.move_field()?;
        let mv = uci_to_move(&uci, pos)?;
        info!(game_id = %game.id, mv = %uci, action, "got move from chessdb.cn");
        Some(MoveDecision::new(mv, MoveSource::Chessdb))
    }
}

// ---------------------------------------------------------------------------
// Lichess cloud analysis
// ---------------------------------------------------------------------------

const LICHESS_CLOUD_URL: &str = "https://lichess.org/api/cloud-eval";

/// Probe Lichess' cloud-analysis endpoint. For `"best"` quality we ask
/// for `multiPv=1` and take the top PV; for `"good"` we ask for
/// `multiPv=5`, filter out PVs whose `cp` differs from the best by more
/// than `max_score_difference`, then pick one randomly — exactly how
/// Python does it.
pub async fn get_lichess_cloud_move<R: Rng + ?Sized>(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &LichessCloudAnalysisConfig,
    rng: &mut R,
) -> Option<MoveDecision> {
    get_lichess_cloud_move_at(LICHESS_CLOUD_URL, li, pos, game, cfg, rng).await
}

async fn get_lichess_cloud_move_at<R: Rng + ?Sized>(
    url: &str,
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &LichessCloudAnalysisConfig,
    rng: &mut R,
) -> Option<MoveDecision> {
    if !time_window_open(pos, game, cfg.enabled, cfg.min_time, cfg.max_time) {
        return None;
    }
    let multipv = if cfg.move_quality == "best" { 1 } else { 5 };
    let multipv_s = multipv.to_string();
    let fen = fen_of(pos);
    let variant = variant_param(&game.variant_key);
    let params = [
        ("fen", fen.as_str()),
        ("multiPv", multipv_s.as_str()),
        ("variant", variant),
    ];
    let data = li.online_book_get(url, &params).await.ok()?;
    if data.error.is_some() {
        return None;
    }
    let depth = data.depth?;
    let knodes = data.knodes.unwrap_or(0);
    if (depth as u32) < cfg.min_depth || (knodes as u64) < cfg.min_knodes {
        return None;
    }
    let pvs = data.pvs.as_ref()?;
    if pvs.is_empty() {
        return None;
    }
    // Cloud returns cp from white's perspective. Negate for black so
    // `score` is always pov-the-side-to-move; this matches Python's
    // `score = pv["cp"] if side == "wtime" else -pv["cp"]`.
    let side_is_white = pos.turn() == Color::White;
    let pv = if cfg.move_quality == "best" {
        pvs.first()?
    } else {
        let best_cp = pvs.first().and_then(|p| p.cp)?;
        let max_diff = cfg.max_score_difference;
        let candidates: Vec<_> = pvs
            .iter()
            .filter(|p| {
                let cp = match p.cp {
                    Some(v) => v,
                    None => return false,
                };
                if side_is_white {
                    cp >= best_cp - max_diff
                } else {
                    cp <= best_cp + max_diff
                }
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        candidates.choose(rng).copied()?
    };

    let moves_str = pv.moves.as_deref()?;
    let first_uci = moves_str.split_whitespace().next()?;
    let mv = uci_to_move(first_uci, pos)?;
    let cp = pv.cp?;
    let score_cp = if side_is_white { cp } else { -cp };
    info!(
        game_id = %game.id,
        mv = %first_uci,
        depth,
        knodes,
        cp = score_cp,
        "got move from lichess cloud analysis"
    );
    Some(MoveDecision {
        mv,
        source: MoveSource::LichessCloud,
        score: Some(PovScore::from_cp(score_cp)),
        draw_offered: false,
        resigned: false,
    })
}

// ---------------------------------------------------------------------------
// Lichess opening explorer
// ---------------------------------------------------------------------------

const LICHESS_EXPLORER_BASE: &str = "https://explorer.lichess.ovh";

/// Probe the Lichess opening explorer (masters / player / lichess
/// archives) and return the most-played or highest-winrate move,
/// depending on `cfg.sort`. The masters archive is chess-only; the
/// other two work for any variant Lichess publishes.
pub async fn get_opening_explorer_move(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &LichessOpeningExplorerConfig,
) -> Option<MoveDecision> {
    get_opening_explorer_move_at(LICHESS_EXPLORER_BASE, li, pos, game, cfg).await
}

async fn get_opening_explorer_move_at(
    base_url: &str,
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &LichessOpeningExplorerConfig,
) -> Option<MoveDecision> {
    if !time_window_open(pos, game, cfg.enabled, cfg.min_time, cfg.max_time) {
        return None;
    }
    if cfg.source == "masters" && !is_chess_variant(&game.variant_key) {
        return None;
    }
    let fen = fen_of(pos);
    let variant = variant_param(&game.variant_key);
    let (url, params, source_label) = match cfg.source.as_str() {
        "masters" => (
            format!("{base_url}/masters"),
            vec![("fen", fen.clone()), ("moves", "100".into())],
            "Masters",
        ),
        "player" => {
            let player = if cfg.player_name.is_empty() {
                game.username.clone()
            } else {
                cfg.player_name.clone()
            };
            let color = if pos.turn() == Color::White { "white" } else { "black" };
            (
                format!("{base_url}/player"),
                vec![
                    ("player", player),
                    ("fen", fen.clone()),
                    ("moves", "100".into()),
                    ("variant", variant.to_string()),
                    ("recentGames", "0".into()),
                    ("color", color.to_string()),
                ],
                "Player",
            )
        }
        _ => (
            format!("{base_url}/lichess"),
            vec![
                ("fen", fen.clone()),
                ("moves", "100".into()),
                ("variant", variant.to_string()),
                ("topGames", "0".into()),
                ("recentGames", "0".into()),
            ],
            "Lichess",
        ),
    };
    let params_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let data = li.online_book_get(&url, &params_refs).await.ok()?;
    let moves = data.moves.as_ref()?;
    let side_is_white = pos.turn() == Color::White;
    let mut scored: Vec<(f64, f64, String)> = Vec::new();
    for entry in moves {
        let games_played =
            entry.white.unwrap_or(0) + entry.black.unwrap_or(0) + entry.draws.unwrap_or(0);
        if games_played < cfg.min_games as i64 || games_played <= 0 {
            continue;
        }
        let mut winrate = (entry.white.unwrap_or(0) as f64
            + entry.draws.unwrap_or(0) as f64 * 0.5)
            / games_played as f64;
        if !side_is_white {
            winrate = 1.0 - winrate;
        }
        let Some(uci) = entry.uci.clone() else { continue };
        let (primary, secondary) = if cfg.sort == "winrate" {
            (winrate, games_played as f64)
        } else {
            (games_played as f64, winrate)
        };
        scored.push((primary, secondary, uci));
    }
    // Sort descending on primary; ties broken by secondary descending,
    // then by uci ascending for stable output.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });
    let pick = scored.into_iter().next()?;
    let mv = uci_to_move(&pick.2, pos)?;
    info!(
        game_id = %game.id,
        mv = %pick.2,
        source = source_label,
        sort = %cfg.sort,
        primary = pick.0,
        "got move from lichess opening explorer"
    );
    Some(MoveDecision::new(
        mv,
        MoveSource::LichessExplorer(source_label.into()),
    ))
}

// ---------------------------------------------------------------------------
// Online EGTB (Lichess + chessdb)
// ---------------------------------------------------------------------------

const LICHESS_EGTB_BASE: &str = "https://tablebase.lichess.ovh";

/// Map Lichess EGTB's `category` field to an integer WDL from the *opponent's*
/// view, exactly as Python's `name_to_wld` dict. The orchestrator negates
/// it once to get our own WDL.
fn lichess_category_to_wdl(category: &str) -> Option<i32> {
    match category {
        "loss" => Some(-2),
        "maybe-loss" | "blessed-loss" => Some(-1),
        "draw" => Some(0),
        "cursed-win" | "maybe-win" => Some(1),
        "win" => Some(2),
        _ => None,
    }
}

/// chessdb's per-move `score` integer to WDL. Python's
/// `score_to_wdl = piecewise_function([(-20000, "e", -2), (0, "e", -1),
///   (0, "i", 0), (20000, "i", 1)], 2, score)`. Boundaries match
/// exactly: `< -20000` → -2, `< 0` → -1, `== 0` → 0, `<= 20000` → 1,
/// `> 20000` → 2.
fn chessdb_score_to_wdl(score: i64) -> i32 {
    if score < -20_000 {
        -2
    } else if score < 0 {
        -1
    } else if score == 0 {
        0
    } else if score <= 20_000 {
        1
    } else {
        2
    }
}

/// Build the final `PreEngineResult` for an online-EGTB hit. Mirrors
/// `egtb::build_decision` but tags the source as `LichessEgtb` /
/// `Chessdb` depending on `source`.
fn build_online_egtb_decision(
    selection: OnlineEgtbSelection,
    wdl: i32,
    source: MoveSource,
    dor_cfg: &DrawOrResignConfig,
) -> PreEngineResult {
    let score = PovScore::from_cp(wdl_to_score(wdl));
    let offer_draw = dor_cfg.offer_draw_enabled && dor_cfg.offer_draw_for_egtb_zero && wdl == 0;
    let resign = dor_cfg.resign_enabled && dor_cfg.resign_for_egtb_minus_two && wdl == -2;
    match selection {
        OnlineEgtbSelection::Single(mv) => PreEngineResult::Decision(MoveDecision {
            mv,
            source,
            score: Some(score),
            draw_offered: offer_draw,
            resigned: resign,
        }),
        OnlineEgtbSelection::Many(moves) => PreEngineResult::Suggest(moves),
    }
}

enum OnlineEgtbSelection {
    Single(shakmaty::Move),
    Many(Vec<shakmaty::Move>),
}

fn use_egtb_variant_for_source(variant_key: &str, source: &str) -> bool {
    match source {
        "lichess" => matches!(
            variant_key,
            "" | "standard" | "fromPosition" | "antichess" | "atomic"
        ),
        "chessdb" => is_chess_variant(variant_key),
        _ => false,
    }
}

/// Orchestrate the online-EGTB probe. Honours the time-window and
/// piece-count guards, then dispatches to `get_lichess_egtb_move` or
/// `get_chessdb_egtb_move`. Castling rights disqualify the position
/// for *all* EGTB sources (Python: `or board.castling_rights`).
async fn get_online_egtb_move(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    cfg: &OnlineEgtbConfig,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    if !time_window_open(pos, game, cfg.enabled, cfg.min_time, cfg.max_time) {
        return None;
    }
    if !use_egtb_variant_for_source(&game.variant_key, &cfg.source) {
        return None;
    }
    let piece_count = pos.board().occupied().count() as u32;
    if piece_count > cfg.max_pieces {
        return None;
    }
    if !pos.castles().castling_rights().is_empty() {
        return None;
    }
    match cfg.source.as_str() {
        "lichess" => {
            let variant = variant_param(&game.variant_key);
            get_lichess_egtb_move_at(LICHESS_EGTB_BASE, li, pos, game, &cfg.move_quality, variant, dor_cfg).await
        }
        "chessdb" => {
            get_chessdb_egtb_move_at(CHESSDB_URL, li, pos, game, &cfg.move_quality, dor_cfg).await
        }
        _ => None,
    }
}

/// Probe Lichess EGTB. `quality == "suggest"` returns every move that
/// shares the top WDL when there are at least two such moves; otherwise
/// the single best move is returned (with the API's first entry as
/// authoritative — Lichess already orders by quality).
pub async fn get_lichess_egtb_move(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    quality: &str,
    variant: &str,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    get_lichess_egtb_move_at(LICHESS_EGTB_BASE, li, pos, game, quality, variant, dor_cfg).await
}

async fn get_lichess_egtb_move_at(
    base_url: &str,
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    quality: &str,
    variant: &str,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    let url = format!("{base_url}/{variant}");
    let fen = fen_of(pos);
    let params = [("fen", fen.as_str())];
    let data = li.online_book_get(&url, &params).await.ok()?;
    let moves = data.moves.as_ref()?;
    if moves.is_empty() {
        return None;
    }
    // API's category is from the *opponent's* perspective after our
    // move, so we negate. Python does `* -1`.
    let best_wdl_opp = lichess_category_to_wdl(moves[0].category.as_deref()?)?;
    let best_wdl = -best_wdl_opp;
    if quality == "suggest" {
        let same: Vec<&OnlineMoveType> = moves
            .iter()
            .filter(|m| {
                m.category
                    .as_deref()
                    .and_then(lichess_category_to_wdl)
                    .map(|w| -w == best_wdl)
                    .unwrap_or(false)
            })
            .collect();
        if same.len() > 1 {
            let parsed: Vec<shakmaty::Move> = same
                .iter()
                .filter_map(|m| m.uci.as_deref().and_then(|u| uci_to_move(u, pos)))
                .collect();
            if parsed.len() >= 2 {
                info!(game_id = %game.id, wdl = best_wdl, count = parsed.len(), "lichess egtb suggest");
                return Some(build_online_egtb_decision(
                    OnlineEgtbSelection::Many(parsed),
                    best_wdl,
                    MoveSource::LichessEgtb,
                    dor_cfg,
                ));
            }
        }
    }
    let first_uci = moves[0].uci.as_deref()?;
    let mv = uci_to_move(first_uci, pos)?;
    info!(game_id = %game.id, mv = %first_uci, wdl = best_wdl, "lichess egtb best");
    Some(build_online_egtb_decision(
        OnlineEgtbSelection::Single(mv),
        best_wdl,
        MoveSource::LichessEgtb,
        dor_cfg,
    ))
}

/// Probe chessdb's EGTB. `quality == "best"` uses `action=querypv`
/// (returns `score` + `pv`); anything else uses `action=queryall`
/// (returns `moves[*].score`).
pub async fn get_chessdb_egtb_move(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    quality: &str,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    get_chessdb_egtb_move_at(CHESSDB_URL, li, pos, game, quality, dor_cfg).await
}

async fn get_chessdb_egtb_move_at(
    url: &str,
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    quality: &str,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    let action = if quality == "best" { "querypv" } else { "queryall" };
    let fen = fen_of(pos);
    let params = [("action", action), ("board", fen.as_str()), ("json", "1")];
    let data = li.online_book_get(url, &params).await.ok()?;
    if data.status.as_deref() != Some("ok") {
        return None;
    }
    if quality == "best" {
        let score = data.score?;
        let wdl = chessdb_score_to_wdl(score);
        let pv = data.pv.as_ref()?;
        let first_uci = pv.first()?;
        let mv = uci_to_move(first_uci, pos)?;
        info!(game_id = %game.id, mv = %first_uci, wdl, "chessdb egtb best");
        return Some(build_online_egtb_decision(
            OnlineEgtbSelection::Single(mv),
            wdl,
            MoveSource::Chessdb,
            dor_cfg,
        ));
    }
    // "suggest" — pick every move tied with the top entry's WDL.
    let moves = data.moves.as_ref()?;
    if moves.is_empty() {
        return None;
    }
    let best_wdl = chessdb_score_to_wdl(moves[0].score?);
    let same: Vec<&OnlineMoveType> = moves
        .iter()
        .filter(|m| m.score.map(chessdb_score_to_wdl) == Some(best_wdl))
        .collect();
    if same.len() > 1 {
        let parsed: Vec<shakmaty::Move> = same
            .iter()
            .filter_map(|m| m.uci.as_deref().and_then(|u| uci_to_move(u, pos)))
            .collect();
        if parsed.len() >= 2 {
            info!(game_id = %game.id, wdl = best_wdl, count = parsed.len(), "chessdb egtb suggest");
            return Some(build_online_egtb_decision(
                OnlineEgtbSelection::Many(parsed),
                best_wdl,
                MoveSource::Chessdb,
                dor_cfg,
            ));
        }
    }
    let first_uci = moves[0].uci.as_deref()?;
    let mv = uci_to_move(first_uci, pos)?;
    info!(game_id = %game.id, mv = %first_uci, wdl = best_wdl, "chessdb egtb best (suggest fallback)");
    Some(build_online_egtb_decision(
        OnlineEgtbSelection::Single(mv),
        best_wdl,
        MoveSource::Chessdb,
        dor_cfg,
    ))
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Top-level call site for [`crate::engine_wrapper::choose_pre_engine_move`].
/// Mirrors Python's `get_online_move`:
///
/// 1. Try the online EGTB. If it returns a move, use it.
/// 2. Otherwise check the per-game out-of-book counter. If we've already
///    fallen out of every opening book `max_out_of_book_moves` times in
///    this game, stop probing online opening books — engine takes over.
/// 3. Try chessdb → lichess cloud → lichess explorer in order; first hit
///    wins.
/// 4. If none of the three opening sources had a hit, increment the
///    counter and (when crossing the threshold) log that we're done
///    with online books for the rest of the game.
pub async fn get_online_move<R: Rng + ?Sized>(
    li: &Lichess,
    pos: &Chess,
    game: &Game,
    online_moves_cfg: &OnlineMovesConfig,
    draw_or_resign_cfg: &DrawOrResignConfig,
    rng: &mut R,
) -> Option<PreEngineResult> {
    if let Some(result) =
        get_online_egtb_move(li, pos, game, &online_moves_cfg.online_egtb, draw_or_resign_cfg)
            .await
    {
        return Some(result);
    }

    // `max_depth` is `Option<u32>` in our config (= Python `math.inf`).
    // Python: `max_opening_moves = max_depth * 2 - 1` (counted in
    // half-moves); when `max_depth` is infinite, we never gate.
    let played = game
        .state
        .moves
        .as_deref()
        .map(|s| s.split_whitespace().count())
        .unwrap_or(0);
    if let Some(max_depth) = online_moves_cfg.max_depth {
        let max_opening_plies = (max_depth as usize).saturating_mul(2).saturating_sub(1);
        if played > max_opening_plies {
            return None;
        }
    }
    if out_of_book_count(&game.id) >= online_moves_cfg.max_out_of_book_moves {
        return None;
    }

    if let Some(d) = get_chessdb_move(li, pos, game, &online_moves_cfg.chessdb_book).await {
        return Some(PreEngineResult::Decision(d));
    }
    if let Some(d) =
        get_lichess_cloud_move(li, pos, game, &online_moves_cfg.lichess_cloud_analysis, rng).await
    {
        return Some(PreEngineResult::Decision(d));
    }
    if let Some(d) =
        get_opening_explorer_move(li, pos, game, &online_moves_cfg.lichess_opening_explorer).await
    {
        return Some(PreEngineResult::Decision(d));
    }

    let n = bump_out_of_book(&game.id);
    let used_any = online_moves_cfg.chessdb_book.enabled
        || online_moves_cfg.lichess_cloud_analysis.enabled
        || online_moves_cfg.lichess_opening_explorer.enabled;
    if used_any && n == online_moves_cfg.max_out_of_book_moves {
        info!(game_id = %game.id, "out of online opening books for this game");
    }
    None
}

// ---------------------------------------------------------------------------
// Helper: pull the `move` field out of an `OnlineType` (chessdb books)
// ---------------------------------------------------------------------------

trait OnlineMoveField {
    fn move_field(&self) -> Option<String>;
}

impl OnlineMoveField for OnlineType {
    fn move_field(&self) -> Option<String> {
        // serde_yaml_ng's `flatten` collects unknown fields; for chessdb
        // `querybest` / `query` the `move` field is the answer. It's
        // present on `OnlineMoveType.uci` if folded into `moves`, but
        // for the single-move chessdb responses the field is top-level
        // — extracted here via a helper so call sites stay readable.
        self.opening
            .as_ref()
            .and_then(|m| m.get("move").cloned())
            .or_else(|| {
                self.moves
                    .as_ref()
                    .and_then(|v| v.first())
                    .and_then(|m| m.uci.clone())
            })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lichess::Lichess;
    use crate::lichess_types::{GameEventType, GameStateType, VariantInfo};
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use reqwest::Url;
    use serde_json::json;
    use shakmaty::Chess;
    use std::time::Duration;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_game(id: &str, wtime_ms: i64, clock_initial: Duration) -> Game {
        let mut info = GameEventType::default();
        info.id = Some(id.into());
        info.speed = Some("blitz".into());
        let mut state = GameStateType::default();
        state.wtime = Some(wtime_ms);
        state.btime = Some(wtime_ms);
        info.state = Some(state);
        let mut variant = VariantInfo::default();
        variant.key = Some("standard".into());
        info.variant = Some(variant);
        // Stuff a clock so `clock_initial` ends up correct.
        info.clock = Some(crate::lichess_types::TimeControlType {
            initial: Some(clock_initial.as_millis() as i64),
            increment: Some(0),
            ..Default::default()
        });
        Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO)
    }

    async fn lichess_against(server: &MockServer) -> Lichess {
        let url = Url::parse(&server.uri()).unwrap();
        Lichess::new_raw("fake".into(), url, "0".into(), 1).expect("client")
    }

    fn pos_after(moves: &[&str]) -> Chess {
        use shakmaty::Position;
        let mut pos = Chess::default();
        for uci in moves {
            let uci_move = shakmaty::uci::UciMove::from_ascii(uci.as_bytes()).unwrap();
            let mv = uci_move.to_move(&pos).unwrap();
            pos.play_unchecked(&mv);
        }
        pos
    }

    // -----------------------------------------------------------------------
    // Counter
    // -----------------------------------------------------------------------

    #[test]
    fn out_of_book_counter_starts_at_zero_and_can_be_reset() {
        let id = "counter-test-A";
        reset_out_of_book_counter(id);
        assert_eq!(out_of_book_count(id), 0);
        bump_out_of_book(id);
        bump_out_of_book(id);
        assert_eq!(out_of_book_count(id), 2);
        reset_out_of_book_counter(id);
        assert_eq!(out_of_book_count(id), 0);
    }

    // -----------------------------------------------------------------------
    // Filter helpers
    // -----------------------------------------------------------------------

    #[test]
    fn time_window_rejects_disabled_and_clock_out_of_range() {
        let pos = Chess::default();
        let game = fixture_game("tw", 30_000, Duration::from_secs(180));
        // Disabled → false regardless.
        assert!(!time_window_open(&pos, &game, false, 0, 60_000));
        // Time left below min_time → false.
        assert!(!time_window_open(&pos, &game, true, 60, 60_000));
        // Clock initial above max_time → false.
        assert!(!time_window_open(&pos, &game, true, 0, 60));
        // Otherwise → true.
        assert!(time_window_open(&pos, &game, true, 10, 60_000));
    }

    #[test]
    fn is_chess_variant_recognises_lichess_variant_keys() {
        assert!(is_chess_variant(""));
        assert!(is_chess_variant("standard"));
        assert!(is_chess_variant("fromPosition"));
        assert!(is_chess_variant("chess960"));
        assert!(!is_chess_variant("antichess"));
        assert!(!is_chess_variant("atomic"));
    }

    #[test]
    fn variant_param_maps_standard_to_standard() {
        assert_eq!(variant_param("standard"), "standard");
        assert_eq!(variant_param(""), "standard");
        assert_eq!(variant_param("antichess"), "antichess");
        assert_eq!(variant_param("atomic"), "atomic");
    }

    #[test]
    fn variant_param_maps_chess960_to_standard() {
        // Explorer/cloud query strings treat chess960 as standard
        // because the API endpoints don't take a 960 variant.
        assert_eq!(variant_param("chess960"), "standard");
    }

    // -----------------------------------------------------------------------
    // ChessDB book (URL-injected helper)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chessdb_best_returns_first_pv_move_when_depth_high_enough() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cdb.php"))
            .and(query_param("action", "querypv"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "ok",
                "depth": 25,
                "score": 42,
                "pv": ["e2e4", "e7e5"]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/cdb.php", server.uri());
        let game = fixture_game("cdb-best", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = ChessdbBookConfig {
            enabled: true,
            min_time: 0,
            max_time: 60_000,
            move_quality: "best".into(),
            min_depth: 20,
        };
        let decision = get_chessdb_move_at(&url, &li, &pos, &game, &cfg)
            .await
            .expect("a move");
        assert_eq!(decision.source, MoveSource::Chessdb);
        assert_eq!(decision.uci_standard(), "e2e4");
        assert_eq!(decision.score.and_then(|s| s.cp), Some(42));
    }

    #[tokio::test]
    async fn chessdb_best_depth_too_low_returns_none() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cdb.php"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "ok", "depth": 5, "score": 0, "pv": ["e2e4"]
            })))
            .mount(&server)
            .await;
        let url = format!("{}/cdb.php", server.uri());
        let game = fixture_game("cdb-shallow", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = ChessdbBookConfig {
            enabled: true,
            min_time: 0,
            max_time: 60_000,
            move_quality: "best".into(),
            min_depth: 20,
        };
        assert!(get_chessdb_move_at(&url, &li, &pos, &game, &cfg).await.is_none());
    }

    #[tokio::test]
    async fn chessdb_status_not_ok_returns_none() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cdb.php"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"status": "unknown"})),
            )
            .mount(&server)
            .await;
        let url = format!("{}/cdb.php", server.uri());
        let game = fixture_game("cdb-status", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = ChessdbBookConfig {
            enabled: true,
            min_time: 0,
            max_time: 60_000,
            move_quality: "best".into(),
            min_depth: 0,
        };
        assert!(get_chessdb_move_at(&url, &li, &pos, &game, &cfg).await.is_none());
    }

    #[tokio::test]
    async fn chessdb_disabled_returns_none_without_http() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        // No mock — disabled cfg must short-circuit before any request.
        let url = format!("{}/cdb.php", server.uri());
        let game = fixture_game("cdb-off", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = ChessdbBookConfig {
            enabled: false,
            ..ChessdbBookConfig::default()
        };
        assert!(get_chessdb_move_at(&url, &li, &pos, &game, &cfg).await.is_none());
    }

    // -----------------------------------------------------------------------
    // Lichess cloud (URL-injected helper)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn lichess_cloud_disabled_returns_none() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        let game = fixture_game("cloud-off", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = LichessCloudAnalysisConfig::default(); // enabled = false
        let mut rng = StdRng::seed_from_u64(0);
        assert!(get_lichess_cloud_move(&li, &pos, &game, &cfg, &mut rng).await.is_none());
    }

    #[tokio::test]
    async fn lichess_cloud_best_returns_first_pv_negated_for_black() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cloud-eval"))
            .and(query_param("multiPv", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "depth": 30,
                "knodes": 200,
                "pvs": [{"moves": "e7e5 g1f3", "cp": -25}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/cloud-eval", server.uri());
        // Position after 1.e4 → black to move. cp comes from API as white-pov.
        let pos = pos_after(&["e2e4"]);
        let game = fixture_game("cloud-best", 30_000, Duration::from_secs(180));
        let mut cfg = LichessCloudAnalysisConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.min_depth = 20;
        let mut rng = StdRng::seed_from_u64(0);
        let decision = get_lichess_cloud_move_at(&url, &li, &pos, &game, &cfg, &mut rng)
            .await
            .expect("a move");
        assert_eq!(decision.source, MoveSource::LichessCloud);
        assert_eq!(decision.uci_standard(), "e7e5");
        // Black to move, cp=-25 (white-pov) → pov score = +25.
        assert_eq!(decision.score.and_then(|s| s.cp), Some(25));
    }

    #[tokio::test]
    async fn lichess_cloud_min_depth_rejects_shallow_responses() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cloud-eval"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "depth": 5, "knodes": 10, "pvs": [{"moves": "e2e4", "cp": 0}]
            })))
            .mount(&server)
            .await;
        let url = format!("{}/cloud-eval", server.uri());
        let pos = Chess::default();
        let game = fixture_game("cloud-shallow", 30_000, Duration::from_secs(180));
        let mut cfg = LichessCloudAnalysisConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.min_depth = 20;
        let mut rng = StdRng::seed_from_u64(0);
        assert!(get_lichess_cloud_move_at(&url, &li, &pos, &game, &cfg, &mut rng).await.is_none());
    }

    // -----------------------------------------------------------------------
    // Opening explorer (URL-injected helper)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explorer_disabled_returns_none() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        let game = fixture_game("exp-off", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let cfg = LichessOpeningExplorerConfig::default();
        assert!(get_opening_explorer_move(&li, &pos, &game, &cfg).await.is_none());
    }

    #[tokio::test]
    async fn explorer_lichess_picks_most_played_above_min_games() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/lichess"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "moves": [
                    {"uci": "e2e4", "white": 60, "black": 30, "draws": 10},
                    {"uci": "d2d4", "white": 200, "black": 100, "draws": 50},
                    {"uci": "g1f3", "white": 1,   "black": 1,   "draws": 0}
                ]
            })))
            .mount(&server)
            .await;
        let game = fixture_game("exp-lichess", 30_000, Duration::from_secs(180));
        let pos = Chess::default();
        let mut cfg = LichessOpeningExplorerConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.source = "lichess".into();
        cfg.sort = "games_played".into();
        cfg.min_games = 5;
        let decision = get_opening_explorer_move_at(&server.uri(), &li, &pos, &game, &cfg)
            .await
            .expect("a move");
        // d2d4 has 350 games (200+100+50), most among entries above min.
        assert_eq!(decision.uci_standard(), "d2d4");
        assert!(matches!(decision.source, MoveSource::LichessExplorer(_)));
    }

    #[tokio::test]
    async fn explorer_masters_skipped_on_non_chess_variant() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        // No mock — variant filter must short-circuit before any HTTP.
        let pos = Chess::default();
        let mut info = GameEventType::default();
        info.id = Some("exp-variant".into());
        info.speed = Some("blitz".into());
        let mut v = VariantInfo::default();
        v.key = Some("antichess".into());
        info.variant = Some(v);
        let mut state = GameStateType::default();
        state.wtime = Some(60_000);
        info.state = Some(state);
        info.clock = Some(crate::lichess_types::TimeControlType {
            initial: Some(180_000),
            increment: Some(0),
            ..Default::default()
        });
        let game = Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO);
        let mut cfg = LichessOpeningExplorerConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.source = "masters".into();
        cfg.min_games = 1;
        assert!(get_opening_explorer_move_at(&server.uri(), &li, &pos, &game, &cfg).await.is_none());
    }

    // -----------------------------------------------------------------------
    // Orchestrator
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // WDL helpers
    // -----------------------------------------------------------------------

    #[test]
    fn chessdb_score_to_wdl_matches_python_thresholds() {
        assert_eq!(chessdb_score_to_wdl(-30_000), -2);
        assert_eq!(chessdb_score_to_wdl(-20_001), -2);
        assert_eq!(chessdb_score_to_wdl(-20_000), -1); // not < -20000 → -1
        assert_eq!(chessdb_score_to_wdl(-1), -1);
        assert_eq!(chessdb_score_to_wdl(0), 0);
        assert_eq!(chessdb_score_to_wdl(1), 1);
        assert_eq!(chessdb_score_to_wdl(20_000), 1);
        assert_eq!(chessdb_score_to_wdl(20_001), 2);
    }

    #[test]
    fn lichess_category_to_wdl_maps_each_name() {
        assert_eq!(lichess_category_to_wdl("win"), Some(2));
        assert_eq!(lichess_category_to_wdl("cursed-win"), Some(1));
        assert_eq!(lichess_category_to_wdl("maybe-win"), Some(1));
        assert_eq!(lichess_category_to_wdl("draw"), Some(0));
        assert_eq!(lichess_category_to_wdl("maybe-loss"), Some(-1));
        assert_eq!(lichess_category_to_wdl("blessed-loss"), Some(-1));
        assert_eq!(lichess_category_to_wdl("loss"), Some(-2));
        assert_eq!(lichess_category_to_wdl("bogus"), None);
    }

    // -----------------------------------------------------------------------
    // Lichess EGTB
    // -----------------------------------------------------------------------

    /// KQK position with plenty of legal king + queen moves to choose
    /// from. `4k3/8/8/8/4K3/8/8/7Q w - - 0 1`: kings far enough apart
    /// that `e4e5`/`e4d5`/`e4f5` are all legal. Castling rights are
    /// already cleared so the EGTB filter passes.
    fn pos_kqk_white_to_move() -> Chess {
        use shakmaty::CastlingMode;
        let fen = "4k3/8/8/8/4K3/8/8/7Q w - - 0 1";
        fen.parse::<shakmaty::fen::Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap()
    }

    fn endgame_game(id: &str) -> Game {
        let mut info = GameEventType::default();
        info.id = Some(id.into());
        info.speed = Some("classical".into());
        let mut state = GameStateType::default();
        state.wtime = Some(600_000);
        state.btime = Some(600_000);
        info.state = Some(state);
        let mut variant = VariantInfo::default();
        variant.key = Some("standard".into());
        info.variant = Some(variant);
        info.clock = Some(crate::lichess_types::TimeControlType {
            initial: Some(180_000),
            increment: Some(0),
            ..Default::default()
        });
        Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO)
    }

    #[tokio::test]
    async fn lichess_egtb_best_negates_opponent_category() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/standard"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "moves": [
                    {"uci": "e4e5", "category": "loss", "dtz": 1, "dtm": 1},
                    {"uci": "e4d4", "category": "draw", "dtz": 0, "dtm": 0}
                ]
            })))
            .mount(&server)
            .await;
        let pos = pos_kqk_white_to_move();
        let game = endgame_game("egtb-best");
        let dor = DrawOrResignConfig::default();
        let result = get_lichess_egtb_move_at(
            &server.uri(),
            &li,
            &pos,
            &game,
            "best",
            "standard",
            &dor,
        )
        .await
        .expect("a result");
        match result {
            PreEngineResult::Decision(d) => {
                assert_eq!(d.source, MoveSource::LichessEgtb);
                assert_eq!(d.uci_standard(), "e4e5");
                // opponent's loss → our win (+2) → score 9900.
                assert_eq!(d.score.and_then(|s| s.cp), Some(9900));
            }
            PreEngineResult::Suggest(_) => panic!("expected Decision"),
        }
    }

    #[tokio::test]
    async fn lichess_egtb_suggest_returns_shortlist_when_multiple_tied() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/standard"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "moves": [
                    {"uci": "e4e5", "category": "loss"},
                    {"uci": "e4d5", "category": "loss"},
                    {"uci": "e4f5", "category": "draw"}
                ]
            })))
            .mount(&server)
            .await;
        let pos = pos_kqk_white_to_move();
        let game = endgame_game("egtb-sugg");
        let dor = DrawOrResignConfig::default();
        let result = get_lichess_egtb_move_at(
            &server.uri(),
            &li,
            &pos,
            &game,
            "suggest",
            "standard",
            &dor,
        )
        .await
        .expect("a result");
        match result {
            PreEngineResult::Suggest(moves) => {
                assert_eq!(moves.len(), 2);
                let ucis: Vec<String> = moves
                    .iter()
                    .map(|m| shakmaty::uci::UciMove::from_standard(m).to_string())
                    .collect();
                assert!(ucis.contains(&"e4e5".to_string()));
                assert!(ucis.contains(&"e4d5".to_string()));
                assert!(!ucis.contains(&"e4f5".to_string()));
            }
            PreEngineResult::Decision(_) => panic!("expected Suggest"),
        }
    }

    #[tokio::test]
    async fn chessdb_egtb_best_uses_score_to_wdl_mapping() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        Mock::given(method("GET"))
            .and(path("/cdb.php"))
            .and(query_param("action", "querypv"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "ok",
                "score": 25_000,
                "pv": ["e4e5", "e8e7"]
            })))
            .mount(&server)
            .await;
        let url = format!("{}/cdb.php", server.uri());
        let pos = pos_kqk_white_to_move();
        let game = endgame_game("cdb-egtb");
        let dor = DrawOrResignConfig::default();
        let result = get_chessdb_egtb_move_at(&url, &li, &pos, &game, "best", &dor)
            .await
            .expect("a result");
        match result {
            PreEngineResult::Decision(d) => {
                assert_eq!(d.source, MoveSource::Chessdb);
                assert_eq!(d.uci_standard(), "e4e5");
                // score 25000 > 20000 → wdl=2 → score 9900.
                assert_eq!(d.score.and_then(|s| s.cp), Some(9900));
            }
            PreEngineResult::Suggest(_) => panic!("expected Decision"),
        }
    }

    #[tokio::test]
    async fn online_egtb_skipped_with_castling_rights() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        // No mock — castling-rights filter must short-circuit.
        let pos = Chess::default(); // full board with castling rights
        let game = endgame_game("egtb-castling");
        let mut cfg = OnlineEgtbConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.max_pieces = 32;
        cfg.source = "lichess".into();
        cfg.move_quality = "best".into();
        let dor = DrawOrResignConfig::default();
        let r = get_online_egtb_move(&li, &pos, &game, &cfg, &dor).await;
        assert!(r.is_none());
        // Sanity: starting position has castling rights.
        assert!(!pos.castles().castling_rights().is_empty());
    }

    #[tokio::test]
    async fn online_egtb_skipped_when_too_many_pieces() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        // No mock — piece count filter must short-circuit.
        let pos = Chess::default(); // 32 pieces
        let game = endgame_game("egtb-many");
        let mut cfg = OnlineEgtbConfig::default();
        cfg.enabled = true;
        cfg.min_time = 0;
        cfg.max_time = 60_000;
        cfg.max_pieces = 7;
        cfg.source = "lichess".into();
        let dor = DrawOrResignConfig::default();
        let r = get_online_egtb_move(&li, &pos, &game, &cfg, &dor).await;
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn orchestrator_increments_out_of_book_when_all_sources_silent() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        // Default-configured online_moves: every book disabled.
        let game = fixture_game("orch-empty", 30_000, Duration::from_secs(180));
        reset_out_of_book_counter(&game.id);
        let pos = Chess::default();
        let cfg = OnlineMovesConfig::default();
        let dor = DrawOrResignConfig::default();
        let mut rng = StdRng::seed_from_u64(0);
        let r = get_online_move(&li, &pos, &game, &cfg, &dor, &mut rng).await;
        assert!(r.is_none());
        // Even with everything disabled, we should bump the counter — Python
        // does the same.
        assert_eq!(out_of_book_count(&game.id), 1);
    }

    #[tokio::test]
    async fn orchestrator_short_circuits_after_max_out_of_book_moves() {
        let server = MockServer::start().await;
        let li = lichess_against(&server).await;
        let game = fixture_game("orch-stop", 30_000, Duration::from_secs(180));
        reset_out_of_book_counter(&game.id);
        // Force-fill the counter past the threshold.
        for _ in 0..10 {
            bump_out_of_book(&game.id);
        }
        let pos = Chess::default();
        let mut cfg = OnlineMovesConfig::default();
        cfg.max_out_of_book_moves = 5;
        let dor = DrawOrResignConfig::default();
        let mut rng = StdRng::seed_from_u64(0);
        let r = get_online_move(&li, &pos, &game, &cfg, &dor, &mut rng).await;
        assert!(r.is_none());
        // Counter should NOT have been bumped again — we short-circuited.
        assert_eq!(out_of_book_count(&game.id), 10);
    }
}
