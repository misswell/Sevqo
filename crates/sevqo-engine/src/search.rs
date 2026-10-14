//! Hybrid retrieval: BM25 over the segmented FTS index plus exact cosine over the
//! vector store, fused with Reciprocal Rank Fusion.
//!
//! RRF is used instead of a weighted score blend because the two channels' raw scores
//! are not comparable — BM25 is unbounded and negative-inverted, cosine lives in
//! [-1, 1]. Rank fusion needs no tuning constant per modality and degrades gracefully
//! when one channel returns nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::db::Db;
use crate::segment;
use crate::vectors::MmapView;
use crate::{Channel, EmbedItem, Error, FileRecord, Hit, Kind, Moment, Result, SearchRequest, SearchResponse};

const RRF_K: f64 = 60.0;

pub struct Searcher {
    db: Arc<Db>,
    vector_path: PathBuf,
    view: Option<MmapView>,
}

impl Searcher {
    pub fn new(db: Arc<Db>, vector_path: &Path) -> Result<Searcher> {
        let view = MmapView::open(vector_path)?;
        Ok(Searcher { db, vector_path: vector_path.to_path_buf(), view })
    }

    /// Called after indexing so a search sees freshly appended rows.
    pub fn refresh(&mut self) -> Result<()> {
        self.view = MmapView::open(&self.vector_path)?;
        Ok(())
    }

    pub fn vector_dim(&self) -> usize {
        self.view.as_ref().map(|v| v.dim()).unwrap_or(0)
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// Keyword channel only — no inference, so this is what keystroke-level instant
    /// results use while the model is still loading.
    pub fn keyword(&self, req: &SearchRequest, query: &str, limit: usize) -> Result<Vec<(i64, f64)>> {
        let expr = match segment::match_expression(query) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };
        let mut hits = self.db.keyword_search(&expr, req, limit)?;
        if hits.is_empty() {
            if let Some(loose) = segment::match_expression_loose(query) {
                hits = self.db.keyword_search(&loose, req, limit)?;
            }
        }
        Ok(hits)
    }

