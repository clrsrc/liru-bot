//! JBK2 experience-overlay writer.
//!
//! After each finished game the bot harvests the WDL outcome of clrsrc's
//! own opening moves and appends them to `clrsrc.exp.overlay`. clrsrc
//! consolidates the overlay offline into the main book via `expmerge`;
//! at runtime nobody reads the overlay (clrsrc's reader only loads
//! `clrsrc.exp`), so a plain append-only writer is enough.
//!
//! Format authority: `chess_engine/rust_engine/docs/exp_v2_format.md`
//! (shared JBK2 v2 spec, 32-byte header + 32-byte entries, little-endian).
//!
//! Our entries are **Selfplay-WDL** records. When the move came with an
//! engine search eval we persist clrsrc's centipawn judgment so it
//! survives the offline merge: `clrsrc_score` (offset 28) carries the cp
//! and the source bitmask gets the clrsrc bit (`source = 0x28 =
//! Selfplay|clrsrc`). Without `0x20` the canonical `score` (offset 10) is
//! a derived mirror that `expmerge` recomputes from `jug→sf→clrsrc→0`, so
//! a cp written only to `score` would be lost on consolidation. We are the
//! sole `0x20` writer (clrsrc's own LearnDuringPlay path is off).
//!
//! Book / tablebase moves have no cp: those stay `source = 0x08` with
//! `score`/`clrsrc_score = i16::MIN`. `jug_score`/`sf_score`/`nnue_eval`
//! are always the `i16::MIN` sentinel — those belong to other sources and
//! are filled during the offline merge, never by us.

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const HEADER_SIZE: usize = 32;
pub const ENTRY_SIZE: usize = 32;

const MAGIC: [u8; 4] = *b"JBK2";
const VERSION: u16 = 2;
/// Header flag bit 1 — marks the file as an append-only overlay (§3).
const FLAG_OVERLAY: u16 = 0x0002;

/// Source bitmask bit 3 — "move was actually played in a selfplay game" (§8).
pub const SOURCE_SELFPLAY: u8 = 0x08;
/// Source bitmask bit 5 — "clrsrc (foreign engine) judged this (key,move)";
/// gates the validity of `clrsrc_score` (§8 interop note, 2026-05-27).
pub const SOURCE_CLRSRC: u8 = 0x20;
/// Entry flag bit 0 — the move was verified legal (§7). Always true for us:
/// we only harvest moves we actually played, which Lichess accepted.
pub const FLAG_VALIDATED: u8 = 0x01;

/// "Field not set" sentinel for the i16 score fields (§4).
pub const UNSET_SCORE: i16 = i16::MIN;

/// Game outcome from clrsrc's point of view, used to fill `wdl_w`/`wdl_l`.
/// Draws are implicit (`count - wdl_w - wdl_l`), so a draw sets neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameWdl {
    Win,
    Loss,
    Draw,
}

/// One JBK2 selfplay-WDL entry, ready to serialize. Engine-score fields
/// that don't belong to us are pinned to [`UNSET_SCORE`] in [`Self::to_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jbk2Entry {
    /// Polyglot hash of the position **before** clrsrc's move.
    pub key: u64,
    /// Polyglot 16-bit encoding of the move clrsrc played.
    pub packed_move: u16,
    /// Side-to-move centipawn eval from the engine info line, or
    /// [`UNSET_SCORE`] for book / tablebase moves with no score.
    pub score: i16,
    /// Search depth from the engine info line, or 0 when unknown.
    pub depth: i16,
    /// Game outcome (sets `wdl_w`/`wdl_l`).
    pub wdl: GameWdl,
}

impl Jbk2Entry {
    /// Build a selfplay entry. `score_cp` / `depth` come from the engine's
    /// info line for clrsrc's move; pass `None` for book / tablebase moves
    /// (→ `score = i16::MIN`, `depth = 0`).
    pub fn selfplay(
        key: u64,
        packed_move: u16,
        score_cp: Option<i64>,
        depth: Option<u32>,
        wdl: GameWdl,
    ) -> Self {
        Self {
            key,
            packed_move,
            score: score_cp.map_or(UNSET_SCORE, clamp_to_i16),
            depth: depth.map_or(0, |d| d.min(i16::MAX as u32) as i16),
            wdl,
        }
    }

