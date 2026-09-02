//! Anonymous export/import normalization hook.
//!
//! Mirrors `src/bundler/lib/anonymous.ts` from the TypeScript implementation.
//!
//! When a file has an anonymous default export such as:
//! ```ts
//! export default () => 42;
//! export default function () { return 1; }
//! export default class {}
//! export default { foo: 1 };
//! export default 42;
//! export default "hello";
//! export default [1, 2, 3];
//! ```
//! the bundler cannot reference the export by name. This hook assigns a
//! unique name (`_a<file>$<n>`) to the anonymous declaration,
//! rewrites the export to use that name, and then updates all importing files
//! to use the new name instead of their original default import binding.
//!
//! ## Pipeline
//!
//! 1. **`anonymous_export_handler`** — Scan every file for anonymous default
//!    exports and assign unique names. Record the mapping `(file_stem → new_name)`
//!    in `anonymous_export_name_map`.
//! 2. **`anonymous_import_handler`** — Scan every file for default imports
//!    whose source module's file stem matches an entry in
//!    `anonymous_export_name_map`. Record the mapping `(local_name, file → new_name)`
//!    in `anonymous_import_name_map`.
//! 3. **`anonymous_call_expression_handler`** — Rename all references (call
//!    expressions, property access, new expressions, export specifiers) that
//!    use the old default-import binding to the new anonymous export name.
//!
//! All three sub-handlers operate on source text via AST round-tripping, the
//! same span-replacement strategy used by `apply_renames` in
//! `susee_utils::apply_renames`.

use std::path::Path;

use oxc::ast::ast::{
    ExportDefaultDeclaration, ExportDefaultDeclarationKind, ExportSpecifier, Expression,
    ImportDeclaration, ImportDeclarationSpecifier, ModuleExportName, Program, Statement,
};
use oxc::ast_visit::Visit;
use oxc::span::{GetSpan, Span};

use crate::types::DepsFile;
use crate::unique_name::{UniqueName, sigil};
use crate::utils::with_parsed_program;

/// The category key used for all anonymous export names, mirroring the TS
/// implementation (`uniqueName.setPrefix({ key: "AnonymousName", ... })`).
const ANONYMOUS_PREFIX_KEY: &str = "AnonymousName";

/// Extract the file stem (basename without extension) from a file path.
///
/// Mirrors `path.basename(file).split(".")[0]` from the TS implementation.
fn file_stem(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("anon")
        .to_string()
}

/// Extract the module specifier string from an import declaration.
///
/// Returns the raw string inside the quotes, e.g. `"./foo"` → `./foo`.
fn import_source(decl: &ImportDeclaration<'_>) -> String {
    decl.source.value.as_str().to_string()
}

// ---------------------------------------------------------------------------
// Data: name maps
// ---------------------------------------------------------------------------

/// A mapping from an anonymous export's file stem to the generated name.
#[derive(Debug, Clone)]
struct ExportNameEntry {
    /// The file stem (basename without extension) of the exporting file.
    file: String,
    /// The generated unique name.
    new_name: String,
}

/// A mapping from a default import's local binding name to the generated
/// anonymous export name, scoped to the importing file.
#[derive(Debug, Clone)]
struct ImportNameEntry {
    /// The importing file path.
    file: String,
    /// The original local binding name used in `import X from "..."`.
    base: String,
    /// The anonymous export name to replace it with.
    new_name: String,
}

/// Mutable state shared across the three sub-handlers.
struct AnonymousState {
    unique: UniqueName,
    export_map: Vec<ExportNameEntry>,
    import_map: Vec<ImportNameEntry>,
}

