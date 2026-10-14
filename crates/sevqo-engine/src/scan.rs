use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Kind, Result};

#[derive(Debug, Clone)]
pub struct Scanned {
    pub root_id: i64,
    pub path: String,
    pub name: String,
    pub ext: String,
    pub kind: Kind,
    pub size: i64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub walked: u64,
    pub indexed_candidates: u64,
    pub skipped_hidden: u64,
    pub skipped_too_big: u64,
    pub skipped_excluded: u64,
    pub unreadable: u64,
    pub errors: u64,
}

pub struct ScanOptions<'a> {
    pub exclude_dirs: &'a [String],
    pub exclude_prefixes: &'a [String],
    pub max_file_bytes: u64,
    pub kinds: &'a [Kind],
}

impl Default for ScanOptions<'static> {
    fn default() -> Self {
        ScanOptions {
            exclude_dirs: &[],
            exclude_prefixes: &[],
            max_file_bytes: 256 * 1024 * 1024,
            kinds: &[],
        }
    }
}

fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// True when the file should be indexed at all.
pub fn wanted(ext: &str, kinds: &[Kind]) -> bool {
    let k = Kind::from_ext(ext);
    if kinds.is_empty() {
        return k != Kind::Other;
    }
    kinds.contains(&k)
}

fn is_excluded_dir(name: &str, excludes: &[String]) -> bool {
    excludes.iter().any(|e| e.eq_ignore_ascii_case(name))
}

fn under_prefixes(path: &Path, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| !p.is_empty() && path.starts_with(expand_home(p)))
}

/// Apply the same scope exclusions to a watcher event as to a directory walk.
pub fn excluded_path(root: &Path, path: &Path, opts: &ScanOptions) -> bool {
    if under_prefixes(path, opts.exclude_prefixes) {
        return true;
    }
    let Ok(relative) = path.strip_prefix(root) else { return true };
    relative.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name.starts_with('.') || is_excluded_dir(&name, opts.exclude_dirs)
    })
}

/// Walk one root, emitting candidate files. `seen_ns` stamps every emitted file so a
/// later sweep can tell "not present" from "not yet visited".
pub fn scan<F>(
    root_id: i64,
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    mut emit: F,
) -> Result<ScanStats>
where
    F: FnMut(Scanned),
{
    let mut stats = ScanStats::default();
    if !root.exists() {
        return Err(Error::Config(format!("目录不存在：{}", root.display())));
    }
    let mut builder = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_open(64)
        .into_iter();
    while let Some(entry) = builder.next() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                stats.errors += 1;
                if err.depth() <= 1 {
                    tracing::debug!("walk: {err}");
                }
                continue;
            }
        };
        let path = entry.path();
        stats.walked += 1;
        let file_type = entry.file_type();
        let name_os = match path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        // Depth 0 is the root the user picked: a dot in its name (or a temp dir under
        // `/var/folders/.tmp…`) must not silently index nothing.
        if entry.depth() > 0 && name_os.starts_with('.') && name_os != "." {
            stats.skipped_hidden += 1;
            if file_type.is_dir() {
                builder.skip_current_dir();
            }
            continue;
        }
        if file_type.is_dir() {
            if is_excluded_dir(&name_os, opts.exclude_dirs) || under_prefixes(path, opts.exclude_prefixes) {
                stats.skipped_excluded += 1;
                builder.skip_current_dir();
            }
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if under_prefixes(path, opts.exclude_prefixes) {
            stats.skipped_excluded += 1;
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                stats.unreadable += 1;
                continue;
            }
        };
        let size = meta.len() as i64;
        if size > opts.max_file_bytes as i64 {
            stats.skipped_too_big += 1;
            continue;
        }
        let ext = ext_of(&name_os);
        if !wanted(&ext, opts.kinds) {
            continue;
        }
        stats.indexed_candidates += 1;
        emit(Scanned {
            root_id,
            path: path.to_string_lossy().to_string(),
            name: name_os,
            ext: ext.clone(),
            kind: Kind::from_ext(&ext),
            size,
            mtime_ns: mtime_ns(&meta),
        });
    }
    Ok(stats)
}

/// Image size from the file header only — no full decode.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

pub fn dir_label(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.to_string_lossy().to_string())
}

pub fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(p));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn wanted_exts_map_to_kinds() {
        assert_eq!(Kind::from_ext("heic"), Kind::Image);
        assert_eq!(Kind::from_ext("mov"), Kind::Video);
        assert_eq!(Kind::from_ext("md"), Kind::Document);
        assert_eq!(Kind::from_ext("rs"), Kind::Code);
        assert_eq!(Kind::from_ext("bin"), Kind::Other);
        assert!(!wanted("bin", &[]));
        assert!(wanted("pdf", &[]));
        assert!(!wanted("pdf", &[Kind::Image]));
    }

    #[test]
    fn scan_skips_hidden_and_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("a.txt"), "x").unwrap();
        std::fs::create_dir(d.join(".git")).unwrap();
        std::fs::write(d.join(".git/config"), "x").unwrap();
        std::fs::create_dir(d.join("node_modules")).unwrap();
        std::fs::write(d.join("node_modules/b.txt"), "x").unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/c.md"), "x").unwrap();
        std::fs::write(d.join("big.bin"), vec![0u8; 10]).unwrap();

        let excl = ["node_modules".to_string()];
        let opts = ScanOptions { exclude_dirs: &excl, ..Default::default() };
        let cancel = AtomicBool::new(false);
        let mut found = Vec::new();
        let stats = scan(1, d, &opts, &cancel, |s| found.push(s.name)).unwrap();
        found.sort();
        assert_eq!(found, vec!["a.txt", "c.md"]);
        assert!(stats.skipped_hidden >= 1);
        assert_eq!(stats.skipped_excluded, 1);
    }

    #[test]
    fn max_bytes_skips_large() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let opts = ScanOptions { max_file_bytes: 3, ..Default::default() };
        let cancel = AtomicBool::new(false);
        let mut found = Vec::new();
        let stats = scan(1, dir.path(), &opts, &cancel, |s| found.push(s.name)).unwrap();
        assert!(found.is_empty());
        assert_eq!(stats.skipped_too_big, 1);
    }
}
