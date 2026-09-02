//! JSON module handler — converts `.json` dependency files into ESM modules.
//!
//! Mirrors `src/bundler/lib/resolveJSON.ts` from the TypeScript implementation.
//!
//! When a project imports a `.json` file (e.g. `import cfg from "./config.json"`),
//! the bundler needs to convert the JSON file into a valid ESM module so it
//! can be merged with the rest of the dependency tree. This handler:
//!
//! 1. **`resolve_json_handler`** — Converts each JSON dep into
//!    `const _j<key>$<n> = {...}; export default _j<key>$<n>`
//!    and changes `module_type` to `Esm`.
//! 2. **`json_module_import_handler`** — Rewrites default imports from JSON
//!    modules (e.g. `import cfg from "./config.json"` →
//!    `import _jconfig$1 from "./config.json"`). Named and namespace
//!    imports are left unchanged.
//! 3. **`json_module_call_expression_handler`** — Rewrites all references to
//!    the old local binding (call expressions, property access, new
//!    expressions, export specifiers) to use the new `_j` name.
//!
//! All sub-handlers operate on source text via AST round-tripping, the same
//! span-replacement strategy used by `apply_renames` in `susee_utils::apply_renames`.

use std::path::Path;

use oxc::ast::ast::{
    ExportSpecifier, Expression, ImportDeclaration, ImportDeclarationSpecifier, ModuleExportName,
    Statement,
};
use oxc::ast_visit::Visit;
use oxc::span::Span;

use crate::susee_log;
use crate::types::{DepsFile, ModuleType, ValidExts};
use crate::utils::with_parsed_program;

/// The category sigil for JSON module default exports.
///
/// Generated names follow the form `_j<key>$<n>`, consistent with the
/// shared [`UniqueName`](crate::core::susee_unique_name::UniqueName) scheme.
const JSON_SIGIL: &str = "j";

// ---------------------------------------------------------------------------
// Path utilities (mirrors helpers.ts)
// ---------------------------------------------------------------------------

/// Normalize a file path into a key: `dir/name` (without extension), or just
/// `dir` if the file is named `index`.
///
/// Mirrors `getFileKey` from `__local__/ts/bundler/lib/helpers.ts`.
fn get_file_key(file: &str) -> String {
    let path = Path::new(file);
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    if stem == "index" {
        if dir.is_empty() { ".".to_string() } else { dir }
    } else if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    }
}

