//! Filesystem watch for incremental indexing.
//!
//! Editors and downloads write the same file several times in a row, and cloud sync
//! providers rename then re-create. Acting on every event would re-embed the same file
//! repeatedly, so events are coalesced per path until it goes quiet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
    /// The watcher lost events (overflow, unmounted volume, too many opens) and the
    /// only safe recovery is a full rescan of that root.
    Rescan(PathBuf),
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Change::Created(p) | Change::Modified(p) | Change::Removed(p) | Change::Rescan(p) => p,
        }
    }
    pub fn kind_name(&self) -> &'static str {
        match self {
            Change::Created(_) => "created",
            Change::Modified(_) => "modified",
            Change::Removed(_) => "removed",
            Change::Rescan(_) => "rescan",
        }
    }
}

fn classify(event: Event) -> Vec<Change> {
    let mut out = Vec::new();
    match event.kind {
        EventKind::Remove(_) => {
            for p in event.paths {
                out.push(Change::Removed(p));
            }
        }
        EventKind::Create(_) => {
            for p in event.paths {
                out.push(Change::Created(p));
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            for p in event.paths {
                out.push(Change::Created(p));
            }
        }
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => {
            // a rename leaves the old path behind as far as the index is concerned
            for p in event.paths {
                out.push(Change::Rescan(p));
            }
        }
        EventKind::Modify(_) => {
            for p in event.paths {
                out.push(Change::Modified(p));
            }
        }
        EventKind::Other => {
            for p in event.paths {
                out.push(Change::Rescan(p));
            }
        }
        _ => {}
    }
    out
}

/// Holds a path until it stops changing, then releases it once.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    pending: HashMap<PathBuf, (Change, Instant)>,
}

impl Debouncer {
    pub fn new(quiet: Duration) -> Debouncer {
        Debouncer { quiet, pending: HashMap::new() }
    }

    /// Absorb an event; returns paths that have been quiet long enough to act on.
    /// Removals are immediate — waiting only risks indexing a file that is gone.
    pub fn push(&mut self, change: Change, now: Instant) -> Vec<Change> {
        let mut ready = Vec::new();
        match &change {
            Change::Removed(p) => {
                self.pending.remove(p);
                return vec![change];
            }
            Change::Rescan(p) => {
                self.pending.remove(p);
                return vec![change];
            }
            Change::Created(p) | Change::Modified(p) => {
                // Created then Modified for the same path is one write: keep Created,
                // which is what decides whether the row is new.
                let replace = match self.pending.get(p) {
                    Some((Change::Modified(_), _)) => true,
                    Some((Change::Created(_), _)) => matches!(&change, Change::Created(_)),
                    _ => true,
                };
                if replace {
                    self.pending.insert(p.clone(), (change, now));
                } else {
                    if let Some(entry) = self.pending.get_mut(p) {
                        entry.1 = now;
                    }
                }
            }
        }
        let stale: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= self.quiet)
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            if let Some((c, _)) = self.pending.remove(&p) {
                ready.push(c);
            }
        }
        ready
    }

    /// Release everything, e.g. before shutdown or when the indexer is about to idle.
    pub fn flush(&mut self) -> Vec<Change> {
        self.pending.drain().map(|(_, (c, _))| c).collect()
    }

    pub fn waiting(&self) -> usize {
        self.pending.len()
    }
}

pub struct WatcherHandle {
    watcher: RecommendedWatcher,
    pub roots: Vec<PathBuf>,
}

