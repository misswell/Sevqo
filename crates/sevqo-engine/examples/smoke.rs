//! End-to-end smoke run: index a fixture folder with the real sidecar, then search it.
//!
//! ```text
//! SEVQO_MODEL_HOME=… cargo run -p sevqo-engine --example smoke -- <fixtures> <data-dir>
//! ```
//!
//! This is the acceptance harness for the whole pipeline (scan → extract → embed →
//! FTS + vectors → hybrid search → video moments). It prints one line per check and
//! exits non-zero if any check fails, so it can be run from CI or a shell loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::unbounded;

use sevqo_engine::db::Db;
use sevqo_engine::infer::Client;
use sevqo_engine::jobs::{Indexer, Mode};
use sevqo_engine::search::{self, Searcher};
use sevqo_engine::{Channel, Filters, Kind, Paths, Progress, SearchRequest, Settings};

fn ch(c: Channel) -> &'static str {
    match c {
        Channel::Keyword => "keyword",
        Channel::Semantic => "semantic",
        Channel::Both => "both",
    }
}

mod checks {
    pub struct Tracker {
        pub failed: usize,
        pub passed: usize,
    }

    impl Tracker {
        pub fn ok(&mut self, name: &str, detail: impl std::fmt::Display) {
            self.passed += 1;
            println!("  PASS  {name}: {detail}");
        }
        pub fn bad(&mut self, name: &str, detail: impl std::fmt::Display) {
            self.failed += 1;
            println!("  FAIL  {name}: {detail}");
        }
        pub fn check(&mut self, name: &str, cond: bool, detail: impl std::fmt::Display) {
            if cond {
                self.ok(name, detail)
            } else {
                self.bad(name, detail)
            }
        }
    }
}

fn paths_for(data: &Path, model_dir: &Path) -> Paths {
    Paths {
        data_dir: data.to_path_buf(),
        cache_dir: data.to_path_buf(),
        model_dir: model_dir.to_path_buf(),
        thumb_dir: data.join("thumbs"),
        clip_thumb_dir: data.join("clip-thumbs"),
        db_path: data.join("index.sqlite"),
        vector_path: data.join("vectors.f32"),
        settings_path: data.join("settings.json"),
        log_dir: data.join("logs"),
    }
}

fn first_file(dir: &Path, ext: &str) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = collect(dir, ext);
    v.sort();
    v.into_iter().next()
}

fn collect(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case(ext)).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