/// Extract the file stem (basename without extension) from a file path.
fn file_stem(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Convert a string into a valid JS identifier tail for a JSON module name.
///
/// The result is meant to be used as `_j<tail>$<n>`: non-alphanumeric
/// characters are replaced with `_`, and a leading `_` is added if the
/// result doesn't start with a valid identifier start character.
fn to_identifier_tail(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let starts_valid = cleaned
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$');
    if starts_valid {
        cleaned
    } else if cleaned.is_empty() {
        "_".to_string()
    } else {
        format!("_{cleaned}")
    }
}

/// Build a JSON module variable name of the form `_j<tail>$<count>`.
fn json_module_name(file_key: &str, count: usize) -> String {
    let tail = to_identifier_tail(file_key);
    format!("_{JSON_SIGIL}{tail}${count}")
}

/// Convert JSON content into an ESM module string.
///
/// Mirrors `toJsonModuleCode` from `resolveJSON.ts`.
///
/// Returns `None` if the JSON content is invalid, so the caller can log
/// a diagnostic and skip the file instead of panicking.
fn to_json_module_code(var_name: &str, content: &str, _file: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    let json_object = serde_json::to_string(&parsed).ok()?;
    Some(format!(
        "const {var_name} = {json_object};\nexport default {var_name}"
    ))
}

// ---------------------------------------------------------------------------
// Data: name maps
// ---------------------------------------------------------------------------

/// A mapping from a JSON module's file stem to the generated variable name.
#[derive(Debug, Clone)]
struct JsonExportEntry {
    /// The file stem (basename without extension) of the JSON file.
    file: String,
    /// The generated variable name (e.g. `_jconfig$1`).
    new_name: String,
}

/// A mapping from a default import's local binding name to the generated
/// JSON module variable name, scoped to the importing file.
#[derive(Debug, Clone)]
struct JsonImportEntry {
    /// The importing file path.
    file: String,
    /// The original local binding name (e.g. `cfg` in `import cfg from "..."`).
    base: String,
    /// The JSON module variable name to replace it with.
    new_name: String,
}

/// Mutable state for the JSON module handler.
struct JsonState {
    export_map: Vec<JsonExportEntry>,
    import_map: Vec<JsonImportEntry>,
}

impl JsonState {
    fn new() -> Self {
        Self {
            export_map: Vec::new(),
            import_map: Vec::new(),
        }
    }

    /// Look up the JSON export entry for a given file stem.
    fn find_export(&self, file_stem: &str) -> Option<&JsonExportEntry> {
        self.export_map.iter().find(|m| m.file == file_stem)
    }

    /// Look up the JSON import rename for a given (file, base).
    #[allow(dead_code)]
    fn find_import(&self, file: &str, base: &str) -> Option<&str> {
        self.import_map
            .iter()
            .find(|m| m.file == file && m.base == base)
            .map(|m| m.new_name.as_str())
    }
}

// ---------------------------------------------------------------------------
// 1. resolve_json_handler — convert JSON files to ESM
// ---------------------------------------------------------------------------

/// Check whether a `DepsFile` is a JSON module.
fn is_json_module(dep: &DepsFile) -> bool {
    dep.module_type == ModuleType::Json && dep.file_ext == ValidExts::Json
}

/// Convert JSON dependency files into ESM modules.
///
/// Mirrors `resolveJSONHandler` from `resolveJSON.ts`. Each JSON file is
/// replaced with `const _j<key>$<n> = {...}; export default _j<key>$<n>`
/// and its `module_type` is changed to `Esm`. Non-JSON files are passed through
/// unchanged.
fn resolve_json_handler(deps: Vec<DepsFile>, state: &mut JsonState) -> Vec<DepsFile> {
    let mut scoped_name_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    deps.into_iter()
        .map(|dep| {
            if !is_json_module(&dep) {
                return dep;
            }

            let stem = file_stem(&dep.file);
            let file_key = get_file_key(&dep.file);
            let count = *scoped_name_count.get(&file_key).unwrap_or(&0) + 1;
            let json_var_name = json_module_name(&file_key, count);
            scoped_name_count.insert(file_key, count);

            state.export_map.push(JsonExportEntry {
                file: stem,
                new_name: json_var_name.clone(),
            });

            let Some(content) = to_json_module_code(&json_var_name, &dep.content, &dep.file) else {
                // Invalid JSON — log and pass the file through unchanged so
                // the bundler can continue instead of panicking.
                susee_log::warning(&format!(
                    "Invalid JSON syntax in dependency file: {}; skipping JSON module conversion",
                    dep.file
                ));
                return dep;
            };
            let bytes = content.len();

            DepsFile {
                file: dep.file,
                content,
                bytes,
                module_type: ModuleType::Esm,
                file_ext: dep.file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 2. json_module_import_handler — rewrite default imports from JSON modules
// ---------------------------------------------------------------------------

/// Extract the module specifier string from an import declaration.
fn import_source(decl: &ImportDeclaration<'_>) -> String {
    decl.source.value.as_str().to_string()
}

/// Process a single file to rewrite default imports from JSON modules.
///
/// Mirrors `jsonModuleImportHandler` from `resolveJSON.ts`. For each
/// `import X from "./foo.json"`, if `foo` matches a JSON export mapping,
/// the local binding `X` is renamed to `_jfoo$1` and the mapping
/// is recorded for the call-expression handler. Named and namespace imports
/// are left unchanged.
fn json_module_import_handler(dep: &DepsFile, state: &mut JsonState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let mut spans: Vec<(usize, usize, String)> = Vec::new();

        for stmt in &program.body {
            if let Statement::ImportDeclaration(import_decl) = stmt {
                let source = import_source(import_decl);
                let import_stem = file_stem(&source);

                let Some(mapping) = state.find_export(&import_stem).cloned() else {
                    continue;
                };

                // Only handle default import specifiers.
                if let Some(specifiers) = &import_decl.specifiers {
                    for spec in specifiers {
                        if let ImportDeclarationSpecifier::ImportDefaultSpecifier(default_spec) =
                            spec
                        {
                            let local_name = default_spec.local.name.as_str().to_string();
                            state.import_map.push(JsonImportEntry {
                                file: dep.file.clone(),
                                base: local_name.clone(),
                                new_name: mapping.new_name.clone(),
                            });
                            spans.push((
                                default_spec.local.span.start as usize,
                                default_spec.local.span.end as usize,
                                mapping.new_name.clone(),
                            ));
                        }
                    }
                }
            }
        }

        if spans.is_empty() {
            return dep.content.clone();
        }

        spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        let mut result = dep.content.clone();
        for (start, end, replacement) in &spans {
            if *start <= result.len() && *end <= result.len() && *start <= *end {
                result.replace_range(*start..*end, replacement);
            }
        }

        result
    })
}

// ---------------------------------------------------------------------------
// 3. json_module_call_expression_handler — rewrite references
// ---------------------------------------------------------------------------

/// Collect all identifier reference spans that should be renamed because
/// they refer to a renamed JSON module default import binding.
struct JsonReferenceCollector<'a> {
    import_map: &'a [JsonImportEntry],
    file: &'a str,
    spans: Vec<(Span, String)>,
}

impl<'a> JsonReferenceCollector<'a> {
    fn find_mapping(&self, name: &str) -> Option<&str> {
        self.import_map
            .iter()
            .find(|m| m.file == self.file && m.base == name)
            .map(|m| m.new_name.as_str())
    }
}

impl<'a, 'ast> Visit<'ast> for JsonReferenceCollector<'a> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.object
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_new_expression(self, it);
    }

    fn visit_export_named_declaration(&mut self, it: &oxc::ast::ast::ExportNamedDeclaration<'ast>) {
        for spec in &it.specifiers {
            self.check_export_specifier(spec);
        }
        oxc::ast_visit::walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &oxc::ast::ast::ExportFromDeclaration<'ast>) {
        for spec in &it.specifiers {
            self.check_export_specifier(spec);
        }
        oxc::ast_visit::walk::walk_export_from_declaration(self, it);
    }
}