    /// Serialize to the 32-byte little-endian on-disk layout (§4).
    pub fn to_bytes(&self) -> [u8; ENTRY_SIZE] {
        let (wdl_w, wdl_l): (u16, u16) = match self.wdl {
            GameWdl::Win => (1, 0),
            GameWdl::Loss => (0, 1),
            GameWdl::Draw => (0, 0),
        };
        // A real cp can never equal the sentinel (clamp floor is MIN+1), so
        // `score != UNSET` reliably means "we have clrsrc's search eval".
        let has_cp = self.score != UNSET_SCORE;
        let source = SOURCE_SELFPLAY | if has_cp { SOURCE_CLRSRC } else { 0 };

        let mut b = [0u8; ENTRY_SIZE];
        b[0..8].copy_from_slice(&self.key.to_le_bytes());
        b[8..10].copy_from_slice(&self.packed_move.to_le_bytes());
        b[10..12].copy_from_slice(&self.score.to_le_bytes());
        b[12..14].copy_from_slice(&self.depth.to_le_bytes());
        b[14..16].copy_from_slice(&1u16.to_le_bytes()); // count
        b[16] = source;
        b[17] = FLAG_VALIDATED;
        b[18..20].copy_from_slice(&wdl_w.to_le_bytes());
        b[20..22].copy_from_slice(&wdl_l.to_le_bytes());
        b[22..24].copy_from_slice(&UNSET_SCORE.to_le_bytes()); // nnue_eval
        b[24..26].copy_from_slice(&UNSET_SCORE.to_le_bytes()); // jug_score (Jugernaut-exclusive)
        b[26..28].copy_from_slice(&UNSET_SCORE.to_le_bytes()); // sf_score
        // clrsrc_score (offset 28): clrsrc's cp, gated by the 0x20 source bit.
        b[28..30].copy_from_slice(&self.score.to_le_bytes());
        b[30..32].copy_from_slice(&0u16.to_le_bytes()); // reserved
        b
    }
}

