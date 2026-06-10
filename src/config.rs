//! Configuration loading & validation. Mirrors `lib/config.py`.
//!
//! The Python version walks a free-form `dict` and fills defaults via
//! `set_config_default()`. We use strongly-typed `serde` structs with
//! `#[serde(default)]` plus a post-load `normalize()` step for the few cases
//! where the Python code rewrites values in place (lists, infinity sentinels).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::lichess_types::{FilterType, JsonValue};
use crate::timer::{days, minutes};

/// Sentinel for `math.inf` in Python configs (see `config_assert(... math.inf)`
/// in `validate_config`). Stored as `None` in `Option<i64>` fields.
pub const INFINITE: Option<i64> = None;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub token: String,
    pub url: String,

    pub engine: EngineConfig,

    #[serde(default = "default_abort_time")]
    pub abort_time: u64,
    #[serde(default)]
    pub fake_think_time: bool,
    #[serde(default)]
    pub rate_limiting_delay: u64,
    #[serde(default = "default_move_overhead")]
    pub move_overhead: u64,
    #[serde(default)]
    pub max_takebacks_accepted: u64,
    #[serde(default)]
    pub quit_after_all_games_finish: bool,

    #[serde(default)]
    pub pgn_directory: Option<String>,
    #[serde(default = "default_pgn_grouping")]
    pub pgn_file_grouping: String,

    /// Optional source-code URL reported by the `!source` chat command.
    /// When unset, `!source` replies with a placeholder.
    #[serde(default)]
    pub source_url: Option<String>,

    #[serde(default)]
    pub correspondence: CorrespondenceConfig,

    pub challenge: ChallengeConfig,

    #[serde(default)]
    pub greeting: GreetingConfig,

    #[serde(default)]
    pub matchmaking: MatchmakingConfig,
}

fn default_abort_time() -> u64 { 20 }
fn default_move_overhead() -> u64 { 1000 }
fn default_pgn_grouping() -> String { "game".into() }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub name: String,

    #[serde(default)]
    pub working_dir: String,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub ponder: bool,
    #[serde(default)]
    pub uci_ponder: bool,
    #[serde(default)]
    pub debug: bool,
    #[serde(default)]
    pub silence_stderr: bool,

    /// Use the in-process embedded clrsrc engine instead of spawning it as a
    /// UCI subprocess (clrsrc's EMBEDDED.md B1). Only honoured in a
    /// build with `--features embedded`, and only for standard-chess games
    /// (clrsrc's FEN parser has no Chess960 castling). Defaults to the
    /// subprocess backend; the engine binary/options under `uci_options` are
    /// reused to initialise the embedded engine (Hash, EvalFile, SyzygyPath…).
    #[serde(default)]
    pub embedded: bool,

    #[serde(default)]
    pub interpreter: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub interpreter_options: Vec<String>,

    #[serde(default)]
    pub engine_options: Option<HashMap<String, JsonValue>>,
    #[serde(default)]
    pub homemade_options: Option<HashMap<String, JsonValue>>,
    #[serde(default)]
    pub uci_options: HashMap<String, JsonValue>,
    #[serde(default)]
    pub xboard_options: HashMap<String, JsonValue>,

    #[serde(default)]
    pub polyglot: PolyglotConfig,
    #[serde(default)]
    pub draw_or_resign: DrawOrResignConfig,
    #[serde(default)]
    pub online_moves: OnlineMovesConfig,
    #[serde(default)]
    pub lichess_bot_tbs: LichessBotTbsConfig,
    #[serde(default)]
    pub experience: ExperienceConfig,
}

fn default_protocol() -> String { "uci".into() }

/// WDL-harvest into the engine's JBK2 experience overlay. After each
/// finished game the bot appends the outcome of its own opening moves to
/// `overlay_path`, which clrsrc consolidates offline via `expmerge`.
/// Disabled by default — only meaningful with a JBK2-aware engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperienceConfig {
    #[serde(default)]
    pub harvest_enabled: bool,
    /// Append target, e.g. `P:/Projekte/clrsrc/clrsrc.exp.overlay`.
    #[serde(default)]
    pub overlay_path: String,
    /// How many of the bot's own moves (from move 1) to harvest per game.
    #[serde(default = "experience_harvest_depth")]
    pub harvest_depth: usize,
}

fn experience_harvest_depth() -> usize { 16 }

impl Default for ExperienceConfig {
    fn default() -> Self {
        Self {
            harvest_enabled: false,
            overlay_path: String::new(),
            harvest_depth: experience_harvest_depth(),
        }
    }
}

