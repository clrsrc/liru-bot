//! Homemade engines — Rust port of `homemade.py`.
//!
//! Four simple engines for bot builders who don't want to wire up a
//! full UCI binary. Python's port loads them via `importlib` at runtime;
//! Rust uses a compile-time registry instead — every engine implements
//! the [`HomemadeEngine`] trait and gets a static factory in
//! [`HOMEMADE_REGISTRY`]. To plug a homemade engine into the main loop,
//! select it by name in `config.yml` (`engine.protocol: homemade`,
//! `engine.name: RandomMove`).
//!
//! **Status:** Trait + four reference implementations + tests are in
//! place; the main `play_game` loop still goes through `UciClient`
//! only, so picking a homemade engine in the config currently has no
//! effect. Wiring this up is a follow-up — pulling `play_game` and
//! `play_move` over a single `ChessEngine` trait. Until then, this
//! module is a copy-paste-ready starting point for someone who
//! actually wants a homemade bot.
//!
//! Naming and behaviour mirror the Python reference closely so existing
//! tutorials still apply.

use rand::seq::SliceRandom;
use shakmaty::{Chess, Move, Position};

use crate::engine_wrapper::GoLimits;

/// What every homemade engine must implement: pick a move for the
/// position. `pos` is the live game position; `limits` is the same
/// `go ...` budget a UCI engine would receive — `ComboEngine` uses it
/// to switch behaviour at low time.
pub trait HomemadeEngine: Send {
    /// Human-readable name shown in `!name` chat replies / logs.
    fn name(&self) -> &str;

    /// Pick the next move. Implementations MUST return a legal move.
    fn search(
        &mut self,
        pos: &Chess,
        limits: &GoLimits,
        draw_offered: bool,
        rng: &mut dyn rand::RngCore,
    ) -> Move;
}

/// Pick a move uniformly at random. Python's `RandomMove`.
pub struct RandomMove;

impl HomemadeEngine for RandomMove {
    fn name(&self) -> &str {
        "RandomMove"
    }

    fn search(
        &mut self,
        pos: &Chess,
        _limits: &GoLimits,
        _draw_offered: bool,
        rng: &mut dyn rand::RngCore,
    ) -> Move {
        let moves = pos.legal_moves();
        // Position always has at least one legal move when the bot is
        // asked to move — `play_game` only calls us when it's the
        // bot's turn and the game isn't over.
        moves
            .choose(rng)
            .cloned()
            .expect("legal_moves non-empty when it is the bot's turn")
    }
}

/// First move when sorted by SAN. Python's `Alphabetical`.
pub struct Alphabetical;

impl HomemadeEngine for Alphabetical {
    fn name(&self) -> &str {
        "Alphabetical"
    }

    fn search(
        &mut self,
        pos: &Chess,
        _limits: &GoLimits,
        _draw_offered: bool,
        _rng: &mut dyn rand::RngCore,
    ) -> Move {
        let mut scored: Vec<(String, Move)> = pos
            .legal_moves()
            .into_iter()
            .map(|m| (shakmaty::san::San::from_move(pos, &m).to_string(), m))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0));
        scored
            .into_iter()
            .next()
            .map(|(_, m)| m)
            .expect("legal_moves non-empty when it is the bot's turn")
    }
}

/// First move when sorted by UCI. Python's `FirstMove`.
pub struct FirstMove;

impl HomemadeEngine for FirstMove {
    fn name(&self) -> &str {
        "FirstMove"
    }

    fn search(
        &mut self,
        pos: &Chess,
        _limits: &GoLimits,
        _draw_offered: bool,
        _rng: &mut dyn rand::RngCore,
    ) -> Move {
        let mut moves: Vec<Move> = pos.legal_moves().into_iter().collect();
        moves.sort_by(|a, b| {
            let au = shakmaty::uci::UciMove::from_standard(a).to_string();
            let bu = shakmaty::uci::UciMove::from_standard(b).to_string();
            au.cmp(&bu)
        });
        moves
            .into_iter()
            .next()
            .expect("legal_moves non-empty when it is the bot's turn")
    }
}

/// Switch between RandomMove and FirstMove depending on the time
/// budget. Python's `ComboEngine`: random when `time/60 + inc > 10`
/// (so plenty of time), first-move-by-UCI otherwise.
pub struct ComboEngine;

impl HomemadeEngine for ComboEngine {
    fn name(&self) -> &str {
        "ComboEngine"
    }

