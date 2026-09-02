//! Export-default handler (named default exports).
//!
//! Mirrors `src/bundler/lib/exportDefault.ts` from the TypeScript implementation.
//!
//! When a file has a **named** default export such as:
//! ```ts
//! export default function hello() { return 1; }
//! export default class Hello {}
//! export default foo;  // re-export of a local binding
//! ```
//! the bundler renames the symbol to a unique name
//! (`_d<base>$<n>`) so that when multiple files are
//! bundled together, there are no name collisions between default exports
//! from different modules.
//!
//! ## Pipeline
//!
//! 1. **`collect_export_default_mappings`** — Scan all files (except entry
//!    files) for named default exports and assign unique names.
//! 2. **`export_default_local_handler`** — Rename the declaration name and all
//!    local references in the exporting file itself.
//! 3. **`export_default_import_handler`** — Rename default imports and all
//!    references in importing files.
//!
//! All sub-handlers operate on source text via AST round-tripping, the same
//! span-replacement strategy used by `apply_renames` in `susee_utils::apply_renames`.

use std::path::Path;

use oxc::ast::ast::{
    ExportDefaultDeclarationKind, ExportSpecifier, Expression, ImportDeclaration,
    ImportDeclarationSpecifier, ModuleExportName, Program, Statement,
};
use oxc::ast_visit::Visit;
use oxc::span::Span;

use crate::types::DepsFile;
use crate::unique_name::{UniqueName, sigil};
use crate::utils::with_parsed_program;

/// The category key for named default exports, mirroring the TS
/// implementation (`uniqueName.setPrefix({ key: "ExportDefault", ... })`).
const EXPORT_DEFAULT_PREFIX_KEY: &str = "ExportDefault";

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