/// Helper: a YAML value that's either a single string or a list of strings.
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Unexpected};
    let value = JsonValue::deserialize(deserializer)?;
    match value {
        JsonValue::Null => Ok(Vec::new()),
        JsonValue::String(s) => Ok(vec![s]),
        JsonValue::Array(arr) => arr
            .into_iter()
            .map(|v| match v {
                JsonValue::String(s) => Ok(s),
                JsonValue::Number(n) => Ok(n.to_string()),
                JsonValue::Bool(b) => Ok(b.to_string()),
                other => Err(D::Error::invalid_type(
                    Unexpected::Other(&format!("{other:?}")),
                    &"string",
                )),
            })
            .collect(),
        other => Err(D::Error::invalid_type(
            Unexpected::Other(&format!("{other:?}")),
            &"string or list of strings",
        )),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolyglotConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub book: HashMap<String, Vec<String>>,
    #[serde(default = "polyglot_min_weight")]
    pub min_weight: i64,
    #[serde(default = "polyglot_selection")]
    pub selection: String,
    #[serde(default = "polyglot_max_depth")]
    pub max_depth: u32,
    #[serde(default = "polyglot_normalization")]
    pub normalization: String,
}

fn polyglot_min_weight() -> i64 { 1 }
fn polyglot_selection() -> String { "weighted_random".into() }
fn polyglot_max_depth() -> u32 { 8 }
fn polyglot_normalization() -> String { "none".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrawOrResignConfig {
    #[serde(default)]
    pub resign_enabled: bool,
    #[serde(default = "dor_resign_score")]
    pub resign_score: i64,
    #[serde(default = "default_true")]
    pub resign_for_egtb_minus_two: bool,
    #[serde(default = "dor_resign_moves")]
    pub resign_moves: u32,

    #[serde(default)]
    pub offer_draw_enabled: bool,
    #[serde(default)]
    pub offer_draw_score: i64,
    #[serde(default = "default_true")]
    pub offer_draw_for_egtb_zero: bool,
    #[serde(default = "dor_offer_draw_moves")]
    pub offer_draw_moves: u32,
    #[serde(default = "dor_offer_draw_pieces")]
    pub offer_draw_pieces: u32,
}

impl Default for DrawOrResignConfig {
    fn default() -> Self {
        Self {
            resign_enabled: false,
            resign_score: dor_resign_score(),
            resign_for_egtb_minus_two: true,
            resign_moves: dor_resign_moves(),
            offer_draw_enabled: false,
            offer_draw_score: 0,
            offer_draw_for_egtb_zero: true,
            offer_draw_moves: dor_offer_draw_moves(),
            offer_draw_pieces: dor_offer_draw_pieces(),
        }
    }
}

fn default_true() -> bool { true }
fn dor_resign_score() -> i64 { -1000 }
fn dor_resign_moves() -> u32 { 3 }
fn dor_offer_draw_moves() -> u32 { 5 }
fn dor_offer_draw_pieces() -> u32 { 10 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineMovesConfig {
    #[serde(default = "om_max_out_of_book_moves")]
    pub max_out_of_book_moves: u32,
    #[serde(default = "om_max_retries")]
    pub max_retries: u32,
    /// `None` = unlimited (Python uses `math.inf`).
    #[serde(default, deserialize_with = "deserialize_optional_int_or_inf")]
    pub max_depth: Option<u32>,

    #[serde(default)]
    pub chessdb_book: ChessdbBookConfig,
    #[serde(default)]
    pub lichess_cloud_analysis: LichessCloudAnalysisConfig,
    #[serde(default)]
    pub lichess_opening_explorer: LichessOpeningExplorerConfig,
    #[serde(default)]
    pub online_egtb: OnlineEgtbConfig,
}

impl Default for OnlineMovesConfig {
    fn default() -> Self {
        Self {
            max_out_of_book_moves: om_max_out_of_book_moves(),
            max_retries: om_max_retries(),
            max_depth: None,
            chessdb_book: ChessdbBookConfig::default(),
            lichess_cloud_analysis: LichessCloudAnalysisConfig::default(),
            lichess_opening_explorer: LichessOpeningExplorerConfig::default(),
            online_egtb: OnlineEgtbConfig::default(),
        }
    }
}

fn om_max_out_of_book_moves() -> u32 { 10 }
fn om_max_retries() -> u32 { 2 }

/// Accept either an integer or `.inf`/`null`/missing.
fn deserialize_optional_int_or_inf<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = JsonValue::deserialize(deserializer)?;
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i < 0 { return Err(D::Error::custom("max_depth must be non-negative")); }
                Ok(Some(i as u32))
            } else if n.as_f64().map_or(false, |f| f.is_infinite()) {
                Ok(None)
            } else {
                Err(D::Error::custom("expected integer or .inf"))
            }
        }
        JsonValue::String(s) if s.trim().eq_ignore_ascii_case(".inf") => Ok(None),
        _ => Err(D::Error::custom("expected integer, .inf, or null")),
    }
}

