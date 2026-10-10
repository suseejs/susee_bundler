//! End-to-end integration tests for the bundler, exercising the full
//! pipeline (tree → handlers → clean → pretty-print) on temporary
//! projects created on disk.

use susee_bundler::CheckOptions;
use susee_bundler::susee_bundler;
use std::fs;
use tempfile::TempDir;

/// Bundle a project with the given files, using `entry` as the entry point.
///
/// `files` maps relative paths to file contents.
fn bundle_project(entry: &str, files: &[(&str, &str)]) -> String {
    let dir = TempDir::new().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    let result = susee_bundler(
        entry.to_string(),
        Some(dir.path().to_string_lossy().to_string()),
        Some(CheckOptions::default()),
    );
    result.bundled_code
}

#[test]
fn entry_star_reexport_is_expanded() {
    let code = bundle_project(
        "src/index.js",
        &[
            (
                "src/index.js",
                "export * from \"./foo.js\";\nexport const gamma = \"G\";\n",
            ),
            (
                "src/foo.js",
                "export const alpha = \"A\";\nexport function beta() { return alpha; }\nexport default alpha;\n",
            ),
        ],
    );
    // `alpha` and `beta` must be re-exported from the bundle (the `export *`
    // statement must NOT survive).
    assert!(
        code.contains("export {"),
        "star re-export was not expanded, got: {code}"
    );
    assert!(
        code.contains("alpha") && code.contains("beta"),
        "star-exported names missing, got: {code}"
    );
    assert!(
        !code.contains("export *"),
        "`export *` survived into the bundle, got: {code}"
    );
}

#[test]
fn chained_star_reexports_flatten() {
    let code = bundle_project(
        "src/index.js",
        &[
            ("src/index.js", "export * from \"./mid.js\";\n"),
            ("src/mid.js", "export * from \"./leaf.js\";\nexport const m = 1;\n"),
            ("src/leaf.js", "export const l = 2;\n"),
        ],
    );
    assert!(
        !code.contains("export *"),
        "`export *` survived into the bundle, got: {code}"
    );
    assert!(
        code.contains("l") && code.contains("m"),
        "chained star names missing, got: {code}"
    );
    assert!(
        code.contains("export {"),
        "no explicit export statement found, got: {code}"
    );
}

#[test]
fn star_export_target_still_inlined() {
    // The re-export target's *content* must still be in the bundle.
    let code = bundle_project(
        "src/index.js",
        &[
            ("src/index.js", "export * from \"./foo.js\";\n"),
            (
                "src/foo.js",
                "export const meaning = 42;\nexport function compute() { return meaning; }\n",
            ),
        ],
    );
    assert!(
        code.contains("42") && code.contains("compute"),
        "star target content not inlined, got: {code}"
    );
    // The bundled export statement must reference the inlined bindings.
    assert!(
        code.contains("export {") && code.contains("meaning"),
        "inlined bindings not re-exported, got: {code}"
    );
}