//! Share Utils module for susee
//!

use std::path::Path;

use oxc::parser::Parser;
use oxc::span::SourceType;
use oxc::{allocator::Allocator, ast_visit::Visit};

use super::types::{JsxDetector, ModuleType, ModuleTypeDetector};

/// Replace the trailing `.json` extension with `.ts` in a file path.
///
/// Only the *extension* is replaced — if the path contains `.json` in a
/// directory name (e.g. `foo.json/bar.json`), only the final `.json` is
/// changed, producing `foo.json/bar.ts`.
pub fn json_ext_to_ts(file: &str) -> String {
    if let Some(stem) = file.strip_suffix(".json") {
        format!("{stem}.ts")
    } else {
        file.to_string()
    }
}

/// Parse `content` as TypeScript/JavaScript and call `f` with the resulting `Program`.
///
/// The file path is used only to determine the source type (e.g. `.tsx` → TSX).
/// For `.json` files the extension is replaced with `.ts` before parsing,
/// mirroring `jsonExtToTs`.
///
/// This uses a callback pattern to avoid self-referential struct issues —
/// the `Program` borrows from the `Allocator`, so both must stay in the same scope.
pub fn with_parsed_program<R, F>(file: &str, content: &str, f: F) -> R
where
    F: for<'a> FnOnce(&oxc::ast::ast::Program<'a>) -> R,
{
    let ts_file = json_ext_to_ts(file);
    let path = Path::new(&ts_file);
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parser_return = Parser::new(&allocator, content, source_type).parse();
    f(&parser_return.program)
}

/// Detect whether a source file uses CommonJS or ESM syntax
///
/// Returns the [`ModuleType`]:
/// - `Json` for `.json` files.
/// - `Cjs` when CommonJS syntax (`require`, `module.exports`, `exports.x`) is
///   present without ESM syntax.
/// - `Esm` otherwise (ESM syntax present, or no module syntax).
pub fn detect_module_type(content: &str, file_path: &Path) -> ModuleType {
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    if ext == "json" {
        return ModuleType::Json;
    }

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_path).unwrap_or_default();
    // If parsing fails, fall back to ESM.
    let parser_return = Parser::new(&allocator, content, source_type).parse();
    let program = &parser_return.program;

    let mut detector = ModuleTypeDetector::default();
    Visit::visit_program(&mut detector, program);

    // `.cts` files use CommonJS-like semantics via TS `import =` / `export =`
    // syntax. Detect them before the generic CJS/ESM fallback.
    if ext == "cts" && detector.is_cts {
        return ModuleType::Cts;
    }

    if detector.is_common_js && !detector.is_esm {
        ModuleType::Cjs
    } else if detector.is_esm && detector.is_common_js {
        // Mixed — treat as ESM (matches the TS version's `_esmCount++` branch).
        ModuleType::Esm
    } else {
        // ESM or no module syntax detected → default to ESM.
        ModuleType::Esm
    }
}

/// Detect whether a source file contains JSX syntax, mirroring
/// `utils.checks.isJsxContent` from `node_src/helpers/utilities.ts`.
pub fn is_jsx_content(content: &str, file_path: &Path) -> bool {
    let allocator = Allocator::default();
    // Parse as TSX to detect JSX regardless of the file's real extension.
    let source_type = SourceType::from_path(file_path)
        .unwrap_or_default()
        .with_jsx(true);
    let parser_return = Parser::new(&allocator, content, source_type).parse();
    let program = &parser_return.program;

    let mut detector = JsxDetector::default();
    detector.visit_program(program);
    detector.contains_jsx
}

/// Read a file relative to `root`, returning its content and byte length.
pub fn read_file(root: &Path, rel_path: &str) -> std::io::Result<(String, usize)> {
    let abs = root.join(rel_path);
    let content = std::fs::read_to_string(&abs)?;
    let bytes = content.len();
    Ok((content, bytes))
}
