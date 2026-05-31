//! Persistent record of every bot we have challenged via matchmaking.
//!
//! Unlike the in-memory decline cooldowns in [`crate::matchmaking`], this
//! database survives bot restarts. It tracks, per opponent:
//!
//! - how often we challenged them and in which form (time control, rated /
//!   casual, variant),
//! - whether a game actually started (i.e. they accepted),
//! - how often and why they declined,
//! - a permanent `no_bots` flag set when an opponent declines with reason
//!   `noBot` / `onlyBot` ("Spiele nicht gegen Bots"). Such opponents are
//!   never challenged again until the flag is cleared by hand.
//!
//! The store is a single JSON file (`matchmaking.opponent_db_path`), written
//! atomically (temp file + rename) after every mutation. A few hundred bots
//! at a write cadence measured in tens of seconds keeps this trivially cheap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// How many history entries to keep per opponent before dropping the oldest.
const MAX_HISTORY: usize = 50;

/// The time control / mode / variant a challenge was sent with.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChallengeForm {
    /// Clock base time in seconds (0 for correspondence).
    pub base_time: i64,
    /// Clock increment in seconds.
    pub increment: i64,
    /// Days per move for correspondence (0 for real-time games).
    pub days: i64,
    /// Variant key, e.g. `standard`, `chess960`, `atomic`.
    pub variant: String,
    /// `rated` or `casual`.
    pub mode: String,
    /// Derived speed bucket, e.g. `bullet`, `blitz`, `correspondence`.
    pub game_type: String,
}

impl std::fmt::Display for ChallengeForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.days > 0 {
            write!(f, "{} days/move", self.days)?;
        } else {
            write!(f, "{}+{}", self.base_time, self.increment)?;
        }
        write!(f, " {} {} ({})", self.variant, self.mode, self.game_type)
    }
}

/// A single timeline event for one opponent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// RFC-3339 local timestamp.
    pub at: String,
    /// `challenged`, `accepted`, or `declined:<reason_key>`.
    pub event: String,
    /// The form involved (the last challenge form for `accepted`).
    pub form: ChallengeForm,
}

/// Everything we know about one opponent bot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpponentRecord {
    /// Timestamp of the most recent challenge we sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_challenged: Option<String>,
    /// Form of the most recent challenge we sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_form: Option<ChallengeForm>,
    /// Total challenges we have sent to this opponent.
    #[serde(default)]
    pub challenges_sent: u32,
    /// Number of games that actually started (challenge accepted).
    #[serde(default)]
    pub games_played: u32,
    /// Number of declines received.
    #[serde(default)]
    pub declines: u32,
    /// The most recent decline reason key (e.g. `nobot`, `later`, `toofast`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_decline_reason: Option<String>,
    /// Permanent "does not play bots" flag — never challenge again.
    #[serde(default)]
    pub no_bots: bool,
    /// Capped event timeline, newest last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
}

impl OpponentRecord {
    fn push_history(&mut self, at: String, event: String, form: ChallengeForm) {
        self.history.push(HistoryEntry { at, event, form });
        if self.history.len() > MAX_HISTORY {
            let overflow = self.history.len() - MAX_HISTORY;
            self.history.drain(0..overflow);
        }
    }
}

/// Persistent opponent database backed by a JSON file.
pub struct OpponentDb {
    /// Backing file path. Empty path means "disabled" — all ops are no-ops
    /// and nothing is written to disk.
    path: PathBuf,
    records: HashMap<String, OpponentRecord>,
}

