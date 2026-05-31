//! Polyglot opening-book reader — Rust port of the `get_book_move` path
//! through `chess.polyglot.open_reader(...)` in `lib/engine_wrapper.py`.
//!
//! The Polyglot format is 16 bytes per entry, sorted by Zobrist key:
//! `u64 key | u16 move | u16 weight | u32 learn`, all big-endian.
//! The 16-bit move encoding is `[to_file:3][to_rank:3][from_file:3]
//! [from_rank:3][promotion:3][unused:1]` with castling encoded king→rook
//! (Chess960-style), which lines up with shakmaty's `Move::Castle`.
//!
//! Zobrist hashes are computed with `pos.zobrist_hash::<Zobrist64>(
//! EnPassantMode::Legal)` — shakmaty ships the Polyglot tables natively,
//! so the lookup key matches what `chess.polyglot` writes/reads on the
//! Python side.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rand::seq::SliceRandom;
use rand::Rng;
use shakmaty::zobrist::{ZobristHash, Zobrist64};
use shakmaty::{EnPassantMode, File as ChessFile, Move, Position, Rank, Role, Square};
use thiserror::Error;
use tracing::{debug, warn};

use crate::config::PolyglotConfig;

/// One 16-byte Polyglot entry, untouched from the file. `raw_move` still
/// needs decoding against a concrete position to become a legal `Move`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolyglotEntry {
    pub key: u64,
    pub raw_move: u16,
    pub weight: u16,
    pub learn: u32,
}

pub const ENTRY_SIZE: u64 = 16;

#[derive(Debug, Error)]
pub enum BookError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("book file has length {0} which is not a multiple of 16")]
    UnalignedSize(u64),
}

impl PolyglotEntry {
    pub fn from_bytes(buf: [u8; 16]) -> Self {
        let key = u64::from_be_bytes(buf[0..8].try_into().unwrap());
        let raw_move = u16::from_be_bytes([buf[8], buf[9]]);
        let weight = u16::from_be_bytes([buf[10], buf[11]]);
        let learn = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Self { key, raw_move, weight, learn }
    }

    pub fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&self.key.to_be_bytes());
        buf[8..10].copy_from_slice(&self.raw_move.to_be_bytes());
        buf[10..12].copy_from_slice(&self.weight.to_be_bytes());
        buf[12..16].copy_from_slice(&self.learn.to_be_bytes());
        buf
    }

    /// Decode `raw_move` against the position the book entry belongs to.
    /// Returns `None` if no matching legal move exists (corrupt book or
    /// stale entry).
    pub fn decode_move<P: Position>(&self, pos: &P) -> Option<Move> {
        decode_raw_move(self.raw_move, pos)
    }
}

/// Decode a 16-bit Polyglot move against `pos` by enumerating its legal
/// moves and matching `(from, to, promotion)`. Castling falls out
/// automatically because shakmaty's `Move::Castle { king, rook }` exposes
/// the same king→rook coordinates Polyglot uses.
pub fn decode_raw_move<P: Position>(raw: u16, pos: &P) -> Option<Move> {
    let to = square_from_polyglot(raw & 0b111, (raw >> 3) & 0b111)?;
    let from = square_from_polyglot((raw >> 6) & 0b111, (raw >> 9) & 0b111)?;
    let promotion = match (raw >> 12) & 0b111 {
        0 => None,
        1 => Some(Role::Knight),
        2 => Some(Role::Bishop),
        3 => Some(Role::Rook),
        4 => Some(Role::Queen),
        _ => return None,
    };
    pos.legal_moves()
        .into_iter()
        .find(|m| m.from() == Some(from) && m.to() == to && m.promotion() == promotion)
}

fn square_from_polyglot(file: u16, rank: u16) -> Option<Square> {
    let f = ChessFile::try_from(file as u32).ok()?;
    let r = Rank::try_from(rank as u32).ok()?;
    Some(Square::from_coords(f, r))
}

