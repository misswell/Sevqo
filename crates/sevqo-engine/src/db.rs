use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, Row};

use crate::{segment, EmbedRowKind, Error, FileRecord, FileState, Kind, Result, SearchRequest};

pub const SCHEMA_VERSION: i64 = 2;

/// Sequential migrations: `(target_version, SQL)`. Each entry upgrades a database from
/// `target_version - 1` and runs exactly once, inside its own transaction, after a
/// verified backup has been taken (P1-13). A database newer than this build refuses to
/// open rather than guessing at unknown columns.
const MIGRATIONS: &[(i64, &str)] = &[(
    2,
    // v2: chunk/clip gain per-unit index state (P0-03). Legacy rows were written under
    // the old regime: a row with an *activated* vec_map entry really has bytes on disk,
    // everything else (never reserved, or staged and never activated) needs re-embedding.
    // Done files carrying non-persisted units go back to `stale` so the queue picks them
    // up instead of silently keeping a partial index.
    r#"
    ALTER TABLE chunk ADD COLUMN status TEXT NOT NULL DEFAULT 'persisted';
    ALTER TABLE chunk ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE chunk ADD COLUMN error TEXT;
    ALTER TABLE clip ADD COLUMN status TEXT NOT NULL DEFAULT 'persisted';
    ALTER TABLE clip ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE clip ADD COLUMN error TEXT;
    UPDATE chunk SET status='pending' WHERE vec_row IS NULL
      OR id IN (SELECT ref_id FROM vec_map WHERE target_kind='chunk' AND alive=0);
    UPDATE clip SET status='pending' WHERE vec_row IS NULL
      OR id IN (SELECT ref_id FROM vec_map WHERE target_kind='clip' AND alive=0);
    UPDATE file SET state='stale' WHERE state='done' AND kind IN ('document','code')
      AND id IN (SELECT file_id FROM chunk WHERE status!='persisted');
    UPDATE file SET state='stale' WHERE state='done' AND kind='video'
      AND id IN (SELECT file_id FROM clip WHERE status!='persisted');
    CREATE INDEX IF NOT EXISTS chunk_status ON chunk(file_id, status);
    CREATE INDEX IF NOT EXISTS clip_status ON clip(file_id, status);
    "#,
    ),
    (
        3,
    // v3: 扫描履历（第六章 6.2）：每次扫描落一行结果，供诊断与状态页使用；
    // sweep 的安全性由调用方保证——只有完整成功且无读错的扫描才允许删除。
    r#"
    CREATE TABLE IF NOT EXISTS scan_run(
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      root_id INTEGER NOT NULL,
      status TEXT NOT NULL,
      started_ns INTEGER NOT NULL,
      finished_ns INTEGER NOT NULL,
      walked INTEGER NOT NULL DEFAULT 0,
      queued INTEGER NOT NULL DEFAULT 0,
      removed INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS scan_run_root ON scan_run(root_id, id);
    "#,
)];

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS root(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT NOT NULL UNIQUE,
  label TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  last_scan_ns INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS file(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  root_id INTEGER REFERENCES root(id) ON DELETE CASCADE,
  path TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  ext TEXT NOT NULL,
  kind TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  duration_ms INTEGER,
  width INTEGER,
  height INTEGER,
  state TEXT NOT NULL DEFAULT 'pending',
  error TEXT,
  indexed_ns INTEGER NOT NULL DEFAULT 0,
  thumb TEXT
);
CREATE INDEX IF NOT EXISTS file_state ON file(state, kind);
CREATE INDEX IF NOT EXISTS file_kind ON file(kind, state);
CREATE INDEX IF NOT EXISTS file_root ON file(root_id, state);

CREATE TABLE IF NOT EXISTS doc(
  file_id INTEGER PRIMARY KEY REFERENCES file(id) ON DELETE CASCADE,
  title TEXT,
  chars INTEGER NOT NULL DEFAULT 0,
  truncated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS text_store(
  file_id INTEGER PRIMARY KEY REFERENCES file(id) ON DELETE CASCADE,
  text TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunk(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,
  text TEXT NOT NULL,
  vec_row INTEGER,
  status TEXT NOT NULL DEFAULT 'pending',
  retry_count INTEGER NOT NULL DEFAULT 0,
  error TEXT
);
CREATE INDEX IF NOT EXISTS chunk_file ON chunk(file_id, ord);
CREATE INDEX IF NOT EXISTS chunk_status ON chunk(file_id, status);

CREATE TABLE IF NOT EXISTS clip(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,
  start_ms INTEGER NOT NULL,
  end_ms INTEGER NOT NULL,
  vec_row INTEGER,
  thumb TEXT,
  status TEXT NOT NULL DEFAULT 'pending',
  retry_count INTEGER NOT NULL DEFAULT 0,
  error TEXT
);
CREATE INDEX IF NOT EXISTS clip_file ON clip(file_id, start_ms);
CREATE INDEX IF NOT EXISTS clip_status ON clip(file_id, status);

CREATE TABLE IF NOT EXISTS vec_map(
  row_id INTEGER PRIMARY KEY,
  target_kind TEXT NOT NULL,
  ref_id INTEGER NOT NULL,
  file_id INTEGER NOT NULL,
  alive INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS vec_map_target ON vec_map(target_kind, ref_id);
CREATE INDEX IF NOT EXISTS vec_map_file ON vec_map(file_id);
CREATE INDEX IF NOT EXISTS vec_map_alive ON vec_map(alive);

CREATE TABLE IF NOT EXISTS scan_run(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  root_id INTEGER NOT NULL,
  status TEXT NOT NULL,
  started_ns INTEGER NOT NULL,
  finished_ns INTEGER NOT NULL,
  walked INTEGER NOT NULL DEFAULT 0,
  queued INTEGER NOT NULL DEFAULT 0,
  removed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS scan_run_root ON scan_run(root_id, id);

CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
  name_seg, path_seg, title_seg, body_seg,
  tokenize='unicode61', content='', contentless_delete=1
);
"#;

const FILE_COLS: &str =
    "id,root_id,path,name,ext,kind,size,mtime_ns,duration_ms,width,height,state,error,indexed_ns,thumb";

pub struct Db {
    path: PathBuf,
    write: Mutex<Connection>,
}

fn open_conn(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=8000;")?;
    Ok(c)
}

/// Upgrade an old database one migration step at a time (P1-13). Before touching
/// anything, `VACUUM INTO` writes a consistent snapshot next to the index; if the backup
/// fails, the migration is refused and the old database stays exactly as it was. Each
/// step runs in its own transaction together with its version bump, so a crash mid-step
/// simply leaves the old version to be retried on next launch.
fn migrate(c: &mut Connection, path: &Path, from: i64) -> Result<()> {
    let bak = path.with_extension(format!("bak-v{from}.sqlite"));
    let _ = std::fs::remove_file(&bak);
    c.execute("VACUUM INTO ?1", params![bak.to_string_lossy().to_string()])?;
    for (target, sql) in MIGRATIONS {
        if *target <= from {
            continue;
        }
        let tx = c.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('schema_version',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![target.to_string()],
        )?;
        tx.commit()?;
        tracing::info!("索引数据库已从 v{from} 迁移到 v{target}（备份: {}）", bak.display());
    }
    Ok(())
}

pub fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Image => "image",
        Kind::Video => "video",
        Kind::Document => "document",
        Kind::Code => "code",
        Kind::Audio => "audio",
        Kind::Other => "other",
    }
}

fn kind_of(s: &str) -> Kind {
    match s {
        "image" => Kind::Image,
        "video" => Kind::Video,
        "document" => Kind::Document,
        "code" => Kind::Code,
        "audio" => Kind::Audio,
        _ => Kind::Other,
    }
}

pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn read_file(r: &Row) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: r.get(0)?,
        root_id: r.get(1)?,
        path: r.get(2)?,
        name: r.get(3)?,
        ext: r.get(4)?,
        kind: kind_of(&r.get::<_, String>(5)?),
        size: r.get(6)?,
        mtime_ns: r.get(7)?,
        duration_ms: r.get(8)?,
        width: r.get(9)?,
        height: r.get(10)?,
        state: FileState::parse(&r.get::<_, String>(11)?),
        error: r.get(12)?,
        indexed_ns: r.get(13)?,
        thumb: r.get(14)?,
    })
}