/// Accept either integer / `.inf`. Used for `max_base`, `max_days`.
fn deserialize_int_or_inf<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = JsonValue::deserialize(deserializer)?;
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Some(i))
            } else if n.as_f64().map_or(false, |f| f.is_infinite()) {
                Ok(None)
            } else {
                Err(D::Error::custom("expected integer or .inf"))
            }
        }
        JsonValue::String(s) if s.trim().eq_ignore_ascii_case(".inf") => Ok(None),
        _ => Err(D::Error::custom("expected integer, .inf, or null")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChessdbBookConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "min_time_default")]
    pub min_time: u32,
    #[serde(default = "max_time_default")]
    pub max_time: u32,
    #[serde(default = "chessdb_quality")]
    pub move_quality: String,
    #[serde(default = "chessdb_min_depth")]
    pub min_depth: u32,
}

impl Default for ChessdbBookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_time: min_time_default(),
            max_time: max_time_default(),
            move_quality: chessdb_quality(),
            min_depth: chessdb_min_depth(),
        }
    }
}

fn min_time_default() -> u32 { 20 }
fn max_time_default() -> u32 { 10_800 }
fn chessdb_quality() -> String { "good".into() }
fn chessdb_min_depth() -> u32 { 20 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LichessCloudAnalysisConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "min_time_default")] pub min_time: u32,
    #[serde(default = "max_time_default")] pub max_time: u32,
    #[serde(default = "lc_quality")] pub move_quality: String,
    #[serde(default = "lc_min_depth")] pub min_depth: u32,
    #[serde(default)] pub min_knodes: u64,
    #[serde(default = "lc_max_score_diff")] pub max_score_difference: i64,
}

impl Default for LichessCloudAnalysisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_time: min_time_default(),
            max_time: max_time_default(),
            move_quality: lc_quality(),
            min_depth: lc_min_depth(),
            min_knodes: 0,
            max_score_difference: lc_max_score_diff(),
        }
    }
}

fn lc_quality() -> String { "best".into() }
fn lc_min_depth() -> u32 { 20 }
fn lc_max_score_diff() -> i64 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LichessOpeningExplorerConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "min_time_default")] pub min_time: u32,
    #[serde(default = "max_time_default")] pub max_time: u32,
    #[serde(default = "loe_source")] pub source: String,
    #[serde(default)] pub player_name: String,
    #[serde(default = "loe_sort")] pub sort: String,
    #[serde(default = "loe_min_games")] pub min_games: u32,
}

impl Default for LichessOpeningExplorerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_time: min_time_default(),
            max_time: max_time_default(),
            source: loe_source(),
            player_name: String::new(),
            sort: loe_sort(),
            min_games: loe_min_games(),
        }
    }
}

fn loe_source() -> String { "masters".into() }
fn loe_sort() -> String { "winrate".into() }
fn loe_min_games() -> u32 { 10 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineEgtbConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default = "min_time_default")] pub min_time: u32,
    #[serde(default = "max_time_default")] pub max_time: u32,
    #[serde(default = "egtb_max_pieces")] pub max_pieces: u32,
    #[serde(default = "egtb_source")] pub source: String,
    #[serde(default = "egtb_quality")] pub move_quality: String,
}

impl Default for OnlineEgtbConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_time: min_time_default(),
            max_time: max_time_default(),
            max_pieces: egtb_max_pieces(),
            source: egtb_source(),
            move_quality: egtb_quality(),
        }
    }
}

fn egtb_max_pieces() -> u32 { 7 }
fn egtb_source() -> String { "lichess".into() }
fn egtb_quality() -> String { "best".into() }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LichessBotTbsConfig {
    #[serde(default)]
    pub syzygy: SyzygyConfig,
    #[serde(default)]
    pub gaviota: GaviotaConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyzygyConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub paths: Vec<String>,
    #[serde(default = "syzygy_max_pieces")] pub max_pieces: u32,
    #[serde(default = "egtb_quality")] pub move_quality: String,
}

impl Default for SyzygyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            paths: Vec::new(),
            max_pieces: syzygy_max_pieces(),
            move_quality: egtb_quality(),
        }
    }
}