/// Encode a `(from, to, promotion)` triple into the 16-bit Polyglot move
/// layout. Exact inverse of [`decode_raw_move`]'s bit extraction:
/// `[to_file:3][to_rank:3][from_file:3][from_rank:3][promotion:3]`,
/// promotion `1=N..4=Q`. Castling must already be expressed king→rook
/// (which is what shakmaty's `Move::{from,to}` give for `Move::Castle`).
pub fn encode_raw_move(from: Square, to: Square, promotion: Option<Role>) -> u16 {
    let to_bits = u32::from(to.file()) | (u32::from(to.rank()) << 3);
    let from_bits = u32::from(from.file()) | (u32::from(from.rank()) << 3);
    let promotion_bits = match promotion {
        None => 0,
        Some(Role::Knight) => 1,
        Some(Role::Bishop) => 2,
        Some(Role::Rook) => 3,
        Some(Role::Queen) => 4,
        // Polyglot only encodes N/B/R/Q; a king-promotion can't occur.
        Some(Role::Pawn) | Some(Role::King) => 0,
    };
    (to_bits | (from_bits << 6) | (promotion_bits << 12)) as u16
}

/// Polyglot Zobrist hash of `pos` — the JBK2 entry key. Same hash the
/// book reader uses for lookups (`shakmaty`'s native Polyglot tables,
/// `EnPassantMode::Legal`). Verified against the canonical start-position
/// vector `0x463B96181691FC9C` in the tests.
pub fn polyglot_hash<P: Position + ZobristHash>(pos: &P) -> u64 {
    let key: Zobrist64 = pos.zobrist_hash(EnPassantMode::Legal);
    key.0
}

/// Encode a concrete shakmaty [`Move`] into its 16-bit Polyglot form.
/// Returns `None` for moves without an origin square (drops — never
/// produced by standard chess). Castling falls out correctly because
/// shakmaty exposes `Move::Castle` as `from = king square`, `to = rook
/// square`, which is exactly Polyglot's king→rook convention. Verified
/// against the canonical e1→h1 = `0x0107` vector in the tests.
pub fn encode_move(mv: &Move) -> Option<u16> {
    let from = mv.from()?;
    Some(encode_raw_move(from, mv.to(), mv.promotion()))
}

/// Random-access reader for a `.bin` Polyglot book. Holds an open file
/// handle and seeks to the requested entry; the file is not loaded into
/// memory because real books can be hundreds of megabytes.
pub struct BookReader {
    file: BufReader<File>,
    entries: u64,
    path: PathBuf,
}

impl BookReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BookError> {
        let path = path.as_ref().to_path_buf();
        let f = File::open(&path)?;
        let len = f.metadata()?.len();
        if len % ENTRY_SIZE != 0 {
            return Err(BookError::UnalignedSize(len));
        }
        Ok(Self {
            file: BufReader::new(f),
            entries: len / ENTRY_SIZE,
            path,
        })
    }

    pub fn entry_count(&self) -> u64 {
        self.entries
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_at(&mut self, idx: u64) -> io::Result<PolyglotEntry> {
        self.file.seek(SeekFrom::Start(idx * ENTRY_SIZE))?;
        let mut buf = [0u8; 16];
        self.file.read_exact(&mut buf)?;
        Ok(PolyglotEntry::from_bytes(buf))
    }

    /// All entries with the given Zobrist key, in file order. Uses
    /// binary search (the book is sorted by key) followed by a linear
    /// scan over the equal-key run.
    pub fn find_all(&mut self, key: u64) -> io::Result<Vec<PolyglotEntry>> {
        if self.entries == 0 {
            return Ok(Vec::new());
        }
        // bisect_left
        let (mut lo, mut hi) = (0u64, self.entries);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.read_at(mid)?;
            if entry.key < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut out = Vec::new();
        let mut i = lo;
        while i < self.entries {
            let e = self.read_at(i)?;
            if e.key != key {
                break;
            }
            out.push(e);
            i += 1;
        }
        Ok(out)
    }

    /// Convenience wrapper: compute the Polyglot Zobrist hash of `pos`
    /// and return all matching entries.
    pub fn find_all_for<P: Position + ZobristHash>(
        &mut self,
        pos: &P,
    ) -> io::Result<Vec<PolyglotEntry>> {
        let key: Zobrist64 = pos.zobrist_hash(EnPassantMode::Legal);
        self.find_all(key.0)
    }
}