/// Terminal-state roll-up of one file's embeddable units (chunk/clip rows; an image's
/// unit lives at file level and never appears here).
#[derive(Debug, Clone, PartialEq)]
pub struct UnitSummary {
    pub total: i64,
    pub persisted: i64,
    /// failures whose retry_count reached the cap
    pub exhausted: i64,
    /// failures still allowed to re-queue
    pub retryable: i64,
    pub error: Option<String>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut c = open_conn(path)?;
        // v2 的 DDL 会建引用新列的索引，因此必须先确定版本：全新库一次事务建表+记版本；
        // 旧库先迁移补列，最后再跑一遍幂等 DDL 兜底。
        let stored: Option<String> = c
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .ok();
        let known_tables: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('file','chunk')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let version = match stored {
            None if known_tables == 0 => {
                let tx = c.transaction()?;
                tx.execute_batch(DDL)?;
                tx.execute("INSERT INTO meta(key,value) VALUES('schema_version',?)", params![SCHEMA_VERSION.to_string()])?;
                tx.commit()?;
                SCHEMA_VERSION
            }
            // 有表但没记版本：最老的 v1 库。
            None => 1,
            Some(v) => v.parse::<i64>().unwrap_or(0),
        };
        if version > SCHEMA_VERSION {
            return Err(Error::Config(format!(
                "索引数据库版本 {version} 比当前程序要求的 {SCHEMA_VERSION} 更新，请升级程序后打开：{}",
                path.display()
            )));
        }
        if version < SCHEMA_VERSION {
            migrate(&mut c, path, version)?;
        }
        c.execute_batch(DDL)?;
        // A crash mid-item must not leave rows stuck in `indexing` forever.
        c.execute("UPDATE file SET state='pending' WHERE state='indexing'", [])?;
        // Older builds only inserted FTS rows when a document had body text,
        // leaving images, videos and empty documents unsearchable by filename.
        let missing: Vec<(i64, String, String)> = {
            let mut stmt = c.prepare("SELECT id,name,path FROM file WHERE state='done' AND id NOT IN (SELECT rowid FROM fts)")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (id, name, path) in missing {
            ensure_filename_row(&c, id, &name, &path)?;
        }
        Ok(Db { path: path.to_path_buf(), write: Mutex::new(c) })
    }

