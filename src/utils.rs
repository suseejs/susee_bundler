//! Share Utils module for susee
//!

use std::path::Path;

use oxc::parser::Parser;
use oxc::span::SourceType;
use oxc::{allocator::Allocator, ast_visit::Visit};

use super::types::{DepsFile, JsxDetector, ModuleType, ModuleTypeDetector, ValidExts};

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

// ---------------------------------------------------------------------------
// Internal: collecting top-level declaration names (AST-level fallback)
// ---------------------------------------------------------------------------

/// Creates a `DepsFile` for use in tests, from the given file path and content.
///
/// The resulting `DepsFile` defaults to ESM module type, `.ts` file extension,
/// with JSX and entry flags set to `false`.
#[allow(dead_code)]
pub fn make_dep(file: &str, content: &str) -> DepsFile {
    DepsFile {
        file: file.to_string(),
        content: content.to_string(),
        bytes: content.len(),
        module_type: ModuleType::Esm,
        file_ext: ValidExts::Ts,
        is_jsx: false,
        is_entry: false,
    }
}

/// Compute a path relative to `base`, using `/` as separator.
pub fn path_relative(base: &Path, abs: &Path) -> String {
    abs.strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().replace('\\', "/"))
}

/// Check whether a backtick is "active" (i.e. we are inside a template
/// literal) after scanning a single line.
///
/// `in_template` is the state *before* this line starts. The function
/// returns the state *after* the line. Lines that are entirely inside a
/// template literal (no opening or closing backtick on them) will return
/// the same state unchanged.
///
/// Escaped backticks (`\``) and backticks inside line comments (`//`) and
/// single/double-quoted strings on the same line are ignored.
fn update_template_state(line: &str, in_template: bool) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut in_tmpl = in_template;

    while i < chars.len() {
        let c = chars[i];

        if in_tmpl {
            // Inside a template literal — look for the closing backtick.
            if c == '\\' {
                i += 2; // skip escaped char
                continue;
            }
            if c == '`' {
                in_tmpl = false;
            }
            i += 1;
            continue;
        }

        // Outside a template literal.
        match c {
            '\\' => {
                i += 2; // skip escaped char
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                // Line comment — rest of the line is ignored.
                break;
            }
            '"' | '\'' => {
                // Skip a single/double-quoted string on this line.
                let quote = c;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '`' => {
                in_tmpl = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    in_tmpl
}

/// Template-literal-aware filter predicate for a single line.
///
/// Returns `true` if the line should be **kept**. Blank lines and `;`-only
/// lines are dropped **only when outside a template literal**. Inside a
/// template literal, every line is kept verbatim.
///
/// `in_template` is the state *before* this line; it is updated by
/// [`update_template_state`] and returned via the closure's captured state.
fn keep_line(line: &str, in_template: &mut bool, also_skip_semicolon: bool) -> bool {
    let in_tmpl = *in_template;
    *in_template = update_template_state(line, in_tmpl);

    if in_tmpl {
        // We were inside a template literal at the start of this line —
        // always keep it, even if it's blank.
        return true;
    }

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if also_skip_semicolon && trimmed == ";" {
        return false;
    }
    // Also handle the bundler/mod.rs variant: skip lines starting with ';'
    if !also_skip_semicolon && trimmed.starts_with(';') {
        return false;
    }
    true
}

/// Strip lines that are empty (or consist of only a semicolon) **outside**
/// of template literals. Blank lines inside multi-line template literals
/// are preserved.
///
/// Used after import/export removal and tree-shaking to clean up the
/// leftover blank/`;`-only lines.
pub fn strip_empty_lines(content: &str) -> String {
    strip_empty_lines_impl(content, true)
}

/// Like [`strip_empty_lines`] but skips lines that *start with* `;` rather
/// than lines that are exactly `;`. Used by the main bundler pipeline
/// (`bundler/mod.rs`).
pub fn strip_empty_lines_bundler(content: &str) -> String {
    strip_empty_lines_impl(content, false)
}

fn strip_empty_lines_impl(content: &str, semicolon_only: bool) -> String {
    let mut in_template = false;
    content
        .lines()
        .filter(|line| keep_line(line, &mut in_template, semicolon_only))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}
/// Extract the default import name from a simple `import Foo from "module"`.
pub fn extract_default_name(import_str: &str, is_type: bool) -> Option<String> {
    let after_import = if is_type {
        import_str.strip_prefix("import type")?.trim()
    } else {
        import_str.strip_prefix("import")?.trim()
    };

    // The name is the first word before `from`
    let from_pos = after_import.find(" from ")?;
    let before_from = after_import[..from_pos].trim();
    if before_from.is_empty() {
        return None;
    }
    Some(before_from.to_string())
}
/// Extract the first string literal from a string.
pub fn extract_string_literal(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start = i + 1;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            if i < bytes.len() {
                return Some(s[start..i].to_string());
            }
        }
        i += 1;
    }
    None
}
/// Check if an import statement is a non-local import (not from ./ or ../).
pub fn is_non_local_import(s: &str) -> bool {
    let trimmed = s.trim_start();
    if !trimmed.starts_with("import") {
        return false;
    }
    // Find the string literal (module specifier)
    // Look for " or ' after `from` or directly after `import`
    let module_spec = extract_string_literal(trimmed);
    match module_spec {
        Some(spec) => !spec.starts_with("./") && !spec.starts_with("../"),
        None => false,
    }
}
/// Extract the module path from an import statement.
pub fn extract_module_path(import_str: &str) -> Option<String> {
    extract_string_literal(import_str)
}

/// Extract the import clause from an import statement.
pub fn extract_import_clause(
    import_str: &str,
    _module_path: &str,
    is_type: bool,
) -> Option<String> {
    // Remove `import` and optionally `type`
    let after_import = if is_type {
        import_str.strip_prefix("import type")?.trim()
    } else {
        import_str.strip_prefix("import")?.trim()
    };

    // Find ` from ` to split clause from module specifier
    let from_pos = after_import
        .find(" from ")
        .or_else(|| after_import.find("\tfrom "));
    if let Some(pos) = from_pos {
        let clause = after_import[..pos].trim();
        if clause.is_empty() {
            return None;
        }
        return Some(clause.to_string());
    }

    // No `from` — it's a side-effect import: `import "module"`
    None
}
/// Merge content from dependency files.
///
/// Returns (dep_files_content, main_file_content) where the main file is
/// the last file in the list (the entry file).
pub fn merge_content(deps_files: &[DepsFile]) -> (String, String) {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Dep files = all except the last (entry file)
    let dep_content: Vec<String> = deps_files
        .split_at(deps_files.len().saturating_sub(1))
        .0
        .iter()
        .map(|dep| {
            let file = format!("//{}", path_relative(&cwd, Path::new(&dep.file)));
            format!("{file}\n{}", dep.content)
        })
        .collect();

    // Main file = the last file (entry file)
    let main_content: Vec<String> = deps_files
        .split_at(deps_files.len().saturating_sub(1))
        .1
        .iter()
        .map(|dep| {
            let file = format!("//{}", path_relative(&cwd, Path::new(&dep.file)));
            format!("{file}\n{}", dep.content)
        })
        .collect();

    let dep_content = dep_content.join("\n").trim().to_string();
    let main_content = main_content.join("\n").trim().to_string();

    (dep_content, main_content)
}

/// Merge import statements by combining imports from the same module.
///
/// Ported from `utils.gen.mergeImportsStatement` in `utilities.ts`.
pub fn merge_imports_statement(imports: &[String]) -> Vec<String> {
    use std::collections::BTreeMap;

    let mut import_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut type_import_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut default_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut type_default_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut namespace_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for import_str in imports {
        // Parse: import [type] [clause] from "module";
        let import_str_trimmed = import_str.trim();

        // Extract module path
        let module_match = extract_module_path(import_str_trimmed);
        let Some(module_path) = module_match else {
            continue;
        };

        let is_type_import = import_str_trimmed.contains("import type");

        // Extract import clause (between `import [type]` and `from`)
        let clause = extract_import_clause(import_str_trimmed, &module_path, is_type_import);

        match &clause {
            None => {
                // Default import or side-effect import: `import Foo from "module"`
                if let Some(name) = extract_default_name(import_str_trimmed, is_type_import) {
                    let target_map = if is_type_import {
                        &mut type_default_imports
                    } else {
                        &mut default_imports
                    };
                    let entry = target_map.entry(module_path.clone()).or_default();
                    if !entry.contains(&name) {
                        entry.push(name);
                    }
                }
            }
            Some(clause) => {
                if clause.starts_with('{') {
                    // Named imports: `import { a, b } from "module"`
                    let names: Vec<String> = clause
                        .trim_matches(|c| c == '{' || c == '}')
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();

                    let target_map = if is_type_import {
                        &mut type_import_map
                    } else {
                        &mut import_map
                    };
                    let entry = target_map.entry(module_path.clone()).or_default();
                    for name in names {
                        if !entry.contains(&name) {
                            entry.push(name);
                        }
                    }
                } else if clause.starts_with("* as") {
                    // Namespace import: `import * as name from "module"`
                    if let Some(name) = clause.strip_prefix("* as").map(|s| s.trim().to_string()) {
                        let entry = namespace_imports.entry(module_path.clone()).or_default();
                        if !entry.contains(&name) {
                            entry.push(name);
                        }
                    }
                } else {
                    // Default import: `import name from "module"`
                    let target_map = if is_type_import {
                        &mut type_default_imports
                    } else {
                        &mut default_imports
                    };
                    let entry = target_map.entry(module_path.clone()).or_default();
                    let name = clause.trim().to_string();
                    if !entry.contains(&name) {
                        entry.push(name);
                    }
                }
            }
        }
    }

    let mut merged: Vec<String> = Vec::new();

    // Process named imports
    for (module_path, regular_names) in &import_map {
        let type_names = type_import_map.get(module_path);

        // Build list of (name, is_type) pairs so that type-only specifiers
        // retain their inline `type` keyword after merging.
        let mut entries: Vec<(String, bool)> =
            regular_names.iter().map(|n| (n.clone(), false)).collect();

        if let Some(type_names) = type_names {
            for type_name in type_names {
                if !regular_names.contains(type_name) {
                    entries.push((type_name.clone(), true));
                }
            }
        }

        if !entries.is_empty() {
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let import_names = entries
                .iter()
                .map(|(name, is_type)| {
                    if *is_type {
                        format!("type {name}")
                    } else {
                        name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            merged.push(format!(
                "import {{ {import_names} }} from \"{module_path}\";"
            ));
        }
    }

    // Add remaining type-only named imports
    for (module_path, type_names) in &type_import_map {
        if !import_map.contains_key(module_path) && !type_names.is_empty() {
            let mut names = type_names.clone();
            names.sort();
            let import_names = names.join(", ");
            merged.push(format!(
                "import type {{ {import_names} }} from \"{module_path}\";"
            ));
        }
    }

    // Process default imports
    for (module_path, regular_names) in &default_imports {
        let type_default_names = type_default_imports.get(module_path);
        let mut final_names: Vec<String> = regular_names.clone();

        if let Some(type_default_names) = type_default_names {
            for type_name in type_default_names {
                if !regular_names.contains(type_name) && !final_names.contains(type_name) {
                    final_names.push(type_name.clone());
                }
            }
        }

        if !final_names.is_empty() {
            let import_names = final_names.join(", ");
            merged.push(format!("import {import_names} from \"{module_path}\";"));
        }
    }

    // Add remaining type-only default imports
    for (module_path, type_default_names) in &type_default_imports {
        if !default_imports.contains_key(module_path) && !type_default_names.is_empty() {
            let import_names = type_default_names.join(", ");
            merged.push(format!(
                "import type {import_names} from \"{module_path}\";"
            ));
        }
    }

    // Process namespace imports
    for (module_path, names) in &namespace_imports {
        if !names.is_empty() {
            let import_names = names.join(", ");
            merged.push(format!(
                "import * as {import_names} from \"{module_path}\";"
            ));
        }
    }

    merged.sort();
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_empty_lines_removes_blank_lines_outside_template() {
        let input = "const a = 1;\n\n\nconst b = 2;\n";
        let result = strip_empty_lines(input);
        assert_eq!(result, "const a = 1;\nconst b = 2;");
    }

    #[test]
    fn strip_empty_lines_removes_semicolon_only_lines() {
        let input = "const a = 1;\n;\nconst b = 2;\n";
        let result = strip_empty_lines(input);
        assert_eq!(result, "const a = 1;\nconst b = 2;");
    }

    #[test]
    fn strip_empty_lines_trims_outer_whitespace() {
        let result = strip_empty_lines("  const x = 1;  ");
        assert_eq!(result, "const x = 1;");
    }

    #[test]
    fn strip_empty_lines_preserves_blank_lines_in_template_literal() {
        let input = "const txt = `# Header\n\n## Section`;\nconst x = 1;";
        let result = strip_empty_lines(input);
        assert!(
            result.contains("# Header\n\n## Section"),
            "blank line inside template should be preserved, got: {result}"
        );
    }

    #[test]
    fn strip_empty_lines_preserves_multiple_blank_lines_in_template() {
        let input = "const txt = `a\n\n\n\nb`;\n";
        let result = strip_empty_lines(input);
        assert!(
            result.contains("a\n\n\n\nb"),
            "multiple blank lines inside template should be preserved, got: {result}"
        );
    }

    #[test]
    fn strip_empty_lines_strips_blank_outside_but_preserves_inside_template() {
        let input = "const a = 1;\n\nconst txt = `line1\n\nline2`;\n\nconst b = 2;\n";
        let result = strip_empty_lines(input);
        // Blank line between `const a` and `const txt` should be removed.
        assert!(!result.contains("const a = 1;\n\nconst txt"));
        // Blank line inside template should be preserved.
        assert!(result.contains("line1\n\nline2"));
        // Blank line between template and `const b` should be removed.
        assert!(!result.contains(";\n\nconst b = 2;"));
    }

    #[test]
    fn strip_empty_lines_handles_escaped_backtick_in_template() {
        let input = "const txt = `escaped \\` backtick\n\ntext`;\n";
        let result = strip_empty_lines(input);
        assert!(
            result.contains("text`"),
            "template should close correctly after escaped backtick, got: {result}"
        );
    }

    #[test]
    fn strip_empty_lines_handles_backtick_in_line_comment() {
        // Backtick in a comment should not open a template literal.
        let input = "const a = 1; // note: ` is a backtick\n\nconst b = 2;\n";
        let result = strip_empty_lines(input);
        assert_eq!(
            result,
            "const a = 1; // note: ` is a backtick\nconst b = 2;"
        );
    }

    #[test]
    fn strip_empty_lines_handles_backtick_in_string() {
        // Backtick inside a regular string should not affect template state.
        let input = "const a = \"`not a template`\";\n\nconst b = 2;\n";
        let result = strip_empty_lines(input);
        assert_eq!(result, "const a = \"`not a template`\";\nconst b = 2;");
    }

    #[test]
    fn strip_empty_lines_bundler_skips_lines_starting_with_semicolon() {
        let input = "const a = 1;\n;\nconst b = 2;\n";
        let result = strip_empty_lines_bundler(input);
        assert_eq!(result, "const a = 1;\nconst b = 2;");
    }

    #[test]
    fn strip_empty_lines_bundler_preserves_template_blanks() {
        let input = "const txt = `x\n\ny`;\n\nconst a = 1;\n";
        let result = strip_empty_lines_bundler(input);
        assert!(
            result.contains("x\n\ny"),
            "blank inside template should be preserved, got: {result}"
        );
        assert!(
            !result.contains("y`;\n\nconst a"),
            "blank outside template should be removed, got: {result}"
        );
    }
}
