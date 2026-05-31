//! Typed wrappers over the Lichess JSON API. Mirrors `lib/lichess_types.py`.
//!
//! Python uses `TypedDict` (with `total=False`) for everything — fields may be
//! absent depending on the endpoint. We model that with `serde` `Option<T>`
//! plus `#[serde(default)]` and keep an `extra` map for forwards compatibility.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Catch-all dynamic JSON value.
pub type JsonValue = serde_json::Value;
/// Free-form options map (UCI/XBoard).
pub type OptionsMap = HashMap<String, JsonValue>;
/// Engine `go` command extras.
pub type GoCommands = HashMap<String, JsonValue>;
/// XBoard endgame-tablebase paths (`gaviota`, `nalimov`, …).
pub type EgtPath = HashMap<String, String>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfType {
    #[serde(default)] pub games: Option<i64>,
    #[serde(default)] pub rating: Option<i64>,
    #[serde(default)] pub rd: Option<i64>,
    #[serde(default)] pub sd: Option<i64>,
    #[serde(default)] pub prov: Option<bool>,
    #[serde(default)] pub prog: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileType {
    #[serde(default)] pub country: Option<String>,
    #[serde(default)] pub location: Option<String>,
    #[serde(default)] pub bio: Option<String>,
    #[serde(default)] pub first_name: Option<String>,
    #[serde(default)] pub last_name: Option<String>,
    #[serde(default)] pub fide_rating: Option<i64>,
    #[serde(default)] pub uscf_rating: Option<i64>,
    #[serde(default)] pub ecf_rating: Option<i64>,
    #[serde(default)] pub cfc_rating: Option<i64>,
    #[serde(default)] pub dsb_rating: Option<i64>,
    #[serde(default)] pub links: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfileType {
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub username: Option<String>,
    #[serde(default)] pub perfs: Option<HashMap<String, PerfType>>,
    #[serde(default)] pub created_at: Option<i64>,
    #[serde(default)] pub profile: Option<ProfileType>,
    #[serde(default)] pub seen_at: Option<i64>,
    #[serde(default)] pub patron: Option<JsonValue>,
    #[serde(default)] pub verified: Option<JsonValue>,
    #[serde(default)] pub play_time: Option<HashMap<String, i64>>,
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub online: Option<bool>,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub followable: Option<bool>,
    #[serde(default)] pub following: Option<bool>,
    #[serde(default)] pub blocking: Option<bool>,
    #[serde(default)] pub follows_you: Option<bool>,
    #[serde(default)] pub count: Option<HashMap<String, i64>>,
}

impl UserProfileType {
    /// Username, without falling back to user-id.
    pub fn username(&self) -> &str {
        self.username.as_deref().unwrap_or_default()
    }

