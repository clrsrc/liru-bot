//! Endgame tablebase lookups — Rust port of `get_egtb_move` /
//! `get_syzygy` / `get_gaviota` from `lib/engine_wrapper.py`.
//!
//! Syzygy probing goes through [`shakmaty_syzygy::Tablebase`] (already a
//! crate dependency). For each call we build a fresh `Tablebase<Chess>`,
//! point it at the configured directories, and probe the position.
//! Probing is cheap once the index is loaded — the underlying tables
//! are memory-mapped lazily by shakmaty-syzygy.
//!
//! Gaviota probing goes through [`gaviota-sys`] (only compiled in when
//! the `gaviota` feature is on). libgtb keeps global state, so we
//! initialise once via a `OnceLock<Mutex<…>>` and serialise every
//! probe behind that mutex — `tb_probe_hard` is documented as not
//! reentrant.

use shakmaty::{Chess, Move, Position};
use shakmaty_syzygy::{Tablebase, Wdl};
use tracing::{debug, warn};

use crate::config::{DrawOrResignConfig, GaviotaConfig, LichessBotTbsConfig, SyzygyConfig};
use crate::engine_wrapper::{MoveDecision, MoveSource, PovScore, PreEngineResult};
use crate::model::Game;

/// Probe local Syzygy / Gaviota tablebases for the position and return a
/// move (or shortlist) if a tablebase covers it. Returns `None` when no
/// tablebase applies or the feature is disabled in the config.
pub fn get_egtb_move(
    pos: &Chess,
    _game: &Game,
    tbs_cfg: &LichessBotTbsConfig,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    if let Some(result) = get_syzygy_move(pos, &tbs_cfg.syzygy, dor_cfg) {
        return Some(result);
    }
    if let Some(result) = get_gaviota_move(pos, &tbs_cfg.gaviota, dor_cfg) {
        return Some(result);
    }
    None
}

