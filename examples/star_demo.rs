//! Demo: star re-export without default export in target.
use susee_bundler::{susee_bundler, CheckOptions};
use std::fs;
use tempfile::TempDir;

fn main() {
    let dir = TempDir::new().unwrap();
    let files = [
        ("src/index.js", "export * from \"./foo.js\";\nexport const gamma = \"G\";\n"),
        ("src/foo.js", "export const alpha = \"A\";\nexport function beta() { return alpha; }\n"),
    ];
    for (path, content) in &files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    let result = susee_bundler(
        "src/index.js".to_string(),
        Some(dir.path().to_string_lossy().to_string()),
        Some(CheckOptions::default()),
    );
    println!("{}", result.bundled_code);
}
