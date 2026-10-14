//! Chinese/Japanese/Korean aware tokenisation for SQLite FTS5.
//!
//! FTS5's `unicode61` tokenizer treats a whole CJK run as one token, so indexing
//! "海边的日落" can never be found by the query "海边". Verified against
//! SQLite 3.50 on this machine. We therefore index CJK one character per token and
//! express a CJK query as an FTS5 phrase, which gives exact-substring semantics —
//! the behaviour people expect from a file search tool.

const CJK_PUNCT: &[char] = &['，', '。', '、', '；', '：', '？', '！', '（', '）', '「', '」', '『', '』', '·', '—', '…', '《', '》', '【', '】'];

pub fn is_cjk(c: char) -> bool {
    matches!(u32::from(c),
        0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af
        | 0xf900..=0xfaff | 0x20000..=0x2a6df | 0x3000..=0x303f)
}

fn is_break(c: char) -> bool {
    c.is_whitespace()
        || matches!(c,
            '.' | ',' | '/' | '-' | '_' | ':' | ';' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}'
            | '@' | '#' | '$' | '%' | '^' | '&' | '*' | '+' | '=' | '~' | '`' | '|' | '\\' | '!' | '?' | '<' | '>')
        || CJK_PUNCT.contains(&c)
}

/// Indexed form of a document field: latin words lowercased and split, CJK split per char.
pub fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(word);
            out.push(' ');
            word.clear();
        }
    };
    for c in text.chars() {
        if is_cjk(c) {
            flush(&mut word, &mut out);
            out.push(c);
            out.push(' ');
        } else if is_break(c) {
            flush(&mut word, &mut out);
        } else {
            word.extend(c.to_lowercase());
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A quoted FTS5 phrase/term, escaping embedded double quotes.
fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Turns free text into a MATCH expression: terms ANDed, CJK runs as char phrases.
pub fn match_expression(query: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        let seg = segment(token);
        let terms: Vec<&str> = seg.split_whitespace().collect();
        match terms.len() {
            0 => {}
            1 => parts.push(quote(terms[0])),
            n => {
                // A CJK run becomes a phrase; a trailing latin word after CJK chars
                // still belongs to the same contiguous run, so keep it inside.
                let phrase = terms.join(" ");
                if n > 1 && phrase.contains(' ') {
                    parts.push(quote(&phrase));
                } else {
                    parts.extend(terms.iter().map(|t| quote(t)));
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

/// OR-joined expansion used when AND-ing produces nothing (recall fallback).
pub fn match_expression_loose(query: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        parts.extend(segment(token).split_whitespace().map(quote));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" OR "))
    }
}

/// Highlights the first matching span of `needle` inside `haystack`.
pub fn snippet(haystack: &str, needle: &str, radius: usize) -> Option<String> {
    let hay = haystack;
    let key: String = needle.chars().filter(|c| !is_break(*c)).collect();
    if key.is_empty() {
        return None;
    }
    let lower = hay.to_lowercase();
    let key_lower = key.to_lowercase();
    let mut best: Option<usize> = None;
    for cand in [key.as_str(), key_lower.as_str()] {
        if let Some(pos) = lower.find(cand) {
            best = Some(pos);
            break;
        }
    }
    best.map(|pos| {
        let start = pos.saturating_sub(radius);
        let end = (pos + key.len() + radius).min(hay.len());
        let mut prefix = String::new();
        if start > 0 {
            prefix.push('…');
        }
        // slice on char boundaries
        let s = floor_char_boundary(hay, start);
        let e = ceil_char_boundary(hay, end);
        let mut out = prefix;
        out.push_str(&hay[s..e]);
        if e < hay.len() {
            out.push('…');
        }
        out
    })
}

fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut j = i;
    while j > 0 && !s.is_char_boundary(j) {
        j -= 1;
    }
    j
}

fn ceil_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut j = i;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_run_splits_per_character() {
        assert_eq!(segment("海边的日落"), "海 边 的 日 落 ");
    }

    #[test]
    fn latin_is_lowercased_and_split() {
        assert_eq!(segment("Redis-Lock impl.go"), "redis lock impl go ");
    }

    #[test]
    fn mixed_keeps_order() {
        assert_eq!(segment("分布式锁 lock"), "分 布 式 锁 lock ");
    }

    #[test]
    fn query_becomes_phrases() {
        assert_eq!(match_expression("海边 日落").as_deref(), Some("\"海 边\" AND \"日 落\""));
        assert_eq!(match_expression("redis").as_deref(), Some("\"redis\""));
        assert_eq!(match_expression("！！！").as_deref(), None);
    }

    #[test]
    fn loose_expands_to_or() {
        assert_eq!(match_expression_loose("海边").as_deref(), Some("\"海\" OR \"边\""));
    }

    #[test]
    fn snippet_cuts_on_char_boundaries() {
        let s = snippet("这是一个关于Redis分布式锁的实现说明，很长很长很长", "分布式", 4).unwrap();
        assert!(s.contains("分布式"));
    }
}