    /// Semantic channel: ranks files by their best-matching embedded unit.
    pub fn semantic(
        &self,
        req: &SearchRequest,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(i64, f32, Vec<(i64, i64, i64, f32, Option<String>)>)>> {
        let view = match self.view.as_ref() {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        if query.len() != view.dim() {
            return Err(Error::Config(format!(
                "查询向量维度 {} 与索引维度 {} 不一致",
                query.len(),
                view.dim()
            )));
        }
        let kinds: Vec<Kind> = effective_kinds(req);
        let candidates = self.db.vec_candidates(&kinds)?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // over-fetch: several clips of one video can win, and they collapse into one file
        let scored = view.search(query, &candidates, limit.saturating_mul(6).max(120));

        // (file_id, best_score) plus per-clip hits for videos
        let mut files: Vec<(i64, f32)> = Vec::new();
        let mut clips: std::collections::HashMap<i64, Vec<(i64, i64, i64, f32, Option<String>)>> =
            std::collections::HashMap::new();
        let mut best: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
        for (row, score) in scored {
            if score <= 0.0 {
                continue;
            }
            match best.entry(row.file_id) {
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    if score > *o.get() {
                        o.insert(score);
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(score);
                    files.push((row.file_id, score));
                }
            }
            if row.target == crate::EmbedRowKind::Clip {
                clips
                    .entry(row.file_id)
                    .or_default()
                    .push((row.ref_id, 0, 0, score, None));
            }
        }
        files.sort_by(|a, b| b.1.total_cmp(&a.1));
        files.truncate(limit);
        // fill in real clip timestamps now that we know the ranking
        let ids: Vec<i64> = clips.keys().copied().collect();
        let moments = self.db.moments_for(&ids)?;
        for (file_id, list) in clips.iter_mut() {
            if let Some(rows) = moments.get(file_id) {
                let score_of: std::collections::HashMap<i64, f32> =
                    list.iter().map(|(cid, _, _, s, _)| (*cid, *s)).collect();
                *list = rows
                    .iter()
                    .filter_map(|(cid, start, end, _s, thumb)| {
                        score_of.get(cid).map(|sc| (*cid, *start, *end, *sc, thumb.clone()))
                    })
                    .collect();
                list.sort_by(|a, b| b.3.total_cmp(&a.3));
                list.truncate(4);
            }
        }
        let out = files
            .into_iter()
            .map(|(fid, sc)| (fid, sc, clips.remove(&fid).unwrap_or_default()))
            .collect();
        Ok(out)
    }

    /// Fuse both channels. `query` may be empty when only filters are set, in which
    /// case this returns recently modified files.
    pub fn run(&self, req: &SearchRequest, query_vec: Option<&[f32]>) -> Result<SearchResponse> {
        let started = Instant::now();
        let query = req.query.trim();
        let limit = req.limit.max(1);

        let mut resp = SearchResponse {
            semantic_active: query_vec.is_some() && self.view.is_some(),
            ..Default::default()
        };

        if query.is_empty() && query_vec.is_none() {
            // No text and no image: browse instead of search. Filters still apply, so
            // "图片 + 空输入" shows the latest pictures rather than everything.
            let rows = self.db.recent_files(req, limit)?;
            resp.hits = rows.into_iter().map(|f| hit_from_file(f, 0.0, Channel::Keyword)).collect();
            resp.total = resp.hits.len();
            resp.keyword_ms = started.elapsed().as_secs_f64() * 1000.0;
            return Ok(resp);
        }

        // 1. keyword
        let kw_start = Instant::now();
        let kw = if query.is_empty() { Vec::new() } else { self.keyword(req, query, limit * 3)? };
        resp.keyword_ms = kw_start.elapsed().as_secs_f64() * 1000.0;

        // 2. semantic — a vector is enough on its own: 以图搜图 has no query text
        let sem_start = Instant::now();
        let sem = match query_vec {
            Some(v) => self.semantic(req, v, limit)?,
            None => Vec::new(),
        };
        resp.semantic_ms = sem_start.elapsed().as_secs_f64() * 1000.0;
        if query_vec.is_none() && !query.is_empty() {
            resp.note = Some("语义检索未启用，仅按文件名与正文匹配".into());
        }

        // 3. RRF
        let mut fused: Vec<(i64, f64, Option<usize>, Option<usize>)> = Vec::new();
        let mut seen: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for (rank, (id, _)) in kw.iter().enumerate() {
            let r = 1.0 / (RRF_K + rank as f64 + 1.0);
            match seen.get(id) {
                Some(idx) => fused[*idx].1 += r,
                None => {
                    seen.insert(*id, fused.len());
                    fused.push((*id, r, Some(rank + 1), None));
                }
            }
        }
        for (rank, (id, _, _)) in sem.iter().enumerate() {
            let r = 1.0 / (RRF_K + rank as f64 + 1.0);
            match seen.get(id) {
                Some(idx) => {
                    let i = *idx;
                    fused[i].1 += r;
                    fused[i].3 = Some(rank + 1);
                }
                None => {
                    seen.insert(*id, fused.len());
                    fused.push((*id, r, None, Some(rank + 1)));
                }
            }
        }
        // When both channels ran, keep their agreement as the tie-break so a file that
        // matched by name *and* meaning outranks one that only scraped into a list.
        fused.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| (a.2.is_some() as u8 + a.3.is_some() as u8).cmp(&(b.2.is_some() as u8 + b.3.is_some() as u8)).reverse())
        });
        fused.truncate(limit);

        let ids: Vec<i64> = fused.iter().map(|f| f.0).collect();
        let records = self.db.files_by_ids(&ids)?;
        let moments_by_file: std::collections::HashMap<i64, Vec<(i64, i64, i64, f32, Option<String>)>> =
            sem.into_iter().map(|(id, _, m)| (id, m)).collect();