fn show(r: &sevqo_engine::SearchResponse) -> String {
    r.hits
        .iter()
        .take(3)
        .map(|h| format!("{} {}/{}", h.name, (h.score * 1000.0).round() / 1000.0, ch(h.channel)))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let fixtures = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".research/fixtures"));
    let data = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("sevqo-smoke"));
    let model_dir = std::env::var("SEVQO_MODEL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("models/google/embeddinggemma-2"));

    if !fixtures.is_dir() {
        eprintln!("fixtures dir missing: {} (run tools/make_fixtures.py)", fixtures.display());
        std::process::exit(2);
    }
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).expect("create data dir");

    let paths = paths_for(&data, &model_dir);
    paths.ensure().expect("ensure paths");

    let mut settings = Settings::default();
    settings.index_images = true;
    settings.index_video = true;
    settings.infer_batch = 4;
    settings.infer_threads = 4;

    let db = Arc::new(Db::open(&paths.db_path).expect("open db"));
    let client = Arc::new(Client::new(&paths, &settings));
    let mut t = checks::Tracker { failed: 0, passed: 0 };

    println!("== runtime");
    match client.load() {
        Ok(rt) => t.ok(
            "sidecar",
            format!("{} {} dim={} {:.0}M params rss={:.0}MB", rt.device, rt.dtype, rt.dim, rt.params_millions, rt.rss_mb),
        ),
        Err(e) => {
            t.bad("sidecar", e);
            println!("\nsidecar unavailable — nothing else can be checked");
            std::process::exit(1);
        }
    }

    println!("== index");
    let root_id = db.add_root(&fixtures.to_string_lossy(), "fixtures").expect("add_root");
    let (tx, rx) = unbounded::<Progress>();
    let printer = std::thread::spawn(move || {
        let mut last = String::new();
        while let Ok(p) = rx.recv() {
            let line = format!("{:?} {}/{} {}", p.phase, p.done, p.total_estimate, p.current.unwrap_or_default());
            if line != last {
                last = line.clone();
                if p.phase != sevqo_engine::Phase::Idle {
                    println!("       {line}");
                }
            }
        }
    });
    let indexer = Indexer::new(db.clone(), paths.clone(), settings.clone(), Some(client.clone()), tx.clone());
    let scanned = indexer.scan_root(root_id, Mode::Full).expect("scan");
    println!("  scanned {} entries, {} candidates", scanned.walked, scanned.indexed_candidates);
    let sum = indexer.run(Mode::Full, None).expect("index");
    // recv() only reports a disconnect once every sender is gone, and the Indexer keeps
    // its own clone — dropping it here is what lets the progress printer thread end.
    drop((indexer, tx));
    let _ = printer.join();
    t.check("files indexed", sum.done >= 15, format!("done={} failed={} skipped={} vectors={}", sum.done, sum.failed, sum.skipped, sum.vectors));
    t.check("scan found everything", scanned.indexed_candidates >= 15, format!("{scanned:?}"));

    println!("== search");
    let mut searcher = Searcher::new(db.clone(), &paths.vector_path).expect("searcher");
    searcher.refresh().ok();

    for (name, query, expect) in [
        ("keyword cjk", "骑自行车", "beach.md"),
        ("keyword latin", "lighthouse", "lighthouse.pdf"),
        ("legacy gbk", "湖面", "legacy.txt"),
    ] {
        let req = SearchRequest { query: query.into(), limit: 10, filters: Filters::default(), semantic: false };
        let resp = searcher.run(&req, None).expect("run");
        let hit = resp.hits.iter().find(|h| h.name == expect);
        t.check(name, hit.is_some(), format!("{} hits, top: {}", resp.total, show(&resp)));
    }

    let qv = search::query_vector(&client, "日落时的海滩", settings.infer_batch);
    match qv {
        Ok(v) => {
            let req = SearchRequest { query: "日落".into(), limit: 5, filters: Filters::default(), semantic: true };
            let resp = searcher.run(&req, Some(&v)).expect("semantic run");
            let best = resp.hits.first().map(|h| h.kind).unwrap_or(Kind::Other);
            t.check("semantic hybrid", resp.total > 0, format!("top kind {best:?}: {}", show(&resp)));

            // 自然语言搜图片 is the headline P0, so prove it on its own: only image rows are
            // searchable and nothing may come back through the keyword channel.
            let img_only = SearchRequest {
                query: "日落".into(),
                limit: 5,
                filters: Filters { kinds: vec![Kind::Image], ..Default::default() },
                semantic: true,
            };
            match searcher.run(&img_only, Some(&v)) {
                Ok(r) => {
                    let top = r.hits.first();
                    let ok = top
                        .map(|h| h.kind == Kind::Image && h.channel != Channel::Keyword)
                        .unwrap_or(false);
                    t.check("text→image", ok, format!("{} hits: {}", r.total, show(&r)));
                }
                Err(e) => t.bad("text→image", e),
            }
        }
        Err(e) => t.bad("semantic hybrid", e),
    }

    if let Some(img) = first_file(&fixtures, "jpg") {
        match search::query_vector_from_image(&client, &img, settings.infer_batch) {
            Ok(v) => {
                let req = SearchRequest {
                    query: String::new(),
                    limit: 5,
                    filters: Filters { kinds: vec![Kind::Image], ..Default::default() },
                    semantic: true,
                };
                let resp = searcher.run(&req, Some(&v)).expect("image run");
                let want = img.file_name().unwrap_or_default().to_string_lossy().to_string();
                let top = resp.hits.first().map(|h| h.name.clone()).unwrap_or_default();
                t.check(
                    "image→image self match",
                    top == want,
                    format!("expected {want}, got {top}: {}", show(&resp)),
                );
            }
            Err(e) => t.bad("image→image self match", e),
        }
    } else {
        t.bad("image→image self match", "no jpg in fixtures");
    }

    println!("== video");
    if let Some(clip) = first_file(&fixtures, "mp4") {
        let rec = db.file_by_path(&clip.to_string_lossy());
        match rec {
            Some(rec) => {
                let clips = db.clip_rows(rec.id).unwrap_or_default();
                t.check(
                    "clips planned",
                    clips.len() >= 2 && rec.duration_ms.unwrap_or(0) > 30_000,
                    format!("{} clips, duration {} ms", clips.len(), rec.duration_ms.unwrap_or(0)),
                );
                let with_vecs = clips.iter().filter(|(_, _, _, vr, _)| vr.is_some()).count();
                t.check(
                    "clip vectors stored",
                    with_vecs == clips.len() && clips.iter().all(|(_, _, _, _, th)| th.is_some()),
                    format!("{with_vecs}/{} clips embedded, all have thumbnails: {}", clips.len(), clips.iter().all(|(_, _, _, _, th)| th.is_some())),
                );
                // 以视频片段搜视频: the clip itself must come back first, and it must carry moments.
                match search::query_vector_from_video(&client, &clip, settings.video_window_s, settings.infer_batch) {
                    Ok(v) => {
                        let req = SearchRequest {
                            query: String::new(),
                            limit: 5,
                            filters: Filters { kinds: vec![Kind::Video], ..Default::default() },
                            semantic: true,
                        };
                        let resp = searcher.run(&req, Some(&v)).expect("clip query run");
                        let want = clip.file_name().unwrap_or_default().to_string_lossy().to_string();
                        let top = resp.hits.first().map(|h| h.name.clone()).unwrap_or_default();
                        let moments = resp.hits.first().map(|h| h.moments.len()).unwrap_or(0);
                        t.check(
                            "clip→video self match",
                            top == want && moments > 0,
                            format!("expected {want}, got {top}, {moments} timestamps: {}", show(&resp)),
                        );
                    }
                    Err(e) => t.bad("clip→video self match", e),
                }
                let req = SearchRequest {
                    query: String::new(),
                    limit: 10,
                    filters: Filters { kinds: vec![Kind::Video], ..Default::default() },
                    semantic: true,
                };
                match search::query_vector(&client, "交替出现的纯色画面", settings.infer_batch) {
                    Ok(v) => {
                        let resp = searcher.run(&req, Some(&v)).expect("video run");
                        let with_moments = resp.hits.iter().filter(|h| !h.moments.is_empty()).count();
                        println!("  INFO  video search: {} hits, {with_moments} with timestamps: {}", resp.total, show(&resp));
                    }
                    Err(e) => println!("  INFO  video search unavailable: {e}"),
                }
            }
            None => t.bad("clips planned", "video not indexed"),
        }
    } else {
        t.bad("clips planned", "no mp4 in fixtures");
    }

    println!("== storage");
    let alive = db.alive_vec_rows().unwrap_or(0);
    t.check("vectors persisted", alive >= 15, format!("{alive} alive rows"));
    t.check(
        "thumbnails written",
        !collect(&paths.thumb_dir, "jpg").is_empty(),
        format!("{} thumbs", collect(&paths.thumb_dir, "jpg").len()),
    );

    let sidecar = client.pid();
    client.shutdown();
    let gone = sidecar
        .map(|p| {
            !std::process::Command::new("ps")
                .args(["-p", &p.to_string()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .unwrap_or(true);
    t.check("sidecar reaped on shutdown", gone, format!("pid {sidecar:?}"));
    println!("\n{} passed, {} failed", t.passed, t.failed);
    if t.failed > 0 {
        std::process::exit(1);
    }
    println!("SMOKE OK");
}