impl OpponentDb {
    /// Load the database from `path`. A missing file yields an empty database
    /// (created lazily on first write). A corrupt file is logged and treated
    /// as empty so a bad write can never wedge matchmaking. An empty path
    /// disables persistence entirely.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() {
            return Self {
                path,
                records: HashMap::new(),
            };
        }
        let records = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, OpponentRecord>>(&raw) {
                Ok(map) => {
                    let blocked = map.values().filter(|r| r.no_bots).count();
                    info!(
                        path = %path.display(),
                        opponents = map.len(),
                        no_bots = blocked,
                        "loaded matchmaking opponent database"
                    );
                    map
                }
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        %err,
                        "opponent database is corrupt; starting empty (file will be overwritten on next write)"
                    );
                    HashMap::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                info!(path = %path.display(), "no opponent database yet; will create on first challenge");
                HashMap::new()
            }
            Err(err) => {
                warn!(path = %path.display(), %err, "could not read opponent database; starting empty");
                HashMap::new()
            }
        };
        Self { path, records }
    }

    /// Whether persistence is enabled (non-empty path).
    fn enabled(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    /// Whether this opponent has permanently refused bot games and must not
    /// be challenged again.
    pub fn is_blocked(&self, username: &str) -> bool {
        self.records
            .get(username)
            .map(|r| r.no_bots)
            .unwrap_or(false)
    }

    /// Read-only access to a single record (used by reporting / tests).
    pub fn get(&self, username: &str) -> Option<&OpponentRecord> {
        self.records.get(username)
    }

    /// Number of opponents currently flagged `no_bots`.
    pub fn blocked_count(&self) -> usize {
        self.records.values().filter(|r| r.no_bots).count()
    }

    /// Record that we sent a challenge to `username` with `form`.
    pub fn record_challenge_sent(&mut self, username: &str, form: ChallengeForm) {
        let now = now_rfc3339();
        let rec = self.records.entry(username.to_string()).or_default();
        rec.challenges_sent = rec.challenges_sent.saturating_add(1);
        rec.last_challenged = Some(now.clone());
        rec.last_form = Some(form.clone());
        rec.push_history(now, "challenged".into(), form);
        self.save();
    }

    /// Record that `username` accepted our challenge and a game started.
    pub fn record_accepted(&mut self, username: &str) {
        let now = now_rfc3339();
        let rec = self.records.entry(username.to_string()).or_default();
        rec.games_played = rec.games_played.saturating_add(1);
        let form = rec.last_form.clone().unwrap_or_default();
        rec.push_history(now, "accepted".into(), form);
        self.save();
    }

    /// Record that `username` declined our challenge. `reason_key` is the
    /// machine-readable Lichess decline key (already lower-cased). The
    /// `noBot` / `onlyBot` keys set the permanent `no_bots` flag.
    pub fn record_declined(&mut self, username: &str, reason_key: &str) {
        let now = now_rfc3339();
        let blocks = matches!(reason_key, "nobot" | "onlybot");
        let rec = self.records.entry(username.to_string()).or_default();
        rec.declines = rec.declines.saturating_add(1);
        rec.last_decline_reason = Some(reason_key.to_string());
        if blocks {
            rec.no_bots = true;
        }
        let form = rec.last_form.clone().unwrap_or_default();
        rec.push_history(now, format!("declined:{reason_key}"), form);
        if blocks {
            info!(
                opponent = %username,
                reason = %reason_key,
                "opponent refuses bot games; permanently excluded from matchmaking"
            );
        }
        self.save();
    }

    /// Atomically persist to disk (temp file in the same directory + rename).
    /// Errors are logged but never propagated — a failed write must not crash
    /// the bot or interrupt matchmaking.
    fn save(&self) {
        if !self.enabled() {
            return;
        }
        let json = match serde_json::to_string_pretty(&self.records) {
            Ok(s) => s,
            Err(err) => {
                warn!(%err, "could not serialize opponent database");
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json.as_bytes()) {
            warn!(path = %tmp.display(), %err, "could not write opponent database temp file");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            warn!(
                from = %tmp.display(),
                to = %self.path.display(),
                %err,
                "could not commit opponent database"
            );
            // Best-effort cleanup of the orphaned temp file.
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Current local time as an RFC-3339 string. Isolated so tests of the data
/// logic don't depend on wall-clock formatting details.
fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn form(mode: &str) -> ChallengeForm {
        ChallengeForm {
            base_time: 300,
            increment: 2,
            days: 0,
            variant: "standard".into(),
            mode: mode.into(),
            game_type: "blitz".into(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        // Unique-ish per test name; tests run single-threaded per file by
        // default for this crate's data tests and each uses a distinct name.
        p.push(format!("lichess_bot_oppdb_{name}.json"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn missing_file_loads_empty() {
        let path = temp_path("missing");
        let db = OpponentDb::load(&path);
        assert!(db.get("Anyone").is_none());
        assert!(!db.is_blocked("Anyone"));
    }

    #[test]
    fn empty_path_disables_persistence() {
        let mut db = OpponentDb::load("");
        db.record_challenge_sent("Foo", form("casual"));
        // Nothing written, but in-memory state still works.
        assert_eq!(db.get("Foo").unwrap().challenges_sent, 1);
    }

    #[test]
    fn nobot_decline_sets_permanent_block() {
        let path = temp_path("nobot");
        let mut db = OpponentDb::load(&path);
        db.record_challenge_sent("RefuserBot", form("rated"));
        db.record_declined("RefuserBot", "nobot");
        assert!(db.is_blocked("RefuserBot"));

        // Survives a reload from disk.
        let db2 = OpponentDb::load(&path);
        assert!(db2.is_blocked("RefuserBot"));
        assert_eq!(db2.blocked_count(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn onlybot_decline_also_blocks() {
        let path = temp_path("onlybot");
        let mut db = OpponentDb::load(&path);
        db.record_declined("HumansOnly", "onlybot");
        assert!(db.is_blocked("HumansOnly"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ordinary_decline_does_not_block() {
        let path = temp_path("later");
        let mut db = OpponentDb::load(&path);
        db.record_challenge_sent("BusyBot", form("casual"));
        db.record_declined("BusyBot", "later");
        assert!(!db.is_blocked("BusyBot"));
        let rec = db.get("BusyBot").unwrap();
        assert_eq!(rec.declines, 1);
        assert_eq!(rec.last_decline_reason.as_deref(), Some("later"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn counters_and_form_round_trip() {
        let path = temp_path("counters");
        let mut db = OpponentDb::load(&path);
        db.record_challenge_sent("PlayBot", form("rated"));
        db.record_accepted("PlayBot");
        db.record_challenge_sent("PlayBot", form("casual"));

        let db2 = OpponentDb::load(&path);
        let rec = db2.get("PlayBot").unwrap();
        assert_eq!(rec.challenges_sent, 2);
        assert_eq!(rec.games_played, 1);
        assert_eq!(rec.last_form.as_ref().unwrap().mode, "casual");
        // challenged, accepted, challenged
        assert_eq!(rec.history.len(), 3);
        assert_eq!(rec.history[1].event, "accepted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn history_is_capped() {
        let path = temp_path("history_cap");
        let mut db = OpponentDb::load(&path);
        for _ in 0..(MAX_HISTORY + 10) {
            db.record_challenge_sent("SpamBot", form("casual"));
        }
        assert_eq!(db.get("SpamBot").unwrap().history.len(), MAX_HISTORY);
        let _ = std::fs::remove_file(&path);
    }
}
