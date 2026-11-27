//! Sources: uploads, pushes and query sources, and their snapshots. A
//! new snapshot runs nothing: a dataset that reads the source changes on
//! its next run, when the application sends one.

use crate::error::{Error, Result};
use crate::source::{Kind, Query, SemanticRead};
use crate::store::{self, Materialization, Snapshot, Source};
use crate::{
    description_of, is_source_id, name_of, new_source_id, recovery, sha256, snapshot, time, Arrived, Page, Prep, Scope, Upload, View,
};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// How many snapshots a source keeps, besides those an output needs.
const SNAPSHOTS_KEPT: usize = 20;

/// Rows posted by an application or an agent, each an object of column
/// to value, and where they came from, as it says.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pushed {
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub origin: Option<serde_json::Value>,
}

/// A new source's or dataset's name and description.
#[derive(Clone, Debug, Default)]
pub struct Named {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// A change to a source: its name, its description, and the types its
/// columns are read as (a new version of how it is read).
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub types: Option<BTreeMap<String, crate::ColumnType>>,
}

impl Prep {
    pub(crate) fn find_source(&self, scope: &Scope, id: &str) -> Result<(PathBuf, Source)> {
        let missing = || Error::NotFound(format!("no source {id} in project {}", scope.project));
        if !is_source_id(id) {
            return Err(missing());
        }
        let dir = store::source_dir(&self.root(scope), id);
        let mut s: Source = match store::read_json(&dir.join(store::SOURCE)) {
            Ok(s) => s,
            Err(_) if !dir.join(store::SOURCE).exists() => return Err(missing()),
            Err(e) => return Err(e),
        };
        s.interrupted = store::read_json(&dir.join(recovery::INTERRUPTED)).ok();
        Ok((dir, s))
    }

    /// The project's sources: what the explorer lists.
    pub fn sources(&self, scope: &Scope) -> Vec<Source> {
        store::sources_in(&self.root(scope)).into_iter().map(|(_, s)| s).collect()
    }

    pub fn source(&self, scope: &Scope, id: &str) -> Result<Source> {
        Ok(self.find_source(scope, id)?.1)
    }

    fn check_bytes(&self, n: usize) -> Result<()> {
        if n as u64 > self.settings.max_bytes {
            return Err(Error::Unprocessable(format!("this file is {n} bytes, more than the {} a source takes", self.settings.max_bytes)));
        }
        Ok(())
    }