fn clamp_to_i16(cp: i64) -> i16 {
    cp.clamp(i16::MIN as i64 + 1, i16::MAX as i64) as i16
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fresh_header(entry_count: u64) -> [u8; HEADER_SIZE] {
    let mut h = [0u8; HEADER_SIZE];
    h[0..4].copy_from_slice(&MAGIC);
    h[4..6].copy_from_slice(&VERSION.to_le_bytes());
    h[6..8].copy_from_slice(&FLAG_OVERLAY.to_le_bytes());
    h[8..16].copy_from_slice(&entry_count.to_le_bytes());
    h[16..24].copy_from_slice(&now_unix_secs().to_le_bytes());
    // 24..32 reserved = 0
    h
}

/// Append `entries` to the overlay at `path`, creating it with a fresh
/// JBK2 header if it doesn't exist yet. On an existing file the magic +
/// version are validated, the entries are appended at the end, and the
/// header's `entry_count` + `build_timestamp` are updated.
///
/// Appends are atomic per call only insofar as the bot serializes them
/// (`concurrency: 1`); there is no cross-process lock because clrsrc never
/// touches the overlay at runtime.
pub fn append_entries(path: impl AsRef<Path>, entries: &[Jbk2Entry]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = path.as_ref();

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    let len = file.metadata()?.len();

    let prev_count = if len == 0 {
        // New file: lay down a header with count 0 first; we rewrite the
        // count below once the entries are on disk.
        file.write_all(&fresh_header(0))?;
        0u64
    } else {
        if len < HEADER_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("overlay {} is too small to hold a JBK2 header", path.display()),
            ));
        }
        let mut header = [0u8; HEADER_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        if header[0..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("overlay {} has bad magic (not JBK2)", path.display()),
            ));
        }
        let version = u16::from_le_bytes([header[4], header[5]]);
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("overlay {} has unsupported version {version}", path.display()),
            ));
        }
        u64::from_le_bytes(header[8..16].try_into().unwrap())
    };

    // Append the entries at the end of the file.
    file.seek(SeekFrom::End(0))?;
    let mut buf = Vec::with_capacity(entries.len() * ENTRY_SIZE);
    for e in entries {
        buf.extend_from_slice(&e.to_bytes());
    }
    file.write_all(&buf)?;

    // Update entry_count and build_timestamp in the header.
    let new_count = prev_count + entries.len() as u64;
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&new_count.to_le_bytes())?;
    file.seek(SeekFrom::Start(16))?;
    file.write_all(&now_unix_secs().to_le_bytes())?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn entry(key: u64, mv: u16, wdl: GameWdl) -> Jbk2Entry {
        Jbk2Entry::selfplay(key, mv, Some(25), Some(18), wdl)
    }

    #[test]
    fn entry_bytes_match_spec_layout() {
        let e = Jbk2Entry::selfplay(0x463B_9618_1691_FC9C, 0x1C6C, Some(25), Some(18), GameWdl::Win);
        let b = e.to_bytes();
        // key (LE)
        assert_eq!(&b[0..8], &0x463B_9618_1691_FC9Cu64.to_le_bytes());
        // packed_move, score=25, depth=18, count=1
        assert_eq!(&b[8..10], &0x1C6Cu16.to_le_bytes());
        assert_eq!(&b[10..12], &25i16.to_le_bytes());
        assert_eq!(&b[12..14], &18i16.to_le_bytes());
        assert_eq!(&b[14..16], &1u16.to_le_bytes());
        // cp present → source = Selfplay|clrsrc (0x28), flags = VALIDATED (0x01)
        assert_eq!(b[16], 0x28);
        assert_eq!(b[17], 0x01);
        // win → wdl_w=1, wdl_l=0
        assert_eq!(&b[18..20], &1u16.to_le_bytes());
        assert_eq!(&b[20..22], &0u16.to_le_bytes());
        // nnue / jug / sf all i16::MIN (0x8000 LE = [0x00, 0x80])
        for off in [22usize, 24, 26] {
            assert_eq!(&b[off..off + 2], &i16::MIN.to_le_bytes(), "field at {off}");
        }
        // clrsrc_score (off28) carries the cp so it survives the merge.
        assert_eq!(&b[28..30], &25i16.to_le_bytes());
        // reserved = 0
        assert_eq!(&b[30..32], &0u16.to_le_bytes());
    }

    #[test]
    fn loss_and_draw_set_wdl_fields() {
        let loss = entry(1, 2, GameWdl::Loss).to_bytes();
        assert_eq!(&loss[18..20], &0u16.to_le_bytes());
        assert_eq!(&loss[20..22], &1u16.to_le_bytes());
        let draw = entry(1, 2, GameWdl::Draw).to_bytes();
        assert_eq!(&draw[18..20], &0u16.to_le_bytes());
        assert_eq!(&draw[20..22], &0u16.to_le_bytes());
    }

    #[test]
    fn bookless_move_uses_unset_score_and_zero_depth() {
        let e = Jbk2Entry::selfplay(1, 2, None, None, GameWdl::Win);
        let b = e.to_bytes();
        assert_eq!(&b[10..12], &i16::MIN.to_le_bytes());
        assert_eq!(&b[12..14], &0i16.to_le_bytes());
        // No cp → no clrsrc bit, clrsrc_score stays unset.
        assert_eq!(b[16], SOURCE_SELFPLAY);
        assert_eq!(&b[28..30], &i16::MIN.to_le_bytes());
    }

    #[test]
    fn move_with_cp_sets_clrsrc_bit_and_score() {
        let e = Jbk2Entry::selfplay(1, 2, Some(-40), Some(15), GameWdl::Loss);
        let b = e.to_bytes();
        assert_eq!(b[16], SOURCE_SELFPLAY | SOURCE_CLRSRC); // 0x28
        assert_eq!(&b[10..12], &(-40i16).to_le_bytes()); // canonical score
        assert_eq!(&b[28..30], &(-40i16).to_le_bytes()); // clrsrc_score
    }

    #[test]
    fn score_is_clamped_into_i16_range() {
        let e = Jbk2Entry::selfplay(1, 2, Some(100_000), Some(99), GameWdl::Win);
        assert_eq!(e.score, i16::MAX);
        // depth also clamped
        let e2 = Jbk2Entry::selfplay(1, 2, Some(0), Some(1_000_000), GameWdl::Win);
        assert_eq!(e2.depth, i16::MAX);
    }

    #[test]
    fn append_creates_header_then_grows_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("clrsrc.exp.overlay");

        append_entries(&path, &[entry(10, 20, GameWdl::Win)]).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"JBK2");
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), FLAG_OVERLAY);
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 1);
        assert_eq!(bytes.len(), HEADER_SIZE + ENTRY_SIZE);

        // Append two more — count becomes 3, file grows by 2 entries.
        append_entries(&path, &[entry(11, 21, GameWdl::Loss), entry(12, 22, GameWdl::Draw)])
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 3);
        assert_eq!(bytes.len(), HEADER_SIZE + 3 * ENTRY_SIZE);

        // First entry's key still intact after the header rewrite.
        let first = &bytes[HEADER_SIZE..HEADER_SIZE + 8];
        assert_eq!(first, &10u64.to_le_bytes());
    }

    #[test]
    fn append_nothing_is_a_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.overlay");
        append_entries(&path, &[]).unwrap();
        assert!(!path.exists(), "no file should be created for zero entries");
    }

    #[test]
    fn rejects_foreign_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.overlay");
        fs::write(&path, b"NOPEnot-a-jbk2-header-padding-bytes...").unwrap();
        let err = append_entries(&path, &[entry(1, 2, GameWdl::Win)]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