    pub fn read(&self) -> Result<Connection> {
        open_conn(&self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get_meta(&self, key: &str) -> Option<String> {
        let g = self.write.lock().ok()?;
        g.query_row("SELECT value FROM meta WHERE key=?", params![key], |r| r.get(0)).ok()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute(
            "INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("DELETE FROM meta WHERE key=?", params![key])?;
        Ok(())
    }

    // ---------------------------------------------------------------- roots

    pub fn add_root(&self, path: &str, label: &str) -> Result<i64> {
        let g = self.write.lock().unwrap();
        g.execute(
            "INSERT INTO root(path,label) VALUES(?,?) ON CONFLICT(path) DO UPDATE SET label=excluded.label, enabled=1",
            params![path, label],
        )?;
        let id: i64 = g.query_row("SELECT id FROM root WHERE path=?", params![path], |r| r.get(0))?;
        Ok(id)
    }

    pub fn list_roots(&self) -> Result<Vec<crate::Root>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare("SELECT id,path,label,enabled,last_scan_ns FROM root ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(crate::Root {
                id: r.get(0)?,
                path: r.get(1)?,
                label: r.get(2)?,
                enabled: matches!(r.get::<_, i64>(3)?, 1),
                last_scan_ns: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Disabling only hides a root; re-enabling must not throw away work. A file that
    /// is `done` and still owns a live vector row is unchanged evidence — resetting it to
    /// `pending` would re-embed the whole root for nothing. Failed files get one retry,
    /// `done` files without vectors (text-only era) queue for the semantic backfill the
    /// same way [`Self::mark_missing_vectors`] would.
    pub fn set_root_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("UPDATE root SET enabled=? WHERE id=?", params![enabled as i64, id])?;
        if enabled {
            g.execute(
                "UPDATE file SET state='stale',error=NULL WHERE root_id=? AND state='failed'",
                params![id],
            )?;
            g.execute(
                "UPDATE file SET state='stale' WHERE root_id=? AND state='done' \
                 AND id NOT IN (SELECT file_id FROM vec_map WHERE alive=1)",
                params![id],
            )?;
        }
        Ok(())
    }

    pub fn remove_root(&self, id: i64) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let ids: Vec<i64> = {
            let mut st = g.prepare("SELECT id FROM file WHERE root_id=?")?;
            let rows = st.query_map(params![id], |r| r.get(0))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        let n = ids.len();
        purge_files(&g, &ids)?;
        g.execute("DELETE FROM root WHERE id=?", params![id])?;
        Ok(n)
    }

    // ------------------------------------------------------------ scanning

    pub fn begin_scan(&self, root_id: i64) -> Result<i64> {
        let ns = now_ns();
        let g = self.write.lock().unwrap();
        g.execute("UPDATE root SET last_scan_ns=? WHERE id=?", params![ns, root_id])?;
        Ok(ns)
    }

    /// 记录一次扫描结果（6.2）：每根目录只保留最近 50 条，履历不会无限增长。
    pub fn record_scan_run(&self, root_id: i64, status: &str, started_ns: i64, walked: u64, queued: u64, removed: u64) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute(
            "INSERT INTO scan_run(root_id,status,started_ns,finished_ns,walked,queued,removed) VALUES(?,?,?,?,?,?,?)",
            params![root_id, status, started_ns, now_ns(), walked as i64, queued as i64, removed as i64],
        )?;
        g.execute(
            "DELETE FROM scan_run WHERE root_id=?1 AND id NOT IN              (SELECT id FROM scan_run WHERE root_id=?1 ORDER BY id DESC LIMIT 50)",
            params![root_id],
        )?;
        Ok(())
    }

    /// 最近一次扫描履历，供诊断与状态展示。
    pub fn last_scan_run(&self, root_id: i64) -> Result<Option<(String, i64, i64)>> {
        let g = self.write.lock().unwrap();
        let row = g
            .query_row(
                "SELECT status,walked,removed FROM scan_run WHERE root_id=? ORDER BY id DESC LIMIT 1",
                params![root_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
            )
            .ok();
        Ok(row)
    }

    /// Insert or refresh one discovered file. `seen_ns` marks the file as present in
    /// this scan; anything under the root with an older mark is gone from disk.
    pub fn upsert_file(&self, f: &crate::scan::Scanned, seen_ns: i64) -> Result<(i64, bool)> {
        let g = self.write.lock().unwrap();
        let existing: Option<(i64, i64, i64, String)> = g
            .query_row(
                "SELECT id,size,mtime_ns,state FROM file WHERE path=?",
                params![f.path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        match existing {
            Some((id, size, mtime, _state)) if size == f.size && mtime == f.mtime_ns => {
                g.execute("UPDATE file SET indexed_ns=? WHERE id=?", params![seen_ns, id])?;
                let needs = g.query_row("SELECT state FROM file WHERE id=?", params![id], |r| r.get::<_, String>(0))
                    .map(|s| s != "done" && s != "skipped")
                    .unwrap_or(true);
                Ok((id, needs))
            }
            Some((id, _size, _mtime, _)) => {
                g.execute(
                    "UPDATE file SET size=?,mtime_ns=?,state='pending',error=NULL,root_id=?,kind=?,ext=?,name=?,indexed_ns=? WHERE id=?",
                    params![f.size, f.mtime_ns, f.root_id, kind_str(f.kind), f.ext, f.name, seen_ns, id],
                )?;
                purge_derived(&g, id)?;
                Ok((id, true))
            }
            None => {
                g.execute(
                    "INSERT INTO file(root_id,path,name,ext,kind,size,mtime_ns,state,indexed_ns) VALUES(?,?,?,?,?,?,?,'pending',?)",
                    params![f.root_id, f.path, f.name, f.ext, kind_str(f.kind), f.size, f.mtime_ns, seen_ns],
                )?;
                Ok((g.last_insert_rowid(), true))
            }
        }
    }

    /// Delete files under `root_id` that this scan did not see.
    pub fn sweep(&self, root_id: i64, seen_ns: i64) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let ids: Vec<i64> = {
            let mut st = g.prepare("SELECT id FROM file WHERE root_id=? AND indexed_ns < ?")?;
            let rows = st.query_map(params![root_id, seen_ns], |r| r.get(0))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        let n = ids.len();
        purge_files(&g, &ids)?;
        Ok(n)
    }

    // ---------------------------------------------------------------- files

    pub fn set_file_meta(
        &self,
        id: i64,
        duration_ms: Option<i64>,
        width: Option<i32>,
        height: Option<i32>,
        thumb: Option<&str>,
    ) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute(
            "UPDATE file SET duration_ms=COALESCE(?,duration_ms),width=COALESCE(?,width),height=COALESCE(?,height),thumb=COALESCE(?,thumb) WHERE id=?",
            params![duration_ms, width, height, thumb, id],
        )?;
        Ok(())
    }

    pub fn set_file_state(&self, id: i64, state: FileState, error: Option<&str>) -> Result<()> {
        let g = self.write.lock().unwrap();
        if state == FileState::Done {
            let (name, path): (String, String) = g.query_row("SELECT name,path FROM file WHERE id=?", params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            ensure_filename_row(&g, id, &name, &path)?;
        }
        g.execute("UPDATE file SET state=?,error=?,indexed_ns=? WHERE id=?", params![state.as_str(), error, now_ns(), id])?;
        Ok(())
    }

    pub fn recover_indexing_files(&self, ids: &[i64]) -> Result<()> {
        let g = self.write.lock().unwrap();
        for id in ids {
            g.execute("UPDATE file SET state='pending' WHERE id=? AND state='indexing'", params![id])?;
        }
        Ok(())
    }

    pub fn file(&self, id: i64) -> Option<FileRecord> {
        let g = self.write.lock().ok()?;
        g.query_row(&format!("SELECT {FILE_COLS} FROM file WHERE id=?"), params![id], read_file).ok()
    }

    pub fn file_by_path(&self, path: &str) -> Option<FileRecord> {
        let g = self.write.lock().ok()?;
        g.query_row(&format!("SELECT {FILE_COLS} FROM file WHERE path=?"), params![path], read_file).ok()
    }

    pub fn files_by_ids(&self, ids: &[i64]) -> Result<HashMap<i64, FileRecord>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let g = self.write.lock().unwrap();
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT {FILE_COLS} FROM file WHERE id IN ({placeholders})");
        let mut st = g.prepare(&sql)?;
        let mut out = HashMap::new();
        for rec in st.query_map(rusqlite::params_from_iter(ids.iter()), read_file)?.flatten() {
            out.insert(rec.id, rec);
        }
        Ok(out)
    }

    /// Work queue for the indexer: images first (fast wins in the UI), then videos,
    /// then documents/code.
    pub fn pending(&self, limit: usize, kinds: &[Kind]) -> Result<Vec<FileRecord>> {
        let g = self.write.lock().unwrap();
        let mut sql = format!("SELECT {FILE_COLS} FROM file WHERE state IN ('pending','stale') AND root_id IN (SELECT id FROM root WHERE enabled=1)");
        if !kinds.is_empty() {
            let list = kinds.iter().map(|k| format!("'{}'", kind_str(*k))).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND kind IN ({list})"));
        }
        sql.push_str(
            " ORDER BY CASE kind WHEN 'image' THEN 0 WHEN 'video' THEN 1 WHEN 'document' THEN 2 WHEN 'code' THEN 3 ELSE 4 END, size ASC LIMIT ?",
        );
        let mut st = g.prepare(&sql)?;
        let rows = st.query_map(params![limit as i64], read_file)?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Same queue as [`Self::pending`], counted instead of materialised: the progress
    /// readout needs the denominator on every batch, not the rows.
    pub fn pending_count(&self, kinds: &[Kind]) -> Result<u64> {
        let g = self.write.lock().unwrap();
        let mut sql = "SELECT count(*) FROM file WHERE state IN ('pending','stale') AND root_id IN (SELECT id FROM root WHERE enabled=1)".to_string();
        if !kinds.is_empty() {
            let list = kinds.iter().map(|k| format!("'{}'", kind_str(*k))).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND kind IN ({list})"));
        }
        let n: i64 = g.query_row(&sql, [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    pub fn counts(&self) -> Result<HashMap<String, i64>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare("SELECT state, count(*) FROM file GROUP BY state")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    pub fn delete_by_path(&self, path: &str) -> Result<Option<i64>> {
        let g = self.write.lock().unwrap();
        let id: Option<i64> = g.query_row("SELECT id FROM file WHERE path=?", params![path], |r| r.get(0)).ok();
        if let Some(id) = id {
            purge_files(&g, &[id])?;
        }
        Ok(id)
    }

    /// Delete a path and everything beneath it (P1-02): a removed *directory* usually
    /// arrives as one event for the directory itself, so matching only the exact path
    /// would orphan every child row — with its FTS text, clips and vector mappings —
    /// until an unrelated full rescan. The prefix is dir-scoped (`dir/…`), so a file
    /// whose name merely starts like the deleted path is never touched.
    pub fn delete_tree(&self, path: &str) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let ids: Vec<i64> = {
            let mut st = g.prepare(
                "SELECT id FROM file WHERE path=? OR substr(path,1,?)=?||'/'",
            )?;
            let rows = st.query_map(params![path, path.chars().count() as i64 + 1, path], |r| r.get(0))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        let n = ids.len();
        if n > 0 {
            purge_files(&g, &ids)?;
        }
        Ok(n)
    }

    // ------------------------------------------------------------- full text

    /// Store extracted text plus the segmented FTS row. `fts` is contentless, so
    /// snippets must be built from `text_store` in Rust, not with snippet().
    pub fn store_text(&self, file_id: i64, title: Option<&str>, text: &str, truncated: bool) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("DELETE FROM fts WHERE rowid=?", params![file_id])?;
        if text.trim().is_empty() && title.map(|t| t.trim().is_empty()).unwrap_or(true) {
            g.execute("DELETE FROM doc WHERE file_id=?", params![file_id])?;
            g.execute("DELETE FROM text_store WHERE file_id=?", params![file_id])?;
            return Ok(());
        }
        let path: String = g.query_row("SELECT path FROM file WHERE id=?", params![file_id], |r| r.get(0))?;
        let name: String = g.query_row("SELECT name FROM file WHERE id=?", params![file_id], |r| r.get(0))?;
        if !text.is_empty() {
            g.execute(
                "INSERT INTO text_store(file_id,text) VALUES(?,?) ON CONFLICT(file_id) DO UPDATE SET text=excluded.text",
                params![file_id, text],
            )?;
        } else {
            g.execute("DELETE FROM text_store WHERE file_id=?", params![file_id])?;
        }
        g.execute(
            "INSERT INTO doc(file_id,title,chars,truncated) VALUES(?,?,?,?) ON CONFLICT(file_id) DO UPDATE SET title=excluded.title,chars=excluded.chars,truncated=excluded.truncated",
            params![file_id, title, text.chars().count() as i64, truncated as i64],
        )?;
        g.execute(
            "INSERT INTO fts(rowid,name_seg,path_seg,title_seg,body_seg) VALUES(?,?,?,?,?)",
            params![
                file_id,
                segment::segment(&name),
                segment::segment(&path),
                segment::segment(title.unwrap_or("")),
                segment::segment(text)
            ],
        )?;
        Ok(())
    }

    pub fn stored_text(&self, file_id: i64) -> Option<String> {
        let g = self.write.lock().ok()?;
        g.query_row("SELECT text FROM text_store WHERE file_id=?", params![file_id], |r| r.get(0)).ok()
    }

    /// Extracted body character count (0 when the file has no doc row). Lets the
    /// aggregator derive semantic-coverage notes without new schema.
    pub fn doc_char_count(&self, file_id: i64) -> Result<i64> {
        let g = self.write.lock().unwrap();
        let n: i64 = g.query_row(
            "SELECT COALESCE((SELECT chars FROM doc WHERE file_id=?1),0)",
            params![file_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    pub fn doc_title(&self, file_id: i64) -> Option<String> {
        let g = self.write.lock().ok()?;
        g.query_row("SELECT title FROM doc WHERE file_id=?", params![file_id], |r| r.get(0)).ok().flatten()
    }

    /// Insert the chunk set for a file, preserving rows whose text is unchanged so their
    /// per-unit state — persisted vector, retry history — survives a re-queue (P0-03).
    /// Changed or new chunks start `pending`; vanished ones are removed and their vector
    /// rows tombstoned. Identity is `(ord, text)`: an edit that shifts chunk boundaries
    /// re-embeds the shifted tail, which is acceptable until content fingerprints (P1-03)
    /// refine the identity.
    pub fn sync_chunks(&self, file_id: i64, chunks: &[String]) -> Result<Vec<(i64, usize)>> {
        let g = self.write.lock().unwrap();
        let old: Vec<(i64, i64, String)> = {
            let mut st = g.prepare("SELECT id,ord,text FROM chunk WHERE file_id=? ORDER BY ord")?;
            let rows = st.query_map(params![file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        let mut out = Vec::with_capacity(chunks.len());
        for (ord, text) in chunks.iter().enumerate() {
            let ord = ord as i64;
            match old.iter().find(|(_, o, _)| *o == ord) {
                Some((id, _, prev)) if prev == text => out.push((*id, ord as usize)),
                Some((id, _, _)) => {
                    g.execute("UPDATE vec_map SET alive=0 WHERE target_kind='chunk' AND ref_id=?", params![id])?;
                    g.execute(
                        "UPDATE chunk SET text=?,status='pending',retry_count=0,error=NULL,vec_row=NULL WHERE id=?",
                        params![text, id],
                    )?;
                    out.push((*id, ord as usize));
                }
                None => {
                    g.execute(
                        "INSERT INTO chunk(file_id,ord,text) VALUES(?,?,?)",
                        params![file_id, ord, text],
                    )?;
                    out.push((g.last_insert_rowid(), ord as usize));
                }
            }
        }
        for (id, _ord, _) in old.iter().filter(|(_, o, _)| *o >= chunks.len() as i64) {
            g.execute("UPDATE vec_map SET alive=0 WHERE target_kind='chunk' AND ref_id=?", params![id])?;
            g.execute("DELETE FROM chunk WHERE id=?", params![id])?;
        }
        Ok(out)
    }

    /// Same preservation contract as [`Self::sync_chunks`], keyed by the clip's time
    /// range: unchanged windows keep their units, moved or resized windows re-embed.
    pub fn sync_clips(&self, file_id: i64, clips: &[(f64, f64)]) -> Result<Vec<(i64, usize)>> {
        let g = self.write.lock().unwrap();
        let old: Vec<(i64, i64, i64, i64)> = {
            let mut st = g.prepare("SELECT id,ord,start_ms,end_ms FROM clip WHERE file_id=? ORDER BY ord")?;
            let rows = st.query_map(params![file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.filter_map(|x| x.ok()).collect()
        };
        let mut out = Vec::with_capacity(clips.len());
        for (ord, (start, end)) in clips.iter().enumerate() {
            let ord = ord as i64;
            let start_ms = (start * 1000.0).round() as i64;
            let end_ms = (end * 1000.0).round() as i64;
            let existing = old.iter().find(|(_, o, _, _)| *o == ord);
            match existing {
                // Same slot, same window: keep unit state untouched.
                Some((id, _, s, e)) if *s == start_ms && *e == end_ms => out.push((*id, ord as usize)),
                Some((id, _, _, _)) => {
                    g.execute("UPDATE vec_map SET alive=0 WHERE target_kind='clip' AND ref_id=?", params![id])?;
                    g.execute(
                        "UPDATE clip SET start_ms=?,end_ms=?,status='pending',retry_count=0,error=NULL,vec_row=NULL WHERE id=?",
                        params![start_ms, end_ms, id],
                    )?;
                    out.push((*id, ord as usize));
                }
                None => {
                    g.execute(
                        "INSERT INTO clip(file_id,ord,start_ms,end_ms) VALUES(?,?,?,?)",
                        params![file_id, ord, start_ms, end_ms],
                    )?;
                    out.push((g.last_insert_rowid(), ord as usize));
                }
            }
        }
        for (id, _ord, _, _) in old.iter().filter(|(_, o, _, _)| *o >= clips.len() as i64) {
            g.execute("UPDATE vec_map SET alive=0 WHERE target_kind='clip' AND ref_id=?", params![id])?;
            g.execute("DELETE FROM clip WHERE id=?", params![id])?;
        }
        Ok(out)
    }

    pub fn clip_rows(&self, file_id: i64) -> Result<Vec<(i64, i64, i64, Option<i64>, Option<String>)>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare("SELECT id,start_ms,end_ms,vec_row,thumb FROM clip WHERE file_id=? ORDER BY ord")?;
        let rows = st.query_map(params![file_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    pub fn chunk_rows(&self, file_id: i64) -> Result<Vec<(i64, String)>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare("SELECT id,text FROM chunk WHERE file_id=? ORDER BY ord")?;
        let rows = st.query_map(params![file_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    // ------------------------------------------------- unit-level index state (P0-03)

    /// Chunks still missing a persisted vector, in order. This is what makes per-unit
    /// retry possible: a pass embeds exactly these, never the units already done.
    pub fn chunks_needing_embed(&self, file_id: i64) -> Result<Vec<(i64, String)>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare(
            "SELECT id,text FROM chunk WHERE file_id=? AND status!='persisted' ORDER BY ord",
        )?;
        let rows = st.query_map(params![file_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Clips still missing a persisted vector, as `(id, start_s, end_s)` in order.
    pub fn clips_needing_embed(&self, file_id: i64) -> Result<Vec<(i64, f64, f64)>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare(
            "SELECT id,start_ms,end_ms FROM clip WHERE file_id=? AND status!='persisted' ORDER BY ord",
        )?;
        let rows = st.query_map(params![file_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as f64 / 1000.0, r.get::<_, i64>(2)? as f64 / 1000.0))
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Whether an embeddable unit already owns an activated (searchable) vector row.
    /// Images use this to skip re-encoding an unchanged picture.
    pub fn has_active_vector(&self, target: EmbedRowKind, ref_id: i64) -> Result<bool> {
        let g = self.write.lock().unwrap();
        let hit: Option<i64> = g
            .query_row(
                "SELECT 1 FROM vec_map WHERE target_kind=? AND ref_id=? AND alive=1",
                params![target.as_str(), ref_id],
                |r| r.get(0),
            )
            .ok();
        Ok(hit.is_some())
    }

    /// Publish successful units: their vector bytes are durable, so flip the unit state
    /// to `persisted`. Image units live at file level and need no row here.
    pub fn mark_units_persisted(&self, units: &[(EmbedRowKind, i64)]) -> Result<()> {
        let g = self.write.lock().unwrap();
        for (target, ref_id) in units {
            match target {
                EmbedRowKind::Chunk => {
                    g.execute(
                        "UPDATE chunk SET status='persisted',error=NULL WHERE id=?",
                        params![ref_id],
                    )?;
                }
                EmbedRowKind::Clip => {
                    g.execute(
                        "UPDATE clip SET status='persisted',error=NULL WHERE id=?",
                        params![ref_id],
                    )?;
                }
                EmbedRowKind::Image => {}
            }
        }
        Ok(())
    }

    /// Record a failed embed attempt per unit: keeps the reason, counts the retry. The
    /// caller aggregates file state from these (see [`Self::unit_summary`]).
    pub fn mark_units_failed(&self, units: &[(EmbedRowKind, i64)], err: &str) -> Result<()> {
        let g = self.write.lock().unwrap();
        for (target, ref_id) in units {
            match target {
                EmbedRowKind::Chunk => {
                    g.execute(
                        "UPDATE chunk SET status='failed',retry_count=retry_count+1,error=? WHERE id=?",
                        params![err, ref_id],
                    )?;
                }
                EmbedRowKind::Clip => {
                    g.execute(
                        "UPDATE clip SET status='failed',retry_count=retry_count+1,error=? WHERE id=?",
                        params![err, ref_id],
                    )?;
                }
                EmbedRowKind::Image => {}
            }
        }
        Ok(())
    }

    /// Terminal-state roll-up of a file's units. A file is only `done` when every unit
    /// is persisted; retryable failures re-queue the file, exhausted ones surface as a
    /// diagnosable failure.
    pub fn unit_summary(&self, file_id: i64, retry_max: i64) -> Result<UnitSummary> {
        let g = self.write.lock().unwrap();
        let sum = g.query_row(
            "SELECT count(*),
                    COALESCE(SUM(status='persisted'),0),
                    COALESCE(SUM(status='failed' AND retry_count>=?2),0),
                    COALESCE(SUM(status='failed' AND retry_count<?2),0),
                    (SELECT error FROM chunk WHERE file_id=?1 AND status='failed'
                     UNION ALL
                     SELECT error FROM clip WHERE file_id=?1 AND status='failed' LIMIT 1)
             FROM (SELECT status,retry_count FROM chunk WHERE file_id=?1
                   UNION ALL
                   SELECT status,retry_count FROM clip WHERE file_id=?1)",
            params![file_id, retry_max],
            |r| {
                Ok(UnitSummary {
                    total: r.get(0)?,
                    persisted: r.get(1)?,
                    exhausted: r.get(2)?,
                    retryable: r.get(3)?,
                    error: r.get(4)?,
                })
            },
        )?;
        Ok(sum)
    }

    // -------------------------------------------------------------- vectors

    /// Allocate the next physical row in the vector file for an embeddable unit.
    ///
    /// The row starts **staged** (`alive=0`): its bytes do not exist yet, so the mapping
    /// must stay invisible to search. Only [`Self::activate_vec_rows`] — called after the
    /// vector bytes are durably written and flushed — publishes it (P0-02). A crash in
    /// between leaves orphaned staged rows that compaction reclaims; a staged row can
    /// never surface as a valid mapping pointing at missing bytes.
    pub fn reserve_vec_row(&self, target: EmbedRowKind, ref_id: i64, file_id: i64) -> Result<i64> {
        let g = self.write.lock().unwrap();
        g.execute("UPDATE vec_map SET alive=0 WHERE target_kind=? AND ref_id=?", params![target.as_str(), ref_id])?;
        let next: i64 = g.query_row("SELECT COALESCE(MAX(row_id),-1)+1 FROM vec_map", [], |r| r.get(0))?;
        g.execute(
            "INSERT INTO vec_map(row_id,target_kind,ref_id,file_id,alive) VALUES(?,?,?,?,0)",
            params![next, target.as_str(), ref_id, file_id],
        )?;
        match target {
            EmbedRowKind::Chunk => {
                g.execute("UPDATE chunk SET vec_row=? WHERE id=?", params![next, ref_id])?;
            }
            EmbedRowKind::Clip => {
                g.execute("UPDATE clip SET vec_row=? WHERE id=?", params![next, ref_id])?;
            }
            EmbedRowKind::Image => {}
        }
        Ok(next)
    }

    /// Publish staged rows once their bytes are durably on disk. Idempotent: activating
    /// twice is a no-op, and a unit never owns two live rows because re-reserving it
    /// tombstones the previous one first.
    pub fn activate_vec_rows(&self, rows: &[i64]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let g = self.write.lock().unwrap();
        let placeholders = vec!["?"; rows.len()].join(",");
        g.execute(
            &format!("UPDATE vec_map SET alive=1 WHERE row_id IN ({placeholders})"),
            rusqlite::params_from_iter(rows.iter()),
        )?;
        Ok(())
    }

    pub fn set_clip_thumb(&self, clip_id: i64, thumb: &str) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("UPDATE clip SET thumb=? WHERE id=?", params![thumb, clip_id])?;
        Ok(())
    }

    pub fn vec_rows(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        Ok(g.query_row("SELECT COALESCE(MAX(row_id),-1)+1 FROM vec_map", [], |r| r.get(0))?)
    }

    pub fn alive_vec_rows(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        Ok(g.query_row("SELECT count(*) FROM vec_map WHERE alive=1", [], |r| r.get(0))?)
    }

    /// Which embedded unit owns a row id — used to attach thumbnails after embedding.
    pub fn vec_owner(&self, row: i64) -> Option<(EmbedRowKind, i64, i64)> {
        let g = self.write.lock().ok()?;
        g.query_row(
            "SELECT target_kind,ref_id,file_id FROM vec_map WHERE row_id=?",
            params![row],
            |r| Ok((EmbedRowKind::parse(&r.get::<_, String>(0)?), r.get(1)?, r.get(2)?)),
        )
        .ok()
    }

    pub fn live_rows_for_compaction(&self) -> Result<Vec<(i64, i64)>> {
        let g = self.write.lock().unwrap();
        let mut st = g.prepare("SELECT row_id FROM vec_map WHERE alive=1 ORDER BY row_id")?;
        let olds: Vec<i64> = st.query_map([], |r| r.get(0))?.filter_map(|x| x.ok()).collect();
        Ok(olds.into_iter().enumerate().map(|(i, old)| (old, i as i64)).collect())
    }

    /// After compaction, rewrite row ids and every `vec_row` pointer *and* flip the
    /// active vector generation — all inside one transaction (P0-01). This commit is the
    /// switch point between generations: before it the new file is an orphan, after it
    /// the new file is authoritative. `mapping` must be ascending by old row id, as
    /// `live_rows_for_compaction` returns.
    pub fn commit_compaction(
        &self,
        mapping: &[(i64, i64)],
        generation: u64,
        rows: usize,
        dim: usize,
        sha256: &str,
    ) -> Result<usize> {
        let mut g = self.write.lock().unwrap();
        let tx = g.transaction()?;
        tx.execute("UPDATE chunk SET vec_row=NULL WHERE vec_row IN (SELECT row_id FROM vec_map WHERE alive=0)", [])?;
        tx.execute("UPDATE clip SET vec_row=NULL WHERE vec_row IN (SELECT row_id FROM vec_map WHERE alive=0)", [])?;
        let dropped = tx.execute("DELETE FROM vec_map WHERE alive=0", [])?;
        for (old, new) in mapping {
            if old == new {
                continue;
            }
            tx.execute("UPDATE vec_map SET row_id=? WHERE row_id=?", params![new, old])?;
            tx.execute("UPDATE chunk SET vec_row=? WHERE vec_row=?", params![new, old])?;
            tx.execute("UPDATE clip SET vec_row=? WHERE vec_row=?", params![new, old])?;
        }
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('vec_generation',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![generation.to_string()],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('vec_row_count',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![rows.to_string()],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('vec_dim',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![dim.to_string()],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('vec_sha256',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![sha256],
        )?;
        tx.commit()?;
        Ok(dropped)
    }

    pub fn dead_rows(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        Ok(g.query_row("SELECT count(*) FROM vec_map WHERE alive=0", [], |r| r.get(0))?)
    }

    /// Searchable vector rows, pre-filtered by modality so the scan can skip whole blocks.
    pub fn vec_candidates(&self, kinds: &[Kind]) -> Result<Vec<crate::VecRow>> {
        let mut req = SearchRequest::default();
        req.filters.kinds = kinds.to_vec();
        self.vec_candidates_for(&req)
    }

    pub fn vec_candidates_for(&self, req: &SearchRequest) -> Result<Vec<crate::VecRow>> {
        let g = self.write.lock().unwrap();
        let mut sql = String::from(
            "SELECT m.row_id,m.target_kind,m.ref_id,m.file_id,f.kind FROM vec_map m JOIN file f ON f.id=m.file_id WHERE m.alive=1 AND f.state='done'",
        );
        let kinds = &req.filters.kinds;
        if !kinds.is_empty() {
            let mut list: Vec<String> = Vec::new();
            for k in kinds {
                let t = match k {
                    Kind::Image => "'image'",
                    Kind::Video => "'clip'",
                    Kind::Document | Kind::Code => "'chunk'",
                    _ => continue,
                };
                list.push(t.to_string());
            }
            list.sort();
            list.dedup();
            if list.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(" AND m.target_kind IN ({})", list.join(",")));
        }
        let mut bind: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        push_filters(&mut sql, &mut bind, req);
        let mut st = g.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(bind.iter()), |r| -> rusqlite::Result<crate::VecRow> {
            Ok(crate::VecRow {
                row: r.get(0)?,
                target: EmbedRowKind::parse(&r.get::<_, String>(1)?),
                ref_id: r.get(2)?,
                file_id: r.get(3)?,
                file_kind: kind_of(&r.get::<_, String>(4)?),
            })
        })?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    pub fn mark_dead_for_files(&self, file_ids: &[i64]) -> Result<usize> {
        if file_ids.is_empty() {
            return Ok(0);
        }
        let g = self.write.lock().unwrap();
        let placeholders = vec!["?"; file_ids.len()].join(",");
        let sql = format!("UPDATE vec_map SET alive=0 WHERE file_id IN ({placeholders})");
        let n = g.execute(&sql, rusqlite::params_from_iter(file_ids.iter()))?;
        Ok(n)
    }

    /// Reclaim rows whose file is no longer indexed; returns rows freed.
    pub fn invalidate_stale_rows(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let n = g.execute("UPDATE vec_map SET alive=0 WHERE alive=1 AND file_id IN (SELECT id FROM file WHERE state!='done')", [])?;
        Ok(n)
    }

    pub fn reset_vectors(&self) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("DELETE FROM vec_map", [])?;
        for t in ["chunk", "clip"] {
            g.execute(&format!("UPDATE {t} SET vec_row=NULL,status='pending',retry_count=0,error=NULL"), [])?;
        }
        Ok(())
    }

    pub fn reindex_all(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let n = g.execute("UPDATE file SET state='pending',error=NULL", [])?;
        g.execute("DELETE FROM vec_map", [])?;
        for t in ["chunk", "clip"] {
            g.execute(&format!("UPDATE {t} SET vec_row=NULL,status='pending',retry_count=0,error=NULL"), [])?;
        }
        Ok(n)
    }

    /// Files indexed for text only: queue them for the semantic pass once the model is
    /// available. Skipped kinds are left alone, and disabled roots stay untouched — their
    /// files cannot be processed, so queueing them only inflates the progress readout.
    pub fn mark_missing_vectors(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let n = g.execute(
            "UPDATE file SET state='stale' WHERE state='done' AND kind IN ('image','video','document','code') \
             AND root_id IN (SELECT id FROM root WHERE enabled=1) \
             AND id NOT IN (SELECT file_id FROM vec_map WHERE alive=1)",
            [],
        )?;
        Ok(n)
    }

    // --------------------------------------------------------------- search

    /// Keyword channel: BM25 over the segmented index. Column weights favour the file
    /// name, which is what people type when they half-remember a filename.
    pub fn keyword_search(&self, expr: &str, req: &SearchRequest, limit: usize) -> Result<Vec<(i64, f64)>> {
        let g = self.write.lock().unwrap();
        let mut sql = String::from(
            "SELECT f.id, bm25(fts, 12.0, 2.0, 6.0, 1.0) FROM fts JOIN file f ON f.id=fts.rowid WHERE fts MATCH ?",
        );
        let mut bind: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(expr.to_string())];
        push_filters(&mut sql, &mut bind, req);
        sql.push_str(" ORDER BY 2 LIMIT ?");
        bind.push(Box::new(limit as i64));
        let mut st = g.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(bind.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    /// Rows for a filter-only browse (empty query).
    pub fn recent_files(&self, req: &SearchRequest, limit: usize) -> Result<Vec<FileRecord>> {
        let g = self.write.lock().unwrap();
        let mut sql = format!("SELECT {FILE_COLS} FROM file f WHERE f.state='done'");
        let mut bind: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        push_filters(&mut sql, &mut bind, req);
        sql.push_str(" ORDER BY f.mtime_ns DESC LIMIT ?");
        bind.push(Box::new(limit as i64));
        let mut st = g.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(bind.iter()), read_file)?;
        Ok(rows.filter_map(|x| x.ok()).collect())
    }

    pub fn moments_for(&self, file_ids: &[i64]) -> Result<HashMap<i64, Vec<(i64, i64, i64, f64, Option<String>)>>> {
        if file_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let g = self.write.lock().unwrap();
        let placeholders = vec!["?"; file_ids.len()].join(",");
        let sql = format!("SELECT file_id,id,start_ms,end_ms,thumb FROM clip WHERE file_id IN ({placeholders}) ORDER BY file_id,start_ms");
        let mut st = g.prepare(&sql)?;
        let mut out: HashMap<i64, Vec<(i64, i64, i64, f64, Option<String>)>> = HashMap::new();
        for row in st
            .query_map(rusqlite::params_from_iter(file_ids.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?))
            })?
            .flatten()
        {
            out.entry(row.0).or_default().push((row.1, row.2, row.3, 0.0, row.4));
        }
        Ok(out)
    }
}

fn push_filters(sql: &mut String, bind: &mut Vec<Box<dyn rusqlite::types::ToSql>>, req: &SearchRequest) {
    sql.push_str(" AND f.root_id IN (SELECT id FROM root WHERE enabled=1)");
    if !req.filters.kinds.is_empty() {
        let list = req.filters.kinds.iter().map(|k| format!("'{}'", kind_str(*k))).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND f.kind IN ({list})"));
    }
    if let Some(r) = req.filters.root_id {
        sql.push_str(" AND f.root_id=?");
        bind.push(Box::new(r));
    }
    if let Some(v) = req.filters.min_size {
        sql.push_str(" AND f.size>=?");
        bind.push(Box::new(v));
    }
    if let Some(v) = req.filters.max_size {
        sql.push_str(" AND f.size<=?");
        bind.push(Box::new(v));
    }
    if let Some(v) = req.filters.after_ns {
        sql.push_str(" AND f.mtime_ns>=?");
        bind.push(Box::new(v));
    }
    if let Some(v) = req.filters.before_ns {
        sql.push_str(" AND f.mtime_ns<=?");
        bind.push(Box::new(v));
    }
    sql.push_str(" AND f.state='done'");
}

fn ensure_filename_row(g: &Connection, id: i64, name: &str, path: &str) -> Result<()> {
    g.execute(
        "INSERT INTO fts(rowid,name_seg,path_seg,title_seg,body_seg) SELECT ?,?,?,'','' WHERE NOT EXISTS (SELECT 1 FROM fts WHERE rowid=?)",
        params![id, segment::segment(name), segment::segment(path), id],
    )?;
    Ok(())
}

/// Remove every derived artefact of these files, including the rows themselves and the
/// thumbnail files their records point at. Thumbnails are derived artefacts too: without
/// this the cache directories grow without bound on a library that churns.
fn purge_files(g: &Connection, ids: &[i64]) -> Result<()> {
    let mut thumbs: Vec<String> = Vec::new();
    for id in ids {
        if let Ok(Some(t)) = g.query_row("SELECT thumb FROM file WHERE id=?", params![*id], |r| {
            r.get::<_, Option<String>>(0)
        }) {
            thumbs.push(t);
        }
        {
            let mut st = g.prepare("SELECT thumb FROM clip WHERE file_id=?")?;
            let rows = st.query_map(params![*id], |r| r.get::<_, Option<String>>(0))?;
            thumbs.extend(rows.flatten().flatten());
        }
        purge_derived(g, *id)?;
        g.execute("DELETE FROM fts WHERE rowid=?", params![*id])?;
        g.execute("DELETE FROM text_store WHERE file_id=?", params![*id])?;
        g.execute("DELETE FROM doc WHERE file_id=?", params![*id])?;
        g.execute("DELETE FROM file WHERE id=?", params![*id])?;
    }
    for t in thumbs {
        // Best effort: an already-missing thumbnail is not a reason to fail the purge.
        let _ = std::fs::remove_file(&t);
    }
    Ok(())
}

fn purge_derived(g: &Connection, file_id: i64) -> Result<()> {
    g.execute("DELETE FROM chunk WHERE file_id=?", params![file_id])?;
    g.execute("DELETE FROM clip WHERE file_id=?", params![file_id])?;
    // The vector file is append-only: a row can only be reclaimed by compaction, so
    // tombstone the map entry instead of deleting it and orphaning the bytes.
    g.execute("UPDATE vec_map SET alive=0 WHERE file_id=?", params![file_id])?;
    g.execute("DELETE FROM fts WHERE rowid=?", params![file_id])?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Totals {
    pub pending: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
}

pub fn totals(db: &Db) -> Result<Totals> {
    let c = db.read()?;
    let mut t = Totals { pending: 0, done: 0, failed: 0, skipped: 0 };
    let mut st = c.prepare("SELECT state,count(*) FROM file GROUP BY state")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
        match row.0.as_str() {
            "done" => t.done = row.1,
            "failed" => t.failed = row.1,
            "skipped" => t.skipped = row.1,
            _ => t.pending += row.1,
        }
    }
    Ok(t)
}

pub fn stats(db: &Db) -> Result<HashMap<String, i64>> {
    let c = db.read()?;
    let mut out: HashMap<String, i64> = HashMap::new();
    out.insert("files".into(), c.query_row("SELECT count(*) FROM file", [], |r| r.get(0))?);
    out.insert("bytes".into(), c.query_row("SELECT COALESCE(SUM(size),0) FROM file", [], |r| r.get(0))?);
    out.insert("vectors".into(), c.query_row("SELECT count(*) FROM vec_map WHERE alive=1", [], |r| r.get(0))?);
    out.insert("clips".into(), c.query_row("SELECT count(*) FROM clip", [], |r| r.get(0))?);
    out.insert("chunks".into(), c.query_row("SELECT count(*) FROM chunk", [], |r| r.get(0))?);
    out.insert("roots".into(), c.query_row("SELECT count(*) FROM root", [], |r| r.get(0))?);
    let mut st = c.prepare("SELECT kind,count(*) FROM file GROUP BY kind")?;
    for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.flatten() {
        out.insert(format!("kind_{}", row.0), row.1);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Scanned;
    use crate::{EmbedRowKind, FileState, Kind};

    fn upsert(db: &Db, root_id: i64, path: &str, kind: Kind) -> i64 {
        let seen = now_ns();
        let s = Scanned {
            root_id,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            ext: path.rsplit('.').next().unwrap_or("").to_lowercase(),
            kind,
            size: 1,
            mtime_ns: seen,
        };
        db.upsert_file(&s, seen).unwrap().0
    }

    /// 停用再启用不该把整个目录的向量作废：有效的 done 记录要原样保留，
    /// 只重试失败文件、补齐缺向量的文件。
    #[test]
    fn reenabling_a_root_keeps_valid_embeddings() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/photos", "照片").unwrap();
        let with_vec = upsert(&db, root, "/tmp/photos/kept.jpg", Kind::Image);
        let no_vec = upsert(&db, root, "/tmp/photos/plain.md", Kind::Document);
        let failed = upsert(&db, root, "/tmp/photos/broken.jpg", Kind::Image);
        db.set_file_state(with_vec, FileState::Done, None).unwrap();
        let row = db.reserve_vec_row(EmbedRowKind::Image, with_vec, with_vec).unwrap();
        db.activate_vec_rows(&[row]).unwrap();
        db.set_file_state(no_vec, FileState::Done, None).unwrap();
        db.set_file_state(failed, FileState::Failed, Some("嵌入失败")).unwrap();

        db.set_root_enabled(root, false).unwrap();
        db.set_root_enabled(root, true).unwrap();

        assert_eq!(db.file(with_vec).unwrap().state, FileState::Done, "size/mtime 未变且向量有效，不该重嵌入");
        assert_eq!(db.file(no_vec).unwrap().state, FileState::Stale, "缺向量的 done 文件应排队补齐");
        assert_eq!(db.file(failed).unwrap().state, FileState::Stale, "失败文件应重试");
        assert!(db.file(failed).unwrap().error.is_none(), "重试要清掉旧错误");
        assert_eq!(db.alive_vec_rows().unwrap(), 1, "启用不清除向量");
        assert_eq!(db.pending(10, &[]).unwrap().len(), 2);
    }

    #[test]
    fn mark_missing_vectors_leaves_disabled_roots_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let on = db.add_root("/tmp/on", "启用").unwrap();
        let off = db.add_root("/tmp/off", "停用").unwrap();
        let a = upsert(&db, on, "/tmp/on/a.md", Kind::Document);
        let b = upsert(&db, off, "/tmp/off/b.md", Kind::Document);
        db.set_file_state(a, FileState::Done, None).unwrap();
        db.set_file_state(b, FileState::Done, None).unwrap();
        db.set_root_enabled(off, false).unwrap();

        let n = db.mark_missing_vectors().unwrap();

        assert_eq!(n, 1, "停用目录的文件不该被排队");
        assert_eq!(db.file(a).unwrap().state, FileState::Stale);
        assert_eq!(db.file(b).unwrap().state, FileState::Done);
    }

    /// 删除索引行必须连带删除缩略图文件，否则缓存目录随着库的增删只增不减。
    #[test]
    fn purging_a_file_removes_its_thumbnail_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/photos", "照片").unwrap();
        let id = upsert(&db, root, "/tmp/photos/a.mp4", Kind::Video);
        db.set_file_state(id, FileState::Done, None).unwrap();
        let file_thumb = dir.path().join("t.jpg");
        std::fs::write(&file_thumb, b"thumb").unwrap();
        db.set_file_meta(id, None, None, None, Some(&file_thumb.to_string_lossy())).unwrap();
        let clips = db.sync_clips(id, &[(0.0, 2.0)]).unwrap();
        let clip_thumb = dir.path().join("clip-0.jpg");
        std::fs::write(&clip_thumb, b"clip").unwrap();
        db.set_clip_thumb(clips[0].0, &clip_thumb.to_string_lossy()).unwrap();

        db.delete_by_path("/tmp/photos/a.mp4").unwrap();

        assert!(db.file(id).is_none());
        assert!(!file_thumb.exists(), "文件缩略图应随行删除");
        assert!(!clip_thumb.exists(), "clip 缩略图应随行删除");
    }

    // -------------------------------------- P0-03: 单元状态 + migration（schema v2）

    /// v1 形态的建表语句，用于构造迁移 fixture。
    const V1_DDL: &str = r#"
    CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE root(
      id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL UNIQUE, label TEXT NOT NULL,
      enabled INTEGER NOT NULL DEFAULT 1, last_scan_ns INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE file(
      id INTEGER PRIMARY KEY AUTOINCREMENT, root_id INTEGER REFERENCES root(id) ON DELETE CASCADE,
      path TEXT NOT NULL UNIQUE, name TEXT NOT NULL, ext TEXT NOT NULL, kind TEXT NOT NULL,
      size INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, duration_ms INTEGER, width INTEGER,
      height INTEGER, state TEXT NOT NULL DEFAULT 'pending', error TEXT,
      indexed_ns INTEGER NOT NULL DEFAULT 0, thumb TEXT);
    CREATE TABLE chunk(
      id INTEGER PRIMARY KEY AUTOINCREMENT, file_id INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
      ord INTEGER NOT NULL, text TEXT NOT NULL, vec_row INTEGER);
    CREATE TABLE clip(
      id INTEGER PRIMARY KEY AUTOINCREMENT, file_id INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
      ord INTEGER NOT NULL, start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL,
      vec_row INTEGER, thumb TEXT);
    CREATE TABLE vec_map(
      row_id INTEGER PRIMARY KEY, target_kind TEXT NOT NULL, ref_id INTEGER NOT NULL,
      file_id INTEGER NOT NULL, alive INTEGER NOT NULL DEFAULT 1);
    "#;

    /// v1 → v2 迁移：已激活向量的单元保持 persisted；只预留/未预留的回 pending；
    /// 带未完成单元的 done 文件回 stale；迁移前生成备份；数据原样保留。
    #[test]
    fn v1_database_migrates_to_v2_with_unit_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.sqlite");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(V1_DDL).unwrap();
            c.execute_batch(r#"
                INSERT INTO meta VALUES('schema_version','1');
                INSERT INTO root(path,label) VALUES('/tmp/d','目录');
                INSERT INTO file(id,root_id,path,name,ext,kind,size,mtime_ns,state) VALUES
                  (1,1,'/tmp/d/a.md','a.md','md','document',10,1,'done'),
                  (2,1,'/tmp/d/b.mp4','b.mp4','mp4','video',10,1,'done');
                -- 文件 1：chunk 1 已激活；chunk 2 只预留（旧协议的半成品）；chunk 3 从未预留
                INSERT INTO chunk(id,file_id,ord,text,vec_row) VALUES
                  (1,1,0,'已编码片段',0),(2,1,1,'只有预留的片段',1),(3,1,2,'从没预留过的片段',NULL);
                INSERT INTO vec_map(row_id,target_kind,ref_id,file_id,alive) VALUES
                  (0,'chunk',1,1,1),(1,'chunk',2,1,0);
                -- 文件 2：一个从未预留的 clip
                INSERT INTO clip(id,file_id,ord,start_ms,end_ms) VALUES (5,2,0,0,12000);
            "#).unwrap();
        }

        let db = Db::open(&path).unwrap();

        assert!(dir.path().join("i.bak-v1.sqlite").exists(), "迁移前必须生成备份");
        assert_eq!(db.get_meta("schema_version").as_deref(), Some("3"), "逐版本迁到最新");
        // v3 的扫描履历表就绪
        assert!(db.record_scan_run(1, "completed", 1, 3, 1, 0).is_ok());
        assert_eq!(db.last_scan_run(1).unwrap().map(|(s, w, _)| (s, w)), Some(("completed".into(), 3)));
        let c = db.read().unwrap();
        let status = |id: i64| -> String {
            c.query_row("SELECT status FROM chunk WHERE id=?", params![id], |r| r.get(0)).unwrap()
        };
        assert_eq!(status(1), "persisted", "已激活的向量保持 persisted");
        assert_eq!(status(2), "pending", "只预留不落盘的片段要重新嵌入");
        assert_eq!(status(3), "pending", "从未预留的片段要重新嵌入");
        let clip_status: String = c.query_row("SELECT status FROM clip WHERE id=5", [], |r| r.get(0)).unwrap();
        assert_eq!(clip_status, "pending");
        let text: String = c.query_row("SELECT text FROM chunk WHERE id=2", [], |r| r.get(0)).unwrap();
        assert_eq!(text, "只有预留的片段", "迁移不得丢数据");
        assert_eq!(db.file(1).unwrap().state, FileState::Stale, "带未完成单元的 done 文件回队列");
        assert_eq!(db.file(2).unwrap().state, FileState::Stale);
        assert_eq!(db.pending(10, &[]).unwrap().len(), 2, "两个文件都要能被重新拾起");
    }

    /// sync 保留未变单元的状态：文本没变的 chunk 连同它的持久化向量一起保留；
    /// 变了的单元回 pending 并作废旧向量行；消失的单元连带回收。
    #[test]
    fn sync_chunks_preserves_unchanged_units() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/d", "目录").unwrap();
        let id = upsert(&db, root, "/tmp/d/a.md", Kind::Document);
        let first = db.sync_chunks(id, &["甲".into(), "乙".into(), "丙".into()]).unwrap();
        let (a, b, c3) = (first[0].0, first[1].0, first[2].0);
        // 模拟乙、丙已持久化
        let rb = db.reserve_vec_row(EmbedRowKind::Chunk, b, id).unwrap();
        db.activate_vec_rows(&[rb]).unwrap();
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, b)]).unwrap();

        let second = db.sync_chunks(id, &["甲".into(), "乙改".into(), "丙".into(), "丁".into()]).unwrap();

        assert_eq!(second.len(), 4);
        assert_eq!(second[0].0, a, "未变的单元保留原 id");
        assert_eq!(second[2].0, c3, "未变的单元保留原 id");
        let st = |cid: i64| -> String {
            db.read().unwrap().query_row("SELECT status FROM chunk WHERE id=?", params![cid], |r| r.get(0)).unwrap()
        };
        assert_eq!(st(b), "pending", "内容变化的单元回到待嵌入");
        assert_eq!(db.alive_vec_rows().unwrap(), 0, "变化单元的向量行已作废");
        assert_eq!(st(second[3].0), "pending");
        assert_eq!(db.unit_summary(id, 3).unwrap(), UnitSummary { total: 4, persisted: 0, exhausted: 0, retryable: 0, error: None });
    }

    /// 目录被删除时按「本路径及子路径」清理：子文件的行、FTS、向量映射随目录
    /// 一起走；名字恰好同前缀的文件不受影响。
    #[test]
    fn deleting_a_directory_clears_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/roots", "根").unwrap();
        let child = upsert(&db, root, "/tmp/roots/会议记录/a.md", Kind::Document);
        let deep = upsert(&db, root, "/tmp/roots/会议记录/2026/b.md", Kind::Document);
        let sibling = upsert(&db, root, "/tmp/roots/会议纪要.md", Kind::Document);
        for id in [child, deep, sibling] {
            db.set_file_state(id, FileState::Done, None).unwrap();
        }
        let row = db.reserve_vec_row(EmbedRowKind::Image, child, child).unwrap();
        db.activate_vec_rows(&[row]).unwrap();
        assert_eq!(db.alive_vec_rows().unwrap(), 1);

        let n = db.delete_tree("/tmp/roots/会议记录").unwrap();

        assert_eq!(n, 2, "目录内两个文件被清理");
        assert!(db.file(child).is_none());
        assert!(db.file(deep).is_none());
        assert_eq!(db.alive_vec_rows().unwrap(), 0, "子文件的向量映射一并回收");
        assert!(db.file(sibling).is_some(), "同前缀不同名的文件不受影响");
    }

    /// 失败计数与聚合：可重试的失败让文件回 stale，重试耗尽则判 failed，
    /// 全部持久化才是 done。
    #[test]
    fn unit_retry_and_aggregation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let root = db.add_root("/tmp/d", "目录").unwrap();
        let id = upsert(&db, root, "/tmp/d/a.md", Kind::Document);
        let chunks = db.sync_chunks(id, &["甲".into(), "乙".into()]).unwrap();
        let (a, b) = (chunks[0].0, chunks[1].0);

        db.mark_units_failed(&[(EmbedRowKind::Chunk, b)], "超时").unwrap();
        let sum = db.unit_summary(id, 3).unwrap();
        assert_eq!((sum.total, sum.persisted, sum.exhausted, sum.retryable), (2, 0, 0, 1), "一个单元失败且可重试");
        assert_eq!(sum.error.as_deref(), Some("超时"));

        // 剩余单元成功后整份文件聚合为 done。
        let ra = db.reserve_vec_row(EmbedRowKind::Chunk, a, id).unwrap();
        db.activate_vec_rows(&[ra]).unwrap();
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, a)]).unwrap();
        let rb = db.reserve_vec_row(EmbedRowKind::Chunk, b, id).unwrap();
        db.activate_vec_rows(&[rb]).unwrap();
        db.mark_units_persisted(&[(EmbedRowKind::Chunk, b)]).unwrap();
        let sum = db.unit_summary(id, 3).unwrap();
        assert_eq!((sum.total, sum.persisted, sum.exhausted), (2, 2, 0), "全部持久化");
        assert!(sum.error.is_none(), "成功要清掉旧错误");

        // 重试计数到上限即视为耗尽。
        for _ in 0..3 {
            db.mark_units_failed(&[(EmbedRowKind::Chunk, a)], "持续失败").unwrap();
        }
        let sum = db.unit_summary(id, 3).unwrap();
        assert_eq!((sum.exhausted, sum.retryable), (1, 0));
        assert_eq!(sum.error.as_deref(), Some("持续失败"));
    }
}