impl AnonymousState {
    fn new() -> Self {
        let mut unique = UniqueName::new();
        unique.set_prefix(ANONYMOUS_PREFIX_KEY, sigil::ANONYMOUS);
        Self {
            unique,
            export_map: Vec::new(),
            import_map: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Anonymous export handler
// ---------------------------------------------------------------------------

/// Check whether an `ExportDefaultDeclaration` has an anonymous declaration
/// (function or class without a name).
fn is_anonymous_function_or_class<'a>(
    decl: &ExportDefaultDeclaration<'a>,
) -> (bool, Option<&'a str>) {
    match &decl.declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(func) => (func.id.is_none(), None),
        ExportDefaultDeclarationKind::ClassDeclaration(cls) => (cls.id.is_none(), None),
        _ => (false, None),
    }
}

/// Collect byte-offset spans to replace for the anonymous export handler.
///
/// Returns a list of `(start_offset, end_offset, replacement_text)` tuples.
/// For anonymous function/class declarations, the replacement inserts a name
/// identifier after the `function`/`class` keyword. For expression default
/// exports (arrow, object, array, string, number, etc.), the entire
/// `export default <expr>;` statement is replaced with
/// `const <name> = <expr>; export default <name>;`.
fn collect_export_spans(
    program: &Program<'_>,
    source_text: &str,
    state: &mut AnonymousState,
    file: &str,
) -> Vec<(usize, usize, String)> {
    let stem = file_stem(file);
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        if let Statement::ExportDefaultDeclaration(export_decl) = stmt {
            let (is_anon, _) = is_anonymous_function_or_class(export_decl);
            if is_anon {
                // `export default function() {}` or `export default class {}`
                let new_name = state.unique.get_name(ANONYMOUS_PREFIX_KEY, &stem);
                state.export_map.push(ExportNameEntry {
                    file: stem.clone(),
                    new_name: new_name.clone(),
                });

                // We need to find the position to insert the name — right
                // after `function ` or `class ` (or `function* ` / `async function `).
                // The span of the declaration covers the whole
                // `function () {}` / `class {}` part. We insert the name
                // right after the keyword.
                let decl_span = export_decl.declaration.span();
                let decl_text = decl_span.source_text(source_text);

                // Find the keyword end position.
                let keyword_end = find_keyword_end(decl_text);
                if let Some(kw_end) = keyword_end {
                    let abs_insert = decl_span.start as usize + kw_end;
                    // Insert ` <name>` after the keyword.
                    spans.push((abs_insert, abs_insert, format!(" {new_name}")));
                }
            } else {
                // Check for expression-type default exports
                let expr_span = match &export_decl.declaration {
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(_)
                    | ExportDefaultDeclarationKind::ObjectExpression(_)
                    | ExportDefaultDeclarationKind::ArrayExpression(_)
                    | ExportDefaultDeclarationKind::StringLiteral(_)
                    | ExportDefaultDeclarationKind::NumericLiteral(_)
                    | ExportDefaultDeclarationKind::BooleanLiteral(_)
                    | ExportDefaultDeclarationKind::NullLiteral(_)
                    | ExportDefaultDeclarationKind::Identifier(_)
                    | ExportDefaultDeclarationKind::TemplateLiteral(_)
                    | ExportDefaultDeclarationKind::FunctionExpression(_)
                    | ExportDefaultDeclarationKind::ClassExpression(_) => {
                        Some(export_decl.declaration.span())
                    }
                    _ => None,
                };

                if let Some(expr_span) = expr_span {
                    let new_name = state.unique.get_name(ANONYMOUS_PREFIX_KEY, &stem);
                    state.export_map.push(ExportNameEntry {
                        file: stem.clone(),
                        new_name: new_name.clone(),
                    });

                    let expr_text = expr_span.source_text(source_text);
                    let replacement =
                        format!("const {new_name} = {expr_text};\nexport default {new_name};");
                    // Replace the entire `export default <expr>;` statement
                    let stmt_span = export_decl.span;
                    spans.push((
                        stmt_span.start as usize,
                        stmt_span.end as usize,
                        replacement,
                    ));
                }
            }
        }
    }

    spans
}

/// Find the byte offset right after the leading keyword (`function`, `function*`,
/// `async function`, `class`, `abstract class`) in a declaration text.
///
/// Returns the position immediately after the keyword (before any following
/// whitespace), so that inserting ` <name>` at this offset produces
/// `function <name>() {}` / `class <name> {}` with the correct single space.
fn find_keyword_end(text: &str) -> Option<usize> {
    let trimmed = text.trim_start();
    let leading_ws = text.len() - trimmed.len();

    // Check longer/more-specific keywords FIRST so that `function*` is not
    // swallowed by the `function` check, and `async function*` is not
    // swallowed by `async function`.
    for keyword in [
        "async function*",
        "async function",
        "function*",
        "function",
        "abstract class",
        "class",
    ] {
        if trimmed.starts_with(keyword) {
            return Some(leading_ws + keyword.len());
        }
    }

    None
}

/// Process a single file for anonymous default exports.
fn anonymous_export_handler(dep: &DepsFile, state: &mut AnonymousState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;
        let mut spans = collect_export_spans(program, source_text, state, &dep.file);

        if spans.is_empty() {
            return dep.content.clone();
        }

        // Sort spans by start offset descending (right to left).
        spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));

        // Remove duplicate spans (same start+end).
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
// 2. Anonymous import handler
// ---------------------------------------------------------------------------

