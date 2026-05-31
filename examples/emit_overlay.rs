//! Dev utility: emit a one-entry JBK2 overlay so the writer can be
//! roundtrip-checked against clrsrc's reader:
//!
//!   cargo run --example emit_overlay -- <out.overlay>
//!   clrsrc.exe exp <out.overlay> "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"
//!
//! The entry is start-position → e2e4, score +25 / depth 18, result = Win.
//! Expected dump fields: src=8 (Selfplay), wdl_w=1, jug/clrsrc=i16::MIN.

use liru_bot::exp_overlay::{append_entries, GameWdl, Jbk2Entry};
use liru_bot::polyglot::{encode_move, polyglot_hash};
use shakmaty::{Chess, Position, Square};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "test.exp.overlay".into());
    let pos = Chess::default();
    let mv = pos
        .legal_moves()
        .into_iter()
        .find(|m| m.from() == Some(Square::E2) && m.to() == Square::E4)
        .expect("e2e4 is legal at the start position");
    let key = polyglot_hash(&pos);
    let packed = encode_move(&mv).expect("e2e4 encodes");
    let entry = Jbk2Entry::selfplay(key, packed, Some(25), Some(18), GameWdl::Win);
    append_entries(&path, &[entry]).expect("append entry");
    println!("wrote {path}: key={key:#018x} packed_move={packed:#06x}");
}
