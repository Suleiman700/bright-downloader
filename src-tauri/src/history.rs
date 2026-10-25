//! Links the user checked in the Add dialog, with automatic clean-up.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How long checked links are remembered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Retention {
    /// Don't record links at all.
    Off,
    /// Keep for this session only; wiped when Bright Downloader quits or starts.
    OnExit,
    Day,
    #[default]
    Week,
    Month,
    Forever,
}

impl Retention {
    fn max_age_secs(self) -> Option<u64> {
        match self {
            Retention::Day => Some(86_400),
            Retention::Week => Some(7 * 86_400),
            Retention::Month => Some(30 * 86_400),
            Retention::Off | Retention::OnExit | Retention::Forever => None,
        }
    }
}

/// Cap so the file stays small even with "Forever".
pub const MAX_ENTRIES: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub url: String,
    pub file_name: Option<String>,
    pub size: Option<u64>,
    /// Unix seconds of the most recent check.
    pub checked_at: u64,
}

/// Drop expired entries and enforce the size cap. Newest first.
pub fn prune(entries: &mut Vec<Entry>, retention: Retention, now: u64) {
    if retention == Retention::Off {
        entries.clear();
        return;
    }
    if let Some(max_age) = retention.max_age_secs() {
        entries.retain(|e| now.saturating_sub(e.checked_at) <= max_age);
    }
    entries.sort_by(|a, b| b.checked_at.cmp(&a.checked_at));
    entries.truncate(MAX_ENTRIES);
}

pub struct History {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
}

impl History {
    pub fn load(path: PathBuf, retention: Retention, now: u64) -> Self {
        let mut entries: Vec<Entry> =
            std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if retention == Retention::OnExit {
            // A crash skips the on-quit wipe; finish it now.
            entries.clear();
        }
        prune(&mut entries, retention, now);
        let history = Self { path, entries: Mutex::new(entries) };
        history.save();
        history
    }

    fn save(&self) {
        let entries = self.entries.lock().unwrap().clone();
        let tmp = self.path.with_extension("json.tmp");
        let ok = serde_json::to_vec_pretty(&entries)
            .ok()
            .and_then(|b| std::fs::write(&tmp, b).ok())
            .and_then(|()| std::fs::rename(&tmp, &self.path).ok());
        if ok.is_none() {
            tracing::error!(path = %self.path.display(), "failed to save link history");
        }
    }

    /// Remember a checked link; re-checking moves it to the top.
    pub fn record(&self, entry: Entry, retention: Retention) {
        if retention == Retention::Off {
            return;
        }
        {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|e| e.url != entry.url);
            let now = entry.checked_at;
            entries.push(entry);
            prune(&mut entries, retention, now);
        }
        self.save();
    }

    pub fn list(&self, retention: Retention, now: u64) -> Vec<Entry> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        prune(&mut entries, retention, now);
        let changed = entries.len() != before;
        let list = entries.clone();
        drop(entries);
        if changed {
            self.save();
        }
        list
    }

    pub fn remove(&self, url: &str) {
        self.entries.lock().unwrap().retain(|e| e.url != url);
        self.save();
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    fn entry(url: &str, at: u64) -> Entry {
        Entry { url: url.into(), file_name: None, size: None, checked_at: at }
    }

    #[test]
    fn prune_by_age_newest_first() {
        let now = 100 * DAY;
        let mut e = vec![entry("old", now - 8 * DAY), entry("recent", now - DAY), entry("new", now)];
        prune(&mut e, Retention::Week, now);
        assert_eq!(e.iter().map(|e| e.url.as_str()).collect::<Vec<_>>(), ["new", "recent"]);

        let mut e = vec![entry("a", now - 2 * DAY), entry("b", now)];
        prune(&mut e, Retention::Day, now);
        assert_eq!(e.len(), 1);

        let mut e = vec![entry("ancient", 0), entry("b", now)];
        prune(&mut e, Retention::Forever, now);
        assert_eq!(e.len(), 2);

        prune(&mut e, Retention::Off, now);
        assert!(e.is_empty());
    }

    #[test]
    fn cap_keeps_newest() {
        let mut e: Vec<_> = (0..MAX_ENTRIES as u64 + 20).map(|i| entry(&i.to_string(), i)).collect();
        prune(&mut e, Retention::Forever, 10_000);
        assert_eq!(e.len(), MAX_ENTRIES);
        assert_eq!(e[0].url, (MAX_ENTRIES + 19).to_string());
    }

    #[test]
    fn record_dedupes_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let h = History::load(path.clone(), Retention::Week, 1000);
        h.record(entry("https://a/x.zip", 1000), Retention::Week);
        h.record(entry("https://b/y.zip", 1001), Retention::Week);
        h.record(Entry { size: Some(5), ..entry("https://a/x.zip", 1002) }, Retention::Week);
        let list = h.list(Retention::Week, 1003);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].url, "https://a/x.zip");
        assert_eq!(list[0].size, Some(5));

        let reloaded = History::load(path.clone(), Retention::Week, 1004);
        assert_eq!(reloaded.list(Retention::Week, 1004), list);

        reloaded.remove("https://a/x.zip");
        assert_eq!(reloaded.list(Retention::Week, 1005).len(), 1);
        reloaded.clear();
        assert!(History::load(path, Retention::Week, 1006).list(Retention::Week, 1006).is_empty());
    }

    #[test]
    fn off_records_nothing_and_on_exit_wipes_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let h = History::load(path.clone(), Retention::Off, 0);
        h.record(entry("https://a/x", 1), Retention::Off);
        assert!(h.list(Retention::Off, 2).is_empty());

        let h = History::load(path.clone(), Retention::OnExit, 0);
        h.record(entry("https://a/x", 1), Retention::OnExit);
        assert_eq!(h.list(Retention::OnExit, 2).len(), 1, "kept during the session");
        let next_session = History::load(path, Retention::OnExit, 3);
        assert!(next_session.list(Retention::OnExit, 3).is_empty());
    }
}