// ---------------------------------------------------------------------------
// Selection strategies
// ---------------------------------------------------------------------------

/// How to pick one move out of the entries returned for the current
/// position. Mirrors `engine.polyglot.selection` in the YAML config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Probability proportional to entry weight.
    WeightedRandom,
    /// Uniform random over entries with `weight >= min_weight`.
    UniformRandom,
    /// Highest-weight entry with `weight >= min_weight`.
    BestMove,
}

impl Selection {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "weighted_random" => Some(Self::WeightedRandom),
            "uniform_random" => Some(Self::UniformRandom),
            "best_move" => Some(Self::BestMove),
            _ => None,
        }
    }
}

/// How to scale the configured `min_weight` (a percentage value 0..=100)
/// against the entries' raw weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Normalization {
    None,
    Sum,
    Max,
}

impl Normalization {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "sum" => Some(Self::Sum),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

/// Effective minimum weight threshold for the current `entries`, given
/// the user-configured `min_weight_pct` (`engine.polyglot.min_weight`,
/// interpreted as a percentage of `scalar`).
pub fn min_weight_threshold(
    entries: &[PolyglotEntry],
    min_weight_pct: i64,
    norm: Normalization,
) -> u32 {
    if entries.is_empty() {
        return 0;
    }
    let scalar: u64 = match norm {
        Normalization::Sum => entries.iter().map(|e| e.weight as u64).sum(),
        Normalization::Max => entries.iter().map(|e| e.weight as u64).max().unwrap_or(0),
        Normalization::None => 100,
    };
    let threshold = (min_weight_pct.max(0) as u64).saturating_mul(scalar) / 100;
    threshold.min(u32::MAX as u64) as u32
}

/// Pick one entry using the chosen [`Selection`] strategy. Returns
/// `None` when the filtered list is empty (e.g. all entries fell below
/// the minimum weight).
pub fn select_entry<R: Rng + ?Sized>(
    entries: &[PolyglotEntry],
    selection: Selection,
    min_weight: u32,
    rng: &mut R,
) -> Option<PolyglotEntry> {
    match selection {
        Selection::WeightedRandom => weighted_choice(entries, rng),
        Selection::UniformRandom => uniform_choice(entries, min_weight, rng),
        Selection::BestMove => best_move(entries, min_weight),
    }
}

fn weighted_choice<R: Rng + ?Sized>(
    entries: &[PolyglotEntry],
    rng: &mut R,
) -> Option<PolyglotEntry> {
    let total: u64 = entries.iter().map(|e| e.weight as u64).sum();
    if total == 0 {
        return entries.choose(rng).copied();
    }
    let mut roll = rng.gen_range(0..total);
    for e in entries {
        let w = e.weight as u64;
        if roll < w {
            return Some(*e);
        }
        roll -= w;
    }
    entries.last().copied()
}

fn uniform_choice<R: Rng + ?Sized>(
    entries: &[PolyglotEntry],
    min_weight: u32,
    rng: &mut R,
) -> Option<PolyglotEntry> {
    let filtered: Vec<PolyglotEntry> = entries
        .iter()
        .copied()
        .filter(|e| (e.weight as u32) >= min_weight)
        .collect();
    filtered.choose(rng).copied()
}

fn best_move(entries: &[PolyglotEntry], min_weight: u32) -> Option<PolyglotEntry> {
    entries
        .iter()
        .copied()
        .filter(|e| (e.weight as u32) >= min_weight)
        .max_by_key(|e| e.weight)
}

// ---------------------------------------------------------------------------
// get_book_move: orchestrate over all configured books for a variant
// ---------------------------------------------------------------------------

/// Successful book hit: the legal move plus a human-readable source
/// label (the book file's stem) for the engine-stats `Source:` line.
#[derive(Debug, Clone)]
pub struct BookMove {
    pub mv: Move,
    pub book_label: String,
}

/// Try every book configured for `variant_label` in order. Returns the
/// first hit; misses (no entries / no legal move / sub-threshold weight)
/// fall through to the next book.
///
/// `variant_label` is the same string Python uses as `polyglot_cfg.book`
/// dictionary key (`"standard"`, `"chess960"`, `"atomic"`, etc.). The
/// caller is responsible for picking it from the position.
pub fn get_book_move<P, R>(
    pos: &P,
    half_move_count: usize,
    cfg: &PolyglotConfig,
    variant_label: &str,
    rng: &mut R,
) -> Option<BookMove>
where
    P: Position + ZobristHash,
    R: Rng + ?Sized,
{
    if !cfg.enabled {
        return None;
    }
    // Python: `max_game_length = polyglot_cfg.max_depth * 2 - 1` —
    // i.e. after `max_depth * 2` halfmoves played we stop looking.
    let max_half_moves = (cfg.max_depth as usize).saturating_mul(2);
    if half_move_count >= max_half_moves {
        return None;
    }

    let selection = Selection::parse(&cfg.selection).unwrap_or(Selection::WeightedRandom);
    let normalization = Normalization::parse(&cfg.normalization).unwrap_or(Normalization::None);

    let books = match cfg.book.get(variant_label) {
        Some(list) => list,
        None => return None,
    };

    for book_path in books {
        match probe_book(book_path, pos, selection, cfg.min_weight, normalization, rng) {
            Ok(Some(book_move)) => return Some(book_move),
            Ok(None) => continue,
            Err(e) => {
                warn!(book = %book_path, error = %e, "polyglot book failed, skipping");
                continue;
            }
        }
    }
    None
}

fn probe_book<P, R>(
    path: &str,
    pos: &P,
    selection: Selection,
    min_weight_pct: i64,
    norm: Normalization,
    rng: &mut R,
) -> Result<Option<BookMove>, BookError>
where
    P: Position + ZobristHash,
    R: Rng + ?Sized,
{
    let mut reader = BookReader::open(path)?;
    let entries = reader.find_all_for(pos)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let min_weight = min_weight_threshold(&entries, min_weight_pct, norm);
    let Some(picked) = select_entry(&entries, selection, min_weight, rng) else {
        return Ok(None);
    };
    let Some(mv) = picked.decode_move(pos) else {
        debug!(book = %path, raw_move = picked.raw_move, "polyglot entry has no matching legal move");
        return Ok(None);
    };
    let label = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string();
    Ok(Some(BookMove { mv, book_label: label }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use shakmaty::fen::Fen;
    use shakmaty::{CastlingMode, Chess};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn start_position() -> Chess {
        Chess::default()
    }

    fn pos_from_fen(fen: &str) -> Chess {
        fen.parse::<Fen>()
            .expect("fen parses")
            .into_position(CastlingMode::Standard)
            .expect("legal position")
    }

    fn polyglot_move(from: Square, to: Square, promotion: Option<Role>) -> u16 {
        let to_bits = u32::from(to.file()) | (u32::from(to.rank()) << 3);
        let from_bits = u32::from(from.file()) | (u32::from(from.rank()) << 3);
        let promotion_bits = match promotion {
            None => 0,
            Some(Role::Knight) => 1,
            Some(Role::Bishop) => 2,
            Some(Role::Rook) => 3,
            Some(Role::Queen) => 4,
            _ => unreachable!("polyglot only encodes N/B/R/Q promotions"),
        };
        (to_bits | (from_bits << 6) | (promotion_bits << 12)) as u16
    }

    fn write_book(entries: &[PolyglotEntry]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("tempfile");
        let mut sorted: Vec<PolyglotEntry> = entries.to_vec();
        sorted.sort_by_key(|e| e.key);
        for e in sorted {
            file.write_all(&e.to_bytes()).expect("write entry");
        }
        file.flush().expect("flush");
        file
    }

    #[test]
    fn entry_roundtrip_through_bytes() {
        let e = PolyglotEntry { key: 0xDEAD_BEEF_CAFE_1234, raw_move: 0x1234, weight: 7, learn: 42 };
        assert_eq!(PolyglotEntry::from_bytes(e.to_bytes()), e);
    }

    #[test]
    fn decode_normal_pawn_move_against_start_position() {
        let pos = start_position();
        let raw = polyglot_move(Square::E2, Square::E4, None);
        let mv = decode_raw_move(raw, &pos).expect("legal e2e4");
        assert_eq!(mv.from(), Some(Square::E2));
        assert_eq!(mv.to(), Square::E4);
        assert!(mv.promotion().is_none());
    }

    #[test]
    fn decode_promotion_move() {
        // White pawn on a7, ready to promote.
        let pos = pos_from_fen("4k3/P7/8/8/8/8/8/4K3 w - - 0 1");
        let raw = polyglot_move(Square::A7, Square::A8, Some(Role::Queen));
        let mv = decode_raw_move(raw, &pos).expect("legal a7a8q");
        assert_eq!(mv.promotion(), Some(Role::Queen));
    }

    #[test]
    fn decode_castling_uses_king_to_rook_coords() {
        // White can castle kingside, classical e1h1 encoding.
        let pos = pos_from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1");
        let raw = polyglot_move(Square::E1, Square::H1, None);
        let mv = decode_raw_move(raw, &pos).expect("legal short castle");
        assert!(mv.is_castle(), "expected Castle, got {mv:?}");
        assert_eq!(mv.from(), Some(Square::E1));
        assert_eq!(mv.to(), Square::H1);
    }

    #[test]
    fn decode_returns_none_for_illegal_move() {
        let pos = start_position();
        let raw = polyglot_move(Square::E2, Square::E5, None);
        assert!(decode_raw_move(raw, &pos).is_none());
    }

    #[test]
    fn encode_move_roundtrips_through_decode_for_all_legal_moves() {
        // Every legal move in a few representative positions must encode
        // and decode back to itself — the strongest guarantee that
        // `encode_move` is the exact inverse of `decode_raw_move`.
        let positions = [
            start_position(),
            pos_from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1"),
            pos_from_fen("4k3/P7/8/8/8/8/8/4K3 w - - 0 1"),
            pos_from_fen("rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3"),
        ];
        for pos in positions {
            for mv in pos.legal_moves() {
                let raw = encode_move(&mv).expect("legal move has an origin square");
                let decoded = decode_raw_move(raw, &pos).expect("re-decode the move we just encoded");
                assert_eq!(decoded, mv, "roundtrip mismatch for {mv:?} (raw {raw:#06x})");
            }
        }
    }

    #[test]
    fn encode_move_matches_canonical_castle_vector() {
        // clrsrc's reference: white kingside castle encodes to 0x0107
        // (king e1 → rook h1). shakmaty's Castle move carries those
        // coords directly, so no special-casing is needed.
        let pos = pos_from_fen("r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1");
        let castle = pos
            .legal_moves()
            .into_iter()
            .find(|m| m.is_castle() && m.to() == Square::H1)
            .expect("short castle is legal");
        assert_eq!(encode_move(&castle), Some(0x0107));
    }

    #[test]
    fn reader_finds_all_entries_for_key_via_binary_search() {
        let key_a = 0x0000_0000_0000_0001;
        let key_b = 0x463b_9618_1691_fc9c; // start position
        let key_c = 0xFFFF_FFFF_FFFF_FFFF;
        let entries = vec![
            PolyglotEntry { key: key_a, raw_move: 0x0001, weight: 5, learn: 0 },
            PolyglotEntry { key: key_b, raw_move: 0x0aaa, weight: 50, learn: 0 },
            PolyglotEntry { key: key_b, raw_move: 0x0bbb, weight: 30, learn: 0 },
            PolyglotEntry { key: key_b, raw_move: 0x0ccc, weight: 20, learn: 0 },
            PolyglotEntry { key: key_c, raw_move: 0x0002, weight: 1, learn: 0 },
        ];
        let tmp = write_book(&entries);
        let mut reader = BookReader::open(tmp.path()).expect("open book");
        let hits = reader.find_all(key_b).expect("find_all");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].weight, 50);
        assert_eq!(hits[2].weight, 20);

        assert!(reader.find_all(0xDEAD).expect("miss").is_empty());
    }

    #[test]
    fn find_all_for_uses_polyglot_zobrist_of_start_position() {
        let pos = start_position();
        let raw = polyglot_move(Square::E2, Square::E4, None);
        let key = 0x463b_9618_1691_fc9c; // shakmaty's reference value
        let entries = vec![PolyglotEntry { key, raw_move: raw, weight: 100, learn: 0 }];
        let tmp = write_book(&entries);
        let mut reader = BookReader::open(tmp.path()).expect("open");
        let hits = reader.find_all_for(&pos).expect("find_all_for");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].decode_move(&pos).map(|m| m.to()), Some(Square::E4));
    }

    #[test]
    fn min_weight_threshold_handles_normalizations() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 0, weight: 50, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 0, weight: 30, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 0, weight: 20, learn: 0 },
        ];
        // none: threshold = min_weight (scaled by 100/100)
        assert_eq!(min_weight_threshold(&entries, 25, Normalization::None), 25);
        // sum: scalar = 100, threshold = 25 * 100 / 100 = 25
        assert_eq!(min_weight_threshold(&entries, 25, Normalization::Sum), 25);
        // max: scalar = 50, threshold = 25 * 50 / 100 = 12
        assert_eq!(min_weight_threshold(&entries, 25, Normalization::Max), 12);
    }

    #[test]
    fn best_move_picks_highest_weight_above_threshold() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 1, weight: 100, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 2, weight: 40, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 3, weight: 200, learn: 0 },
        ];
        let mut rng = StdRng::seed_from_u64(0);
        let pick = select_entry(&entries, Selection::BestMove, 0, &mut rng).unwrap();
        assert_eq!(pick.raw_move, 3);
    }

    #[test]
    fn best_move_respects_min_weight() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 1, weight: 30, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 2, weight: 40, learn: 0 },
        ];
        let mut rng = StdRng::seed_from_u64(0);
        assert!(select_entry(&entries, Selection::BestMove, 50, &mut rng).is_none());
        let pick = select_entry(&entries, Selection::BestMove, 35, &mut rng).unwrap();
        assert_eq!(pick.raw_move, 2);
    }

    #[test]
    fn uniform_random_only_picks_above_threshold() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 1, weight: 10, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 2, weight: 99, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 3, weight: 100, learn: 0 },
        ];
        let mut rng = StdRng::seed_from_u64(42);
        for _ in 0..30 {
            let pick = select_entry(&entries, Selection::UniformRandom, 50, &mut rng).unwrap();
            assert!(pick.raw_move != 1);
        }
    }

    #[test]
    fn weighted_random_converges_towards_weight_distribution() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 1, weight: 9, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 2, weight: 1, learn: 0 },
        ];
        let mut rng = StdRng::seed_from_u64(7);
        let mut hits_1 = 0;
        let mut hits_2 = 0;
        for _ in 0..10_000 {
            let pick = select_entry(&entries, Selection::WeightedRandom, 0, &mut rng).unwrap();
            match pick.raw_move {
                1 => hits_1 += 1,
                2 => hits_2 += 1,
                _ => panic!("unexpected raw_move {}", pick.raw_move),
            }
        }
        // Expected ratio is 9:1 — generous tolerance for RNG variance.
        assert!(hits_1 > 8_500 && hits_1 < 9_500, "hits_1 = {hits_1}");
        assert!(hits_2 > 500 && hits_2 < 1_500, "hits_2 = {hits_2}");
    }

    #[test]
    fn weighted_random_falls_back_to_uniform_on_all_zero_weights() {
        let entries = vec![
            PolyglotEntry { key: 0, raw_move: 1, weight: 0, learn: 0 },
            PolyglotEntry { key: 0, raw_move: 2, weight: 0, learn: 0 },
        ];
        let mut rng = StdRng::seed_from_u64(1);
        let pick = select_entry(&entries, Selection::WeightedRandom, 0, &mut rng).unwrap();
        assert!(pick.raw_move == 1 || pick.raw_move == 2);
    }

    // -------------- get_book_move orchestration --------------

    fn book_cfg_for(books: Vec<(&str, Vec<String>)>) -> PolyglotConfig {
        let mut cfg = PolyglotConfig::default();
        cfg.enabled = true;
        cfg.max_depth = 8;
        cfg.selection = "best_move".into();
        cfg.normalization = "none".into();
        cfg.min_weight = 0;
        for (k, v) in books {
            cfg.book.insert(k.into(), v);
        }
        cfg
    }

    #[test]
    fn get_book_move_returns_first_book_hit() {
        let pos = start_position();
        let raw = polyglot_move(Square::E2, Square::E4, None);
        let entries = vec![PolyglotEntry {
            key: 0x463b_9618_1691_fc9c,
            raw_move: raw,
            weight: 100,
            learn: 0,
        }];
        let tmp = write_book(&entries);
        let path = tmp.path().to_string_lossy().to_string();
        let cfg = book_cfg_for(vec![("standard", vec![path])]);
        let mut rng = StdRng::seed_from_u64(0);
        let hit = get_book_move(&pos, 0, &cfg, "standard", &mut rng).expect("book hit");
        assert_eq!(hit.mv.to(), Square::E4);
    }

    #[test]
    fn get_book_move_respects_max_depth() {
        let pos = start_position();
        let raw = polyglot_move(Square::E2, Square::E4, None);
        let entries = vec![PolyglotEntry {
            key: 0x463b_9618_1691_fc9c,
            raw_move: raw,
            weight: 100,
            learn: 0,
        }];
        let tmp = write_book(&entries);
        let path = tmp.path().to_string_lossy().to_string();
        let mut cfg = book_cfg_for(vec![("standard", vec![path])]);
        cfg.max_depth = 3;
        let mut rng = StdRng::seed_from_u64(0);
        // 5 plies < 6 max half-moves — still inside the book.
        assert!(get_book_move(&pos, 5, &cfg, "standard", &mut rng).is_some());
        // 6 plies == max → fall out.
        assert!(get_book_move(&pos, 6, &cfg, "standard", &mut rng).is_none());
    }

    #[test]
    fn get_book_move_skips_disabled_or_missing_variant() {
        let pos = start_position();
        let mut cfg = PolyglotConfig::default();
        cfg.enabled = false;
        let mut rng = StdRng::seed_from_u64(0);
        assert!(get_book_move(&pos, 0, &cfg, "standard", &mut rng).is_none());

        cfg.enabled = true;
        // No book for "antichess" → None even with enabled.
        assert!(get_book_move(&pos, 0, &cfg, "antichess", &mut rng).is_none());
    }

    #[test]
    fn get_book_move_walks_past_corrupt_first_book() {
        let pos = start_position();
        let raw = polyglot_move(Square::D2, Square::D4, None);
        let entries = vec![PolyglotEntry {
            key: 0x463b_9618_1691_fc9c,
            raw_move: raw,
            weight: 200,
            learn: 0,
        }];
        let good = write_book(&entries);
        let good_path = good.path().to_string_lossy().to_string();
        let cfg = book_cfg_for(vec![(
            "standard",
            vec!["P:/this/path/does/not/exist.bin".to_string(), good_path],
        )]);
        let mut rng = StdRng::seed_from_u64(0);
        let hit = get_book_move(&pos, 0, &cfg, "standard", &mut rng).expect("second book hit");
        assert_eq!(hit.mv.to(), Square::D4);
    }
}
