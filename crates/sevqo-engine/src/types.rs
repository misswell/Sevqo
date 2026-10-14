use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Video,
    Document,
    Code,
    Audio,
    Other,
}

impl Kind {
    pub fn from_ext(ext: &str) -> Kind {
        let e = ext.to_ascii_lowercase();
        match e.as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "dng" => {
                Kind::Image
            }
            "mp4" | "mov" | "m4v" | "mkv" | "avi" | "webm" | "wmv" | "flv" | "mpg" | "mpeg" | "3gp" => Kind::Video,
            "pdf" | "doc" | "docx" | "rtf" | "odt" | "txt" | "md" | "markdown" | "mdx" | "epub" | "pptx" | "xlsx"
            | "csv" | "tsv" | "org" | "rst" | "log" => Kind::Document,
            "rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "hpp"
            | "cc" | "rb" | "php" | "sql" | "sh" | "zsh" | "bash" | "json" | "json5" | "yaml" | "yml" | "toml"
            | "ini" | "xml" | "html" | "css" | "scss" | "vue" | "lua" | "r" | "m" | "mm" | "dart" | "scala"
            | "gradle" | "dockerfile" | "tf" | "proto" => Kind::Code,
            "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" | "oga" | "opus" | "wma" | "aiff" => Kind::Audio,
            _ => Kind::Other,
        }
    }

    pub fn has_text(self) -> bool {
        matches!(self, Kind::Document | Kind::Code)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Kind::Image => "图片",
            Kind::Video => "视频",
            Kind::Document => "文档",
            Kind::Code => "代码",
            Kind::Audio => "音频",
            Kind::Other => "其他",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileState {
    Pending,
    Indexing,
    Done,
    Failed,
    Skipped,
    Stale,
}

impl FileState {
    pub fn as_str(self) -> &'static str {
        match self {
            FileState::Pending => "pending",
            FileState::Indexing => "indexing",
            FileState::Done => "done",
            FileState::Failed => "failed",
            FileState::Skipped => "skipped",
            FileState::Stale => "stale",
        }
    }
    pub fn parse(s: &str) -> FileState {
        match s {
            "done" => FileState::Done,
            "failed" => FileState::Failed,
            "skipped" => FileState::Skipped,
            "stale" => FileState::Stale,
            "indexing" => FileState::Indexing,
            _ => FileState::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub label: String,
    pub enabled: bool,
    pub last_scan_ns: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRecord {
    pub id: i64,
    pub root_id: Option<i64>,
    pub path: String,
    pub name: String,
    pub ext: String,
    pub kind: Kind,
    pub size: i64,
    pub mtime_ns: i64,
    pub duration_ms: Option<i64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub state: FileState,
    pub error: Option<String>,
    pub indexed_ns: i64,
    pub thumb: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Keyword,
    Semantic,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub file_id: i64,
    pub path: String,
    pub name: String,
    pub kind: Kind,
    pub size: i64,
    pub mtime_ns: i64,
    pub score: f64,
    pub channel: Channel,
    pub keyword_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
    pub snippet: Option<String>,
    /// Video moments that matched, so the UI can offer "跳转到 02:18".
    pub moments: Vec<Moment>,
    pub thumb: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Moment {
    pub clip_id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub score: f64,
    pub thumb: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filters {
    pub kinds: Vec<Kind>,
    pub root_id: Option<i64>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    pub after_ns: Option<i64>,
    pub before_ns: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
    pub filters: Filters,
    /// When false, only the keyword channel runs (instant, no inference needed).
    pub semantic: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            query: String::new(),
            limit: 60,
            filters: Filters::default(),
            semantic: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    pub keyword_ms: f64,
    pub semantic_ms: f64,
    pub total: usize,
    pub semantic_active: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Scanning,
    Indexing,
    Watching,
    Paused,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: Phase,
    pub discovered: u64,
    pub queued: u64,
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
    pub total_estimate: u64,
    pub current: Option<String>,
    pub images_per_min: f64,
    pub elapsed_s: f64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbedRowKind {
    Image,
    Clip,
    Chunk,
}

impl EmbedRowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedRowKind::Image => "image",
            EmbedRowKind::Clip => "clip",
            EmbedRowKind::Chunk => "chunk",
        }
    }
    pub fn parse(s: &str) -> EmbedRowKind {
        match s {
            "clip" => EmbedRowKind::Clip,
            "chunk" => EmbedRowKind::Chunk,
            _ => EmbedRowKind::Image,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VecRow {
    pub row: i64,
    pub target: EmbedRowKind,
    pub ref_id: i64,
    pub file_id: i64,
    pub file_kind: Kind,
}

/// One unit of work handed to the inference sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum EmbedItem {
    #[serde(rename_all = "camelCase")]
    Text { key: String, text: String, title: Option<String>, role: String },
    #[serde(rename_all = "camelCase")]
    Image { key: String, path: String, #[serde(skip_serializing_if = "Option::is_none")] thumb: Option<String> },
    #[serde(rename_all = "camelCase")]
    Video {
        key: String,
        path: String,
        start: f64,
        end: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        fps: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thumb: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedResponse {
    #[serde(default)]
    pub vectors: std::collections::HashMap<String, Vec<f32>>,
    #[serde(default)]
    pub errors: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub thumbs: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub ms: f64,
    #[serde(default)]
    pub rss_mb: f64,
}