    /// Bot rating for a given perf-name (case-insensitive).
    pub fn rating_for(&self, perf_name: &str) -> Option<i64> {
        let key = perf_name.to_lowercase();
        self.perfs
            .as_ref()?
            .get(&key)
            .and_then(|p| p.rating)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerType {
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub rating: Option<i64>,
    #[serde(default)] pub provisional: Option<bool>,
    #[serde(default)] pub ai_level: Option<i64>,
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub username: Option<String>,
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub online: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantInfo {
    #[serde(default)] pub key: Option<String>,
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub short: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameStatusType {
    #[serde(default)] pub id: Option<i64>,
    #[serde(default)] pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameType {
    #[serde(default)] pub game_id: Option<String>,
    #[serde(default)] pub full_id: Option<String>,
    #[serde(default)] pub color: Option<String>,
    #[serde(default)] pub fen: Option<String>,
    #[serde(default)] pub has_moved: Option<bool>,
    #[serde(default)] pub is_my_turn: Option<bool>,
    #[serde(default)] pub last_move: Option<String>,
    #[serde(default)] pub opponent: Option<PlayerType>,
    #[serde(default)] pub perf: Option<String>,
    #[serde(default)] pub rated: Option<bool>,
    #[serde(default)] pub seconds_left: Option<i64>,
    #[serde(default)] pub source: Option<String>,
    #[serde(default)] pub status: Option<JsonValue>,
    #[serde(default)] pub speed: Option<String>,
    #[serde(default)] pub variant: Option<VariantInfo>,
    #[serde(default)] pub compat: Option<HashMap<String, bool>>,
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub winner: Option<String>,
    #[serde(default)] pub rating_diff: Option<i64>,
    #[serde(default)] pub pgn: Option<String>,
    #[serde(default)] pub complete: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeControlType {
    #[serde(default)] pub increment: Option<i64>,
    #[serde(default)] pub limit: Option<i64>,
    #[serde(default)] pub show: Option<String>,
    #[serde(default, rename = "type")] pub kind: Option<String>,
    #[serde(default)] pub days_per_turn: Option<i64>,
    #[serde(default)] pub initial: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeType {
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub color: Option<String>,
    #[serde(default)] pub direction: Option<String>,
    #[serde(default)] pub rated: Option<bool>,
    #[serde(default)] pub speed: Option<String>,
    #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub time_control: Option<TimeControlType>,
    #[serde(default)] pub variant: Option<VariantInfo>,
    #[serde(default)] pub challenger: Option<PlayerType>,
    #[serde(default)] pub dest_user: Option<PlayerType>,
    #[serde(default)] pub perf: Option<HashMap<String, JsonValue>>,
    #[serde(default)] pub compat: Option<HashMap<String, bool>>,
    #[serde(default)] pub final_color: Option<String>,
    #[serde(default)] pub decline_reason: Option<String>,
    #[serde(default)] pub decline_reason_key: Option<String>,
    #[serde(default)] pub initial_fen: Option<String>,
    #[serde(default)] pub error: Option<String>,
    #[serde(default)] pub ratelimit: Option<HashMap<String, JsonValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub bot_is_rate_limited: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub opponent_is_rate_limited: Option<bool>,
    #[serde(default, skip)] pub rate_limit_timeout: Option<Duration>,
}

/// First-move abort timer from the game stream (`gameState.expiration`).
/// Lichess sends this only while the game is still abortable (the first
/// move is pending) and drops it once both sides have moved. It is the
/// authoritative deadline by which the side to move must play or the game
/// is aborted — we prefer it over our configured estimate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpirationType {
    /// Milliseconds elapsed since the last move (or game start).
    #[serde(default)] pub idle_millis: Option<i64>,
    /// Milliseconds each player has for their first move before abort.
    #[serde(default)] pub millis_to_move: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameStateType {
    #[serde(default, rename = "type")] pub kind: Option<String>,
    #[serde(default)] pub moves: Option<String>,
    #[serde(default)] pub wtime: Option<i64>,
    #[serde(default)] pub btime: Option<i64>,
    #[serde(default)] pub winc: Option<i64>,
    #[serde(default)] pub binc: Option<i64>,
    #[serde(default)] pub wdraw: Option<bool>,
    #[serde(default)] pub bdraw: Option<bool>,
    #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub winner: Option<String>,
    #[serde(default)] pub wtakeback: Option<bool>,
    #[serde(default)] pub btakeback: Option<bool>,
    #[serde(default)] pub expiration: Option<ExpirationType>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameEventType {
    #[serde(default, rename = "type")] pub kind: Option<String>,
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub rated: Option<bool>,
    #[serde(default)] pub variant: Option<VariantInfo>,
    #[serde(default)] pub speed: Option<String>,
    #[serde(default)] pub perf: Option<HashMap<String, JsonValue>>,
    #[serde(default)] pub created_at: Option<i64>,
    #[serde(default)] pub white: Option<PlayerType>,
    #[serde(default)] pub black: Option<PlayerType>,
    #[serde(default)] pub initial_fen: Option<String>,
    #[serde(default)] pub state: Option<GameStateType>,
    #[serde(default)] pub username: Option<String>,
    #[serde(default)] pub text: Option<String>,
    #[serde(default)] pub room: Option<String>,
    #[serde(default)] pub gone: Option<bool>,
    #[serde(default)] pub claim_win_in_seconds: Option<i64>,

    /// Inline `gameState` fields (Lichess flattens these in some events).
    #[serde(default)] pub moves: Option<String>,
    #[serde(default)] pub wtime: Option<i64>,
    #[serde(default)] pub btime: Option<i64>,
    #[serde(default)] pub winc: Option<i64>,
    #[serde(default)] pub binc: Option<i64>,
    #[serde(default)] pub wdraw: Option<bool>,
    #[serde(default)] pub bdraw: Option<bool>,
    #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub winner: Option<String>,
    #[serde(default)] pub clock: Option<TimeControlType>,
    #[serde(default)] pub wtakeback: Option<bool>,
    #[serde(default)] pub btakeback: Option<bool>,
    #[serde(default)] pub expiration: Option<ExpirationType>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventType {
    #[serde(rename = "type", default)] pub kind: Option<String>,
    #[serde(default)] pub game: Option<GameType>,
    #[serde(default)] pub challenge: Option<ChallengeType>,
    #[serde(default)] pub error: Option<String>,
}

/// What to do if the opponent declines our challenge. Matches Python's
/// `FilterType` enum (kept lowercase for YAML compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterType {
    None,
    Coarse,
    Fine,
}

impl Default for FilterType {
    fn default() -> Self {
        FilterType::None
    }
}

impl fmt::Display for FilterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            FilterType::None => "none",
            FilterType::Coarse => "coarse",
            FilterType::Fine => "fine",
        };
        f.write_str(s)
    }
}

