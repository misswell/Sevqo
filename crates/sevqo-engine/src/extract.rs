//! Text extraction for the keyword channel.
//!
//! PDFs are handled by the inference sidecar (`pypdf`), which decodes embedded font
//! CMaps correctly — CID-encoded Chinese text comes out as mojibake otherwise.

use std::path::Path;

use crate::{Error, Kind, Result};

#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub title: Option<String>,
    pub text: String,
    /// The source was longer than what we keep; surfaced in the UI so a short snippet
    /// is not mistaken for the whole document.
    pub truncated: bool,
}

/// Extensions this module reads without the sidecar.
pub fn native(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "md" | "markdown" | "mdx" | "org" | "rst" | "log" | "csv" | "tsv" | "rtf" | "docx" | "json"
            | "json5" | "yaml" | "yml" | "toml" | "ini" | "xml" | "html" | "css" | "scss" | "vue" | "js"
            | "jsx" | "ts" | "tsx" | "rs" | "go" | "py" | "java" | "kt" | "swift" | "c" | "h" | "cpp"
            | "hpp" | "cc" | "rb" | "php" | "sql" | "sh" | "zsh" | "bash" | "lua" | "r" | "m" | "mm"
            | "dart" | "scala" | "gradle" | "dockerfile" | "tf" | "proto"
    )
}

pub fn title_for(path: &Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().to_string())
}

pub fn extract(path: &Path, ext: &str, max_read_bytes: u64, max_store_kb: usize) -> Result<Extracted> {
    let ext = ext.to_ascii_lowercase();
    let raw = read_bounded(path, max_read_bytes)?;
    if raw.is_empty() {
        return Ok(Extracted { title: title_for(path), text: String::new(), truncated: false });
    }
    let (mut text, truncated) = match ext.as_str() {
        "docx" => (docx_text(&raw)?, false),
        "rtf" => (rtf_text(&raw), false),
        "html" => (html_text(&raw), false),
        _ => {
            if looks_binary(&raw) {
                return Err(Error::Unsupported("二进制内容，无正文可提取".into()));
            }
            (decode_utf8_lossy(&raw), false)
        }
    };
    // One byte budget for every format: a single huge log must not dominate the index.
    let budget = max_store_kb.saturating_mul(1024);
    let store_cut = truncate_bytes(&mut text, budget);
    Ok(Extracted {
        title: title_for(path),
        text,
        truncated: truncated || store_cut,
    })
}

/// Read at most `limit` bytes without loading a huge file into memory first.
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(Error::Unsupported("不是普通文件".into()));
    }
    let mut f = std::fs::File::open(path)?;
    let take = limit.min(meta.len()) as usize;
    let mut buf = Vec::with_capacity(take.min(64 * 1024 * 1024));
    f.by_ref().take(take as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn looks_binary(raw: &[u8]) -> bool {
    let head = &raw[..raw.len().min(4096)];
    head.contains(&0)
}

/// UTF-8 with a GB18030 fallback: legacy Chinese txt files are common enough that a
/// pure lossy decode would replace most of the content with U+FFFD.
fn decode_utf8_lossy(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_owned();
    }
    let lossy = String::from_utf8_lossy(raw);
    let bad = lossy.chars().filter(|c| *c == '\u{fffd}').count();
    if bad as f64 / lossy.chars().count().max(1) as f64 > 0.02 {
        return gbk_to_string(raw);
    }
    lossy.into_owned()
}

/// WHATWG gbk, whose decoder covers GB2312/GBK and the 4-byte GB18030 ranges.
fn gbk_to_string(raw: &[u8]) -> String {
    encoding_rs::GBK.decode(raw).0.into_owned()
}

/// Keep at most `max_bytes` of UTF-8, cutting on a char boundary. Indexed text is
/// capped so one enormous log cannot dominate the FTS index.
fn truncate_bytes(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    true
}

// --------------------------------------------------------------------------- docx

fn docx_text(raw: &[u8]) -> Result<String> {
    use std::io::{Cursor, Read};
    let mut archive = zip::ZipArchive::new(Cursor::new(raw.to_vec()))
        .map_err(|e| Error::Unsupported(format!("DOCX 解析失败: {e}")))?;
    let mut parts = vec!["word/document.xml"];
    // headers/footers carry titles and footnotes people actually search for
    let names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("word/header") || n.starts_with("word/footer"))
        .map(|s| s.to_owned())
        .collect();
    parts.extend(names.iter().map(|s| s.as_str()));
    let mut out = String::new();
    for name in parts {
        let mut idx = match archive.by_name(name) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if idx.compressed_size() > 32 * 1024 * 1024 {
            continue;
        }
        let mut xml = String::new();
        idx.read_to_string(&mut xml)?;
        out.push_str(&xml_paragraphs(&xml));
    }
    Ok(out)
}