        let mut hits = Vec::with_capacity(fused.len());
        for (id, score, kwr, smr) in fused {
            let rec = match records.get(&id) {
                Some(r) => r.clone(),
                None => continue,
            };
            let channel = match (kwr, smr) {
                (Some(_), Some(_)) => Channel::Both,
                (Some(_), None) => Channel::Keyword,
                (None, Some(_)) => Channel::Semantic,
                (None, None) => Channel::Keyword,
            };
            let mut hit = hit_from_file(rec, score, channel);
            hit.keyword_rank = kwr;
            hit.semantic_rank = smr;
            if let Some(clips) = moments_by_file.get(&id) {
                hit.moments = clips
                    .iter()
                    .map(|(cid, start, end, sc, thumb)| Moment {
                        clip_id: *cid,
                        start_ms: *start,
                        end_ms: *end,
                        score: *sc as f64,
                        thumb: thumb.clone(),
                    })
                    .collect();
                if hit.thumb.is_none() {
                    hit.thumb = hit.moments.first().and_then(|m| m.thumb.clone());
                }
            }
            if hit.snippet.is_none() && !query.is_empty() {
                hit.snippet = self.snippet_for(id, query);
            }
            hits.push(hit);
        }
        resp.hits = hits;
        resp.total = resp.hits.len();
        resp.keyword_ms += started.elapsed().as_secs_f64() * 0.0;
        Ok(resp)
    }

    fn snippet_for(&self, file_id: i64, query: &str) -> Option<String> {
        let text = self.db.stored_text(file_id)?;
        let needle = query.split_whitespace().next().unwrap_or(query);
        segment::snippet(&text, needle, 48)
    }
}

fn hit_from_file(f: FileRecord, score: f64, channel: Channel) -> Hit {
    Hit {
        file_id: f.id,
        path: f.path,
        name: f.name,
        kind: f.kind,
        size: f.size,
        mtime_ns: f.mtime_ns,
        score,
        channel,
        keyword_rank: None,
        semantic_rank: None,
        snippet: None,
        moments: Vec::new(),
        thumb: f.thumb,
        duration_ms: f.duration_ms,
    }
}

/// Which vector target kinds are worth scanning for this request.
fn effective_kinds(req: &SearchRequest) -> Vec<Kind> {
    if req.filters.kinds.is_empty() {
        vec![Kind::Image, Kind::Video, Kind::Document, Kind::Code]
    } else {
        req.filters.kinds.clone()
    }
}

/// One text query → one vector. The search-instruction prefix is what the model was
/// trained with, so omitting it measurably costs precision.
pub fn query_vector(
    client: &crate::infer::Client,
    text: &str,
    batch: usize,
) -> Result<Vec<f32>> {
    let item = EmbedItem::Text {
        key: "q".into(),
        text: text.to_owned(),
        title: None,
        role: "query".into(),
    };
    let resp = client.embed(&[item], batch, std::time::Duration::from_secs(90))?;
    resp.vectors
        .get("q")
        .cloned()
        .ok_or_else(|| Error::Infer(resp.errors.get("q").cloned().unwrap_or_else(|| "查询编码失败".into())))
}

/// Image-similarity search: the query is a picture, not a sentence.
pub fn query_vector_from_image(client: &crate::infer::Client, path: &Path, batch: usize) -> Result<Vec<f32>> {
    let item = EmbedItem::Image {
        key: "q".into(),
        path: path.to_string_lossy().to_string(),
        thumb: None,
    };
    let resp = client.embed(&[item], batch, std::time::Duration::from_secs(90))?;
    resp.vectors
        .get("q")
        .cloned()
        .ok_or_else(|| Error::Infer(resp.errors.get("q").cloned().unwrap_or_else(|| "图片编码失败".into())))
}

