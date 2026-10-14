//! `sevqo-media://` — hands the webview the bytes of an indexed video so a result can
//! play from the second that matched. Requests carry a file id rather than a path, so
//! nothing outside the index is reachable, and `Range` is honoured because that is how
//! a `<video>` element seeks.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use tauri::http::header;
use tauri::http::{Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use sevqo_engine::Kind;

use crate::state::AppState;

/// `bytes=0-` must not pull a multi-GB film into RAM: the player asks for the next
/// range when it needs more, exactly like a streaming server.
const CHUNK: u64 = 8 << 20;

pub fn handle<R: Runtime>(ctx: UriSchemeContext<'_, R>, req: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let state = ctx.app_handle().state::<Arc<AppState>>().inner().clone();
    let Some(id) = parse_id(req.uri().path()) else {
        return responder.respond(status(StatusCode::BAD_REQUEST, "路径格式错误"));
    };
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    tauri::async_runtime::spawn_blocking(move || responder.respond(serve(&state, id, &range)));
}

fn serve(state: &AppState, id: i64, range: &str) -> Response<Vec<u8>> {
    let Some(rec) = state.db.file(id) else {
        return status(StatusCode::NOT_FOUND, "文件未在索引中");
    };
    if rec.kind != Kind::Video {
        return status(StatusCode::FORBIDDEN, "只有视频支持应用内播放");
    }
    let allowed = rec
        .root_id
        .and_then(|rid| state.db.list_roots().ok().map(|roots| roots.iter().any(|r| r.id == rid && r.enabled)))
        .unwrap_or(false);
    if !allowed {
        return status(StatusCode::FORBIDDEN, "所属目录已停用或被删除");
    }

    let path = Path::new(&rec.path);
    let Ok(meta) = std::fs::metadata(path) else {
        return status(StatusCode::NOT_FOUND, "文件已经不存在，可能被移动或删除");
    };
    let total = meta.len();
    let Some((start, end)) = span(range, total) else {
        return status(StatusCode::RANGE_NOT_SATISFIABLE, "请求范围无效");
    };

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return status(StatusCode::NOT_FOUND, "文件没有读取权限"),
    };
    let mut buf = vec![0u8; (end - start + 1) as usize];
    if file.seek(SeekFrom::Start(start)).is_err() || read_span(&mut file, &mut buf) < buf.len() {
        // The file shrank between `metadata` and the read — the next scan will fix the index.
        return status(StatusCode::NOT_FOUND, "文件长度与索引不符，请重新扫描");
    }

    let partial = start > 0 || end + 1 < total;
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type(&rec.ext))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, buf.len().to_string());
    builder = if partial {
        builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))
    } else {
        builder.status(StatusCode::OK)
    };
    builder.body(buf).unwrap_or_else(|_| Response::new(Vec::new()))
}

/// `/video/<id>` — the `#t=` fragment never reaches the handler, so the start time is
/// applied by the player.
fn parse_id(path: &str) -> Option<i64> {
    let mut seg = path.trim_start_matches('/').split('/');
    if seg.next()? != "video" {
        return None;
    }
    seg.next()?.parse().ok()
}

/// A request without `Range` is answered from 0, still capped by the chunk size.
fn span(raw: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let last = total - 1;
    let Some(spec) = raw.trim().strip_prefix("bytes=").map(str::trim) else {
        return Some((0, last.min(CHUNK - 1)));
    };
    let (head, tail) = spec.split(',').next()?.trim().split_once('-')?;
    if head.is_empty() {
        // open start: the last N bytes
        let n = tail.parse::<u64>().ok()?.min(total).min(CHUNK);
        if n == 0 { return None; }
        return Some((total - n, last));
    }
    let start = head.parse::<u64>().ok()?;
    if start > last {
        return None;
    }
    let wanted = if tail.trim().is_empty() { last } else { tail.trim().parse::<u64>().ok()?.min(last) };
    if wanted < start { return None; }
    Some((start, wanted.min(start.saturating_add(CHUNK - 1))))
}

fn content_type(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" | "mp4v" => "video/mp4",
        "mov" | "qt" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "ts" | "m2ts" => "video/mp2t",
        "flv" => "video/x-flv",
        "wmv" => "video/x-ms-wmv",
        "3gp" => "video/3gpp",
        _ => "application/octet-stream",
    }
}

fn status(code: StatusCode, msg: &str) -> Response<Vec<u8>> {
    let body = msg.as_bytes().to_vec();
    Response::builder()
        .status(code)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .body(body)
        .unwrap_or_else(|_| Response::new(Vec::new()))
}

/// `read_exact` reports a short file as an error; a stale index deserves a real message.
fn read_span(file: &mut std::fs::File, buf: &mut [u8]) -> usize {
    let mut got = 0usize;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    got
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_parses_from_path() {
        assert_eq!(parse_id("/video/42"), Some(42));
        assert_eq!(parse_id("/image/42"), None);
        assert_eq!(parse_id("/video/"), None);
        assert_eq!(parse_id("/video/abc"), None);
    }

    #[test]
    fn spans_are_bounded() {
        assert_eq!(span("", 1_000), Some((0, 999)));
        assert_eq!(span("bytes=0-", 1_000), Some((0, 999)));
        // an open-ended request on a big file stops at the chunk
        assert_eq!(span("bytes=100-", 1 << 30), Some((100, 100 + CHUNK - 1)));
        assert_eq!(span("bytes=0-49", 1_000), Some((0, 49)));
        assert_eq!(span("bytes=-10", 1_000), Some((990, 999)));
        assert_eq!(span("bytes=2000-", 1_000), None);
        assert_eq!(span("bytes=0-20000", 1_000), Some((0, 999)));
        assert_eq!(span("bytes=0-100", 0), None);
        assert_eq!(span("garbage", 1_000), Some((0, 999)));
    }

    #[test]
    fn suffix_ranges_cannot_allocate_the_whole_video() {
        let total = 1 << 30;
        assert_eq!(span("bytes=-1073741824", total), Some((total - CHUNK, total - 1)));
    }

    #[test]
    fn invalid_ranges_never_underflow_or_overflow() {
        for range in ["bytes=10-1", "bytes=-0", "bytes=0-oops"] {
            assert_eq!(span(range, 100), None);
        }
        assert_eq!(span("bytes=18446744073709551612-", u64::MAX), Some((u64::MAX - 3, u64::MAX - 1)));
    }

    #[test]
    fn mime_follows_extension() {
        assert_eq!(content_type("MP4"), "video/mp4");
        assert_eq!(content_type("mov"), "video/quicktime");
        assert_eq!(content_type("txt"), "application/octet-stream");
    }
}
