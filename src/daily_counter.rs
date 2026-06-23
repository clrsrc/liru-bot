//! Persistent best-effort counter of bot-vs-bot games started today (UTC).
//!
//! Lichess caps each bot at **100 bot-vs-bot games per UTC day** (incoming +
//! outgoing, rated + casual; the limit is enforced server-side at game start,
//! resets 00:00 UTC). This counter lets our bot *know* its own daily tally so
//! it can stop cleanly at the limit — and optionally reserve part of the quota
//! for self-sought matchmaking — instead of blindly running into Lichess'
//! `400` / rate-limit responses once the cap is hit.
//!
//! It is deliberately a **courtesy layer**: Lichess remains the hard enforcer,
//! so approximate accuracy is fine. We count at the two points where the
//! opponent is *certainly* a bot (an accepted outbound matchmaking challenge,
//! which only ever targets bots; or an accepted inbound challenge whose
//! challenger carries the `BOT` flag) rather than at `gameStart`, because the
//! compact `gameStart` opponent object omits the `title` field and would make
//! bot detection there impossible.
//!
//! Reset is keyed to **00:00 UTC** (matching Lichess), not local midnight.
//! The tally is persisted to a small JSON file so a restart mid-day does not
//! lose the count and let the bot blow past the cap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// On-disk shape: which UTC day the tally belongs to, and the tally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CounterState {
    /// UTC calendar day (`YYYY-MM-DD`) the count below belongs to.
    #[serde(default)]
    date_utc: String,
    /// Bot-vs-bot games started on `date_utc`.
    #[serde(default)]
    bot_games: u32,
}

/// Persistent UTC-daily bot-vs-bot game counter backed by a JSON file.
pub struct DailyCounter {
    /// Backing file path. Empty path means "disabled" — the tally lives in
    /// memory only and nothing is written to disk (mirrors [`crate::opponent_db`]).
    path: PathBuf,
    state: CounterState,
}

impl DailyCounter {
    /// Load the counter from `path`. A missing file yields a fresh zero count;
    /// a corrupt file is logged and treated as zero so a bad write can never
    /// wedge the bot. An empty path disables persistence entirely.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() {
            return Self {
                path,
                state: CounterState::default(),
            };
        }
        let state = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<CounterState>(&raw) {
                Ok(s) => {
                    info!(
                        path = %path.display(),
                        date = %s.date_utc,
                        bot_games = s.bot_games,
                        "loaded daily bot-game counter"
                    );
                    s
                }
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        %err,
                        "daily counter is corrupt; starting at zero (file will be overwritten on next bot game)"
                    );
                    CounterState::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                info!(path = %path.display(), "no daily counter yet; will create on first bot game");
                CounterState::default()
            }
            Err(err) => {
                warn!(path = %path.display(), %err, "could not read daily counter; starting at zero");
                CounterState::default()
            }
        };
        Self { path, state }
    }

    /// Whether persistence is enabled (non-empty path).
    fn enabled(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    /// Reset the tally to zero if the stored day is not `today` (UTC). Pure
    /// in-memory; isolated from wall-clock formatting so the rollover is
    /// directly testable.
    fn roll_over(&mut self, today: &str) {
        if self.state.date_utc != today {
            self.state.date_utc = today.to_string();
            self.state.bot_games = 0;
        }
    }

    /// Bot-vs-bot games started so far today (UTC). Transparently rolls the
    /// tally over when the UTC day has changed since the last write.
    pub fn count(&mut self) -> u32 {
        self.roll_over(&today_utc());
        self.state.bot_games
    }

    /// Record one more bot-vs-bot game start and persist. Returns the new tally.
    pub fn increment(&mut self) -> u32 {
        self.roll_over(&today_utc());
        self.state.bot_games = self.state.bot_games.saturating_add(1);
        self.save();
        self.state.bot_games
    }

    /// Atomically persist to disk (temp file + rename). Errors are logged but
    /// never propagated — a failed write must not crash the bot.
    fn save(&self) {
        if !self.enabled() {
            return;
        }
        let json = match serde_json::to_string_pretty(&self.state) {
            Ok(s) => s,
            Err(err) => {
                warn!(%err, "could not serialize daily counter");
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(err) = std::fs::write(&tmp, json.as_bytes()) {
            warn!(path = %tmp.display(), %err, "could not write daily counter temp file");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            warn!(
                from = %tmp.display(),
                to = %self.path.display(),
                %err,
                "could not commit daily counter"
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Current UTC date as `YYYY-MM-DD`. Deliberately UTC (not local) because the
/// Lichess bot-vs-bot day limit resets at 00:00 UTC.
fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_counter_is_zero() {
        let mut c = DailyCounter::load("");
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn roll_over_resets_only_on_a_new_day() {
        let mut c = DailyCounter::load("");
        c.state.date_utc = "2026-06-12".into();
        c.state.bot_games = 5;

        // Same day → tally preserved.
        c.roll_over("2026-06-12");
        assert_eq!(c.state.bot_games, 5);

        // New day → tally reset and the date advances.
        c.roll_over("2026-06-13");
        assert_eq!(c.state.bot_games, 0);
        assert_eq!(c.state.date_utc, "2026-06-13");
    }

    #[test]
    fn increment_accumulates_in_memory() {
        let mut c = DailyCounter::load(""); // disabled path → in-memory only
        assert_eq!(c.increment(), 1);
        assert_eq!(c.increment(), 2);
        assert_eq!(c.count(), 2);
    }

    #[test]
    fn stale_dated_tally_reads_as_zero_today() {
        let mut c = DailyCounter::load("");
        // A tally stamped for a day that is definitely not today must not leak
        // into today's budget.
        c.state.date_utc = "2000-01-01".into();
        c.state.bot_games = 99;
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn persists_and_reloads_across_instances() {
        // Unique-ish path without Date/rand: process id keeps parallel test
        // binaries from colliding.
        let path = std::env::temp_dir().join(format!("liru_daily_counter_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let mut c = DailyCounter::load(&path);
            c.increment();
            c.increment();
        }
        // A second instance loads the persisted tally for the same UTC day.
        let mut reloaded = DailyCounter::load(&path);
        assert_eq!(reloaded.count(), 2);

        let _ = std::fs::remove_file(&path);
    }
}