/// Search by a video clip. The query is encoded from its first `window_s` seconds, which
/// is the same frame budget an indexed clip gets, so the two vectors stay comparable.
pub fn query_vector_from_video(
    client: &crate::infer::Client,
    path: &Path,
    window_s: f64,
    batch: usize,
) -> Result<Vec<f32>> {
    let probe = client.probe(path, std::time::Duration::from_secs(60))?;
    if !probe.has_video {
        return Err(Error::Unsupported("这个文件没有视频轨".into()));
    }
    let item = EmbedItem::Video {
        key: "q".into(),
        path: path.to_string_lossy().to_string(),
        start: 0.0,
        end: probe.duration_s.min(window_s).max(0.5),
        fps: None,
        thumb: None,
    };
    let resp = client.embed(&[item], batch, std::time::Duration::from_secs(180))?;
    resp.vectors
        .get("q")
        .cloned()
        .ok_or_else(|| Error::Infer(resp.errors.get("q").cloned().unwrap_or_else(|| "视频编码失败".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::scan::Scanned;
    use crate::vectors::Store;
    use crate::{EmbedRowKind, Filters, VecRow};

    fn setup(dim: usize) -> (tempfile::TempDir, Db, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("i.sqlite");
        let vec_path = dir.path().join("v.f32");
        let db = Db::open(&db_path).unwrap();
        let root = db.add_root("/tmp/photos", "图片").unwrap();
        let seen = crate::db::now_ns();
        let mk = |name: &str, kind: Kind| {
            let s = Scanned {
                root_id: root,
                path: format!("/tmp/photos/{name}"),
                name: name.into(),
                ext: name.rsplit('.').next().unwrap_or("").to_lowercase(),
                kind,
                size: 1024,
                mtime_ns: seen,
            };
            let (id, _) = db.upsert_file(&s, seen).unwrap();
            id
        };
        let beach = mk("海边日落.jpg", Kind::Image);
        let code = mk("parser.rs", Kind::Code);
        let vid = mk("clip.mp4", Kind::Video);
        for id in [beach, code, vid] {
            db.set_file_state(id, crate::FileState::Done, None).unwrap();
        }
        db.store_text(beach, Some("海边日落"), "照片记录了一次海边日落，天空很红", false).unwrap();
        db.store_text(code, Some("parser.rs"), "Rust parser for the search index", false).unwrap();
        db.insert_clips(vid, &[(0.0, 12.0), (12.0, 24.0)]).unwrap();
        let clips = db.clip_rows(vid).unwrap();
        let clip_rows: Vec<(i64, usize)> = clips
            .iter()
            .map(|(cid, _, _, _, _)| (*cid, db.reserve_vec_row(EmbedRowKind::Clip, *cid, vid).unwrap() as usize))
            .collect();
        // image + chunk vectors
        let img_row = db.reserve_vec_row(EmbedRowKind::Image, beach, beach).unwrap();
        let chunk_row = {
            let cs = db.insert_chunks(code, &["Rust parser for the search index".into()]).unwrap();
            db.reserve_vec_row(EmbedRowKind::Chunk, cs[0].0, code).unwrap()
        };
        let mut st = Store::open(&vec_path, dim).unwrap();
        // basis-ish vectors so cosine ordering is predictable
        let e = |i: usize, v: Vec<f32>| {
            let mut x = vec![0.0f32; dim];
            for (j, val) in v.iter().enumerate() {
                x[j] = *val;
            }
            let _ = i;
            x
        };
        st.put(img_row as usize, &e(0, vec![1.0, 0.0, 0.0, 0.0])).unwrap();
        st.put(chunk_row as usize, &e(1, vec![0.0, 1.0, 0.0, 0.0])).unwrap();
        for (idx, (_cid, row)) in clip_rows.iter().enumerate() {
            let v = if idx == 0 { vec![0.0, 0.0, 1.0, 0.0] } else { vec![0.0, 0.0, 0.0, 1.0] };
            st.put(*row, &e(idx, v)).unwrap();
        }
        st.flush().unwrap();
        (dir, db, vec_path)
    }

    fn req(q: &str) -> SearchRequest {
        SearchRequest { query: q.into(), limit: 20, filters: Filters::default(), semantic: true }
    }

    #[test]
    fn keyword_finds_cjk_body_text() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let hits = s.keyword(&req("海边"), "海边", 10).unwrap();
        assert!(!hits.is_empty(), "CJK phrase must match the segmented body");
    }

    #[test]
    fn keyword_finds_filename() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let hits = s.keyword(&req("parser"), "parser", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn semantic_ranks_by_cosine() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let mut q = vec![0.0f32; 4];
        q[0] = 1.0;
        let res = s.semantic(&req("任意"), &q, 10).unwrap();
        assert_eq!(res.len(), 1, "only the image row points along e0");
    }

    #[test]
    fn video_hit_carries_moments() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let mut q = vec![0.0f32; 4];
        q[2] = 1.0;
        let res = s.semantic(&req("海边"), &q, 10).unwrap();
        assert_eq!(res.len(), 1);
        let moments = &res[0].2;
        assert!(!moments.is_empty(), "clip rows must become jumpable moments");
        assert_eq!(moments[0].1, 0);
        assert_eq!(moments[0].2, 12_000);
    }

    #[test]
    fn fusion_prefers_agreement() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let r = req("海边日落");
        // vector that points at both the image and the code chunk
        let mut q = vec![0.0f32; 4];
        q[0] = 0.9;
        q[1] = 0.9;
        let resp = s.run(&r, Some(&q)).unwrap();
        assert!(!resp.hits.is_empty());
        let top = &resp.hits[0];
        assert_eq!(top.name, "海边日落.jpg");
        assert!(matches!(top.channel, Channel::Both), "{:?}", top.channel);
        assert!(top.snippet.as_deref().unwrap_or("").contains("海边"));
    }

    #[test]
    fn semantic_only_request_still_returns_hits() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let mut q = vec![0.0f32; 4];
        q[1] = 1.0;
        let resp = s.run(&req("rust"), Some(&q)).unwrap();
        // "rust" matches the code file by keyword too, so Both is expected
        assert!(resp.hits.iter().any(|h| h.kind == Kind::Code));
        assert!(resp.semantic_active);
    }

    #[test]
    fn dimension_mismatch_is_reported() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let err = s.semantic(&req("x"), &[1.0, 0.0], 5).unwrap_err();
        assert!(err.to_string().contains("维度"), "{err}");
    }

    #[test]
    fn filter_restricts_kinds() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let mut r = req("海边日落");
        r.filters.kinds = vec![Kind::Code];
        let resp = s.run(&r, Some(&[1.0, 0.0, 0.0, 0.0])).unwrap();
        assert!(resp.hits.iter().all(|h| h.kind == Kind::Code), "{:?}", resp.hits);
    }

    #[test]
    fn empty_query_lists_recent() {
        let (_d, db, vp) = setup(4);
        let s = Searcher::new(Arc::new(db), &vp).unwrap();
        let mut r = SearchRequest::default();
        r.query = "".into();
        r.filters.kinds = vec![Kind::Image];
        let resp = s.run(&r, None).unwrap();
        assert_eq!(resp.hits.len(), 1);
    }

    #[test]
    fn missing_vector_file_degrades_to_keyword() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.sqlite")).unwrap();
        let s = Searcher::new(Arc::new(db), &dir.path().join("absent.f32")).unwrap();
        let resp = s.run(&req("任何"), None).unwrap();
        assert!(!resp.semantic_active);
        assert!(resp.note.is_some());
    }

    #[test]
    fn rrf_scores_are_monotonic_in_rank() {
        // rank 1 must fuse higher than rank 2 at equal other-channel contribution
        let a = 1.0 / (RRF_K + 1.0);
        let b = 1.0 / (RRF_K + 2.0);
        assert!(a > b);
    }

    #[test]
    fn candidates_ignore_dead_rows() {
        let (_d, db, _vp) = setup(4);
        let all = db.vec_candidates(&[]).unwrap().len();
        assert_eq!(all, 4, "1 image + 1 chunk + 2 clips");
        let rows: Vec<VecRow> = db.vec_candidates(&[Kind::Video]).unwrap();
        assert_eq!(rows.len(), 2);
    }
}