impl WatcherHandle {
    /// Watch each root recursively. Roots that cannot be watched (unmounted, deleted)
    /// are reported through the channel as a `Rescan` rather than failing the call, so
    /// one absent volume does not stop the rest.
    pub fn start(roots: &[PathBuf], emit: Sender<Change>) -> Result<WatcherHandle> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| Error::Config(format!("无法创建文件监听: {e}")))?;

        let mut watched = Vec::new();
        for root in roots {
            if !root.exists() {
                let _ = emit.send(Change::Rescan(root.clone()));
                continue;
            }
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => watched.push(root.clone()),
                Err(e) => {
                    tracing::warn!("监听 {} 失败: {e}", root.display());
                    let _ = emit.send(Change::Rescan(root.clone()));
                }
            }
        }

        let quiet = Duration::from_millis(1200);
        std::thread::spawn(move || {
            let mut deb = Debouncer::new(quiet);
            loop {
                match rx.recv_timeout(Duration::from_millis(300)) {
                    Ok(Ok(ev)) => {
                        for c in classify(ev) {
                            for ready in deb.push(c, Instant::now()) {
                                let _ = emit.send(ready);
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("监听错误: {e}");
                        let _ = emit.send(Change::Rescan(PathBuf::from("")));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        for ready in deb.push_timeout(Instant::now()) {
                            let _ = emit.send(ready);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        for c in deb.flush() {
                            let _ = emit.send(c);
                        }
                        return;
                    }
                }
            }
        });

        Ok(WatcherHandle { watcher, roots: watched })
    }

    pub fn unwatch(&mut self, root: &Path) {
        let _ = self.watcher.unwatch(root);
    }
}

impl Debouncer {
    /// Called on a timer so long-quiet paths still get released when no new events come.
    fn push_timeout(&mut self, now: Instant) -> Vec<Change> {
        let stale: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, (_, at))| now.saturating_duration_since(*at) >= self.quiet)
            .map(|(p, _)| p.clone())
            .collect();
        stale.into_iter().filter_map(|p| self.pending.remove(&p).map(|(c, _)| c)).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Not under any indexed root, or an extension we do not index.
    Ignored,
    /// New or changed content that the queue should pick up.
    NeedsIndex,
    /// Nothing to do: the file is byte-identical to what is already indexed.
    Unchanged,
    Removed,
    /// A directory moved or vanished; only a rescan can tell what happened.
    NeedsRescan,
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Apply one watched change to the index. A file that no longer exists is deleted from
/// the index, so results never point at a moved or trashed file.
pub fn apply_change(db: &crate::db::Db, change: &Change, wanted_kinds: &[crate::Kind], max_file_bytes: u64) -> crate::Result<Applied> {
    apply_change_with_options(db, change, &crate::scan::ScanOptions {
        kinds: wanted_kinds,
        max_file_bytes,
        exclude_dirs: &[],
        exclude_prefixes: &[],
    })
}

pub fn apply_change_with_options(db: &crate::db::Db, change: &Change, opts: &crate::scan::ScanOptions) -> crate::Result<Applied> {
    use crate::db;
    use crate::scan::Scanned;

    let path = change.path();
    let s = path.to_string_lossy().to_string();
    if !path.as_os_str().is_empty() {
        let Some(root_id) = root_for(db, path)? else { return Ok(Applied::Ignored) };
        let Some(root) = db.list_roots()?.into_iter().find(|r| r.id == root_id) else {
            // 根目录在校验和读取之间被删掉了：当作不在索引范围内，别把进程带崩。
            return Ok(Applied::Ignored);
        };
        if crate::scan::excluded_path(&crate::scan::expand_home(&root.path), path, opts) {
            return Ok(Applied::Ignored);
        }
    }
    if let Change::Rescan(_) = change {
        return if path.as_os_str().is_empty() || !path.exists() {
            Ok(Applied::NeedsRescan)
        } else if let Some(existing) = db.file_by_path(&s) {
            db.set_file_state(existing.id, crate::FileState::Stale, None)?;
            Ok(Applied::NeedsIndex)
        } else {
            Ok(Applied::NeedsRescan)
        };
    }

    if matches!(change, Change::Removed(_)) || !path.exists() {
        return match db.delete_by_path(&s)? {
            Some(_) => Ok(Applied::Removed),
            None => Ok(Applied::Ignored),
        };
    }

    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => return Ok(Applied::Ignored),
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let ext = ext_of(&name);
    if opts.kinds.is_empty() || !crate::scan::wanted(&ext, opts.kinds) || meta.len() > opts.max_file_bytes {
        return Ok(Applied::Ignored);
    }
    let root_id = match root_for(db, path)? {
        Some(id) => id,
        None => return Ok(Applied::Ignored),
    };
    let seen = db::now_ns();
    let kind = crate::Kind::from_ext(&ext);
    let scanned = Scanned {
        root_id,
        path: s,
        name,
        ext,
        kind,
        size: meta.len() as i64,
        mtime_ns: mtime_ns(&meta),
    };
    let (_, needs) = db.upsert_file(&scanned, seen)?;
    Ok(if needs { Applied::NeedsIndex } else { Applied::Unchanged })
}

/// The enabled root that contains `path`, longest match winning.
pub fn root_for(db: &crate::db::Db, path: &Path) -> crate::Result<Option<i64>> {
    let mut best: Option<(usize, i64)> = None;
    for root in db.list_roots()? {
        if !root.enabled {
            continue;
        }
        let rp = crate::scan::expand_home(&root.path);
        if path.starts_with(&rp) {
            let len = rp.as_os_str().len();
            if best.map(|(l, _)| len > l).unwrap_or(true) {
                best = Some((len, root.id));
            }
        }
    }
    Ok(best.map(|(_, id)| id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_obeys_hidden_directory_and_prefix_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("index.sqlite")).unwrap();
        db.add_root(&dir.path().to_string_lossy(), "test").unwrap();
        let kinds = [crate::Kind::Document];
        let prefixes = [dir.path().join("private").to_string_lossy().to_string()];
        let opts = crate::scan::ScanOptions {
            kinds: &kinds, max_file_bytes: 1024,
            exclude_dirs: &["node_modules".into()], exclude_prefixes: &prefixes,
        };
        for name in [".hidden/note.txt", "node_modules/note.txt", "private/note.txt"] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "excluded").unwrap();
            assert_eq!(apply_change_with_options(&db, &Change::Modified(path.clone()), &opts).unwrap(), Applied::Ignored);
            assert!(db.file_by_path(&path.to_string_lossy()).is_none());
        }
        let path = dir.path().join("note.txt");
        std::fs::write(&path, "included").unwrap();
        assert_eq!(apply_change_with_options(&db, &Change::Modified(path), &opts).unwrap(), Applied::NeedsIndex);
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn removal_is_immediate() {
        let mut d = Debouncer::new(Duration::from_millis(500));
        let out = d.push(Change::Removed(p("/a/b.jpg")), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(d.waiting(), 0);
    }

    #[test]
    fn writes_coalesce_into_one_change() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(100));
        assert!(d.push(Change::Created(p("/a/b.txt")), t0).is_empty());
        assert!(d.push(Change::Modified(p("/a/b.txt")), t0 + Duration::from_millis(30)).is_empty());
        assert!(d.push(Change::Modified(p("/a/b.txt")), t0 + Duration::from_millis(60)).is_empty());
        let out = d.push_timeout(t0 + Duration::from_millis(200));
        assert_eq!(out.len(), 1, "three writes must produce one re-index");
        // the first kind survives, so a brand-new file is not treated as a modification
        assert_eq!(out[0], Change::Created(p("/a/b.txt")));
    }

    #[test]
    fn distinct_paths_are_tracked_separately() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(50));
        d.push(Change::Modified(p("/a/1.txt")), t0);
        d.push(Change::Modified(p("/a/2.txt")), t0);
        assert_eq!(d.waiting(), 2);
        let out = d.push_timeout(t0 + Duration::from_millis(80));
        assert_eq!(out.len(), 2);
        assert_eq!(d.waiting(), 0);
    }

    #[test]
    fn flush_releases_pending_work() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_secs(30));
        d.push(Change::Created(p("/a/x.md")), t0);
        assert_eq!(d.flush().len(), 1);
        assert_eq!(d.waiting(), 0);
    }

    #[test]
    fn rename_asks_for_a_rescan() {
        let ev = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![p("/a/old.md")],
            attrs: Default::default(),
        };
        let out = classify(ev);
        assert!(matches!(out[0], Change::Rescan(_)), "{out:?}");
    }

    #[test]
    fn create_and_remove_map_directly() {
        let c = classify(Event { kind: EventKind::Create(notify::event::CreateKind::File), paths: vec![p("/x")], attrs: Default::default() });
        assert_eq!(c[0], Change::Created(p("/x")));
        let r = classify(Event { kind: EventKind::Remove(notify::event::RemoveKind::File), paths: vec![p("/y")], attrs: Default::default() });
        assert_eq!(r[0], Change::Removed(p("/y")));
    }

    #[test]
    fn watching_a_missing_root_reports_rescan_instead_of_failing() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = WatcherHandle::start(&[p("/definitely/not/here")], tx);
        assert!(handle.is_ok());
        let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(got, Change::Rescan(_)), "{got:?}");
        assert!(handle.unwrap().roots.is_empty(), "nothing was actually watched");
    }

    #[test]
    fn a_real_directory_can_be_watched() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = WatcherHandle::start(&[dir.path().to_path_buf()], tx).unwrap();
        assert_eq!(handle.roots.len(), 1);
        // drop the watcher so the background thread exits with the channel
        drop(handle);
        drop(rx);
    }
}