/// Probe Syzygy tables. Honours `cfg.move_quality`:
///
/// - `"best"` → returns the single tablebase-optimal move
///   (`Tablebase::best_move`, which already picks WDL-best then DTZ-best).
/// - `"suggest"` → returns every legal move that shares the position's
///   best WDL, so the engine can search among them.
pub fn get_syzygy_move(
    pos: &Chess,
    cfg: &SyzygyConfig,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    if !cfg.enabled || cfg.paths.is_empty() {
        return None;
    }
    let piece_count = pos.board().occupied().count();
    if piece_count > cfg.max_pieces as usize {
        return None;
    }

    let mut tables: Tablebase<Chess> = Tablebase::new();
    let mut any_loaded = false;
    for path in &cfg.paths {
        match tables.add_directory(path) {
            Ok(n) => {
                any_loaded |= n > 0;
            }
            Err(e) => warn!(path = %path, error = %e, "syzygy add_directory failed"),
        }
    }
    if !any_loaded {
        return None;
    }

    let wdl = match tables.probe_wdl(pos) {
        Ok(amb) => amb.after_zeroing(),
        Err(e) => {
            debug!(error = %e, "syzygy probe_wdl failed");
            return None;
        }
    };

    let quality = SyzygyQuality::parse(&cfg.move_quality);
    let result = match quality {
        SyzygyQuality::Suggest => suggest_moves(&tables, pos, wdl),
        SyzygyQuality::Best => best_move(&tables, pos),
    }?;

    Some(build_decision(result, wdl, dor_cfg))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyzygyQuality {
    Best,
    Suggest,
}

impl SyzygyQuality {
    fn parse(s: &str) -> Self {
        match s {
            "suggest" => Self::Suggest,
            _ => Self::Best,
        }
    }
}

enum MoveSelection {
    Single(Move),
    Many(Vec<Move>),
}

fn best_move(tables: &Tablebase<Chess>, pos: &Chess) -> Option<MoveSelection> {
    match tables.best_move(pos) {
        Ok(Some((mv, _dtz))) => Some(MoveSelection::Single(mv)),
        Ok(None) => None,
        Err(e) => {
            debug!(error = %e, "syzygy best_move failed");
            None
        }
    }
}

/// Equivalent of Python's `move_quality == "suggest"` branch: enumerate
/// legal moves, probe each successor WDL, keep every move that ties
/// with the position's best WDL.
fn suggest_moves(tables: &Tablebase<Chess>, pos: &Chess, our_wdl: Wdl) -> Option<MoveSelection> {
    let mut shortlist: Vec<Move> = Vec::new();
    for mv in pos.legal_moves() {
        let mut after = pos.clone();
        after.play_unchecked(&mv);
        match tables.probe_wdl(&after) {
            Ok(amb) => {
                // Opponent's WDL after our move, negated → our WDL of the move.
                let move_wdl = -amb.after_zeroing();
                if move_wdl == our_wdl {
                    shortlist.push(mv);
                }
            }
            Err(_) => continue,
        }
    }
    match shortlist.len() {
        0 => None,
        1 => Some(MoveSelection::Single(shortlist.into_iter().next().unwrap())),
        _ => Some(MoveSelection::Many(shortlist)),
    }
}

fn build_decision(
    selection: MoveSelection,
    wdl: Wdl,
    dor_cfg: &DrawOrResignConfig,
) -> PreEngineResult {
    let wdl_int = wdl as i32;
    let score = PovScore::from_cp(wdl_to_score(wdl_int));
    let offer_draw =
        dor_cfg.offer_draw_enabled && dor_cfg.offer_draw_for_egtb_zero && wdl_int == 0;
    let resign =
        dor_cfg.resign_enabled && dor_cfg.resign_for_egtb_minus_two && wdl_int == -2;

    match selection {
        MoveSelection::Single(mv) => PreEngineResult::Decision(MoveDecision {
            mv,
            source: MoveSource::SyzygyEgtb,
            score: Some(score),
            draw_offered: offer_draw,
            resigned: resign,
        }),
        MoveSelection::Many(moves) => PreEngineResult::Suggest(moves),
    }
}

/// Python's `wdl_to_score = {2: 9900, 1: 500, 0: 0, -1: -500, -2: -9900}`.
/// Exposed publicly so the online-EGTB module (later) can reuse it.
pub fn wdl_to_score(wdl: i32) -> i64 {
    match wdl {
        2 => 9900,
        1 => 500,
        0 => 0,
        -1 => -500,
        -2 => -9900,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Gaviota — FFI via gaviota-sys
// ---------------------------------------------------------------------------

/// Probe Gaviota tables. Mirrors Python's `get_gaviota`. Returns
/// `None` when the feature is disabled, the position has too many
/// pieces, or libgtb refuses the lookup.
///
/// Gaviota tables don't cover positions with castling rights, so we
/// filter those out before probing — same as Python's
/// `with chess.gaviota.open_tablebase(...)`-block (python-chess does
/// the same internally).
pub fn get_gaviota_move(
    pos: &Chess,
    cfg: &GaviotaConfig,
    dor_cfg: &DrawOrResignConfig,
) -> Option<PreEngineResult> {
    if !cfg.enabled || cfg.paths.is_empty() {
        return None;
    }
    let piece_count = pos.board().occupied().count();
    if piece_count > cfg.max_pieces as usize {
        return None;
    }
    if !pos.castles().castling_rights().is_empty() {
        return None;
    }
    if !gaviota_ffi::ensure_init(&cfg.paths) {
        return None;
    }

    // Iterate legal moves, probe each successor for the opponent's WDL,
    // collect them with `(move, our_wdl, plies_to_mate)`. Python uses
    // DTM (distance-to-mate) for tie-breaking; libgtb returns that
    // value in the `plies` out-parameter of `tb_probe_hard`.
    let mut scored: Vec<(Move, i32, u32)> = Vec::new();
    for mv in pos.legal_moves() {
        let mut after = pos.clone();
        after.play_unchecked(&mv);
        if let Some((opp_wdl_pov, plies)) = gaviota_ffi::probe(&after) {
            let our_wdl = -opp_wdl_pov;
            scored.push((mv, our_wdl, plies));
        }
    }
    if scored.is_empty() {
        return None;
    }

    let best_wdl = scored.iter().map(|(_, w, _)| *w).max()?;
    let same: Vec<&(Move, i32, u32)> = scored.iter().filter(|(_, w, _)| *w == best_wdl).collect();

    let quality = if cfg.move_quality == "suggest" { Quality::Suggest } else { Quality::Best };
    if quality == Quality::Suggest && same.len() > 1 {
        let moves: Vec<Move> = same.iter().map(|(m, _, _)| m.clone()).collect();
        return Some(build_gaviota_decision(MoveSelection::Many(moves), best_wdl, dor_cfg));
    }

    // Among moves tying on WDL, pick by DTM:
    //   winning → shortest mate
    //   losing  → longest mate (resist longest)
    //   drawing → first (libgtb already orders by piece-index, no
    //             "better draw" exists anyway)
    let pick = if best_wdl > 0 {
        same.iter().min_by_key(|(_, _, p)| *p).copied()?
    } else if best_wdl < 0 {
        same.iter().max_by_key(|(_, _, p)| *p).copied()?
    } else {
        same.first().copied()?
    };
    Some(build_gaviota_decision(
        MoveSelection::Single(pick.0.clone()),
        best_wdl,
        dor_cfg,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quality {
    Best,
    Suggest,
}

fn build_gaviota_decision(
    selection: MoveSelection,
    wdl: i32,
    dor_cfg: &DrawOrResignConfig,
) -> PreEngineResult {
    let score = PovScore::from_cp(wdl_to_score(wdl));
    let offer_draw = dor_cfg.offer_draw_enabled && dor_cfg.offer_draw_for_egtb_zero && wdl == 0;
    let resign = dor_cfg.resign_enabled && dor_cfg.resign_for_egtb_minus_two && wdl == -2;
    match selection {
        MoveSelection::Single(mv) => PreEngineResult::Decision(MoveDecision {
            mv,
            source: MoveSource::GaviotaEgtb,
            score: Some(score),
            draw_offered: offer_draw,
            resigned: resign,
        }),
        MoveSelection::Many(moves) => PreEngineResult::Suggest(moves),
    }
}

// ---------------------------------------------------------------------------
// libgtb FFI module (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "gaviota")]
mod gaviota_ffi {
    use std::ffi::CString;
    use std::sync::{Mutex, OnceLock};

    use gaviota_sys::*;
    use shakmaty::{Chess, Color, Position, Role};
    use tracing::{debug, warn};

    /// Holds the CStrings backing the path-array we hand off to libgtb.
    /// libgtb keeps internal pointers into those strings, so they must
    /// outlive the entire process — we therefore stash them in a
    /// `OnceLock` that's never dropped.
    struct GaviotaState {
        initialized: bool,
        _paths: Vec<CString>,
    }

    static STATE: OnceLock<Mutex<GaviotaState>> = OnceLock::new();

    fn state() -> &'static Mutex<GaviotaState> {
        STATE.get_or_init(|| Mutex::new(GaviotaState { initialized: false, _paths: Vec::new() }))
    }

    /// Run `tb_init` once for the lifetime of the process. Subsequent
    /// calls with different paths log a warning and keep the original
    /// init — libgtb has no "reinit with new paths" API that we can
    /// rely on across versions.
    pub fn ensure_init(paths: &[String]) -> bool {
        let mut s = match state().lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if s.initialized {
            return true;
        }
        if paths.is_empty() {
            return false;
        }
        let cstrs: Vec<CString> = paths.iter().filter_map(|p| CString::new(p.as_str()).ok()).collect();
        if cstrs.is_empty() {
            warn!("gaviota: no path strings convertible to CString");
            return false;
        }
        let ok = unsafe {
            tbstats_reset();
            if tbcache_init(32 * 1024 * 1024, 50) != 0 {
                debug!("gaviota: tbcache_init returned non-zero (cache already initialised?)");
            }
            let mut ps = tbpaths_init();
            for c in &cstrs {
                ps = tbpaths_add(ps, c.as_ptr());
            }
            let info = tb_init(0, TB_compression_scheme::tb_CP4 as i32, ps);
            !info.is_null()
        };
        if !ok {
            warn!("gaviota: tb_init returned NULL — paths likely unreadable");
            return false;
        }
        s._paths = cstrs;
        s.initialized = true;
        true
    }

    /// Probe one position. Returns `Some((wdl_from_stm_pov, plies_to_mate))`
    /// on success. `wdl_from_stm_pov` is one of `-2, 0, 2` — libgtb's
    /// DTM tables don't distinguish blessed-loss / cursed-win from full
    /// loss / win, which is fine for the EGTB use case.
    pub fn probe(pos: &Chess) -> Option<(i32, u32)> {
        // libgtb's tb_probe_hard is not reentrant — serialise.
        let _guard = state().lock().ok()?;
        if !_guard.initialized {
            return None;
        }

        let mut wsq: Vec<u32> = Vec::with_capacity(17);
        let mut wpc: Vec<u8> = Vec::with_capacity(17);
        let mut bsq: Vec<u32> = Vec::with_capacity(17);
        let mut bpc: Vec<u8> = Vec::with_capacity(17);
        for (sq, piece) in pos.board() {
            let pc = piece_to_gtb(piece.role);
            let sq_u32 = u32::from(sq);
            if piece.color == Color::White {
                wsq.push(sq_u32);
                wpc.push(pc);
            } else {
                bsq.push(sq_u32);
                bpc.push(pc);
            }
        }
        wsq.push(TB_squares::tb_NOSQUARE as u32);
        wpc.push(TB_pieces::tb_NOPIECE as u8);
        bsq.push(TB_squares::tb_NOSQUARE as u32);
        bpc.push(TB_pieces::tb_NOPIECE as u8);

        let stm = if pos.turn() == Color::White {
            TB_sides::tb_WHITE_TO_MOVE as u32
        } else {
            TB_sides::tb_BLACK_TO_MOVE as u32
        };
        let epsq = TB_squares::tb_NOSQUARE as u32;
        let castles = 0u32; // already filtered out at call site
        let mut info: u32 = 0;
        let mut plies: u32 = 0;
        let success = unsafe {
            tb_probe_hard(
                stm,
                epsq,
                castles,
                wsq.as_ptr(),
                bsq.as_ptr(),
                wpc.as_ptr(),
                bpc.as_ptr(),
                &mut info,
                &mut plies,
            )
        };
        if success == 0 {
            return None;
        }

        // libgtb info codes: 0=DRAW, 1=WMATE, 2=BMATE, 3=FORBID, 4=UNKNOWN.
        let wdl_white_pov: i32 = match info {
            0 => 0,
            1 => 2,
            2 => -2,
            _ => return None,
        };
        let wdl_stm_pov = if pos.turn() == Color::White {
            wdl_white_pov
        } else {
            -wdl_white_pov
        };
        Some((wdl_stm_pov, plies))
    }

    fn piece_to_gtb(role: Role) -> u8 {
        match role {
            Role::Pawn => TB_pieces::tb_PAWN as u8,
            Role::Knight => TB_pieces::tb_KNIGHT as u8,
            Role::Bishop => TB_pieces::tb_BISHOP as u8,
            Role::Rook => TB_pieces::tb_ROOK as u8,
            Role::Queen => TB_pieces::tb_QUEEN as u8,
            Role::King => TB_pieces::tb_KING as u8,
        }
    }
}

#[cfg(not(feature = "gaviota"))]
mod gaviota_ffi {
    use shakmaty::Chess;
    pub fn ensure_init(_paths: &[String]) -> bool {
        false
    }
    pub fn probe(_pos: &Chess) -> Option<(i32, u32)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DrawOrResignConfig;
    use crate::lichess_types::GameEventType;
    use shakmaty::fen::Fen;
    use shakmaty::{CastlingMode, Chess};
    use std::time::Duration;

    fn fixture_game() -> Game {
        let mut info = GameEventType::default();
        info.id = Some("egtbtest".into());
        Game::new(&info, "tester", "https://lichess.org/", Duration::ZERO)
    }

    fn pos_from_fen(fen: &str) -> Chess {
        fen.parse::<Fen>()
            .expect("fen")
            .into_position(CastlingMode::Standard)
            .expect("position")
    }

    #[test]
    fn wdl_to_score_matches_python_table() {
        assert_eq!(wdl_to_score(2), 9900);
        assert_eq!(wdl_to_score(1), 500);
        assert_eq!(wdl_to_score(0), 0);
        assert_eq!(wdl_to_score(-1), -500);
        assert_eq!(wdl_to_score(-2), -9900);
        assert_eq!(wdl_to_score(42), 0);
    }

    #[test]
    fn disabled_syzygy_returns_none() {
        let pos = Chess::default();
        let cfg = SyzygyConfig::default();
        let dor = DrawOrResignConfig::default();
        assert!(get_syzygy_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn enabled_but_empty_paths_returns_none() {
        let pos = Chess::default();
        let cfg = SyzygyConfig { enabled: true, ..SyzygyConfig::default() };
        let dor = DrawOrResignConfig::default();
        assert!(get_syzygy_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn piece_count_above_threshold_returns_none() {
        // Start position has 32 pieces — well above the default 7.
        let pos = Chess::default();
        let cfg = SyzygyConfig {
            enabled: true,
            paths: vec!["./nonexistent".into()],
            max_pieces: 7,
            ..SyzygyConfig::default()
        };
        let dor = DrawOrResignConfig::default();
        assert!(get_syzygy_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn nonexistent_path_under_threshold_returns_none_gracefully() {
        // KvK endgame (2 pieces, ≤ max_pieces) so the path-loading branch
        // is exercised; nonexistent path → no tables loaded → None.
        let pos = pos_from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1");
        let cfg = SyzygyConfig {
            enabled: true,
            paths: vec!["P:/this/does/not/exist/syzygy".into()],
            max_pieces: 7,
            move_quality: "best".into(),
        };
        let dor = DrawOrResignConfig::default();
        assert!(get_syzygy_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn get_egtb_move_falls_through_when_syzygy_disabled() {
        let pos = Chess::default();
        let game = fixture_game();
        let tbs = LichessBotTbsConfig::default();
        let dor = DrawOrResignConfig::default();
        assert!(get_egtb_move(&pos, &game, &tbs, &dor).is_none());
    }

    // -----------------------------------------------------------------------
    // Gaviota filter logic
    // -----------------------------------------------------------------------
    //
    // We can't actually probe libgtb without real Gaviota tablebases on
    // disk; these tests only cover the filter shortcuts that must skip
    // the FFI entirely (disabled, no paths, too many pieces, castling
    // rights). The probe path is exercised live when running against an
    // actual TB directory.

    #[test]
    fn gaviota_disabled_returns_none() {
        let pos = Chess::default();
        let cfg = crate::config::GaviotaConfig::default(); // enabled=false
        let dor = DrawOrResignConfig::default();
        assert!(get_gaviota_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn gaviota_enabled_but_empty_paths_returns_none() {
        let pos = pos_from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1");
        let cfg = crate::config::GaviotaConfig {
            enabled: true,
            paths: Vec::new(),
            max_pieces: 5,
            min_dtm_to_consider_as_wdl_1: 120,
            move_quality: "best".into(),
        };
        let dor = DrawOrResignConfig::default();
        assert!(get_gaviota_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn gaviota_piece_count_above_threshold_returns_none() {
        let pos = Chess::default(); // 32 pieces
        let cfg = crate::config::GaviotaConfig {
            enabled: true,
            paths: vec!["./nonexistent".into()],
            max_pieces: 5,
            min_dtm_to_consider_as_wdl_1: 120,
            move_quality: "best".into(),
        };
        let dor = DrawOrResignConfig::default();
        assert!(get_gaviota_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn gaviota_castling_rights_short_circuit() {
        // Start position has castling rights but is also above piece
        // count — bump piece-count threshold so castling becomes the
        // active gate.
        let pos = Chess::default();
        let cfg = crate::config::GaviotaConfig {
            enabled: true,
            paths: vec!["./nonexistent".into()],
            max_pieces: 64,
            min_dtm_to_consider_as_wdl_1: 120,
            move_quality: "best".into(),
        };
        let dor = DrawOrResignConfig::default();
        assert!(get_gaviota_move(&pos, &cfg, &dor).is_none());
        assert!(!pos.castles().castling_rights().is_empty());
    }

    #[test]
    fn gaviota_nonexistent_path_returns_none_gracefully() {
        // Castling-clear KvK so the only remaining gate is path-loading.
        let pos = pos_from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1");
        let cfg = crate::config::GaviotaConfig {
            enabled: true,
            paths: vec!["P:/this/does/not/exist/gaviota".into()],
            max_pieces: 5,
            min_dtm_to_consider_as_wdl_1: 120,
            move_quality: "best".into(),
        };
        let dor = DrawOrResignConfig::default();
        // With the `gaviota` feature on, `ensure_init` returns false
        // because libgtb finds no tables; with it off, the FFI stub
        // always returns false. Both paths must short-circuit cleanly.
        assert!(get_gaviota_move(&pos, &cfg, &dor).is_none());
    }

    #[test]
    fn syzygy_quality_parse_defaults_to_best() {
        assert_eq!(SyzygyQuality::parse("best"), SyzygyQuality::Best);
        assert_eq!(SyzygyQuality::parse("suggest"), SyzygyQuality::Suggest);
        assert_eq!(SyzygyQuality::parse("garbage"), SyzygyQuality::Best);
    }

    #[test]
    fn build_decision_packs_score_and_draw_resign_flags() {
        // WDL=0 (draw) with offer_draw_for_egtb_zero enabled → offer_draw=true
        let dor = DrawOrResignConfig {
            offer_draw_enabled: true,
            offer_draw_for_egtb_zero: true,
            resign_enabled: true,
            resign_for_egtb_minus_two: true,
            ..DrawOrResignConfig::default()
        };
        let mv = Chess::default().legal_moves().into_iter().next().unwrap();
        let res = build_decision(MoveSelection::Single(mv.clone()), Wdl::Draw, &dor);
        match res {
            PreEngineResult::Decision(d) => {
                assert!(d.draw_offered);
                assert!(!d.resigned);
                assert_eq!(d.score, Some(PovScore::from_cp(0)));
            }
            _ => panic!("expected Decision"),
        }
        // WDL=-2 (Loss) → resign=true
        let res = build_decision(MoveSelection::Single(mv.clone()), Wdl::Loss, &dor);
        if let PreEngineResult::Decision(d) = res {
            assert!(d.resigned);
            assert_eq!(d.score, Some(PovScore::from_cp(-9900)));
        } else {
            panic!("expected Decision");
        }
    }

    #[test]
    fn build_decision_many_returns_suggest_variant() {
        let moves: Vec<Move> = Chess::default().legal_moves().into_iter().take(3).collect();
        let res = build_decision(MoveSelection::Many(moves.clone()), Wdl::Win, &DrawOrResignConfig::default());
        match res {
            PreEngineResult::Suggest(list) => assert_eq!(list.len(), 3),
            _ => panic!("expected Suggest"),
        }
    }
}