/// Pull paragraph text out of WordprocessingML: `<w:t>` runs joined, `<w:p>` as newlines.
fn xml_paragraphs(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 4);
    for para in xml.split("</w:p>") {
        let mut buf = String::new();
        let bytes = para.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'<' && para[i..].starts_with("<w:t") {
                let close = match para[i..].find('>') {
                    Some(o) => i + o + 1,
                    None => break,
                };
                let end = match para[close..].find("</w:t>") {
                    Some(o) => close + o,
                    None => para.len(),
                };
                buf.push_str(&unescape_xml(&para[close..end]));
                i = end + 6;
                continue;
            }
            i += 1;
        }
        // tab and break elements are meaningful separators in Word
        if para.contains("<w:tab") {
            buf = buf.replace(' ', "\t");
        }
        if !buf.trim().is_empty() {
            out.push_str(buf.trim_end());
            out.push('\n');
        }
    }
    out
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#160;", " ")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------- rtf

fn rtf_text(raw: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    let mut skip_group_depth = 0usize;
    let mut depth = 0usize;
    while i < raw.len() {
        let b = raw[i];
        match b {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if skip_group_depth > 0 && depth < skip_group_depth {
                    skip_group_depth = 0;
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= raw.len() {
                    break;
                }
                if raw[i] == b'\'' {
                    let mut bytes = Vec::new();
                    while i + 2 < raw.len() && raw[i] == b'\'' {
                        let hi = hex_val(raw[i + 1]);
                        let lo = hex_val(raw[i + 2]);
                        bytes.push((hi << 4) | lo);
                        i += 3;
                    }
                    // runs of `\'hh` are one multi-byte character in the ANSI codepage
                    if skip_group_depth == 0 {
                        push_clean(&mut out, &decode_rtf_bytes(&bytes));
                    }
                    continue;
                }
                let word: String = raw[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphabetic())
                    .map(|c| *c as char)
                    .collect();
                match word.as_str() {
                    "" => {
                        // escaped literal: \\ \{ \}
                        let c = raw[i] as char;
                        i += 1;
                        if skip_group_depth == 0 {
                            out.push(c);
                        }
                    }
                    "u" => {
                        let n: String = raw[i + 1..]
                            .iter()
                            .take_while(|c| c.is_ascii_digit() || **c == b'-')
                            .map(|c| *c as char)
                            .collect();
                        if let Ok(v) = n.parse::<i64>() {
                            if skip_group_depth == 0 {
                                if let Some(c) = char::from_u32(v.unsigned_abs() as u32) {
                                    out.push(c);
                                }
                            }
                        }
                        i += 1 + n.len();
                        if raw.get(i).copied() == Some(b' ') {
                            i += 1;
                        }
                        // the ANSI fallback character that follows \uN is not text
                        if raw.get(i).copied() == Some(b'?') {
                            i += 1;
                        } else if raw.get(i).copied() == Some(b'\\')
                            && raw.get(i + 1).copied() == Some(b'\'')
                        {
                            i += 3;
                        }
                    }
                    w => {
                        // destinations we never want in search results
                        if matches!(
                            w,
                            "header"
                                | "footer"
                                | "headerf"
                                | "footerf"
                                | "stylesheet"
                                | "fonttbl"
                                | "colortbl"
                                | "info"
                                | "pict"
                                | "object"
                                | "thematicbreak"
                                | "generator"
                                | "listtable"
                                | "listoverridetable"
                                | "filetbl"
                                | "NonShippableObjList"
                        ) {
                            skip_group_depth = skip_group_depth.max(depth);
                        }
                        i += w.len();
                        let num: String = raw[i..]
                            .iter()
                            .take_while(|c| c.is_ascii_digit() || **c == b'-')
                            .map(|c| *c as char)
                            .collect();
                        i += num.len();
                        if raw.get(i).copied() == Some(b' ') {
                            i += 1;
                        }
                        if matches!(w, "par" | "line" | "sect") {
                            if skip_group_depth == 0 {
                                out.push('\n');
                            }
                        } else if w == "tab" && skip_group_depth == 0 {
                            out.push('\t');
                        }
                    }
                }
            }
            _ => {
                if skip_group_depth > 0 {
                    i += 1;
                    continue;
                }
                if b >= 0x80 {
                    let start = i;
                    while i < raw.len() && raw[i] >= 0x80 {
                        i += 1;
                    }
                    push_clean(&mut out, &decode_rtf_bytes(&raw[start..i]));
                    continue;
                }
                out.push(if b.is_ascii_whitespace() { ' ' } else { b as char });
                i += 1;
            }
        }
    }
    dedup_blank_lines(&out)
}

/// RTF is byte-oriented: newer files embed UTF-8 text directly, older ANSI ones use a
/// multi-byte codepage. Decoding as UTF-8 first is what keeps Chinese documents
/// readable instead of turning them into Latin-1 noise.
fn decode_rtf_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => gbk_to_string(bytes),
    }
}