    /// A new source, built whole in a staging directory and renamed into
    /// place; `first` takes its first snapshot there, when it has one.
    fn new_source(
        &self,
        scope: &Scope,
        kind: Kind,
        named: &Named,
        fallback: &str,
        query: Option<Query>,
        first: impl FnOnce(&Prep, &Path, &mut Source) -> Result<Option<Snapshot>>,
    ) -> Result<(Source, Option<Snapshot>)> {
        if self.is_stopping() {
            return Err(Error::Stopping("the server is stopping; try again in a moment".into()));
        }
        let id = new_source_id(&scope.project);
        let home = self.root(scope).join(store::SOURCES);
        let staging = home.join(format!(".new-{id}"));
        let now = time::now();
        let mut s = Source {
            id: id.clone(),
            name: name_of(named.name.as_deref(), Some(fallback))?,
            description: description_of(named.description.as_deref())?,
            kind,
            created: now.clone(),
            updated: now.clone(),
            snapshot: 0,
            columns: Vec::new(),
            query,
            types: BTreeMap::new(),
            read: 1,
            reads: vec![store::ReadOptions { version: 1, at: now, types: BTreeMap::new() }],
            backup: None,
            interrupted: None,
        };
        let made = (|| {
            std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
            let snap = first(self, &staging, &mut s)?;
            store::write_json(&staging.join(store::SOURCE), &s)?;
            let target = home.join(&id);
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            store::sync_dir(Some(&home))?;
            Ok(snap)
        })();
        let snap = match made {
            Ok(snap) => snap,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        self.backed_up(&home.join(&id));
        tracing::info!("source {id} ({kind:?}) made in {}/{}", scope.org, scope.project);
        Ok((self.source(scope, &id)?, snap))
    }

    /// A new upload source, from its first file.
    pub fn create_upload(&self, scope: &Scope, named: Named, upload: Upload<'_>) -> Result<(Source, Snapshot)> {
        self.check_bytes(upload.bytes.len())?;
        let fallback = upload.file_name.clone().unwrap_or_else(|| "upload".into());
        let (s, snap) =
            self.new_source(scope, Kind::Upload, &named, &fallback, None, |prep, dir, s| prep.take_upload(dir, s, &upload).map(Some))?;
        Ok((s, snap.ok_or_else(|| Error::Internal("an upload made no snapshot".into()))?))
    }

    /// A new push source, from its first rows.
    pub fn create_push(&self, scope: &Scope, named: Named, pushed: Pushed) -> Result<(Source, Snapshot)> {
        let (s, snap) =
            self.new_source(scope, Kind::Push, &named, "pushed rows", None, |prep, dir, s| prep.take_push(dir, s, &pushed).map(Some))?;
        Ok((s, snap.ok_or_else(|| Error::Internal("a push made no snapshot".into()))?))
    }

    /// A new query source. It has no snapshot until a run or a preview
    /// pulls it, under the context that run is sent with.
    pub fn create_query(&self, scope: &Scope, named: Named, query: Query) -> Result<Source> {
        if query.spec.is_some() == query.expr.is_some() {
            return Err(Error::Invalid("a query is a spec or an expr, one of them".into()));
        }
        let fallback = format!("query on {}", query.branch);
        Ok(self.new_source(scope, Kind::Query, &named, &fallback, Some(query), |_, _, _| Ok(None))?.0)
    }

    /// A new snapshot of an upload source, from a new file.
    pub fn add_upload(&self, scope: &Scope, id: &str, upload: Upload<'_>) -> Result<(Source, Snapshot)> {
        self.check_bytes(upload.bytes.len())?;
        self.add_snapshot(scope, id, Kind::Upload, recovery::JobKind::Upload, |prep, dir, s| prep.take_upload(dir, s, &upload))
    }

    /// A new snapshot of a push source, from rows posted.
    pub fn add_push(&self, scope: &Scope, id: &str, pushed: Pushed) -> Result<(Source, Snapshot)> {
        self.add_snapshot(scope, id, Kind::Push, recovery::JobKind::Push, |prep, dir, s| prep.take_push(dir, s, &pushed))
    }

    /// A new snapshot of a query source, from the rows a pull returned,
    /// with the fingerprint of the context it ran under.
    pub fn pulled(&self, scope: &Scope, id: &str, arrived: Arrived) -> Result<(Source, Snapshot)> {
        self.add_snapshot(scope, id, Kind::Query, recovery::JobKind::Pull, |prep, dir, s| prep.take_rows(dir, s, &arrived))
    }

    fn add_snapshot(
        &self,
        scope: &Scope,
        id: &str,
        kind: Kind,
        job: recovery::JobKind,
        take: impl FnOnce(&Prep, &Path, &mut Source) -> Result<Snapshot>,
    ) -> Result<(Source, Snapshot)> {
        let (dir, _) = self.find_source(scope, id)?;
        let held = self.job(&dir, job)?;
        let (_, mut s) = self.find_source(scope, id)?;
        if s.kind != kind {
            let how = match s.kind {
                Kind::Upload => "an upload source takes a new file",
                Kind::Push => "a push source takes posted rows",
                Kind::Query => "a query source is pulled by a run",
            };
            return Err(Error::Invalid(format!("source {id} is not that kind: {how}")));
        }
        let snap = take(self, &dir, &mut s)?;
        s.interrupted = None;
        store::write_json(&dir.join(store::SOURCE), &s)?;
        self.prune(&self.root(scope), &dir, &s);
        drop(held);
        self.backed_up(&dir);
        Ok((self.source(scope, id)?, snap))
    }

    fn take_upload(&self, dir: &Path, s: &mut Source, upload: &Upload<'_>) -> Result<Snapshot> {
        let session = self.engine.session()?;
        let taken = snapshot::from_csv(&session, upload.bytes, &upload.options, &s.columns, self.settings.max_rows)?;
        let sha = sha256(upload.bytes);
        self.keep_snapshot(dir, s, (upload.file_name.clone(), Some(sha)), None, None, taken)
    }

    fn take_push(&self, dir: &Path, s: &mut Source, pushed: &Pushed) -> Result<Snapshot> {
        let (columns, values) = snapshot::pushed(&pushed.rows, self.settings.max_rows)?;
        let session = self.engine.session()?;
        let taken = snapshot::from_texts(&session, &columns, values, &s.columns)?;
        self.keep_snapshot(dir, s, (None, None), None, pushed.origin.clone(), taken)
    }

    fn take_rows(&self, dir: &Path, s: &mut Source, arrived: &Arrived) -> Result<Snapshot> {
        let session = self.engine.session()?;
        let taken = snapshot::from_rows(&session, &arrived.rows, &arrived.roles, &s.columns, self.settings.max_rows)?;
        self.keep_snapshot(dir, s, (None, None), Some(arrived.read.clone()), None, taken)
    }

    /// Keeps the next snapshot of `s` under `dir`, whole or not at all.
    fn keep_snapshot(
        &self,
        dir: &Path,
        s: &mut Source,
        (file_name, sha): (Option<String>, Option<String>),
        semantic: Option<SemanticRead>,
        origin: Option<serde_json::Value>,
        taken: snapshot::Taken,
    ) -> Result<Snapshot> {
        let n = s.snapshot + 1;
        let snaps = dir.join("snapshots");
        let staging = snaps.join(format!(".s{n}.staging-{}", time::nanos()));
        let target = store::snapshot_dir(dir, n);
        let made = (|| {
            let bytes = store::write_rows(&staging.join(store::ROWS), &taken.rows)?;
            if bytes > self.settings.max_bytes {
                return Err(Error::Unprocessable(format!(
                    "the snapshot is {bytes} bytes, more than the {} a source holds",
                    self.settings.max_bytes
                )));
            }
            let sha256 = match sha {
                Some(h) => h,
                None => sha256(&std::fs::read(staging.join(store::ROWS)).map_err(|e| Error::io(&staging, e))?),
            };
            let snap = Snapshot {
                source: s.id.clone(),
                number: n,
                kind: s.kind,
                taken: time::now(),
                file_name,
                sha256,
                bytes,
                parse: taken.parse,
                semantic,
                origin,
                columns: taken.columns.clone(),
                rows: taken.rows.num_rows() as u64,
                changes: taken.changes,
            };
            store::write_json(&staging.join("manifest.json"), &snap)?;
            if target.exists() {
                // Left by a job that did not finish: no manifest pointed at it.
                std::fs::remove_dir_all(&target).map_err(|e| Error::io(&target, e))?;
            }
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            store::sync_dir(Some(&snaps))?;
            Ok(snap)
        })();
        let snap = match made {
            Ok(snap) => snap,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        s.snapshot = n;
        s.columns = taken.columns;
        s.updated = time::now();
        Ok(snap)
    }

    /// Lets go of a source's oldest snapshots past what it keeps, but
    /// never one an output points to, or a live output read.
    fn prune(&self, root: &Path, dir: &Path, s: &Source) {
        if (s.snapshot as usize) <= SNAPSHOTS_KEPT {
            return;
        }
        let mut pinned: HashSet<u32> = HashSet::new();
        for (ddir, d) in store::datasets_in(root) {
            for v in recovery::entries(&ddir) {
                let Ok(m) = store::read_json::<Materialization>(&v.join("manifest.json")) else { continue };
                if let Some(p) = m.pointer.filter(|p| p.source == s.id) {
                    pinned.insert(p.snapshot);
                }
            }
            if let Some(Ok(l)) = d.live.map(|k| store::read_lineage(&ddir, k)) {
                pinned.extend(l.sources.iter().filter(|r| r.source == s.id).map(|r| r.snapshot));
            }
        }
        let oldest_kept = s.snapshot - SNAPSHOTS_KEPT as u32;
        for n in 1..=oldest_kept {
            let sd = store::snapshot_dir(dir, n);
            if !pinned.contains(&n) && sd.exists() {
                if let Err(e) = store::remove(&sd) {
                    tracing::warn!("snapshot {n} of source {} was to go and did not: {e}", s.id);
                }
            }
        }
    }

    /// A source's name, description, or the types its columns are read
    /// as: a change of types is a new version of how it is read, which
    /// every graph reading the source reads with on its next run.
    pub fn change_source(&self, scope: &Scope, id: &str, change: SourceChange) -> Result<Source> {
        let (dir, _) = self.find_source(scope, id)?;
        let held = self.job(&dir, recovery::JobKind::Upload)?;
        let (_, mut s) = self.find_source(scope, id)?;
        if let Some(n) = change.name.as_deref() {
            s.name = name_of(Some(n), None)?;
        }
        if let Some(d) = change.description.as_deref() {
            s.description = description_of(Some(d))?;
        }
        if let Some(types) = change.types {
            if types != s.types {
                s.read += 1;
                s.types = types.clone();
                s.reads.push(store::ReadOptions { version: s.read, at: time::now(), types });
            }
        }
        s.updated = time::now();
        s.interrupted = None;
        store::write_json(&dir.join(store::SOURCE), &s)?;
        drop(held);
        self.backed_up(&dir);
        self.source(scope, id)
    }

    /// A source's snapshots that are kept, the newest first.
    pub fn snapshots(&self, scope: &Scope, id: &str) -> Result<Vec<Snapshot>> {
        let (dir, s) = self.find_source(scope, id)?;
        Ok((1..=s.snapshot).rev().filter_map(|n| store::read_json(&store::snapshot_dir(&dir, n).join("manifest.json")).ok()).collect())
    }

    /// A snapshot's manifest: the newest, or the one asked for.
    pub fn snapshot(&self, scope: &Scope, id: &str, n: Option<u32>) -> Result<Snapshot> {
        let (dir, s) = self.find_source(scope, id)?;
        let n = n.unwrap_or(s.snapshot);
        store::read_json(&store::snapshot_dir(&dir, n).join("manifest.json"))
            .map_err(|_| Error::NotFound(format!("source {id} has no snapshot {n} kept")))
    }

    /// A page of a snapshot: the newest, or the one asked for.
    pub fn source_rows(&self, scope: &Scope, id: &str, n: Option<u32>, offset: u64, limit: u64, view: &View<'_>) -> Result<Page> {
        let (dir, _) = self.find_source(scope, id)?;
        let snap = self.snapshot(scope, id, n)?;
        let path = store::snapshot_dir(&dir, snap.number).join(store::ROWS);
        let (total, rows) = crate::materialize::page_of(&self.engine, &snap.columns, (&path, snap.rows), offset, limit, view)?;
        Ok(Page { version: snap.number, columns: snap.columns, total, offset, rows })
    }

    /// Removes a source and every snapshot of it. A dataset output that
    /// is one of its snapshots as it is gets a copy of those rows of its
    /// own first, so it keeps answering; a dataset that reads it fails
    /// its next run, saying so. The datasets given a copy.
    pub fn delete_source(&self, scope: &Scope, id: &str) -> Result<Vec<String>> {
        let (dir, _) = self.find_source(scope, id)?;
        let held = self.job(&dir, recovery::JobKind::Delete)?;
        let root = self.root(scope);
        let mut copied = Vec::new();
        for (ddir, d) in store::datasets_in(&root) {
            for v in recovery::entries(&ddir) {
                let path = v.join("manifest.json");
                let Ok(mut m) = store::read_json::<Materialization>(&path) else { continue };
                let Some(p) = m.pointer.clone().filter(|p| p.source == id) else { continue };
                let rows = store::read_rows(&store::snapshot_dir(&dir, p.snapshot).join(store::ROWS))?;
                m.bytes = store::write_rows(&v.join(store::ROWS), &rows)?;
                m.pointer = None;
                store::write_json(&path, &m)?;
                if !copied.contains(&d.id) {
                    copied.push(d.id.clone());
                }
            }
        }
        store::remove(&dir)?;
        drop(held);
        for d in &copied {
            self.backed_up(&root.join(d));
        }
        self.forget_backup(&scope.org, &scope.project, &["sources", id]);
        tracing::info!("source {id} removed from {}/{}", scope.org, scope.project);
        Ok(copied)
    }
}
