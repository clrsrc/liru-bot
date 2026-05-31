//! In-process clrsrc engine backend (clrsrc's EMBEDDED.md, B1-B6).
//!
//! Links clrsrc as a library and drives its search directly, so the bot can
//! hand the search an authoritative absolute wall-clock deadline (B3) instead
//! of the stacked double time-approximation the subprocess path uses. Only
//! compiled with `--features embedded`, and only selected for standard chess
//! (clrsrc's `Position::from_fen` has no Chess960 castling).
//!
//! ## Contract (clrsrc Postfach 043)
//!
//! clrsrc encapsulates the whole pre-search setup behind a stateful
//! [`clrsrc::EmbeddedEngine`]: `init(EmbeddedConfig{…})` once per game (loads
//! NNUE/TT/Syzygy **and** the opening/experience book), then `search_position(
//! start_fen, moves, limits, &cancel)` per move — which rebuilds the position,
//! loads the repetition history, refreshes NNUE, **probes the book** (the gap
//! this build surfaced: the bare `search_embedded` facade never did) and, on a
//! book miss, searches. A book hit comes back as a normal `SearchOutcome`
//! (`depth=0, nodes=0`), so this module does not special-case it. None of
//! clrsrc's `SearchInfo` internals are part of the bot contract. The engine sets
//! `silent=false` (root TB probe stays active, UCI-identical) + `print_info=false`
//! (no stdout spam) itself — the bot touches neither.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::EngineConfig;
use crate::engine_wrapper::{
    pv_to_san, EmbeddedTiming, EngineError, EngineLike, EngineResult, MoveCommentary, MoveDecision,
    MoveSource, OpponentInfo, PovScore,
};
use crate::model::Game;

/// In-process clrsrc engine, held by the bot loop for the lifetime of one game
/// (so clrsrc's `SearchInfo` — TT / history / NNUE — stays warm across moves).
pub struct EmbeddedEngine {
    name: String,
    /// The clrsrc engine handle. Holds the persistent per-game `SearchInfo`
    /// (TT/History/NNUE warm across moves) and the opening/experience book
    /// internally; `search_position` does the whole position bridge + book probe.
    engine: clrsrc::EmbeddedEngine,
    /// External abort flag bridged to clrsrc's process-global STOP inside
    /// `search_position` (sufficient for `concurrency:1`; live per-search cancel
    /// is clrsrc's deferred C4).
    cancel: AtomicBool,
    // Last-search telemetry, for chat (`!pv` / `!stats`) and move commentary.
    last_pv_uci: Vec<String>,
    last_score: Option<PovScore>,
    last_depth: Option<u32>,
    last_nodes: Option<u64>,
    // Per-game commentary, mirroring `UciClient`'s machinery (PGN + harvest).
    move_commentary: Vec<MoveCommentary>,
    comment_start_index: Option<usize>,
}

