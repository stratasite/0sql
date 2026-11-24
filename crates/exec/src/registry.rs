//! Where the records of runs are kept. Two places answer to one trait:
//! a database every node of a cluster shares, and this node's own disk
//! when there is none.

use crate::engine::{parse_rfc3339, rfc3339, unix_now};
use crate::error::{Error, Result};
use crate::record::{Record, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What ended a run nobody is running any more.
pub const RESTARTED: &str = "the server restarted, which killed queries in progress";

/// The records of runs.
pub trait Registry: Send + Sync {
    /// A new record. Its id is the caller's and must not be taken.
    fn insert(&self, record: &Record) -> Result<()>;

    /// The record as it now stands, in place of what was kept under its id.
    fn save(&self, record: &Record) -> Result<()>;

    fn get(&self, id: &str) -> Result<Option<Record>>;

    /// The newest result that answers `cache_key` now: succeeded, not
    /// dirty, not expired.
    fn cached(&self, cache_key: &str) -> Result<Option<Record>>;

    /// A run of `cache_key` that has not ended, for an identical request
    /// to attach to.
    fn in_flight(&self, cache_key: &str) -> Result<Option<Record>>;

    /// Ends every run this node had queued or running, as failed with
    /// `why`: what a node does when it starts, since nothing it was
    /// running survived. How many it ended.
    fn fail_unfinished(&self, hostname: &str, why: &str) -> Result<usize>;

    /// Says this node is alive: every run it has queued or running is
    /// marked as seen now.
    fn touch(&self, hostname: &str) -> Result<()>;

    /// Ends, as failed with `why`, every run that is queued or running
    /// and has not been seen for `quiet_for`: its node stopped, or was
    /// replaced under another name, and nothing is running it. How many.
    fn fail_abandoned(&self, quiet_for: std::time::Duration, why: &str) -> Result<usize>;
}

/// Records on this node's disk: a file each, read when the server
/// starts, so a finished result still answers after a restart.
pub struct Local {
    dir: PathBuf,
    records: Mutex<HashMap<String, Record>>,
}

impl Local {
    /// Opens the records under `dir`. A file that is not a record is
    /// left where it is and reported, not read past in silence.
    pub fn open(dir: &Path) -> Result<Local> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
        let mut records = HashMap::new();
        for entry in std::fs::read_dir(dir).map_err(|e| Error::Io(dir.display().to_string(), e))? {
            let path = entry.map_err(|e| Error::Io(dir.display().to_string(), e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let read = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Record>(&t).map_err(|e| e.to_string()));
            match read {
                Ok(record) => {
                    records.insert(record.id.clone(), record);
                }
                Err(why) => tracing::warn!(file = %path.display(), "a record could not be read and is not served: {why}"),
            }
        }
        Ok(Local { dir: dir.to_path_buf(), records: Mutex::new(records) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Record>> {
        self.records.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Written beside its name and renamed over it, so a reader of the
    /// directory finds the record as it was or as it is.
    fn write(&self, record: &Record) -> Result<()> {
        let path = self.dir.join(format!("{}.json", record.id));
        let tmp = self.dir.join(format!("{}.json.tmp", record.id));
        let io = |p: &Path, e: std::io::Error| Error::Io(p.display().to_string(), e);
        std::fs::write(&tmp, serde_json::to_string_pretty(record)?).map_err(|e| io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| io(&path, e))
    }
}

impl Registry for Local {
    fn insert(&self, record: &Record) -> Result<()> {
        let mut records = self.lock();
        if records.contains_key(&record.id) {
            return Err(Error::Duplicate(record.id.clone()));
        }
        self.write(record)?;
        records.insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn save(&self, record: &Record) -> Result<()> {
        let mut records = self.lock();
        self.write(record)?;
        records.insert(record.id.clone(), record.clone());
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<Record>> {
        Ok(self.lock().get(id).cloned())
    }

    fn cached(&self, cache_key: &str) -> Result<Option<Record>> {
        let now = unix_now();
        Ok(self
            .lock()
            .values()
            .filter(|r| r.cache_key.as_deref() == Some(cache_key) && r.status == Status::Succeeded && !r.dirty)
            .filter(|r| r.expires_at.as_deref().and_then(parse_rfc3339).is_some_and(|until| until > now))
            .max_by(|a, b| a.finished_at.cmp(&b.finished_at).then_with(|| a.id.cmp(&b.id)))
            .cloned())
    }

    fn in_flight(&self, cache_key: &str) -> Result<Option<Record>> {
        Ok(self.lock().values().find(|r| r.cache_key.as_deref() == Some(cache_key) && !r.status.is_final()).cloned())
    }

    fn fail_unfinished(&self, hostname: &str, why: &str) -> Result<usize> {
        let unfinished: Vec<Record> = self.lock().values().filter(|r| r.hostname == hostname && !r.status.is_final()).cloned().collect();
        let now = rfc3339(unix_now());
        for mut record in unfinished.iter().cloned() {
            record.status = Status::Failed;
            record.error = Some(why.to_string());
            record.finished_at = Some(now.clone());
            record.updated_at = now.clone();
            self.save(&record)?;
        }
        Ok(unfinished.len())
    }

    fn touch(&self, hostname: &str) -> Result<()> {
        // In memory only: nobody else reads this node's records, and a
        // restart ends what was unfinished whatever the time on it.
        let now = rfc3339(unix_now());
        for record in self.lock().values_mut().filter(|r| r.hostname == hostname && !r.status.is_final()) {
            record.updated_at = now.clone();
        }
        Ok(())
    }

    fn fail_abandoned(&self, quiet_for: std::time::Duration, why: &str) -> Result<usize> {
        let before = unix_now().saturating_sub(quiet_for.as_secs());
        let quiet: Vec<Record> = self
            .lock()
            .values()
            .filter(|r| !r.status.is_final() && parse_rfc3339(&r.updated_at).is_some_and(|seen| seen < before))
            .cloned()
            .collect();
        let now = rfc3339(unix_now());
        for mut record in quiet.iter().cloned() {
            record.status = Status::Failed;
            record.error = Some(why.to_string());
            record.finished_at = Some(now.clone());
            record.updated_at = now.clone();
            self.save(&record)?;
        }
        Ok(quiet.len())
    }
}