fn push_clean(out: &mut String, s: &str) {
    for ch in s.chars() {
        out.push(if ch == '\u{fffd}' { '?' } else { ch });
    }
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

// --------------------------------------------------------------------------- html

fn html_text(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(s.len() / 2);
    let mut tag = false;
    let mut in_script = false;
    let lower = s.to_ascii_lowercase();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '<' {
            tag = true;
            let rest = &lower[i..];
            if rest.starts_with("<script") || rest.starts_with("<style") {
                in_script = true;
            } else if rest.starts_with("</script") || rest.starts_with("</style") {
                in_script = false;
            }
            continue;
        }
        if c == '>' {
            tag = false;
            // Tag boundaries separate words, but adjacent tags (`<b>a</b><i>b</i>`)
            // must not stack up spaces — CJK text would then differ from the file.
            if !out.ends_with(|ch: char| ch.is_whitespace()) {
                out.push(' ');
            }
            continue;
        }
        if !tag && !in_script {
            out.push(c);
        }
    }
    dedup_blank_lines(&unescape_xml(&out))
}

fn dedup_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

/// Split text into embedding-sized pieces. Sentence boundaries are preferred so a chunk
/// rarely starts or ends mid-clause, which is what makes doc snippets match queries.
pub fn chunk_text(text: &str, target: usize, max_chunks: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let hard_end = (start + target).min(chars.len());
        let end = if hard_end == chars.len() {
            hard_end
        } else {
            find_boundary(&chars, start, hard_end)
        };
        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
            if out.len() >= max_chunks {
                break;
            }
        }
        if end >= chars.len() {
            break;
        }
        start = end;
    }
    out
}

/// Look back up to 25% of the window for a natural break: paragraph, sentence end,
/// then whitespace. CJK sentence marks (。！？；) count, since they are the only
/// whitespace-free break points in Chinese prose.
fn find_boundary(chars: &[char], start: usize, hard_end: usize) -> usize {
    let min = start + (hard_end - start) / 2;
    let mut i = hard_end;
    while i > min {
        let c = chars[i - 1];
        if c == '\n' && chars.get(i - 2).copied() == Some('\n') {
            return i;
        }
        if matches!(c, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';' | ' ') && i > min {
            return i;
        }
        i -= 1;
    }
    hard_end
}

pub fn kind_wants_text(k: Kind) -> bool {
    k.has_text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_chunking_prefers_sentence_breaks() {
        let text = "海边日落。云层很厚。远处有船在行驶。".repeat(30);
        let chunks = chunk_text(&text, 40, 100);
        assert!(chunks.len() > 3);
        for c in &chunks {
            assert!(c.chars().count() <= 40);
            assert!(!c.is_empty());
        }
        // no chunk may start with a dangling sentence fragment marker
        assert!(chunks.iter().all(|c| !c.starts_with('。')));
    }

    #[test]
    fn plain_text_extracts_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.md");
        std::fs::write(&p, "# 标题\n海边的日落照片").unwrap();
        let got = extract(&p, "md", 1 << 20, 4).unwrap();
        assert!(got.text.contains("海边的日落照片"));
        assert_eq!(got.title.as_deref(), Some("a"));
    }

    #[test]
    fn legacy_gbk_text_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.txt");
        // "海边的日落" in GB2312 — what Chinese editors wrote before UTF-8
        std::fs::write(&p, [0xBAu8, 0xA3, 0xB1, 0xDF, 0xB5, 0xC4, 0xC8, 0xD5, 0xC2, 0xE4]).unwrap();
        let got = extract(&p, "txt", 1 << 20, 64).unwrap();
        assert_eq!(got.text, "海边的日落", "{:?}", got.text);
    }

    #[test]
    fn binary_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.bin");
        std::fs::write(&p, [0u8, 1, 2, 3, 255, 9]).unwrap();
        assert!(extract(&p, "bin", 1 << 20, 64).is_err());
    }

    #[test]
    fn html_drops_script_body() {
        let html = "<html><body><script>var x=1;</script><p>海 <b>边</b></p></body></html>".as_bytes();
        assert_eq!(html_text(html).trim(), "海 边");
    }

    #[test]
    fn xml_paragraphs_join_runs() {
        let xml = r#"<w:document><w:body><w:p><w:r><w:t>海</w:t></w:r><w:r><w:t>边</w:t></w:r></w:p><w:p><w:r><w:t>日落 &amp; 云</w:t></w:r></w:p></w:body></w:document>"#;
        let got = xml_paragraphs(xml);
        assert!(got.contains("海边"), "{got}");
        assert!(got.contains("日落 & 云"), "{got}");
    }

    #[test]
    fn rtf_strips_control_words() {
        let rtf = r"{\rtf1\ansi\deff0{\fonttbl{\f0 Arial;}}海边的\par 日落\'0a}";
        let got = rtf_text(rtf.as_bytes());
        assert!(got.contains("海边的"), "{got}");
        assert!(got.contains("日落"), "{got}");
        assert!(!got.contains("Arial"), "{got}");
    }

    #[test]
    fn unescape_handles_common_entities() {
        assert_eq!(unescape_xml("&lt;a&gt;&amp;&#160;b"), "<a>& b");
    }

    #[test]
    fn native_covers_doc_family() {
        assert!(native("md"));
        assert!(native("docx"));
        assert!(!native("pdf"));
        assert!(!native("jpg"));
    }
}