impl EmbeddedEngine {
    /// Initialise the embedded engine from the same `uci_options` the subprocess
    /// backend would pass to clrsrc (`Hash`, `EvalFile`, `SyzygyPath`,
    /// `SyzygyProbeLimit`, …). Reuses clrsrc's option semantics so the embedded
    /// and subprocess engines are configured identically.
    ///
    /// Builds [`clrsrc::EmbeddedConfig`] from the same `uci_options` keys the
    /// subprocess passes today and calls `clrsrc::EmbeddedEngine::init`, which
    /// loads NNUE/TT/Syzygy **and** the opening/experience book internally. The
    /// engine sets its own `silent=false` (UCI-identical behaviour: root TB probe
    /// stays active — `silent=true` would skip it, see clrsrc Postfach 043) +
    /// `print_info=false` (no stdout spam); the bot touches neither flag.
    pub fn new(cfg: &EngineConfig, uci_options: &HashMap<String, String>) -> Result<Self, String> {
        let _ = cfg; // engine binary/dir not needed in-process

        let config = clrsrc::EmbeddedConfig {
            eval_file: uci_options.get("EvalFile").filter(|s| !s.is_empty()).cloned(),
            hash_mb: uci_options
                .get("Hash")
                .and_then(|s| s.parse().ok())
                .unwrap_or(64),
            syzygy_path: uci_options.get("SyzygyPath").filter(|s| !s.is_empty()).cloned(),
            syzygy_probe_limit: uci_options.get("SyzygyProbeLimit").and_then(|s| s.parse().ok()),
            syzygy_probe_depth: uci_options.get("SyzygyProbeDepth").and_then(|s| s.parse().ok()),
            syzygy_50move: uci_options.get("Syzygy50MoveRule").map(|s| truthy(s)),
            exp_file: uci_options.get("ExpFile").filter(|s| !s.is_empty()).cloned(),
            play_from_exp: uci_options.get("PlayFromExp").map(|s| truthy(s)).unwrap_or(false),
            book_variety: uci_options
                .get("BookVariety")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            own_book: uci_options.get("OwnBook").map(|s| truthy(s)).unwrap_or(false),
            book_file: uci_options.get("BookFile").filter(|s| !s.is_empty()).cloned(),
            best_book_move: uci_options.get("BestBookMove").map(|s| truthy(s)).unwrap_or(true),
        };

        let engine = clrsrc::EmbeddedEngine::init(config);

        // Fail-fast (clrsrc Postfach 047): if an EvalFile was configured but the
        // net didn't load, refuse rather than silently play with classical eval
        // (~-200 ELO, invisible). A missing EvalFile key means "classical on
        // purpose" and is allowed.
        if let Some(path) = uci_options.get("EvalFile").filter(|s| !s.is_empty()) {
            if !engine.nnue_loaded() {
                return Err(format!(
                    "embedded NNUE failed to load (EvalFile={path}) — refusing to run with \
                     classical eval (~-200 ELO, invisible); check the path"
                ));
            }
        }

        Ok(Self {
            name: "clrsrc (embedded)".to_string(),
            engine,
            cancel: AtomicBool::new(false),
            last_pv_uci: Vec::new(),
            last_score: None,
            last_depth: None,
            last_nodes: None,
            move_commentary: Vec::new(),
            comment_start_index: None,
        })
    }

    /// The subprocess `send_opponent_info` sends `UCI_Opponent`; the embedded
    /// engine has no such channel, so this is a no-op.
    pub async fn send_opponent_info(
        &mut self,
        _opp: &OpponentInfo,
        _our_rating: Option<i64>,
    ) -> EngineResult<()> {
        Ok(())
    }

    /// The subprocess sends a UCI `gameover`; nothing to do in-process.
    pub async fn send_game_result(&mut self, _game: &Game) -> EngineResult<()> {
        Ok(())
    }

    /// Pondering (B5) is deferred for the embedded backend, so there is never an
    /// in-flight ponder search to cancel.
    pub async fn cancel_ponder(&mut self) -> EngineResult<()> {
        Ok(())
    }