impl<'a> JsonReferenceCollector<'a> {
    fn check_export_specifier(&mut self, spec: &ExportSpecifier<'_>) {
        if let ModuleExportName::IdentifierReference(ident) = &spec.local
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
    }
}

/// Process a single file to rename all references to renamed JSON imports.
///
/// Mirrors `jsonModuleCallExpressionHandler` from `resolveJSON.ts`.
fn json_module_call_expression_handler(dep: &DepsFile, state: &JsonState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let mut collector = JsonReferenceCollector {
            import_map: &state.import_map,
            file: &dep.file,
            spans: Vec::new(),
        };
        collector.visit_program(program);

        if collector.spans.is_empty() {
            return dep.content.clone();
        }

        let mut spans = collector.spans;
        spans.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
        spans.dedup_by(|a, b| a.0 == b.0);

        let mut result = dep.content.clone();
        for (span, new_name) in &spans {
            let start = span.start as usize;
            let end = span.end as usize;
            if start <= result.len() && end <= result.len() && start <= end {
                result.replace_range(start..end, new_name);
            }
        }

        result
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Convert JSON dependency files into ESM modules and rewrite all references.
///
/// This is the Rust counterpart of `jsonModuleHandlers` from
/// `src/bundler/lib/resolveJSON.ts`. It runs three sub-handlers in sequence:
///
/// 1. `resolve_json_handler` — convert JSON files to ESM
/// 2. `json_module_import_handler` — rewrite default imports from JSON modules
/// 3. `json_module_call_expression_handler` — rewrite all references
///
/// The state (export/import name maps) is freshly created per call, matching
/// the TS version which resets module-level maps via `resetJsonModuleState()`.
pub fn json_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    let mut state = JsonState::new();

    // Phase 1: Convert JSON files to ESM modules.
    let phase1 = resolve_json_handler(deps, &mut state);

    // Phase 2: Rewrite default imports from JSON modules.
    let phase2: Vec<DepsFile> = phase1
        .iter()
        .map(|dep| {
            let content = json_module_import_handler(dep, &mut state);
            DepsFile {
                file: dep.file.clone(),
                content,
                bytes: 0,
                module_type: dep.module_type,
                file_ext: dep.file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            }
        })
        .collect();

    // Phase 3: Rewrite all references to renamed JSON imports.
    let phase3: Vec<DepsFile> = phase2
        .iter()
        .map(|dep| {
            let content = json_module_call_expression_handler(dep, &state);
            let bytes = content.len();
            DepsFile {
                file: dep.file.clone(),
                content,
                bytes,
                module_type: dep.module_type,
                file_ext: dep.file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            }
        })
        .collect();

    phase3
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_json_dep(file: &str, content: &str) -> DepsFile {
        DepsFile {
            file: file.to_string(),
            content: content.to_string(),
            bytes: content.len(),
            module_type: ModuleType::Json,
            file_ext: ValidExts::Json,
            is_jsx: false,
            is_entry: false,
        }
    }

    fn make_esm_dep(file: &str, content: &str) -> DepsFile {
        DepsFile {
            file: file.to_string(),
            content: content.to_string(),
            bytes: content.len(),
            module_type: ModuleType::Esm,
            file_ext: ValidExts::from_ext(
                Path::new(file)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("js"),
            )
            .unwrap_or(ValidExts::Js),
            is_jsx: false,
            is_entry: false,
        }
    }

    #[test]
    fn converts_json_dependency_into_esm_module() {
        let deps = vec![make_json_dep(
            "src/config.json",
            r#"{"app":"bundler","count":2}"#,
        )];
        let result = json_handler(deps);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].module_type, ModuleType::Esm);
        let content = &result[0].content;
        assert!(content.contains("const _j"));
        assert!(content.contains(r#""app":"bundler""#));
        assert!(content.contains("export default _j"));
    }

    #[test]
    fn renames_default_json_imports_and_local_usages() {
        let dep_a = make_json_dep("src/config.json", r#"{"app":"bundler"}"#);
        let dep_b = make_esm_dep(
            "src/main.ts",
            "import cfg from \"./config.json\";\nconsole.log(cfg.app);\n",
        );
        let result = json_handler(vec![dep_a, dep_b]);
        let main_content = &result[1].content;

        // The import should use the new JSON module name.
        assert!(
            main_content.contains("import _j"),
            "content was: {main_content}"
        );
        // The property access should be rewritten.
        assert!(main_content.contains("_j"), "content was: {main_content}");
        // The old name should be gone.
        assert!(!main_content.contains("cfg"));
    }

    #[test]
    fn keeps_named_and_namespace_json_imports_unchanged() {
        let dep_a = make_json_dep("src/config.json", r#"{"app":"bundler","count":2}"#);
        let dep_b = make_esm_dep(
            "src/main.ts",
            "import * as cfg from \"./config.json\";\nimport { app as appName } from \"./config.json\";\nconsole.log(cfg.count, appName);\n",
        );
        let result = json_handler(vec![dep_a, dep_b]);
        let main_content = &result[1].content;

        // Namespace import should be unchanged.
        assert!(
            main_content.contains("import * as cfg from \"./config.json\""),
            "content was: {main_content}"
        );
        // Named import should be unchanged.
        assert!(
            main_content.contains("import { app as appName } from \"./config.json\""),
            "content was: {main_content}"
        );
    }

    #[test]
    fn keeps_require_json_calls_unchanged() {
        let dep_a = make_json_dep("src/config.json", r#"{"app":"bundler"}"#);
        let dep_b = make_esm_dep(
            "src/main.ts",
            "const cfg = require(\"./config.json\");\nmodule.exports = cfg;\n",
        );
        let result = json_handler(vec![dep_a, dep_b]);
        let main_content = &result[1].content;

        // require() should be unchanged.
        assert!(
            main_content.contains("require(\"./config.json\")"),
            "content was: {main_content}"
        );
    }

    #[test]
    fn returns_original_deps_when_no_json_module() {
        let deps = vec![
            make_esm_dep("src/a.ts", "export const x = 1;\n"),
            make_esm_dep("src/b.ts", "export const y = 2;\n"),
        ];
        let result = json_handler(deps.clone());
        assert_eq!(result[0].content, deps[0].content);
        assert_eq!(result[1].content, deps[1].content);
    }

    #[test]
    fn invalid_json_does_not_panic() {
        // Bug fix: previously panicked on invalid JSON; now returns the
        // file unchanged with a warning.
        let dep = make_json_dep("src/bad.json", "{invalid json}");
        let result = json_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        // File should be passed through unchanged (not converted to ESM).
        assert_eq!(result[0].module_type, ModuleType::Json);
    }

    #[test]
    fn to_identifier_tail_replaces_invalid_chars() {
        assert_eq!(to_identifier_tail("config"), "config");
        assert_eq!(to_identifier_tail("foo/bar"), "foo_bar");
        assert_eq!(to_identifier_tail("123bad"), "_123bad");
        assert_eq!(to_identifier_tail(""), "_");
    }

    #[test]
    fn json_module_name_uses_j_sigil() {
        assert_eq!(json_module_name("config", 1), "_jconfig$1");
        assert_eq!(json_module_name("foo/bar", 2), "_jfoo_bar$2");
    }

    #[test]
    fn to_json_module_code_produces_valid_esm() {
        let code = to_json_module_code("_jconfig$1", r#"{"a":1}"#, "config.json").unwrap();
        assert!(code.contains(r#"const _jconfig$1 = {"a":1};"#));
        assert!(code.contains("export default _jconfig$1"));
    }

    #[test]
    fn handles_nested_json_objects() {
        let dep = make_json_dep("src/data.json", r#"{"nested":{"deep":true}}"#);
        let result = json_handler(vec![dep]);
        let content = &result[0].content;
        assert!(content.contains(r#""nested":{"deep":true}"#));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn handles_json_arrays() {
        let dep = make_json_dep("src/items.json", r#"[1,2,3]"#);
        let result = json_handler(vec![dep]);
        let content = &result[0].content;
        assert!(content.contains("[1,2,3]"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn to_json_module_code_invalid_json_returns_none() {
        // Bug fix: previously panicked on invalid JSON; now returns None.
        assert!(to_json_module_code("_jconfig$1", "{invalid}", "config.json").is_none());
    }

    #[test]
    fn json_handler_invalid_json_does_not_panic() {
        // Bug fix: `json_handler` should not panic on invalid JSON content.
        // It should pass the file through unchanged and log a warning.
        let dep = make_json_dep("broken.json", "{not valid json");
        let result = json_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        // The file should be passed through (not converted to ESM).
        assert_eq!(result[0].module_type, ModuleType::Json);
        assert_eq!(result[0].content, "{not valid json");
    }
}