    fn search(
        &mut self,
        pos: &Chess,
        limits: &GoLimits,
        _draw_offered: bool,
        rng: &mut dyn rand::RngCore,
    ) -> Move {
        // Pull our remaining time + increment in seconds; mirror
        // Python's `time_limit.time` shortcut by treating `movetime` as
        // "we have this much time on this move".
        let (my_time_s, my_inc_s) = if let Some(mt) = limits.movetime_ms {
            (mt as f64 / 1000.0, 0.0)
        } else if pos.turn() == shakmaty::Color::White {
            (
                limits.wtime_ms.unwrap_or(0) as f64 / 1000.0,
                limits.winc_ms.unwrap_or(0) as f64 / 1000.0,
            )
        } else {
            (
                limits.btime_ms.unwrap_or(0) as f64 / 1000.0,
                limits.binc_ms.unwrap_or(0) as f64 / 1000.0,
            )
        };

        if my_time_s / 60.0 + my_inc_s > 10.0 {
            RandomMove.search(pos, limits, false, rng)
        } else {
            FirstMove.search(pos, limits, false, rng)
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Look up a homemade engine by name. Returns a boxed instance ready to
/// `search()`. Mirrors Python's `importlib.import_module("homemade").
/// getattr(name)` but resolves at compile time.
pub fn make(name: &str) -> Option<Box<dyn HomemadeEngine>> {
    match name {
        "RandomMove" => Some(Box::new(RandomMove)),
        "Alphabetical" => Some(Box::new(Alphabetical)),
        "FirstMove" => Some(Box::new(FirstMove)),
        "ComboEngine" => Some(Box::new(ComboEngine)),
        _ => None,
    }
}

/// Names of every homemade engine the registry knows about.
pub const HOMEMADE_REGISTRY: &[&str] = &["RandomMove", "Alphabetical", "FirstMove", "ComboEngine"];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn start_pos() -> Chess {
        Chess::default()
    }

    fn empty_limits() -> GoLimits {
        GoLimits::default()
    }

    fn pos_after(moves: &[&str]) -> Chess {
        let mut p = Chess::default();
        for uci in moves {
            let m = shakmaty::uci::UciMove::from_ascii(uci.as_bytes()).unwrap();
            let mv = m.to_move(&p).unwrap();
            p.play_unchecked(&mv);
        }
        p
    }

    #[test]
    fn registry_knows_all_four_engines() {
        for name in HOMEMADE_REGISTRY {
            assert!(make(name).is_some(), "missing engine: {name}");
        }
        assert!(make("Nonexistent").is_none());
    }

    #[test]
    fn random_move_picks_a_legal_move() {
        let pos = start_pos();
        let mut rng = StdRng::seed_from_u64(0);
        let mv = RandomMove.search(&pos, &empty_limits(), false, &mut rng);
        let legal: Vec<Move> = pos.legal_moves().into_iter().collect();
        assert!(legal.iter().any(|m| *m == mv));
    }

    #[test]
    fn random_move_is_deterministic_for_fixed_seed() {
        let pos = start_pos();
        let mv_a = {
            let mut rng = StdRng::seed_from_u64(42);
            RandomMove.search(&pos, &empty_limits(), false, &mut rng)
        };
        let mv_b = {
            let mut rng = StdRng::seed_from_u64(42);
            RandomMove.search(&pos, &empty_limits(), false, &mut rng)
        };
        assert_eq!(mv_a, mv_b);
    }

    #[test]
    fn alphabetical_picks_first_by_san() {
        // From the start: legal moves include "a3", "a4", ..., "Na3",
        // "Nc3", "Nf3", "Nh3". 'N' (ASCII 78) < 'a' (97), so capital-N
        // moves sort before any pawn move; "Na3" < "Nc3" < "Nf3" < "Nh3".
        let pos = start_pos();
        let mut rng = StdRng::seed_from_u64(0);
        let mv = Alphabetical.search(&pos, &empty_limits(), false, &mut rng);
        let san = shakmaty::san::San::from_move(&pos, &mv).to_string();
        assert_eq!(san, "Na3");
    }

    #[test]
    fn first_move_picks_first_by_uci() {
        // UCI sort: "a2a3" < "a2a4" < "b1a3" < ... So "a2a3" wins.
        let pos = start_pos();
        let mut rng = StdRng::seed_from_u64(0);
        let mv = FirstMove.search(&pos, &empty_limits(), false, &mut rng);
        let uci = shakmaty::uci::UciMove::from_standard(&mv).to_string();
        assert_eq!(uci, "a2a3");
    }

    #[test]
    fn combo_picks_random_with_plenty_of_time() {
        // 11 minutes left → time/60 + inc = 11 + 0 > 10 → RandomMove.
        let mut limits = GoLimits::default();
        limits.wtime_ms = Some(11 * 60 * 1000);
        let pos = start_pos();
        let mut rng = StdRng::seed_from_u64(0);
        let mv = ComboEngine.search(&pos, &limits, false, &mut rng);
        // Same seed against RandomMove → same move.
        let mut rng2 = StdRng::seed_from_u64(0);
        let expected = RandomMove.search(&pos, &limits, false, &mut rng2);
        assert_eq!(mv, expected);
    }

    #[test]
    fn combo_picks_first_under_time_pressure() {
        // 30 seconds left, no increment → 0.5 + 0 < 10 → FirstMove → "a2a3".
        let mut limits = GoLimits::default();
        limits.wtime_ms = Some(30_000);
        let pos = start_pos();
        let mut rng = StdRng::seed_from_u64(0);
        let mv = ComboEngine.search(&pos, &limits, false, &mut rng);
        let uci = shakmaty::uci::UciMove::from_standard(&mv).to_string();
        assert_eq!(uci, "a2a3");
    }

    #[test]
    fn combo_uses_black_clock_when_black_to_move() {
        // After 1.e4 it's Black to move. wtime is huge, btime tiny — combo
        // should look at btime and fall through to FirstMove.
        let mut limits = GoLimits::default();
        limits.wtime_ms = Some(10 * 60 * 1000);
        limits.btime_ms = Some(30_000);
        let pos = pos_after(&["e2e4"]);
        let mut rng = StdRng::seed_from_u64(0);
        let mv = ComboEngine.search(&pos, &limits, false, &mut rng);
        // FirstMove on the post-1.e4 position should pick the
        // alphabetically-first legal black move. "a7a5" vs "a7a6":
        // both start "a7a", then '5' < '6'. So "a7a5".
        let uci = shakmaty::uci::UciMove::from_standard(&mv).to_string();
        assert_eq!(uci, "a7a5");
    }
}