fn syzygy_max_pieces() -> u32 { 7 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaviotaConfig {
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub paths: Vec<String>,
    #[serde(default = "gaviota_max_pieces")] pub max_pieces: u32,
    #[serde(default = "gaviota_min_dtm_wdl1")] pub min_dtm_to_consider_as_wdl_1: u32,
    #[serde(default = "egtb_quality")] pub move_quality: String,
}

impl Default for GaviotaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            paths: Vec::new(),
            max_pieces: gaviota_max_pieces(),
            min_dtm_to_consider_as_wdl_1: gaviota_min_dtm_wdl1(),
            move_quality: egtb_quality(),
        }
    }
}

fn gaviota_max_pieces() -> u32 { 5 }
fn gaviota_min_dtm_wdl1() -> u32 { 120 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeConfig {
    #[serde(default = "ch_concurrency")] pub concurrency: u32,
    #[serde(default = "ch_sort_by")] pub sort_by: String,
    #[serde(default = "ch_preference")] pub preference: String,
    #[serde(default)] pub accept_bot: bool,
    #[serde(default)] pub only_bot: bool,
    #[serde(default = "ch_max_increment")] pub max_increment: u32,
    #[serde(default)] pub min_increment: u32,
    #[serde(default, deserialize_with = "deserialize_int_or_inf")]
    pub max_base: Option<i64>,
    #[serde(default)] pub min_base: u32,
    #[serde(default, deserialize_with = "deserialize_int_or_inf")]
    pub max_days: Option<i64>,
    #[serde(default = "ch_min_days")] pub min_days: u32,
    #[serde(default)] pub variants: Vec<String>,
    #[serde(default)] pub time_controls: Vec<String>,
    #[serde(default)] pub modes: Vec<String>,
    #[serde(default)] pub block_list: Vec<String>,
    #[serde(default)] pub online_block_list: Vec<String>,
    #[serde(default)] pub allow_list: Vec<String>,
    #[serde(default)] pub recent_bot_challenge_age: Option<u32>,
    #[serde(default)] pub max_recent_bot_challenges: Option<u32>,
    #[serde(default)] pub min_rating: i64,
    #[serde(default = "ch_max_rating")] pub max_rating: i64,
    #[serde(default)] pub rating_difference: Option<i64>,
    #[serde(default)] pub bullet_requires_increment: bool,
    #[serde(default = "ch_max_simultaneous")] pub max_simultaneous_games_per_user: u32,
}

fn ch_concurrency() -> u32 { 1 }
fn ch_sort_by() -> String { "best".into() }
fn ch_preference() -> String { "none".into() }
fn ch_max_increment() -> u32 { 180 }
fn ch_min_days() -> u32 { 1 }
fn ch_max_rating() -> i64 { 4000 }
fn ch_max_simultaneous() -> u32 { 5 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrespondenceConfig {
    #[serde(default = "corr_move_time")] pub move_time: u64,
    #[serde(default = "corr_checkin_period")] pub checkin_period: u64,
    #[serde(default = "corr_disconnect_time")] pub disconnect_time: u64,
    #[serde(default)] pub ponder: bool,
    #[serde(default)] pub uci_ponder: bool,
}

impl Default for CorrespondenceConfig {
    fn default() -> Self {
        Self {
            move_time: corr_move_time(),
            checkin_period: corr_checkin_period(),
            disconnect_time: corr_disconnect_time(),
            ponder: false,
            uci_ponder: false,
        }
    }
}

fn corr_move_time() -> u64 { 60 }
fn corr_checkin_period() -> u64 { 600 }
fn corr_disconnect_time() -> u64 { 300 }

impl CorrespondenceConfig {
    pub fn checkin_period_duration(&self) -> Duration { Duration::from_secs(self.checkin_period) }
    pub fn disconnect_duration(&self) -> Duration { Duration::from_secs(self.disconnect_time) }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GreetingConfig {
    #[serde(default)] pub hello: String,
    #[serde(default)] pub goodbye: String,
    #[serde(default)] pub hello_spectators: String,
    #[serde(default)] pub goodbye_spectators: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchmakingConfig {
    #[serde(default)] pub allow_matchmaking: bool,
    #[serde(default)] pub allow_during_games: bool,
    #[serde(default = "mm_variant")] pub challenge_variant: String,
    #[serde(default = "mm_timeout")] pub challenge_timeout: u64,
    #[serde(default, deserialize_with = "deserialize_optional_int_list")]
    pub challenge_initial_time: Vec<Option<i64>>,
    #[serde(default, deserialize_with = "deserialize_optional_int_list")]
    pub challenge_increment: Vec<Option<i64>>,
    #[serde(default, deserialize_with = "deserialize_optional_int_list")]
    pub challenge_days: Vec<Option<i64>>,
    #[serde(default = "mm_opp_min_rating")] pub opponent_min_rating: i64,
    #[serde(default = "mm_opp_max_rating")] pub opponent_max_rating: i64,
    #[serde(default)] pub opponent_rating_difference: Option<i64>,
    #[serde(default = "mm_rating_pref")] pub rating_preference: String,
    #[serde(default = "mm_mode")] pub challenge_mode: String,
    #[serde(default)] pub challenge_filter: FilterType,
    /// Path to the persistent opponent database (JSON). Tracks who we have
    /// challenged, in which form, whether they played, and a permanent block
    /// for bots that decline with `noBot` / `onlyBot`. Empty string disables
    /// persistence (in-memory only). Relative paths resolve against the
    /// bot's working directory.
    #[serde(default = "mm_opponent_db")] pub opponent_db_path: String,
    /// Diversity brake: the maximum number of challenges we *initiate* against
    /// the same opponent per local calendar day. Once reached, that bot is
    /// skipped in matchmaking until the next day — we would rather idle than
    /// farm a single opponent. `0` disables the cap (unlimited).
    #[serde(default = "mm_max_per_opp")] pub max_challenges_per_opponent_per_day: u32,
    #[serde(default)] pub block_list: Vec<String>,
    #[serde(default)] pub online_block_list: Vec<String>,
    #[serde(default)] pub include_challenge_block_list: bool,
    #[serde(default)] pub overrides: HashMap<String, MatchmakingOverride>,
}

impl Default for MatchmakingConfig {
    fn default() -> Self {
        Self {
            allow_matchmaking: false,
            allow_during_games: false,
            challenge_variant: mm_variant(),
            challenge_timeout: mm_timeout(),
            challenge_initial_time: vec![None],
            challenge_increment: vec![None],
            challenge_days: vec![None],
            opponent_min_rating: mm_opp_min_rating(),
            opponent_max_rating: mm_opp_max_rating(),
            opponent_rating_difference: None,
            rating_preference: mm_rating_pref(),
            challenge_mode: mm_mode(),
            challenge_filter: FilterType::None,
            opponent_db_path: mm_opponent_db(),
            max_challenges_per_opponent_per_day: mm_max_per_opp(),
            block_list: Vec::new(),
            online_block_list: Vec::new(),
            include_challenge_block_list: false,
            overrides: HashMap::new(),
        }
    }
}

fn mm_variant() -> String { "random".into() }
fn mm_timeout() -> u64 { 30 }
fn mm_opp_min_rating() -> i64 { 600 }
fn mm_opp_max_rating() -> i64 { 4000 }
fn mm_rating_pref() -> String { "none".into() }
fn mm_mode() -> String { "random".into() }
fn mm_opponent_db() -> String { "matchmaking_opponents.json".into() }
fn mm_max_per_opp() -> u32 { 5 }

/// Accept missing / null / scalar / list — Python's `change_value_to_list`.
fn deserialize_optional_int_list<'de, D>(deserializer: D) -> Result<Vec<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = JsonValue::deserialize(deserializer)?;
    let extract = |v: &JsonValue| -> Result<Option<i64>, D::Error> {
        match v {
            JsonValue::Null => Ok(None),
            JsonValue::Number(n) => n
                .as_i64()
                .map(Some)
                .ok_or_else(|| D::Error::custom("expected integer")),
            other => Err(D::Error::custom(format!("expected int or null, got {other:?}"))),
        }
    };
    match value {
        JsonValue::Null => Ok(vec![None]),
        JsonValue::Array(arr) if arr.is_empty() => Ok(vec![None]),
        JsonValue::Array(arr) => arr.iter().map(extract).collect(),
        scalar => Ok(vec![extract(&scalar)?]),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatchmakingOverride {
    #[serde(default)] pub challenge_variant: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_int_list")]
    pub challenge_initial_time: Option<Vec<Option<i64>>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_int_list")]
    pub challenge_increment: Option<Vec<Option<i64>>>,
    #[serde(default, deserialize_with = "deserialize_optional_optional_int_list")]
    pub challenge_days: Option<Vec<Option<i64>>>,
    #[serde(default)] pub opponent_min_rating: Option<i64>,
    #[serde(default)] pub opponent_max_rating: Option<i64>,
    #[serde(default)] pub opponent_rating_difference: Option<JsonValue>,
    #[serde(default)] pub challenge_mode: Option<String>,
    #[serde(default)] pub rating_preference: Option<String>,
}

fn deserialize_optional_optional_int_list<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<Option<i64>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<JsonValue>::deserialize(deserializer)?;
    match opt {
        None | Some(JsonValue::Null) => Ok(None),
        Some(value) => {
            // Re-use the non-optional list deserializer logic.
            let de = serde_json::value::Value::deserialize(serde::de::IntoDeserializer::into_deserializer(value))
                .map_err(serde::de::Error::custom)?;
            // delegate to the helper:
            let helper = match de {
                JsonValue::Null => vec![None],
                JsonValue::Array(arr) if arr.is_empty() => vec![None],
                JsonValue::Array(arr) => arr
                    .into_iter()
                    .map(|v| match v {
                        JsonValue::Null => Ok(None),
                        JsonValue::Number(n) => n
                            .as_i64()
                            .map(Some)
                            .ok_or_else(|| serde::de::Error::custom("expected integer")),
                        other => Err(serde::de::Error::custom(format!(
                            "expected int or null, got {other:?}"
                        ))),
                    })
                    .collect::<Result<Vec<_>, D::Error>>()?,
                JsonValue::Number(n) => vec![n.as_i64()],
                _ => return Err(serde::de::Error::custom("expected list, scalar, or null")),
            };
            Ok(Some(helper))
        }
    }
}

// ---------------------------------------------------------------------------
// Loading & validation
// ---------------------------------------------------------------------------

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("could not read config file `{}`", path.display()))?;
        let mut cfg: Config = serde_yaml_ng::from_str(&raw)
            .with_context(|| "syntax problem in config.yml")?;
        cfg.normalize();
        if let Ok(token) = std::env::var("LICHESS_BOT_TOKEN") {
            cfg.token = token;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply Python's `set_config_default` post-processing for the few cases
    /// where defaults depend on *other* config values or runtime state.
    pub fn normalize(&mut self) {
        // engine.working_dir defaults to cwd (only when blank)
        if self.engine.working_dir.trim().is_empty() {
            if let Ok(cwd) = std::env::current_dir() {
                self.engine.working_dir = cwd.display().to_string();
            }
        }

        // matchmaking.challenge_timeout has a min of 1 minute.
        if self.matchmaking.challenge_timeout < 1 {
            self.matchmaking.challenge_timeout = 1;
        }

        // include_challenge_block_list copies challenge.block_list into
        // matchmaking.block_list at startup.
        if self.matchmaking.include_challenge_block_list {
            self.matchmaking
                .block_list
                .extend(self.challenge.block_list.iter().cloned());
        }

        // empty list inputs become `[None]` in Python — handled by the
        // deserializer for the top-level lists already; do the same for overrides.
        for override_cfg in self.matchmaking.overrides.values_mut() {
            if let Some(list) = override_cfg.challenge_initial_time.as_mut() {
                if list.is_empty() { *list = vec![None]; }
            }
            if let Some(list) = override_cfg.challenge_increment.as_mut() {
                if list.is_empty() { *list = vec![None]; }
            }
            if let Some(list) = override_cfg.challenge_days.as_mut() {
                if list.is_empty() { *list = vec![None]; }
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        // engine dir / binary
        let engine_dir = PathBuf::from(&self.engine.dir);
        if !engine_dir.is_dir() {
            bail!(
                "Your engine directory `{}` is not a directory.",
                self.engine.dir
            );
        }
        let working_dir = &self.engine.working_dir;
        if !working_dir.is_empty() && !PathBuf::from(working_dir).is_dir() {
            bail!("Your engine's working directory `{working_dir}` is not a directory.");
        }
        let engine_bin = engine_dir.join(&self.engine.name);
        let homemade = self.engine.protocol == "homemade";
        if !homemade {
            if !engine_bin.is_file() {
                bail!("The engine {} file does not exist.", engine_bin.display());
            }
            // `os.access(.., X_OK)` only exists on POSIX; on Windows we trust
            // .exe extension. Best-effort: skip the executable bit check.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::metadata(&engine_bin)?;
                if meta.permissions().mode() & 0o111 == 0 {
                    bail!(
                        "The engine {} doesn't have execute (x) permission.",
                        engine_bin.display()
                    );
                }
            }
        }

        if self.engine.protocol == "xboard" {
            for (section, sub) in [
                ("online_moves", "online_egtb"),
                ("lichess_bot_tbs", "syzygy"),
                ("lichess_bot_tbs", "gaviota"),
            ] {
                let (enabled, quality) = match (section, sub) {
                    ("online_moves", "online_egtb") => (
                        self.engine.online_moves.online_egtb.enabled,
                        self.engine.online_moves.online_egtb.move_quality.as_str(),
                    ),
                    ("lichess_bot_tbs", "syzygy") => (
                        self.engine.lichess_bot_tbs.syzygy.enabled,
                        self.engine.lichess_bot_tbs.syzygy.move_quality.as_str(),
                    ),
                    ("lichess_bot_tbs", "gaviota") => (
                        self.engine.lichess_bot_tbs.gaviota.enabled,
                        self.engine.lichess_bot_tbs.gaviota.move_quality.as_str(),
                    ),
                    _ => unreachable!(),
                };
                if enabled && quality == "suggest" {
                    bail!("XBoard engines can't be used with `move_quality` set to `suggest` in {sub}.");
                }
            }
        }

        if self.challenge.concurrency == 0 {
            warn!("With challenge.concurrency set to 0, the bot won't accept or create any challenges.");
        }

        if !["best", "first"].contains(&self.challenge.sort_by.as_str()) {
            bail!("challenge.sort_by can be either `first` or `best`.");
        }
        if !["none", "human", "bot"].contains(&self.challenge.preference.as_str()) {
            bail!("challenge.preference should be `none`, `human`, or `bot`.");
        }

        if self.challenge.min_increment > self.challenge.max_increment {
            warn!("challenge.max_increment < challenge.min_increment will result in no real-time challenges being accepted.");
        }
        if let Some(max_base) = self.challenge.max_base {
            if (self.challenge.min_base as i64) > max_base {
                warn!("challenge.max_base < challenge.min_base will result in no real-time challenges being accepted.");
            }
        }
        if let Some(max_days) = self.challenge.max_days {
            if (self.challenge.min_days as i64) > max_days {
                warn!("challenge.max_days < challenge.min_days will result in no correspondence challenges being accepted.");
            }
        }

        if self.challenge.min_rating > self.challenge.max_rating {
            warn!("challenge.max_rating < challenge.min_rating will result in no challenges being accepted.");
        }
        if let Some(diff) = self.challenge.rating_difference {
            if diff < 0 {
                warn!("challenge.rating_difference < 0 will result in no challenges being accepted.");
            }
        }

        if self.matchmaking.allow_matchmaking {
            if self.matchmaking.opponent_min_rating > self.matchmaking.opponent_max_rating {
                warn!("matchmaking.opponent_max_rating < matchmaking.opponent_min_rating will result in no challenges being created.");
            }
            if self.matchmaking.opponent_rating_difference.unwrap_or(0) < 0 {
                warn!("matchmaking.opponent_rating_difference < 0 will result in no challenges being created.");
            }
            let max_games_per_day: u32 = 100;
            let game_timeout = minutes(self.matchmaking.challenge_timeout as f64);
            // saturating: an absurdly large challenge_timeout must not panic on
            // the Duration multiply — it just means "won't exhaust the quota".
            let exhausts_quota = game_timeout
                .checked_mul(max_games_per_day)
                .is_some_and(|total| total < days(1.0));
            if exhausts_quota {
                warn!(
                    "A bot is only allowed to play {max_games_per_day} games per day against other bots. \
                     Please check your config file to make sure your bot won't use up all its allotted games quickly."
                );
            }
        }

        let valid_pgn = ["game", "opponent", "all"];
        if !valid_pgn.contains(&self.pgn_file_grouping.as_str()) {
            bail!(
                "The `pgn_file_grouping` choice of `{}` is not valid. Please choose from {valid_pgn:?}.",
                self.pgn_file_grouping
            );
        }

        if let Some(pgn_dir) = &self.pgn_directory {
            if std::env::var("LICHESS_BOT_DOCKER").is_ok() {
                warn!(
                    "Games will be saved to '{pgn_dir}', please ensure this folder is in a mounted volume; \
                     the Docker container's internal file system will prevent you accessing the saved files."
                );
            }
        }

        if self.matchmaking.allow_matchmaking {
            let has_valid = |list: &[Option<i64>]| !list.is_empty() && list[0].is_some();
            let ok = (has_valid(&self.matchmaking.challenge_initial_time)
                && has_valid(&self.matchmaking.challenge_increment))
                || has_valid(&self.matchmaking.challenge_days);
            if !ok {
                bail!(
                    "The time control to challenge other bots is not set. Either lists of \
                     challenge_initial_time and challenge_increment are required, or a list of \
                     challenge_days, or both."
                );
            }
        }

        if !["none", "high", "low"].contains(&self.matchmaking.rating_preference.as_str()) {
            bail!(
                "{} is not a valid `matchmaking:rating_preference` option. Valid options are 'none', 'high', or 'low'.",
                self.matchmaking.rating_preference
            );
        }

        // selection / move_quality validation
        let polyglot_choices = ["weighted_random", "uniform_random", "best_move"];
        if !polyglot_choices.contains(&self.engine.polyglot.selection.as_str()) {
            bail!(
                "`{}` is not a valid `engine:polyglot:selection` value. Please choose from {polyglot_choices:?}.",
                self.engine.polyglot.selection
            );
        }
        let chessdb_choices = ["all", "good", "best"];
        if !chessdb_choices.contains(&self.engine.online_moves.chessdb_book.move_quality.as_str()) {
            bail!(
                "`{}` is not a valid `engine:online_moves:chessdb_book:move_quality` value. Please choose from {chessdb_choices:?}.",
                self.engine.online_moves.chessdb_book.move_quality
            );
        }
        let cloud_choices = ["good", "best"];
        if !cloud_choices.contains(&self.engine.online_moves.lichess_cloud_analysis.move_quality.as_str()) {
            bail!(
                "`{}` is not a valid `engine:online_moves:lichess_cloud_analysis:move_quality` value. Please choose from {cloud_choices:?}.",
                self.engine.online_moves.lichess_cloud_analysis.move_quality
            );
        }
        let online_egtb_choices = ["best", "suggest"];
        if !online_egtb_choices.contains(&self.engine.online_moves.online_egtb.move_quality.as_str()) {
            bail!(
                "`{}` is not a valid `engine:online_moves:online_egtb:move_quality` value. Please choose from {online_egtb_choices:?}.",
                self.engine.online_moves.online_egtb.move_quality
            );
        }
        if !["none", "max", "sum"].contains(&self.engine.polyglot.normalization.as_str()) {
            bail!(
                "`{}` is not a valid choice for `engine:polyglot:normalization`. Please choose from ['none', 'max', 'sum'].",
                self.engine.polyglot.normalization
            );
        }
        for (tb, quality) in [
            ("syzygy", self.engine.lichess_bot_tbs.syzygy.move_quality.as_str()),
            ("gaviota", self.engine.lichess_bot_tbs.gaviota.move_quality.as_str()),
        ] {
            if !["best", "suggest"].contains(&quality) {
                bail!(
                    "`{quality}` is not a valid choice for `engine:lichess_bot_tbs:{tb}:move_quality`. \
                     Please choose from [\"best\", \"suggest\"]."
                );
            }
        }

        let explorer_sources = ["lichess", "masters", "player"];
        let explorer_sorts = ["winrate", "games_played"];
        let exp = &self.engine.online_moves.lichess_opening_explorer;
        if exp.enabled {
            if !explorer_sources.contains(&exp.source.as_str()) {
                bail!(
                    "`{}` is not a valid `engine:online_moves:lichess_opening_explorer:source` value. Please choose from {explorer_sources:?}.",
                    exp.source
                );
            }
            if !explorer_sorts.contains(&exp.sort.as_str()) {
                bail!(
                    "`{}` is not a valid `engine:online_moves:lichess_opening_explorer:sort` value. Please choose from {explorer_sorts:?}.",
                    exp.sort
                );
            }
        }

        Ok(())
    }
}

/// Convenience: emit the YAML representation of the config with `token` redacted.
pub fn redacted_yaml(cfg: &Config) -> Result<String> {
    let mut clone = cfg.clone();
    clone.token = "logger".into();
    serde_yaml_ng::to_string(&clone).map_err(|e| anyhow!("could not serialize config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
token: "abc"
url: "https://lichess.org/"
engine:
  dir: "."
  name: "engine_name"
  protocol: "homemade"
challenge:
  variants: ["standard"]
  time_controls: ["bullet", "blitz"]
  modes: ["casual"]
"#
    }

    #[test]
    fn loads_minimal_config_with_defaults() {
        let cfg: Config = serde_yaml_ng::from_str(minimal_yaml()).unwrap();
        assert_eq!(cfg.token, "abc");
        assert_eq!(cfg.engine.protocol, "homemade");
        assert_eq!(cfg.challenge.concurrency, 1);
        assert_eq!(cfg.move_overhead, 1000);
        assert_eq!(cfg.matchmaking.challenge_timeout, 30);
    }

    #[test]
    fn matchmaking_lists_get_default_none() {
        let cfg: Config = serde_yaml_ng::from_str(minimal_yaml()).unwrap();
        assert_eq!(cfg.matchmaking.challenge_initial_time, vec![None]);
    }

    #[test]
    fn max_base_inf_string() {
        let yaml = format!("{}\n  max_base: .inf", minimal_yaml().trim_end());
        let cfg: Config = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(cfg.challenge.max_base, None);
    }
}