/// Resolve a module specifier (e.g. `"./foo"`, `"./bar/baz"`) relative to
/// `containing_file`, then normalize it into a key via [`get_file_key`].
///
/// Mirrors `getModuleKeyFromSpecifier` from `__local__/ts/bundler/lib/helpers.ts`.
fn get_module_key(specifier: &str, containing_file: &str) -> String {
    if specifier.starts_with('.') || specifier.starts_with('/') {
        let base_dir = Path::new(containing_file)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let resolved = if specifier.starts_with('/') {
            Path::new(specifier).to_path_buf()
        } else {
            base_dir.join(specifier)
        };
        get_file_key(&resolved.to_string_lossy())
    } else {
        specifier.to_string()
    }
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

/// A mapping from an export-default handler's file key to the generated
/// name and the original base name.
#[derive(Debug, Clone)]
struct ExportDefaultEntry {
    /// The file key (normalized path) of the exporting file.
    file: String,
    /// The original declaration name (e.g. `hello` in
    /// `export default function hello()`).
    base: String,
    /// The generated unique name (e.g. `_dhello$1`).
    new_name: String,
}

/// A mapping from an import default's local binding name to the generated
/// export-default name, scoped to the importing file.
#[derive(Debug, Clone)]
struct ExportDefaultImportEntry {
    /// The importing file path.
    file: String,
    /// The original local binding name used in `import X from "..."`.
    base: String,
    /// The export-default name to replace it with.
    new_name: String,
}

/// Mutable state for the export-default handler.
struct ExportDefaultState {
    unique: UniqueName,
    export_map: Vec<ExportDefaultEntry>,
    import_map: Vec<ExportDefaultImportEntry>,
}

impl ExportDefaultState {
    fn new() -> Self {
        let mut unique = UniqueName::new();
        unique.set_prefix(EXPORT_DEFAULT_PREFIX_KEY, sigil::DEFAULT);
        Self {
            unique,
            export_map: Vec::new(),
            import_map: Vec::new(),
        }
    }

    /// Look up the export-default entry for a given file key.
    fn find_export_default(&self, file_key: &str) -> Option<&ExportDefaultEntry> {
        self.export_map.iter().find(|m| m.file == file_key)
    }

    /// Look up the export-default import rename for a given (file, base).
    fn find_export_default_import(&self, file: &str, base: &str) -> Option<&str> {
        self.import_map
            .iter()
            .find(|m| m.file == file && m.base == base)
            .map(|m| m.new_name.as_str())
    }
}

// ---------------------------------------------------------------------------
// 1. Collect export-default mappings
// ---------------------------------------------------------------------------

/// Collect export-default mappings from all dependency files (except entry
/// files).
///
/// This mirrors `collectExportDefaultMappings` from `exportDefault.ts`.
/// It finds:
/// - `export default function hello()` → base = `hello`
/// - `export default class Hello()` → base = `Hello`
/// - `export default <identifier>` (a re-export of a local binding) →
///   base = the identifier
///
/// Only the **first** matching statement per file is recorded (matching the
/// TS `break` on first match).
///
/// In oxc, `export default function foo()` is parsed as
/// `Statement::ExportDefaultDeclaration` with
/// `ExportDefaultDeclarationKind::FunctionDeclaration` (the Function itself
/// doesn't carry export/default modifiers — they're on the wrapping
/// statement). A standalone `Statement::FunctionDeclaration` is NOT an
/// export-default.
fn collect_export_default_mappings(deps: &[DepsFile], state: &mut ExportDefaultState) {
    for dep in deps {
        if dep.is_entry {
            continue;
        }

        let file_key = get_file_key(&dep.file);

        with_parsed_program(&dep.file, &dep.content, |program| {
            for stmt in &program.body {
                match stmt {
                    // `export default function hello()` — oxc parses this
                    // as ExportDefaultDeclaration wrapping a FunctionDeclaration
                    // with an id.
                    Statement::ExportDefaultDeclaration(export_decl) => {
                        match &export_decl.declaration {
                            ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                                if let Some(id) = &func.id {
                                    let base_name = id.name.as_str().to_string();
                                    let new_name = state
                                        .unique
                                        .get_name(EXPORT_DEFAULT_PREFIX_KEY, &base_name);
                                    state.export_map.push(ExportDefaultEntry {
                                        file: file_key.clone(),
                                        base: base_name,
                                        new_name,
                                    });
                                    break;
                                }
                            }
                            ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
                                if let Some(id) = &cls.id {
                                    let base_name = id.name.as_str().to_string();
                                    let new_name = state
                                        .unique
                                        .get_name(EXPORT_DEFAULT_PREFIX_KEY, &base_name);
                                    state.export_map.push(ExportDefaultEntry {
                                        file: file_key.clone(),
                                        base: base_name,
                                        new_name,
                                    });
                                    break;
                                }
                            }
                            // `export default <identifier>` — a re-export of
                            // a local binding via `export default foo;`
                            ExportDefaultDeclarationKind::Identifier(ident) => {
                                let base_name = ident.name.as_str().to_string();
                                let new_name =
                                    state.unique.get_name(EXPORT_DEFAULT_PREFIX_KEY, &base_name);
                                state.export_map.push(ExportDefaultEntry {
                                    file: file_key.clone(),
                                    base: base_name,
                                    new_name,
                                });
                                break;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 2. Export-default local handler
// ---------------------------------------------------------------------------

/// Collect byte-offset spans to replace for the export-default local handler.
///
/// This rewrites the declaration name and all local references to the
/// renamed base name within the exporting file itself. Mirrors
/// `exportDefaultLocalHandler` from `exportDefault.ts`.
fn collect_export_default_local_spans(
    program: &Program<'_>,
    mapping: &ExportDefaultEntry,
) -> Vec<(usize, usize, String)> {
    let base = &mapping.base;
    let new_name = &mapping.new_name;
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        match stmt {
            // `export default function hello()` — rename `hello` in the
            // function declaration.
            Statement::ExportDefaultDeclaration(export_decl) => {
                match &export_decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                        if let Some(id) = &func.id
                            && id.name.as_str() == base
                        {
                            spans.push((
                                id.span.start as usize,
                                id.span.end as usize,
                                new_name.clone(),
                            ));
                        }
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
                        if let Some(id) = &cls.id
                            && id.name.as_str() == base
                        {
                            spans.push((
                                id.span.start as usize,
                                id.span.end as usize,
                                new_name.clone(),
                            ));
                        }
                    }
                    // `export default <identifier>` — rename the identifier.
                    ExportDefaultDeclarationKind::Identifier(ident)
                        if ident.name.as_str() == base =>
                    {
                        spans.push((
                            ident.span.start as usize,
                            ident.span.end as usize,
                            new_name.clone(),
                        ));
                    }
                    _ => {}
                }
            }
            // Variable declarations: `const hello = ...` → `const <new> = ...`
            Statement::VariableDeclaration(var_decl) => {
                for decl in &var_decl.declarations {
                    if let oxc::ast::ast::BindingPattern::BindingIdentifier(id) = &decl.id
                        && id.name.as_str() == base
                    {
                        spans.push((
                            id.span.start as usize,
                            id.span.end as usize,
                            new_name.clone(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // Collect all identifier references that match `base`.
    let mut ref_collector = ExportDefaultRefCollector {
        base,
        new_name,
        spans: Vec::new(),
    };
    ref_collector.visit_program(program);

    // Convert Span-based to offset-based.
    for (span, name) in &ref_collector.spans {
        spans.push((span.start as usize, span.end as usize, name.clone()));
    }

    // Sort right-to-left and dedup.
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    spans
}

/// Collect all identifier reference spans matching `base` that should be
/// renamed (calls, property access, new expressions, bare identifiers).
struct ExportDefaultRefCollector<'a> {
    base: &'a str,
    new_name: &'a str,
    spans: Vec<(Span, String)>,
}

impl<'a, 'ast> Visit<'ast> for ExportDefaultRefCollector<'a> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && ident.name.as_str() == self.base
        {
            self.spans.push((ident.span, self.new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.object
            && ident.name.as_str() == self.base
        {
            self.spans.push((ident.span, self.new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && ident.name.as_str() == self.base
        {
            self.spans.push((ident.span, self.new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_new_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'ast>) {
        if it.name.as_str() == self.base {
            self.spans.push((it.span, self.new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_identifier_reference(self, it);
    }
}

/// Process a single file for export-default local renames.
///
/// This handles the exporting file itself — renaming the declaration and
/// all local references. Mirrors `exportDefaultLocalHandler`.
fn export_default_local_handler(dep: &DepsFile, state: &mut ExportDefaultState) -> String {
    if dep.is_entry {
        return dep.content.clone();
    }

    let file_key = get_file_key(&dep.file);
    let Some(mapping) = state.find_export_default(&file_key).cloned() else {
        return dep.content.clone();
    };

    with_parsed_program(&dep.file, &dep.content, |program| {
        let spans = collect_export_default_local_spans(program, &mapping);

        if spans.is_empty() {
            return dep.content.clone();
        }

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
// 3. Export-default import handler
// ---------------------------------------------------------------------------

/// Process a single file for export-default import renames and reference
/// updates.
///
/// This mirrors `exportDefaultImportAndUsageHandler` — it scans for default
/// imports from modules that have an export-default mapping, records the
/// import rename, and then renames all references (calls, property access,
/// new expressions, export specifiers, bare identifiers) to use the new name.
fn export_default_import_handler(dep: &DepsFile, state: &mut ExportDefaultState) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        // Phase 3a: Collect import mappings for this file.
        let mut import_spans: Vec<(usize, usize, String)> = Vec::new();

        for stmt in &program.body {
            if let Statement::ImportDeclaration(import_decl) = stmt {
                let source = import_source(import_decl);
                let module_key = get_module_key(&source, &dep.file);

                let Some(mapping) = state.find_export_default(&module_key).cloned() else {
                    continue;
                };

                if let Some(specifiers) = &import_decl.specifiers {
                    for spec in specifiers {
                        if let ImportDeclarationSpecifier::ImportDefaultSpecifier(default_spec) =
                            spec
                        {
                            let local_name = default_spec.local.name.as_str().to_string();
                            state.import_map.push(ExportDefaultImportEntry {
                                file: dep.file.clone(),
                                base: local_name.clone(),
                                new_name: mapping.new_name.clone(),
                            });
                            import_spans.push((
                                default_spec.local.span.start as usize,
                                default_spec.local.span.end as usize,
                                mapping.new_name.clone(),
                            ));
                        }
                    }
                }
            }
        }

        // Phase 3b: Collect all reference spans for renamed imports.
        let mut ref_collector = ExportDefaultImportRefCollector {
            state,
            file: &dep.file,
            spans: Vec::new(),
        };
        ref_collector.visit_program(program);

        // Merge import spans and reference spans.
        let mut all_spans: Vec<(usize, usize, String)> = import_spans;
        for (span, new_name) in &ref_collector.spans {
            all_spans.push((span.start as usize, span.end as usize, new_name.clone()));
        }

        if all_spans.is_empty() {
            return dep.content.clone();
        }

        // Sort right-to-left and dedup.
        all_spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        all_spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        let mut result = dep.content.clone();
        for (start, end, replacement) in &all_spans {
            if *start <= result.len() && *end <= result.len() && *start <= *end {
                result.replace_range(*start..*end, replacement);
            }
        }

        result
    })
}

/// Collect all identifier reference spans that should be renamed because
/// they refer to a renamed default import binding (export-default handler).
struct ExportDefaultImportRefCollector<'a, 'b> {
    state: &'a ExportDefaultState,
    file: &'b str,
    spans: Vec<(Span, String)>,
}

impl<'a, 'b, 'ast> Visit<'ast> for ExportDefaultImportRefCollector<'a, 'b> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self
                .state
                .find_export_default_import(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.object
            && let Some(new_name) = self
                .state
                .find_export_default_import(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self
                .state
                .find_export_default_import(self.file, ident.name.as_str())
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

    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'ast>) {
        if let Some(new_name) = self
            .state
            .find_export_default_import(self.file, it.name.as_str())
        {
            self.spans.push((it.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_identifier_reference(self, it);
    }
}

impl<'a, 'b> ExportDefaultImportRefCollector<'a, 'b> {
    fn check_export_specifier(&mut self, spec: &ExportSpecifier<'_>) {
        if let ModuleExportName::IdentifierReference(ident) = &spec.local
            && let Some(new_name) = self
                .state
                .find_export_default_import(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Rename named default exports and their references across a set of
/// dependency files.
///
/// This is the Rust counterpart of `exportDefaultHandler` from
/// `src/bundler/lib/exportDefault.ts`. It runs three sub-handlers in sequence:
///
/// 1. `collect_export_default_mappings` — find named default exports
/// 2. `export_default_local_handler` — rename the declaration + local references
/// 3. `export_default_import_handler` — rename default imports + all references
///
/// The state (export/import name maps + unique name generator) is reset at
/// the start of each call, matching `resetExportDefaultState()` in the TS version.
pub fn export_default_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    let mut state = ExportDefaultState::new();

    // Phase 1: Collect export-default mappings across all files.
    collect_export_default_mappings(&deps, &mut state);

    // Phase 2: Rename local declarations + references in exporting files.
    let phase2: Vec<DepsFile> = deps
        .iter()
        .map(|dep| {
            let content = export_default_local_handler(dep, &mut state);
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

    // Phase 3: Rename default imports + all references in importing files.
    let phase3: Vec<DepsFile> = phase2
        .iter()
        .map(|dep| {
            let content = export_default_import_handler(dep, &mut state);
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

    #[test]
    fn renames_named_default_function_export() {
        let deps = vec![crate::utils::make_dep(
            "src/exp.ts",
            "export default function hello() { return 1; }\nhello();\n",
        )];
        let result = export_default_handler(deps);
        let content = &result[0].content;
        // The function declaration should be renamed.
        assert!(
            content.contains("export default function _dhello$"),
            "content was: {content}"
        );
        // The local call should be renamed too.
        assert!(content.contains("_dhello$"), "content was: {content}");
        // The old name should not appear as a standalone call.
        assert!(!content.contains("hello()"));
    }

    #[test]
    fn renames_named_default_class_export() {
        let deps = vec![crate::utils::make_dep(
            "src/exp.ts",
            "export default class Hello {}\nnew Hello();\n",
        )];
        let result = export_default_handler(deps);
        let content = &result[0].content;
        assert!(
            content.contains("export default class _dHello$"),
            "content was: {content}"
        );
        assert!(content.contains("new _dHello$"), "content was: {content}");
        assert!(!content.contains("Hello()"));
    }

    #[test]
    fn renames_export_default_identifier() {
        let deps = vec![crate::utils::make_dep(
            "src/exp.ts",
            "const foo = 42;\nexport default foo;\nfoo();\n",
        )];
        let result = export_default_handler(deps);
        let content = &result[0].content;
        // `export default foo` should become `export default _dfoo$1`
        assert!(
            content.contains("export default _dfoo$"),
            "content was: {content}"
        );
    }

    #[test]
    fn export_default_renames_import_in_consumer() {
        let dep_a = crate::utils::make_dep(
            "src/exp.ts",
            "export default function hello() { return 1; }\nhello();\n",
        );
        let dep_b = crate::utils::make_dep("src/main.ts", "import myFn from \"./exp\";\nmyFn();\n");
        let result = export_default_handler(vec![dep_a, dep_b]);
        let main_content = &result[1].content;
        // The import should use the new name.
        assert!(
            main_content.contains("import _dhello$"),
            "content was: {main_content}"
        );
        // The call should be renamed too.
        assert!(
            main_content.contains("_dhello$"),
            "content was: {main_content}"
        );
        // The old name should be gone.
        assert!(!main_content.contains("myFn"));
    }

    #[test]
    fn export_default_skips_entry_file() {
        let mut dep = crate::utils::make_dep(
            "src/index.ts",
            "export default function hello() { return 1; }\nhello();\n",
        );
        dep.is_entry = true;
        let result = export_default_handler(vec![dep]);
        let content = &result[0].content;
        // Entry files should not be renamed by the export-default handler.
        assert!(
            content.contains("function hello()"),
            "content was: {content}"
        );
        // No generated default-export name (`_d<base>$<n>`) should appear.
        assert!(
            !content.contains("_dhello$"),
            "entry file should not be renamed: {content}"
        );
    }

    #[test]
    fn no_changes_when_no_named_default_exports() {
        let src = "export const x = 1;\nexport function foo() { return x; }\n";
        let deps = vec![crate::utils::make_dep("src/normal.ts", src)];
        let result = export_default_handler(deps);
        assert_eq!(result[0].content, src);
    }
}
