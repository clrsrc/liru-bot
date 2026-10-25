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
//! (`depth=0, nodes=0`); the only place this module special-cases it is
//! [`score_from_outcome`], which reports *no* score for it so the harvest
//! writes the move like a Polyglot book move (no ENGINE bit, `clrsrc_score`
//! unset) instead of a fake `clrsrc_score=0` search verdict. None of
//! clrsrc's `SearchInfo` internals are part of the bot contract. The engine sets
//! `silent=false` (root TB probe stays active, UCI-identical) + `print_info=false`
//! (no stdout spam) itself — the bot touches neither.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    /// Separate abort handle for the H2 book-ponder / opponent-brain prefill,
    /// kept distinct from `cancel` on purpose (clrsrc #293): the regular move and
    /// the prefill must never share a cancel flag, or a stale `STOP=true` could
    /// bleed into the next real search and truncate its first iteration into a
    /// silent garbage move. `Arc` so the game loop can keep a clone and flip this
    /// flag *after* the engine has moved into the prefill `spawn_blocking` task
    /// (clrsrc #298 a/b hygiene): the loop raises this before the global STOP so a
    /// prefill entry that runs post-reclaim reads `cancel=true` and stops itself
    /// at the first node. The per-node global STOP (`request_stop`) still aborts
    /// an already-running prefill; this flag only covers the entry-read race.
    prefill_cancel: Arc<AtomicBool>,
    // Last-search telemetry, for chat (`!pv` / `!stats`) and move commentary.
    last_pv_uci: Vec<String>,
    last_score: Option<PovScore>,
    last_depth: Option<u32>,
    last_nodes: Option<u64>,
    /// Wall-clock instant at which the last regular `search_position` call
    /// returned (T0 of the post-search timeline). The play_move / game-loop
    /// "post-search" debug lines report their offset from it so a stall between
    /// "engine decided" and "move sent" can be pinned to one step
    /// (silent-gap class, see main.rs init_logging).
    last_search_returned: Option<std::time::Instant>,
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
                .map(|s| {
                    s.parse().unwrap_or_else(|_| {
                        // A wrong Hash silently defaulting to 64 MB is an
                        // invisible strength loss — surface it instead.
                        tracing::warn!(value = %s, "Hash option is not a valid integer; using 64 MB");
                        64
                    })
                })
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
            // Search-tuning gates. The four booleans default to TRUE, matching the
            // public engine's `EmbeddedConfig::default()` / UCI defaults for the
            // embedded CLRU1 net (clrsrc v1.4.0): without the reader/arch flags the
            // net would silently load in the wrong convention. Set the UCI option
            // to `false` explicitly to switch one off.
            urisk_extension: uci_options.get("URiskExtension").map(|s| truthy(s)).unwrap_or(true),
            urisk_threshold: uci_options.get("URiskThreshold").and_then(|s| s.parse().ok()),
            urisk_extend: uci_options.get("URiskExtend").and_then(|s| s.parse().ok()),
            urisk_minply: uci_options.get("URiskMinPly").and_then(|s| s.parse().ok()),
            urisk_mode_twostage: uci_options.get("URiskModeTwoStage").map(|s| truthy(s)).unwrap_or(false),
            urisk_vthreshold: uci_options.get("URiskVThreshold").and_then(|s| s.parse().ok()),
            conv_floor: uci_options.get("ConvFloor").map(|s| truthy(s)).unwrap_or(true),
            conv_threshold: uci_options.get("ConvThreshold").and_then(|s| s.parse().ok()),
            conv_extend: uci_options.get("ConvExtend").and_then(|s| s.parse().ok()),
            conv_maxpieces: uci_options.get("ConvMaxPieces").and_then(|s| s.parse().ok()),
            conv_minremaining: uci_options.get("ConvMinRemaining").and_then(|s| s.parse().ok()),
            stm_coherent: uci_options.get("StmCoherent").map(|s| truthy(s)).unwrap_or(true),
            urisk_fixed_formula: uci_options.get("URiskFixedFormula").map(|s| truthy(s)).unwrap_or(true),
            threads: uci_options.get("Threads").and_then(|s| s.parse().ok()).unwrap_or(1),
            contempt: 0, // per game via set_game_contempt (rating rule), not static
            // Rating-Band-Repertoire (clrsrc 373e127): per game via `OppBand`, which
            // `spawn_engine_for` injects from `opp_band_for()`; 0 = no band restriction.
            opp_band: uci_options.get("OppBand").and_then(|s| s.parse().ok()).unwrap_or(0),
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
            prefill_cancel: Arc::new(AtomicBool::new(false)),
            last_pv_uci: Vec::new(),
            last_score: None,
            last_depth: None,
            last_nodes: None,
            last_search_returned: None,
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
        // H2 invariant (clrsrc #293): STOP must be false at the entry of every
        // *regular* search. `search_embedded` sets STOP = cancel.load() at
        // entry, so storing false here clears any residual STOP=true left by a
        // prefill abort. A stale STOP=true would give iteration 1 no clean
        // abort-break (per-node `return 0` truncates it) yet still get accepted
        // as "completed depth 1" — a silent garbage move. The caller must have
        // joined the prefill task before reaching here (no overlapping search).
        debug_assert!(
            !self.prefill_cancel.load(Ordering::Relaxed),
            "prefill flag unexpectedly set at regular search entry"
        );
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
        let search_returned = std::time::Instant::now();
        self.last_search_returned = Some(search_returned);

        self.store_telemetry(&outcome);

        let best_uci = outcome.best.to_uci();
        let mv = match shakmaty::uci::UciMove::from_ascii(best_uci.as_bytes())
            .ok()
            .and_then(|uci| uci.to_move(pos).ok())
        {
            Some(mv) => mv,
            None => {
                // The engine returned a move we can't decode or that's illegal
                // against our shakmaty position — a protocol glitch, `(none)` /
                // `0000` at an instant deadline, or a fen+moves rebuild
                // divergence between clrsrc's board and ours. Never forfeit the
                // game on time over it: play any legal move and log loudly so
                // the divergence gets caught. No legal moves means the game is
                // already over, so surfacing the error there is correct.
                match pos.legal_moves().first().cloned() {
                    Some(fallback) => {
                        // Repro trigger (clrsrc #275/A): dump the exact rebuild
                        // inputs — `search_position(start_fen, moves)` is what
                        // clrsrc reconstructs from, so start_fen + the move list
                        // is a complete, replayable case. `ILLEGAL_BESTMOVE:` is
                        // the greppable marker for the production log.
                        tracing::error!(
                            bestmove = %best_uci,
                            start_fen = %start_fen,
                            moves = %moves.join(" "),
                            fallback = %fallback.to_uci(shakmaty::CastlingMode::Standard),
                            "ILLEGAL_BESTMOVE: embedded engine returned an undecodable/illegal \
                             bestmove for this start_fen+moves rebuild; playing a legal fallback \
                             instead of forfeiting. Repro: search_position(start_fen, moves) — report to clrsrc"
                        );
                        fallback
                    }
                    None => return Err(EngineError::BestmoveParse(best_uci.clone())),
                }
            }
        };

        // Per-move engine self-eval for the LMR-137 tactical watch / Klopper
        // analysis (befunde/lmr137_tactical_watch.md): embedded returns the score
        // via the `SearchOutcome` — there is no UCI `info … score cp … pv` text
        // channel to tap — so surface it here at DEBUG. `score` is side-to-move
        // POV, i.e. OUR POV (we only search on our own move), so a sharp drop
        // between our consecutive moves is the search itself seeing the position
        // turn. `depth=0, nodes=0` marks a book hit (no search).
        {
            let score_str = match &self.last_score {
                Some(s) if s.mate.is_some() => format!("mate {}", s.mate.unwrap_or(0)),
                Some(s) => match s.cp {
                    Some(cp) => format!("cp {cp}"),
                    None => "none".to_string(),
                },
                None => "none".to_string(),
            };
            tracing::debug!(
                ply = game_ply,
                bestmove = %best_uci,
                score = %score_str,
                depth = self.last_depth.unwrap_or(0),
                nodes = self.last_nodes.unwrap_or(0),
                pv = %self.last_pv_uci.join(" "),
                post_search_ms = search_returned.elapsed().as_millis() as u64,
                "engine eval"
            );
        }

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
        self.last_score =
            score_from_outcome(outcome.depth, outcome.nodes, outcome.score_cp, outcome.mate);
        self.last_depth = Some(outcome.depth.max(0) as u32);
        self.last_nodes = Some(outcome.nodes);
    }

    pub fn last_info_pv(&self) -> &[String] {
        &self.last_pv_uci
    }

    /// Instant at which the last regular search returned (T0 of the
    /// post-search timeline); `None` before the first search.
    pub fn last_search_returned(&self) -> Option<std::time::Instant> {
        self.last_search_returned
    }

    /// Depth of the last *regular* move's search (`Some(0)` = opening-book hit,
    /// no search). The H2 book gate reads this to decide whether we're still in
    /// the book and a prefill is worthwhile. Untouched by [`Self::prefill_blocking`].
    pub fn last_depth(&self) -> Option<u32> {
        self.last_depth
    }

    /// Clone the prefill-cancel handle so the game loop can flip it after the
    /// engine has moved into the prefill `spawn_blocking` task (clrsrc #298 a/b):
    /// the loop resets it to `false` at spawn and raises it to `true` at reclaim,
    /// before the global STOP. Shares the same `AtomicBool` the prefill's
    /// `search_position` reads at entry.
    pub fn prefill_cancel_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.prefill_cancel)
    }

    /// H2 prefill: warm the TT/history for `fen`+`moves` during the opponent's
    /// time so the upcoming book-exit search starts warm instead of cold.
    ///
    /// Runs the real search — there is no cheaper fill hook (clrsrc #292) — but
    /// with `depth: 100` so `search_position` skips its opening-book probe;
    /// otherwise the probe would hit during the book phase and return instantly
    /// with zero warming. `ponder` stays false: `is_pondering` is the first
    /// check in `should_stop_hard`, so `ponder: true` would disable the
    /// `max_deadline` runaway guard. `max_deadline` is still enforced in depth
    /// mode (checked before the depth/infinite early-return, ~every 2048 nodes).
    /// The outcome is discarded and `store_telemetry` is deliberately NOT called,
    /// so the regular last-move telemetry (the book gate) stays intact. Aborted
    /// cooperatively via [`request_stop`] before the caller joins the task.
    /// Blocking — must run inside `spawn_blocking`.
    pub fn prefill_blocking(
        &mut self,
        fen: Option<&str>,
        moves: &[&str],
        max_deadline: std::time::Instant,
        game_ply: u32,
    ) {
        // The `prefill_cancel` reset to false now lives at the spawn site in the
        // game loop (clrsrc #298 a): the loop owns an `Arc` clone and must clear
        // the flag *before* spawning this task, or a reclaim's `store(true)` on
        // the shared flag could be clobbered here. We only read it below.
        let tc = clrsrc::time::TimeControl {
            wtime: 0,
            btime: 0,
            winc: 0,
            binc: 0,
            movestogo: 0,
            movetime: 0,
            depth: 0,
            infinite: false,
            nodes: 0,
            soft_nodes: 0,
        };
        let limits = clrsrc::EmbeddedLimits {
            tc,
            max_deadline,
            ponder: false,
            game_ply,
            depth: 100,
            nodes: 0,
        };
        let start_fen = fen.filter(|f| !f.is_empty()).unwrap_or("startpos");
        // Result discarded on purpose: only the TT/History warmth persists
        // (hash_history is replayed fresh on the next call — clrsrc #292/#293).
        let _ = self
            .engine
            .search_position(start_fen, moves, limits, self.prefill_cancel.as_ref());
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

/// Rating-based contempt rule (clrsrc #509 stage 1, #518 stage 2, rated-only gate #504):
/// only in RATED games against a weaker bot opponent, d = opponent rating - our rating:
///   d < -300          -> 30 (stage 1)
///   -300 <= d < -100  -> 20 (stage 2)
///   otherwise         -> 0
/// No contempt in casual games, against humans, with an unknown/provisional opponent
/// rating or an unknown own rating.
pub fn contempt_for(
    rated: bool,
    opp_rating: Option<i64>,
    opp_provisional: bool,
    opp_is_bot: bool,
    our_rating: Option<i64>,
) -> i32 {
    match (opp_rating, our_rating) {
        (Some(o), Some(m)) if rated && opp_is_bot && !opp_provisional => {
            let d = o - m;
            if d < -300 {
                30
            } else if d < -100 {
                20
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Rating band for the opening-book repertoire (clrsrc 373e127, `EmbeddedConfig.opp_band`):
/// d = opponent rating minus our rating; d >= 100 -> stronger (+1), d <= -100 -> weaker (-1),
/// otherwise equal (0). Applies to rated AND casual games (it is not contempt); 0 without ratings.
pub fn opp_band_for(opp_rating: Option<i64>, our_rating: Option<i64>) -> i8 {
    match (opp_rating, our_rating) {
        (Some(o), Some(m)) => {
            let d = o - m;
            if d >= 100 {
                clrsrc::book::BAND_STRONGER
            } else if d <= -100 {
                clrsrc::book::BAND_WEAKER
            } else {
                clrsrc::book::BAND_EQUAL
            }
        }
        _ => clrsrc::book::BAND_EQUAL,
    }
}

/// Set the (process-global) engine contempt for the upcoming game; call before the first move.
/// `is_white` = our colour in this game: the reference colour for the contempt sign (clrsrc 28f1852),
/// so the prefill (opponent at the root) and the main search use the same sign in one TT.
pub fn set_game_contempt(value: i32, is_white: bool) {
    clrsrc::search::set_contempt(value);
    clrsrc::set_contempt_color(Some(is_white));
}

/// Abort an in-flight embedded search (the H2 book-ponder / opponent-brain
/// prefill) by raising clrsrc's process-global STOP. Free of `self` because the
/// engine is moved into the prefill `spawn_blocking` task while it runs. The
/// per-call `cancel` flag (`prefill_cancel`) is only read at search *entry*, so
/// the loop's `Arc`-shared `store(true)` (clrsrc #298 b) only helps a prefill
/// that has not yet started searching; once running, only this global STOP stops
/// it — checked per node (~every 2048 nodes), sub-millisecond abort. Re-raised in
/// a join loop (c) to cover the entry-overwrite window. Safe only at
/// `concurrency:1` — it would also stop a concurrent game's live search.
pub fn request_stop() {
    clrsrc::time::set_stop(true);
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

/// Score to surface for a `SearchOutcome`, or `None` when the outcome carries
/// no search verdict at all.
///
/// clrsrc's internal book probe returns a book hit as `depth=0, nodes=0,
/// score_cp=0` (no search ran). Passing that `0` through as a real score made
/// the harvest write the move as `source=SELFPLAY|ENGINE, clrsrc_score=0`, so
/// `probe_best`'s pool tiering counted a never-searched replay move as an
/// *evaluated* one with score 0: it anchored the variety cutoff at 0 and
/// crowded out the honestly negative, deeply searched siblings; on merge the
/// clrsrc mirror also overwrote genuine jug scores with that 0. That was the
/// shared mechanism behind curate rules #18 (4...Qd7), #19 (6...b6),
/// #20 (3...Nf6) and #21 (5...e5) (clrsrc #453/#455/#456).
/// Mapping a book hit to `None` makes the harvest treat it exactly like the
/// bot's own Polyglot path (`MoveDecision::new` → `score: None` → overlay
/// entry `source=0x08`, `clrsrc_score=UNSET`); count and WDL keep accruing.
/// Anything that actually searched (`nodes > 0` or `depth > 0`) keeps its
/// score, mate scores included.
fn score_from_outcome(depth: i32, nodes: u64, score_cp: i32, mate: Option<i32>) -> Option<PovScore> {
    let book_hit = depth <= 0 && nodes == 0;
    if book_hit {
        return None;
    }
    Some(match mate {
        Some(m) => PovScore::from_mate(m as i64),
        None => PovScore::from_cp(score_cp as i64),
    })
}

/// Parse a UCI-option string value as a boolean (`"true"`/`"1"`).
fn truthy(s: &str) -> bool {
    s.eq_ignore_ascii_case("true") || s == "1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opp_band_from_rating_difference_rated_or_not() {
        assert_eq!(opp_band_for(Some(2965), Some(2865)), 1); // d=+100 exactly -> stronger
        assert_eq!(opp_band_for(Some(2964), Some(2865)), 0); // d=+99 -> equal
        assert_eq!(opp_band_for(Some(2765), Some(2865)), -1); // d=-100 exactly -> weaker
        assert_eq!(opp_band_for(Some(2766), Some(2865)), 0); // d=-99 -> equal
        assert_eq!(opp_band_for(Some(2400), Some(2865)), -1);
        assert_eq!(opp_band_for(None, Some(2865)), 0);
        assert_eq!(opp_band_for(Some(2400), None), 0);
    }

    #[test]
    fn contempt_rule_only_vs_much_weaker_rated_bots() {
        assert_eq!(contempt_for(true, Some(2400), false, true, Some(2865)), 30); // d=-465
        assert_eq!(contempt_for(true, Some(2565), false, true, Some(2865)), 20); // d=-300 exactly -> stage 2
        assert_eq!(contempt_for(true, Some(2564), false, true, Some(2865)), 30); // d=-301
        assert_eq!(contempt_for(true, Some(2665), false, true, Some(2865)), 20); // d=-200
        assert_eq!(contempt_for(true, Some(2764), false, true, Some(2865)), 20); // d=-101
        assert_eq!(contempt_for(true, Some(2765), false, true, Some(2865)), 0); // d=-100 exactly
        assert_eq!(contempt_for(true, Some(2800), false, true, Some(2865)), 0);
        assert_eq!(contempt_for(true, Some(3000), false, true, Some(2865)), 0);
        assert_eq!(contempt_for(true, Some(2665), true, true, Some(2865)), 0); // stage 2 provisional
        assert_eq!(contempt_for(true, Some(2665), false, false, Some(2865)), 0); // stage 2 human
        assert_eq!(contempt_for(true, Some(2400), true, true, Some(2865)), 0); // provisional
        assert_eq!(contempt_for(true, Some(2400), false, false, Some(2865)), 0); // human
        assert_eq!(contempt_for(true, None, false, true, Some(2865)), 0);
        assert_eq!(contempt_for(true, Some(2400), false, true, None), 0);
        // rated-only gate (#504): casual -> always 0 in both stages
        assert_eq!(contempt_for(false, Some(2400), false, true, Some(2865)), 0); // casual stage 1
        assert_eq!(contempt_for(false, Some(2665), false, true, Some(2865)), 0); // casual stage 2
        assert_eq!(contempt_for(false, Some(2564), false, true, Some(2865)), 0); // casual d=-301
    }

    #[test]
    fn book_hit_outcome_has_no_score() {
        // clrsrc book probe: depth=0, nodes=0, score_cp=0 → no verdict.
        assert_eq!(score_from_outcome(0, 0, 0, None), None);
        // A book hit never carries a mate, but even if it did there is no search behind it.
        assert_eq!(score_from_outcome(0, 0, 0, Some(3)), None);
    }

    #[test]
    fn searched_outcome_keeps_cp_and_mate() {
        let cp = score_from_outcome(24, 15_263_850, -188, None).unwrap();
        assert_eq!(cp.cp, Some(-188));
        assert_eq!(cp.mate, None);
        let mate = score_from_outcome(12, 4_000, 0, Some(-5)).unwrap();
        assert_eq!(mate.mate, Some(-5));
        // Even a real score of exactly 0 survives once nodes were searched.
        assert_eq!(score_from_outcome(1, 2_048, 0, None).unwrap().cp, Some(0));
    }
}
