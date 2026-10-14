use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, Row};

use crate::{segment, EmbedRowKind, Error, FileRecord, FileState, Kind, Result, SearchRequest};

pub const SCHEMA_VERSION: i64 = 1;

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
  vec_row INTEGER
);
CREATE INDEX IF NOT EXISTS chunk_file ON chunk(file_id, ord);

CREATE TABLE IF NOT EXISTS clip(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  file_id INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  ord INTEGER NOT NULL,
  start_ms INTEGER NOT NULL,
  end_ms INTEGER NOT NULL,
  vec_row INTEGER,
  thumb TEXT
);
CREATE INDEX IF NOT EXISTS clip_file ON clip(file_id, start_ms);

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

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let c = open_conn(path)?;
        c.execute_batch(DDL)?;
        let stored: Option<String> = c
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .ok();
        match stored {
            None => {
                c.execute("INSERT INTO meta(key,value) VALUES('schema_version',?)", params![SCHEMA_VERSION.to_string()])?;
            }
            Some(v) if v.parse::<i64>().unwrap_or(0) != SCHEMA_VERSION => {
                return Err(Error::Config(format!(
                    "索引数据库版本 {v} 与当前程序要求的 {SCHEMA_VERSION} 不兼容，请删除该索引文件后重建：{}",
                    path.display()
                )));
            }
            Some(_) => {}
        }
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

    // ---------------------------------------------------------------- roots

    pub fn add_root(&self, path: &str, label: &str) -> Result<i64> {
        let g = self.write.lock().unwrap();
        g.execute(
            "INSERT INTO root(path,label) VALUES(?,?) ON CONFLICT(path) DO UPDATE SET label=excluded.label, enabled=1",
            params![path, label],
        )?;
        let id: i64 = g.query_row("SELECT id FROM root WHERE path=?", params![path], |r| r.get(0))?;
        g.execute("UPDATE file SET state='pending' WHERE root_id=? AND state IN ('done','skipped','failed')", params![id])?;
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

    pub fn set_root_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let g = self.write.lock().unwrap();
        g.execute("UPDATE root SET enabled=? WHERE id=?", params![enabled as i64, id])?;
        if enabled {
            g.execute("UPDATE file SET state='pending' WHERE root_id=? AND state IN ('done','skipped','failed')", params![id])?;
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

    pub fn doc_title(&self, file_id: i64) -> Option<String> {
        let g = self.write.lock().ok()?;
        g.query_row("SELECT title FROM doc WHERE file_id=?", params![file_id], |r| r.get(0)).ok().flatten()
    }

    pub fn insert_chunks(&self, file_id: i64, chunks: &[String]) -> Result<Vec<(i64, usize)>> {
        let g = self.write.lock().unwrap();
        g.execute("DELETE FROM chunk WHERE file_id=?", params![file_id])?;
        g.execute("UPDATE vec_map SET alive=0 WHERE file_id=? AND target_kind='chunk'", params![file_id])?;
        let mut out = Vec::with_capacity(chunks.len());
        for (ord, text) in chunks.iter().enumerate() {
            g.execute("INSERT INTO chunk(file_id,ord,text) VALUES(?,?,?)", params![file_id, ord as i64, text])?;
            out.push((g.last_insert_rowid(), ord));
        }
        Ok(out)
    }

    pub fn insert_clips(&self, file_id: i64, clips: &[(f64, f64)]) -> Result<Vec<(i64, usize)>> {
        let g = self.write.lock().unwrap();
        g.execute("DELETE FROM clip WHERE file_id=?", params![file_id])?;
        g.execute("UPDATE vec_map SET alive=0 WHERE file_id=? AND target_kind='clip'", params![file_id])?;
        let mut out = Vec::with_capacity(clips.len());
        for (ord, (start, end)) in clips.iter().enumerate() {
            g.execute(
                "INSERT INTO clip(file_id,ord,start_ms,end_ms) VALUES(?,?,?,?)",
                params![file_id, ord as i64, (start * 1000.0).round() as i64, (end * 1000.0).round() as i64],
            )?;
            out.push((g.last_insert_rowid(), ord));
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

    // -------------------------------------------------------------- vectors

    /// Allocate the next physical row in the vector file for an embeddable unit.
    pub fn reserve_vec_row(&self, target: EmbedRowKind, ref_id: i64, file_id: i64) -> Result<i64> {
        let g = self.write.lock().unwrap();
        g.execute("UPDATE vec_map SET alive=0 WHERE target_kind=? AND ref_id=?", params![target.as_str(), ref_id])?;
        let next: i64 = g.query_row("SELECT COALESCE(MAX(row_id),-1)+1 FROM vec_map", [], |r| r.get(0))?;
        g.execute(
            "INSERT INTO vec_map(row_id,target_kind,ref_id,file_id,alive) VALUES(?,?,?,?,1)",
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

    /// After compaction, rewrite row ids and every `vec_row` pointer. `mapping` must be
    /// ascending by old row id, as `live_rows_for_compaction` returns.
    pub fn apply_compaction(&self, mapping: &[(i64, i64)]) -> Result<usize> {
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
        g.execute("UPDATE chunk SET vec_row=NULL", [])?;
        g.execute("UPDATE clip SET vec_row=NULL", [])?;
        Ok(())
    }

    pub fn reindex_all(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let n = g.execute("UPDATE file SET state='pending',error=NULL", [])?;
        g.execute("DELETE FROM vec_map", [])?;
        g.execute("UPDATE chunk SET vec_row=NULL", [])?;
        g.execute("UPDATE clip SET vec_row=NULL", [])?;
        Ok(n)
    }

    /// Files indexed for text only: queue them for the semantic pass once the model is
    /// available. Skipped kinds are left alone.
    pub fn mark_missing_vectors(&self) -> Result<usize> {
        let g = self.write.lock().unwrap();
        let n = g.execute(
            "UPDATE file SET state='stale' WHERE state='done' AND kind IN ('image','video','document','code') \
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

/// Remove every derived artefact of these files, including the rows themselves.
fn purge_files(g: &Connection, ids: &[i64]) -> Result<()> {
    for id in ids {
        purge_derived(g, *id)?;
        g.execute("DELETE FROM fts WHERE rowid=?", params![*id])?;
        g.execute("DELETE FROM text_store WHERE file_id=?", params![*id])?;
        g.execute("DELETE FROM doc WHERE file_id=?", params![*id])?;
        g.execute("DELETE FROM file WHERE id=?", params![*id])?;
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