    /// Run one embedded search and return the chosen move as a shakmaty `Move`.
    /// `pos` is the shakmaty position (used only to decode the UCI bestmove);
    /// the actual search position is rebuilt clrsrc-side from `fen` + `moves`.
    pub async fn search<P>(
        &mut self,
        pos: &P,
        fen: Option<&str>,
        moves: &[&str],
        timing: EmbeddedTiming,
        _can_ponder: bool,
    ) -> EngineResult<MoveDecision>
    where
        P: shakmaty::Position,
    {
        let game_ply = moves.len() as u32;

        // Raw clocks pass through untouched; the single overhead lives in the
        // gap between now and `max_deadline` (B3). depth/nodes go via the
        // top-level fields, which `search_embedded` folds into the time control.
        let tc = clrsrc::time::TimeControl {
            wtime: timing.wtime_ms,
            btime: timing.btime_ms,
            winc: timing.winc_ms,
            binc: timing.binc_ms,
            movestogo: timing.movestogo,
            movetime: timing.movetime_ms,
            depth: 0,
            infinite: false,
            nodes: 0,
            soft_nodes: 0,
        };
        let limits = clrsrc::EmbeddedLimits {
            tc,
            max_deadline: timing.max_deadline,
            ponder: false,
            game_ply,
            depth: timing.depth,
            nodes: timing.nodes,
        };

        // clrsrc rebuilds the position from start_fen + moves, loads the
        // repetition history, refreshes NNUE, probes the opening/experience book,
        // and (on a book miss) searches — all behind `search_position` (Postfach
        // 043). "startpos" is accepted; the bot's `None` initial FEN maps to it.
        let start_fen = fen.filter(|f| !f.is_empty()).unwrap_or("startpos");
        self.cancel.store(false, Ordering::Relaxed);
        let outcome = {
            let engine = &mut self.engine;
            let cancel = &self.cancel;
            // CPU-bound sync search: run it without starving the tokio workers
            // (the game event stream / inactivity tick keep ticking).
            tokio::task::block_in_place(move || {
                engine.search_position(start_fen, moves, limits, cancel)
            })
        };

        self.store_telemetry(&outcome);

        let best_uci = outcome.best.to_uci();
        let uci = shakmaty::uci::UciMove::from_ascii(best_uci.as_bytes())
            .map_err(|_| EngineError::BestmoveParse(best_uci.clone()))?;
        let mv = uci
            .to_move(pos)
            .map_err(|_| EngineError::BestmoveParse(best_uci.clone()))?;

        Ok(MoveDecision {
            mv,
            source: MoveSource::Engine,
            score: self.last_score,
            draw_offered: false,
            resigned: false,
        })
    }

    fn store_telemetry(&mut self, outcome: &clrsrc::SearchOutcome) {
        self.last_pv_uci = outcome.pv.iter().map(|m| m.to_uci()).collect();
        self.last_score = Some(match outcome.mate {
            Some(m) => PovScore::from_mate(m as i64),
            None => PovScore::from_cp(outcome.score_cp as i64),
        });
        self.last_depth = Some(outcome.depth.max(0) as u32);
        self.last_nodes = Some(outcome.nodes);
    }

    pub fn last_info_pv(&self) -> &[String] {
        &self.last_pv_uci
    }

    /// Record commentary for the move just played, mirroring
    /// [`crate::engine_wrapper::UciClient::record_move_commentary`].
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
        let pv_san = pv_to_san(pos, &pv_uci);
        self.move_commentary.push(MoveCommentary {
            score,
            depth: self.last_depth,
            nodes: self.last_nodes,
            time_ms: None,
            nps: None,
            pv_uci,
            pv_san,
        });
    }

    /// Look up commentary by half-move index (same convention as `UciClient`).
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

    /// Nothing to tear down (no subprocess); the warm `SearchInfo` is dropped.
    pub async fn quit(self) -> EngineResult<()> {
        Ok(())
    }
}

impl EngineLike for EmbeddedEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn get_stats(&self, _for_chat: bool) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(score) = &self.last_score {
            if let Some(m) = score.mate {
                out.push(format!("Mate {m}"));
            } else if let Some(cp) = score.cp {
                out.push(format!("Score {:+.2}", cp as f64 / 100.0));
            }
        }
        if let Some(d) = self.last_depth {
            out.push(format!("Depth {d}"));
        }
        if let Some(n) = self.last_nodes {
            out.push(format!("Nodes {n}"));
        }
        out
    }

    fn last_pv(&self) -> &[String] {
        &self.last_pv_uci
    }
}

/// Parse a UCI-option string value as a boolean (`"true"`/`"1"`).
fn truthy(s: &str) -> bool {
    s.eq_ignore_ascii_case("true") || s == "1"
}
