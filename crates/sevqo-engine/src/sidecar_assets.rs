//! The Python sidecar ships inside the binary.
//!
//! A packaged `.app` has no repo layout next to its executable, so `PYTHONPATH` would point at
//! nothing and `python3 -m sevqo_embed` would die on ModuleNotFoundError. The seven modules are
//! under 100 KB, which is cheaper to embed and materialise than to route through the bundler.

use std::path::{Path, PathBuf};

use crate::Result;

const MODULES: &[(&str, &str)] = &[
    (
        "__init__.py",
        include_str!("../../../inference/sevqo_embed/__init__.py"),
    ),
    (
        "__main__.py",
        include_str!("../../../inference/sevqo_embed/__main__.py"),
    ),
    (
        "config.py",
        include_str!("../../../inference/sevqo_embed/config.py"),
    ),
    (
        "decode.py",
        include_str!("../../../inference/sevqo_embed/decode.py"),
    ),
    (
        "documents.py",
        include_str!("../../../inference/sevqo_embed/documents.py"),
    ),
    (
        "model.py",
        include_str!("../../../inference/sevqo_embed/model.py"),
    ),
    (
        "server.py",
        include_str!("../../../inference/sevqo_embed/server.py"),
    ),
];

/// Write the package out and return the directory to put on `PYTHONPATH` — the package itself
/// lives at `<root>/sevqo_embed`.
pub fn stage(root: &Path) -> Result<PathBuf> {
    let pkg = root.join("sevqo_embed");
    std::fs::create_dir_all(&pkg)?;
    for (name, src) in MODULES {
        let path = pkg.join(name);
        // This runs on every start. Rewriting identical bytes would bump mtimes and throw away
        // the `__pycache__` Python builds next to the source.
        if std::fs::read(&path)
            .map(|b| b == src.as_bytes())
            .unwrap_or(false)
        {
            continue;
        }
        std::fs::write(&path, src)?;
    }
    Ok(root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_an_importable_package_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = stage(dir.path()).unwrap();
        assert_eq!(root, dir.path().to_path_buf());
        assert!(root.join("sevqo_embed/__init__.py").is_file());
        assert!(root.join("sevqo_embed/__main__.py").is_file());
    }

    #[test]
    fn restaging_unchanged_modules_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        stage(dir.path()).unwrap();
        let target = dir.path().join("sevqo_embed/server.py");
        let before = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        stage(dir.path()).unwrap();
        let after = std::fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(before, after, "identical modules must not be rewritten");
    }

    #[test]
    fn repairs_a_truncated_staged_module() {
        let dir = tempfile::tempdir().unwrap();
        stage(dir.path()).unwrap();
        let target = dir.path().join("sevqo_embed/model.py");
        std::fs::write(&target, "x = 1\n").unwrap();
        stage(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            MODULES.iter().find(|(n, _)| *n == "model.py").unwrap().1
        );
    }

    /// A module added under `inference/` has to be embedded as well, otherwise a packaged build
    /// ships a sidecar that only works from a source checkout.
    #[test]
    fn embeds_every_module_in_the_package() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../inference/sevqo_embed");
        let Ok(entries) = std::fs::read_dir(&src) else {
            return;
        };
        let mut on_disk = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".py"))
            .collect::<Vec<_>>();
        on_disk.sort();
        let mut embedded = MODULES.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>();
        embedded.sort();
        assert_eq!(embedded, on_disk, "inference/sevqo_embed changed shape");
    }

    /// The embedded bytes are the shipped sidecar, so they have to import — a SyntaxError or a
    /// missing sibling module would only show up on a machine with no repo layout. Costs a torch
    /// import, so it runs on demand: `SEVQO_TEST_PYTHON=.venv/bin/python cargo test -p sevqo-engine`.
    #[test]
    fn staged_package_imports() {
        let Some(py) = std::env::var("SEVQO_TEST_PYTHON").ok().filter(|s| !s.is_empty()) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let root = stage(dir.path()).unwrap();
        // Python reports `__file__` through the resolved temp path (/var → /private/var on macOS),
        // so compare canonical forms or the ownership check below always fails.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let probe = r#"
import glob, importlib, json, py_compile, sys
root = sys.argv[1]
for f in glob.glob(root + "/sevqo_embed/*.py"):
    py_compile.compile(f, doraise=True)          # __main__.py runs but is never imported
mods = ["config", "decode", "documents", "model", "server"]
for m in mods:
    mod = importlib.import_module("sevqo_embed." + m)
    assert mod.__file__.startswith(root), mod.__file__
print(json.dumps({"ok": True, "pkg": importlib.import_module("sevqo_embed").__path__[0]}))
"#;
        let out = std::process::Command::new(&py)
            .arg("-c")
            .arg(probe)
            .arg(&root)
            .env("PYTHONPATH", &root)
            .current_dir(&root)
            .output()
            .unwrap_or_else(|e| panic!("{py} could not run: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stdout.contains("\"ok\": true"),
            "import failed: status {:?}\nstdout {stdout}\nstderr {stderr}",
            out.status.code()
        );
    }
}