/// Collect import rename mappings for default imports whose source file stem
/// matches an anonymous export.
fn collect_import_mappings(program: &Program<'_>, state: &mut AnonymousState, file: &str) {
    for stmt in &program.body {
        if let Statement::ImportDeclaration(import_decl) = stmt {
            let source = import_source(import_decl);
            let import_stem = file_stem(&source);

            // Check if this source module has an anonymous export mapping.
            let Some(mapping) = state.export_map.iter().find(|m| m.file == import_stem) else {
                continue;
            };

            // Check for a default import specifier.
            if let Some(specifiers) = &import_decl.specifiers {
                for spec in specifiers {
                    if let ImportDeclarationSpecifier::ImportDefaultSpecifier(default_spec) = spec {
                        let local_name = default_spec.local.name.as_str().to_string();
                        state.import_map.push(ImportNameEntry {
                            file: file.to_string(),
                            base: local_name,
                            new_name: mapping.new_name.clone(),
                        });
                    }
                }
            }
        }
    }
}

/// Process a single file for anonymous default imports and rename them.
fn anonymous_import_handler(dep: &DepsFile, state: &mut AnonymousState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        // First, collect the import mappings for this file.
        collect_import_mappings(program, state, &dep.file);

        // Now collect spans to replace — the default import specifier's
        // local name should be renamed to the anonymous export name.
        let mut spans: Vec<(usize, usize, String)> = Vec::new();

        for stmt in &program.body {
            if let Statement::ImportDeclaration(import_decl) = stmt {
                let source = import_source(import_decl);
                let import_stem = file_stem(&source);

                let Some(mapping) = state.export_map.iter().find(|m| m.file == import_stem) else {
                    continue;
                };

                if let Some(specifiers) = &import_decl.specifiers {
                    for spec in specifiers {
                        if let ImportDeclarationSpecifier::ImportDefaultSpecifier(default_spec) =
                            spec
                        {
                            let local_span = default_spec.local.span;
                            spans.push((
                                local_span.start as usize,
                                local_span.end as usize,
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
// 3. Anonymous call-expression handler
// ---------------------------------------------------------------------------

/// Collect all identifier reference spans that should be renamed because they
/// refer to a renamed default import binding.
struct ReferenceCollector<'a> {
    import_map: &'a [ImportNameEntry],
    file: &'a str,
    spans: Vec<(Span, String)>,
}

impl<'a> ReferenceCollector<'a> {
    fn find_mapping(&self, name: &str) -> Option<&str> {
        self.import_map
            .iter()
            .find(|m| m.file == self.file && m.base == name)
            .map(|m| m.new_name.as_str())
    }
}

impl<'a, 'ast> Visit<'ast> for ReferenceCollector<'a> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            // Replace just the identifier span, not the whole call.
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

impl<'a> ReferenceCollector<'a> {
    fn check_export_specifier(&mut self, spec: &ExportSpecifier<'_>) {
        // `export { local as exported }` — rename `local` if it matches.
        if let ModuleExportName::IdentifierReference(ident) = &spec.local
            && let Some(new_name) = self.find_mapping(ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
    }
}

/// Process a single file to rename all references to renamed default imports.
fn anonymous_call_expression_handler(dep: &DepsFile, state: &AnonymousState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let mut collector = ReferenceCollector {
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

/// Normalize anonymous default exports and imports across a set of
/// dependency files.
///
/// This is the Rust counterpart of `anonymousHandler` from
/// `src/bundler/lib/anonymous.ts`. It runs three sub-handlers in sequence:
///
/// 1. `anonymous_export_handler` — name anonymous default exports
/// 2. `anonymous_import_handler` — rename default imports from anonymous modules
/// 3. `anonymous_call_expression_handler` — rename all references to those imports
///
/// The state (export/import name maps + unique name generator) is reset at
/// the start of each call, matching `resetAnonymousState()` in the TS version.
pub fn anonymous_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    let mut state = AnonymousState::new();

    // Phase 1: Name anonymous default exports.
    let phase1: Vec<DepsFile> = deps
        .iter()
        .map(|dep| {
            let content = anonymous_export_handler(dep, &mut state);
            DepsFile {
                file: dep.file.clone(),
                content,
                bytes: 0, // recalculated below
                module_type: dep.module_type,
                file_ext: dep.file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            }
        })
        .collect();

    // Phase 2: Rename default imports from anonymous modules.
    let phase2: Vec<DepsFile> = phase1
        .iter()
        .map(|dep| {
            let content = anonymous_import_handler(dep, &mut state);
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

    // Phase 3: Rename all references to renamed imports.
    let phase3: Vec<DepsFile> = phase2
        .iter()
        .map(|dep| {
            let content = anonymous_call_expression_handler(dep, &state);
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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── find_keyword_end ──────────────────────────────────────────────

    #[test]
    fn find_keyword_end_function() {
        assert_eq!(find_keyword_end("function() {}"), Some(8));
    }

    #[test]
    fn find_keyword_end_function_generator() {
        // Bug fix: `function*` must be matched before `function`.
        assert_eq!(find_keyword_end("function*() {}"), Some(9));
    }

    #[test]
    fn find_keyword_end_async_function() {
        assert_eq!(find_keyword_end("async function() {}"), Some(14));
    }

    #[test]
    fn find_keyword_end_async_function_generator() {
        // `async function*` must be matched before `async function`.
        assert_eq!(find_keyword_end("async function*() {}"), Some(15));
    }

    #[test]
    fn find_keyword_end_class() {
        assert_eq!(find_keyword_end("class {}"), Some(5));
    }

    #[test]
    fn find_keyword_end_abstract_class() {
        assert_eq!(find_keyword_end("abstract class Foo {}"), Some(14));
    }

    #[test]
    fn find_keyword_end_with_leading_whitespace() {
        assert_eq!(find_keyword_end("  function() {}"), Some(10));
    }

    #[test]
    fn find_keyword_end_no_match() {
        assert_eq!(find_keyword_end("const x = 1;"), None);
    }

    // ─── file_stem ─────────────────────────────────────────────────────

    #[test]
    fn file_stem_extracts_basename_without_ext() {
        assert_eq!(file_stem("src/foo.ts"), "foo");
        assert_eq!(file_stem("foo.js"), "foo");
        assert_eq!(file_stem("a/b/c/index.tsx"), "index");
    }

    // ─── anonymous_handler integration ─────────────────────────────────

    fn make_dep(file: &str, content: &str) -> DepsFile {
        crate::utils::make_dep(file, content)
    }

    #[test]
    fn anonymous_handler_names_anonymous_function() {
        let dep = make_dep("mod.ts", "export default function() { return 1; }");
        let result = anonymous_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        // The anonymous function should be named with the `_a` sigil.
        assert!(result[0].content.contains("_amod$1"));
        assert!(result[0].content.contains("function _amod$1"));
    }

    #[test]
    fn anonymous_handler_names_anonymous_class() {
        let dep = make_dep("mod.ts", "export default class {}");
        let result = anonymous_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("class _amod$1"));
    }

    #[test]
    fn anonymous_handler_names_anonymous_arrow() {
        let dep = make_dep("mod.ts", "export default () => 42;");
        let result = anonymous_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("const _amod$1"));
        assert!(result[0].content.contains("export default _amod$1"));
    }

    #[test]
    fn anonymous_handler_names_anonymous_generator() {
        // Bug fix: `function*` was not detected by `find_keyword_end`.
        let dep = make_dep("mod.ts", "export default function*() { yield 1; }");
        let result = anonymous_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("_amod$1"));
        // The name should be inserted after `function*`, not after `function`.
        assert!(
            result[0].content.contains("function* _amod$1"),
            "got: {}",
            result[0].content
        );
    }

    #[test]
    fn anonymous_handler_renames_default_import() {
        let export_dep = make_dep("mod.ts", "export default function() { return 1; }");
        let import_dep = make_dep("main.ts", "import fn from './mod';\nconsole.log(fn());");
        let result = anonymous_handler(vec![export_dep, import_dep]);
        // The import should be renamed to the anonymous export name.
        assert!(
            result[1].content.contains("_amod$1"),
            "got: {}",
            result[1].content
        );
    }

    #[test]
    fn anonymous_handler_no_change_without_anonymous_exports() {
        let dep = make_dep("mod.ts", "export const x = 1;");
        let result = anonymous_handler(vec![dep]);
        assert_eq!(result[0].content, "export const x = 1;");
    }

    #[test]
    fn anonymous_handler_preserves_named_default_export() {
        // `export default function foo() {}` is NOT anonymous — should not
        // be renamed.
        let dep = make_dep("mod.ts", "export default function foo() { return 1; }");
        let result = anonymous_handler(vec![dep]);
        assert!(
            !result[0].content.contains("_amod$1"),
            "named default should not be renamed, got: {}",
            result[0].content
        );
    }
}