impl FilterType {
    pub const fn all() -> &'static [FilterType] {
        &[FilterType::None, FilterType::Coarse, FilterType::Fine]
    }

    pub fn parse(s: &str) -> Option<FilterType> {
        match s.trim().to_lowercase().as_str() {
            "none" => Some(FilterType::None),
            "coarse" => Some(FilterType::Coarse),
            "fine" => Some(FilterType::Fine),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDataType {
    #[serde(default)] pub id: Option<String>,
    #[serde(default)] pub username: Option<String>,
    #[serde(default)] pub perfs: Option<HashMap<String, PerfType>>,
    #[serde(default)] pub flair: Option<String>,
    #[serde(default)] pub created_at: Option<i64>,
    #[serde(default)] pub disabled: Option<bool>,
    #[serde(default)] pub tos_violation: Option<bool>,
    #[serde(default)] pub profile: Option<ProfileType>,
    #[serde(default)] pub seen_at: Option<i64>,
    #[serde(default)] pub patron: Option<bool>,
    #[serde(default)] pub verified: Option<bool>,
    #[serde(default)] pub play_time: Option<HashMap<String, i64>>,
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub playing: Option<String>,
    #[serde(default)] pub count: Option<HashMap<String, i64>>,
    #[serde(default)] pub streaming: Option<bool>,
    #[serde(default)] pub streamer: Option<HashMap<String, JsonValue>>,
    #[serde(default)] pub followable: Option<bool>,
    #[serde(default)] pub following: Option<bool>,
    #[serde(default)] pub blocking: Option<bool>,
    #[serde(default)] pub follows_you: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTestType {
    #[serde(default)] pub scopes: Option<String>,
    #[serde(default)] pub user_id: Option<String>,
    #[serde(default)] pub expires: Option<i64>,
}

pub type TokenTests = HashMap<String, Option<TokenTestType>>;

/// Response shape from chessdb / lichess cloud / lichess explorer / lichess egtb.
/// The Python equivalent is a single `OnlineType` `TypedDict` with all fields
/// optional — so we keep one merged struct here too.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineType {
    // common
    #[serde(default)] pub moves: Option<Vec<OnlineMoveType>>,

    // lichess egtb
    #[serde(default)] pub checkmate: Option<bool>,
    #[serde(default)] pub stalemate: Option<bool>,
    #[serde(default)] pub variant_win: Option<bool>,
    #[serde(default)] pub variant_loss: Option<bool>,
    #[serde(default)] pub insufficient_material: Option<bool>,
    #[serde(default)] pub dtz: Option<i64>,
    #[serde(default)] pub precise_dtz: Option<i64>,
    #[serde(default)] pub dtm: Option<i64>,
    #[serde(default)] pub dtc: Option<i64>,
    #[serde(default)] pub category: Option<String>,

    // lichess explorer
    #[serde(default)] pub white: Option<i64>,
    #[serde(default)] pub black: Option<i64>,
    #[serde(default)] pub draws: Option<i64>,
    #[serde(default)] pub top_games: Option<Vec<JsonValue>>,
    #[serde(default)] pub recent_games: Option<Vec<JsonValue>>,
    #[serde(default)] pub opening: Option<HashMap<String, String>>,
    #[serde(default)] pub queue_position: Option<i64>,

    // lichess cloud analysis
    #[serde(default)] pub fen: Option<String>,
    #[serde(default)] pub knodes: Option<i64>,
    #[serde(default)] pub ply: Option<JsonValue>,
    #[serde(default)] pub depth: Option<i64>,
    #[serde(default)] pub pvs: Option<Vec<LichessPvType>>,
    #[serde(default)] pub error: Option<String>,

    // chessdb
    #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub score: Option<i64>,
    #[serde(default)] pub pv: Option<Vec<String>>,
    #[serde(default, rename = "pvSAN")] pub pv_san: Option<Vec<String>>,
    #[serde(default)] pub r#move: Option<String>,
    #[serde(default)] pub egtb: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineMoveType {
    // chessdb
    #[serde(default)] pub uci: Option<String>,
    #[serde(default)] pub san: Option<String>,
    #[serde(default)] pub score: Option<i64>,
    #[serde(default)] pub rank: Option<i64>,
    #[serde(default)] pub note: Option<String>,
    #[serde(default)] pub winrate: Option<String>,

    // lichess explorer
    #[serde(default)] pub average_rating: Option<i64>,
    #[serde(default)] pub performance: Option<i64>,
    #[serde(default)] pub white: Option<i64>,
    #[serde(default)] pub black: Option<i64>,
    #[serde(default)] pub draws: Option<i64>,
    #[serde(default)] pub game: Option<JsonValue>,

    // lichess egtb
    #[serde(default)] pub zeroing: Option<bool>,
    #[serde(default)] pub checkmate: Option<bool>,
    #[serde(default)] pub stalemate: Option<bool>,
    #[serde(default)] pub variant_win: Option<bool>,
    #[serde(default)] pub variant_loss: Option<bool>,
    #[serde(default)] pub insufficient_material: Option<bool>,
    #[serde(default)] pub dtz: Option<i64>,
    #[serde(default)] pub precise_dtz: Option<i64>,
    #[serde(default)] pub dtm: Option<i64>,
    #[serde(default)] pub dtc: Option<i64>,
    #[serde(default)] pub category: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LichessPvType {
    #[serde(default)] pub moves: Option<String>,
    #[serde(default)] pub cp: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_type_round_trips() {
        for variant in FilterType::all() {
            let s = serde_yaml_ng::to_string(&variant).unwrap();
            let back: FilterType = serde_yaml_ng::from_str(&s).unwrap();
            assert_eq!(*variant, back);
        }
    }

    #[test]
    fn challenge_decodes_unknown_keys() {
        let json = r#"{"id":"abc","color":"random","brand-new-field":42}"#;
        let challenge: ChallengeType = serde_json::from_str(json).unwrap();
        assert_eq!(challenge.id.as_deref(), Some("abc"));
        assert_eq!(challenge.color.as_deref(), Some("random"));
    }

    #[test]
    fn profile_camel_case_mapping() {
        let json = r#"{"username":"bot1","createdAt":1234,"playTime":{"total":99}}"#;
        let p: UserProfileType = serde_json::from_str(json).unwrap();
        assert_eq!(p.username.as_deref(), Some("bot1"));
        assert_eq!(p.created_at, Some(1234));
        assert_eq!(p.play_time.unwrap().get("total"), Some(&99));
    }
}
