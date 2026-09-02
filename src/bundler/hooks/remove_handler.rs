//! Import/Export removal hook.
//!
//! Mirrors `src/bundler/lib/remove.ts` from the TypeScript implementation.
//!
//! During bundling, all module-level import and export syntax must be
//! stripped so that the remaining declarations can be concatenated into a
//! single output file. This hook provides two sub-handlers, run in sequence:
//!
//! 1. **`import_all_remove_handler`** — Remove every import declaration
//!    (`import … from "…"`, `import x = require("…")`, `const x = require("…")`)
//!    and collect the removed statement text so the bundler can re-emit
//!    consolidated imports later.
//! 2. **`esm_export_remove_handler`** — Strip `export` / `export default`
//!    modifiers from declarations (functions, classes, interfaces, type
//!    aliases, enums, variable statements) and delete bare
//!    `export { … }` / `export { … } from "…"` statements entirely.
//!
//! Both handlers operate on source text via AST round-tripping, the same
//! span-replacement strategy used by `apply_renames` in
//! `crate::utils::apply_renames`.

use oxc::ast::ast::{
    BindingPattern, ExportDefaultDeclarationKind, ImportOrExportKind, Program, Statement,
    TSModuleReference,
};
use oxc::span::GetSpan;

use crate::types::DepsFile;
use crate::utils::with_parsed_program;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Sort spans by start offset descending (right-to-left) and remove
/// duplicates so that earlier replacements don't invalidate later offsets.
fn sort_and_dedup_spans(spans: &mut Vec<(usize, usize, String)>) {
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
}

/// Apply a list of (start, end, replacement) spans to the source text.
fn apply_spans(content: &str, spans: &[(usize, usize, String)]) -> String {
    let mut result = content.to_string();
    for (start, end, replacement) in spans {
        if *start <= result.len() && *end <= result.len() && *start <= *end {
            result.replace_range(*start..*end, replacement);
        }
    }
    result
}

/// Strip lines that are empty or consist of only a semicolon after removal.
///
/// Re-exports the shared [`crate::utils::strip_empty_lines`].
use crate::utils::strip_empty_lines;

/// Check whether a statement is a TypeScript namespace/module declaration.
/// The TS implementation checks `isInsideNamespace` to avoid stripping `export`
/// from declarations nested in `namespace Foo { … }` blocks.
fn is_namespace_declaration(stmt: &Statement<'_>) -> bool {
    matches!(
        stmt,
        Statement::TSExternalModuleDeclaration(_)
            | Statement::TSNamespaceDeclaration(_)
            | Statement::TSGlobalDeclaration(_)
    )
}

// ---------------------------------------------------------------------------
// 1. Import removal handler
// ---------------------------------------------------------------------------

/// Information about a removed import, for re-emission by the bundler.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RemovedImport {
    /// The original import statement text.
    pub text: String,
    /// The file it was removed from.
    pub file: String,
}

/// Collect import statement spans to remove and record the removed text.
///
/// Handles:
/// - `import … from "…"` (ESM import declarations)
/// - `import x = require("…")` (TS import-equals with external module ref)
/// - `const x = require("…")` / `const { a, b } = require("…")` (CJS require)
///
/// For TS import-equals and CJS require, a replacement ESM import string is
/// generated and recorded in `removed` so the bundler can re-emit it.
fn collect_import_removal_spans(
    program: &Program<'_>,
    source_text: &str,
    file: &str,
    removed: &mut Vec<RemovedImport>,
) -> Vec<(usize, usize, String)> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        match stmt {
            // --- Case 1: ESM import declarations ---
            Statement::ImportDeclaration(_) => {
                let text = stmt.span().source_text(source_text).to_string();
                removed.push(RemovedImport {
                    text: text.clone(),
                    file: file.to_string(),
                });
                // Remove the entire statement (replace with empty).
                let span = stmt.span();
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- Case 2: TS import-equals declarations ---
            Statement::TSImportEqualsDeclaration(import_eq) => {
                let name = import_eq.id.name.as_str().to_string();
                let is_type_only = import_eq.import_kind == ImportOrExportKind::Type;

                let replacement = match &import_eq.module_reference {
                    TSModuleReference::ExternalModuleReference(ext_ref) => {
                        let source = ext_ref.expression.value.as_str().to_string();
                        if is_type_only {
                            Some(format!("import type * as {name} from \"{source}\";"))
                        } else {
                            Some(format!("import {name} from \"{source}\";"))
                        }
                    }
                    TSModuleReference::IdentifierReference(_)
                    | TSModuleReference::QualifiedName(_) => {
                        // Namespace alias like `import x = foo` — no ESM
                        // equivalent; just remove and don't re-emit.
                        None
                    }
                };

                let orig_text = stmt.span().source_text(source_text).to_string();
                removed.push(RemovedImport {
                    text: replacement.clone().unwrap_or_else(|| orig_text.clone()),
                    file: file.to_string(),
                });
                let span = stmt.span();
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- Case 3: CJS require in variable declarations ---
            // `const x = require("…")` or `const { a, b } = require("…")`
            Statement::VariableDeclaration(var_decl) => {
                if var_decl.declarations.len() != 1 {
                    continue;
                }
                let decl = &var_decl.declarations[0];

                let init = match &decl.init {
                    Some(init) => init,
                    None => continue,
                };

                // Check if the initializer is a `require("…")` call.
                let is_require_call = match init {
                    oxc::ast::ast::Expression::CallExpression(call) => match &call.callee {
                        oxc::ast::ast::Expression::Identifier(ident) => {
                            ident.name.as_str() == "require"
                        }
                        _ => false,
                    },
                    _ => false,
                };

                if !is_require_call {
                    continue;
                }

                // Extract the source string from the first argument.
                let source = match init {
                    oxc::ast::ast::Expression::CallExpression(call) => {
                        if let Some(arg) = call.arguments.first() {
                            match &arg {
                                oxc::ast::ast::Argument::StringLiteral(s) => {
                                    Some(s.value.as_str().to_string())
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let Some(source) = source else {
                    continue;
                };

                // Extract the local binding name(s).
                let replacement = match &decl.id {
                    BindingPattern::BindingIdentifier(id) => {
                        let name = id.name.as_str().to_string();
                        Some(format!("import {name} from \"{source}\";"))
                    }
                    BindingPattern::ObjectPattern(obj) => {
                        let names: Vec<String> = obj
                            .properties
                            .iter()
                            .filter_map(|prop| {
                                match &prop.value {
                                    BindingPattern::BindingIdentifier(id) => {
                                        Some(id.name.as_str().to_string())
                                    }
                                    BindingPattern::AssignmentPattern(assign) => {
                                        // `const { x = 1 } = require(...)` —
                                        // extract the name from the left side.
                                        match &assign.left {
                                            BindingPattern::BindingIdentifier(id) => {
                                                Some(id.name.as_str().to_string())
                                            }
                                            _ => None,
                                        }
                                    }
                                    _ => None,
                                }
                            })
                            .collect();
                        if names.is_empty() {
                            None
                        } else {
                            Some(format!(
                                "import {{ {} }} from \"{}\";",
                                names.join(", "),
                                source
                            ))
                        }
                    }
                    BindingPattern::ArrayPattern(_) => {
                        // `const [a, b] = require(...)` — uncommon, fall back to namespace import.
                        None
                    }
                    _ => None,
                };

                let orig_text = stmt.span().source_text(source_text).to_string();
                removed.push(RemovedImport {
                    text: replacement.clone().unwrap_or_else(|| orig_text.clone()),
                    file: file.to_string(),
                });
                let span = stmt.span();
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            _ => {}
        }
    }

    sort_and_dedup_spans(&mut spans);
    spans
}

/// Process a single file, removing all import statements and recording
/// removed imports.
fn import_all_remove_handler(dep: &DepsFile, removed: &mut Vec<RemovedImport>) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;
        let spans = collect_import_removal_spans(program, source_text, &dep.file, removed);
        if spans.is_empty() {
            return dep.content.clone();
        }
        apply_spans(&dep.content, &spans)
    })
}

// ---------------------------------------------------------------------------
// 2. ESM export removal handler
// ---------------------------------------------------------------------------

/// Collect export-removal spans for a single file.
///
/// This handles three cases (mirroring `esmExportRemoveHandler` in `remove.ts`):
///
/// 1. **Strip `export` / `export default` modifiers** from declarations
///    (functions, classes, interfaces, type aliases, enums, variable
///    statements). The declaration itself is kept; only the `export` keyword
///    (and `default` for non-default exports) is removed.
/// 2. **Remove `export { foo }` / `export { foo } from "…"` entirely** — these
///    are pure re-export statements with no declaration.
/// 3. **Remove `export default <identifier>`** — a re-export of a local
///    binding (`export default foo;`). The binding declaration remains.
fn collect_export_removal_spans(
    program: &Program<'_>,
    source_text: &str,
) -> Vec<(usize, usize, String)> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        // Skip namespace/module declarations — don't strip export inside them.
        if is_namespace_declaration(stmt) {
            continue;
        }

        match stmt {
            // --- Case 1: `export const/function/class/interface/type/enum ...`
            // Strip the `export` keyword (and `default` keyword if present).
            Statement::ExportDeclaration(export_decl) => {
                // `export <Declaration>` — the `export` keyword span is from
                // the statement start to the declaration start.
                let decl_start = export_decl.declaration.span().start as usize;
                let stmt_start = export_decl.span.start as usize;
                // Replace from statement start to declaration start with nothing.
                // This removes `export ` (and any leading whitespace before the
                // declaration keyword).
                if decl_start > stmt_start {
                    let prefix = &source_text[stmt_start..decl_start];
                    // Only strip if it actually contains `export`.
                    if prefix.contains("export") {
                        spans.push((stmt_start, decl_start, String::new()));
                    }
                }
            }

            // --- `export default function/class/interface/...`
            Statement::ExportDefaultDeclaration(export_decl) => {
                match &export_decl.declaration {
                    // `export default <identifier>` — re-export of local binding.
                    // Remove the entire statement.
                    ExportDefaultDeclarationKind::Identifier(_) => {
                        let span = export_decl.span;
                        spans.push((span.start as usize, span.end as usize, String::new()));
                    }
                    // `export default function foo() {}` / `export default class Foo {}`
                    // — strip `export default ` prefix, keep the declaration.
                    ExportDefaultDeclarationKind::FunctionDeclaration(_)
                    | ExportDefaultDeclarationKind::ClassDeclaration(_)
                    | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => {
                        let decl_span = export_decl.declaration.span();
                        let stmt_start = export_decl.span.start as usize;
                        let decl_start = decl_span.start as usize;
                        if decl_start > stmt_start {
                            let prefix = &source_text[stmt_start..decl_start];
                            if prefix.contains("export") {
                                spans.push((stmt_start, decl_start, String::new()));
                            }
                        }
                    }
                    // `export default <expression>` (arrow, object, etc.)
                    // These have already been converted to
                    // `const <name> = <expr>; export default <name>;` by the
                    // anonymous handler. Strip the `export default <name>;` part.
                    _ => {
                        let span = export_decl.span;
                        spans.push((span.start as usize, span.end as usize, String::new()));
                    }
                }
            }

            // --- Case 2: `export { foo }` — remove entirely.
            Statement::ExportNamedDeclaration(export_named) => {
                // This is a pure export specifier statement (no declaration).
                // Remove the entire statement.
                let span = export_named.span;
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- `export { foo } from "…"` — remove entirely.
            Statement::ExportFromDeclaration(export_from) => {
                let span = export_from.span;
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- `export * from "…"` / `export * as ns from "…"` — remove.
            Statement::ExportAllDeclaration(export_all) => {
                let span = export_all.span;
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- TS export assignment (`export = foo`) — CTS-specific, remove.
            Statement::TSExportAssignment(export_assign) => {
                let span = export_assign.span;
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            // --- TS namespace export declaration — remove.
            Statement::TSNamespaceExportDeclaration(ns_export) => {
                let span = ns_export.span;
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            _ => {}
        }
    }

    // Also collect export modifiers from type-only declarations that are
    // represented as `ExportDeclaration` wrapping a `Declaration` variant.
    // (Handled above in the `ExportDeclaration` case.)

    sort_and_dedup_spans(&mut spans);
    spans
}

/// Process a single file, stripping/removing export statements.
///
/// Entry files are left unchanged — their exports are the public API of the
/// bundled package and must be preserved.
fn esm_export_remove_handler(dep: &DepsFile) -> String {
    if dep.is_entry {
        return dep.content.clone();
    }
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;
        let spans = collect_export_removal_spans(program, source_text);
        if spans.is_empty() {
            return dep.content.clone();
        }
        apply_spans(&dep.content, &spans)
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Remove all import and export statements from a set of dependency files.
///
/// This is the Rust counterpart of `removeHandlers` from
/// `src/bundler/lib/remove.ts`. It runs two sub-handlers in sequence:
///
/// 1. `import_all_remove_handler` — remove all imports, recording removed text
/// 2. `esm_export_remove_handler` — strip/remove all export statements
///
/// After removal, empty lines and dangling semicolons are cleaned up.
///
/// The `removed_imports` return value contains all removed import statements
/// so the bundler can re-emit consolidated imports at the top of the bundle.
pub fn remove_handler(deps: Vec<DepsFile>) -> (Vec<DepsFile>, Vec<RemovedImport>) {
    let mut removed_imports: Vec<RemovedImport> = Vec::new();

    // Phase 1: Remove all import declarations.
    let phase1: Vec<DepsFile> = deps
        .iter()
        .map(|dep| {
            let content = import_all_remove_handler(dep, &mut removed_imports);
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

    // Phase 2: Strip/remove all export statements.
    let phase2: Vec<DepsFile> = phase1
        .iter()
        .map(|dep| {
            let content = strip_empty_lines(&esm_export_remove_handler(dep));
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

    (phase2, removed_imports)
}

/// Convenience wrapper that only returns the processed files (discards the
/// removed import list). Useful when the caller doesn't need to re-emit imports.
#[allow(dead_code)]
pub fn remove_handler_simple(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    remove_handler(deps).0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Import removal ---

    #[test]
    fn removes_esm_import_declaration() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import { foo } from \"./foo\";\nconst x = foo;\n",
        )];
        let (result, removed) = remove_handler(deps);
        let content = &result[0].content;
        assert!(!content.contains("import"));
        assert!(content.contains("const x = foo;"));
        assert_eq!(removed.len(), 1);
        assert!(removed[0].text.contains("import { foo }"));
    }

    #[test]
    fn removes_default_import() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import bar from \"./bar\";\nbar();\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("import"));
        assert!(result[0].content.contains("bar();"));
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn removes_namespace_import() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import * as utils from \"./utils\";\nutils.doStuff();\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("import"));
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn removes_side_effect_import() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import \"./polyfill\";\nconst x = 1;\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("import"));
        assert!(result[0].content.contains("const x = 1;"));
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn removes_ts_import_equals_require() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import fs = require(\"fs\");\nfs.readFileSync(\"x\");\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("import fs = require"));
        assert!(result[0].content.contains("fs.readFileSync"));
        assert_eq!(removed.len(), 1);
        // The removed text should contain a replacement ESM import.
        assert!(removed[0].text.contains("import fs from \"fs\""));
    }

    #[test]
    fn removes_cjs_require_variable() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "const path = require(\"path\");\npath.join(\"a\", \"b\");\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("require"));
        assert!(result[0].content.contains("path.join"));
        assert_eq!(removed.len(), 1);
        assert!(removed[0].text.contains("import path from \"path\""));
    }

    #[test]
    fn removes_cjs_require_destructuring() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "const { readFileSync, writeFileSync } = require(\"fs\");\nreadFileSync(\"x\");\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("require"));
        assert!(result[0].content.contains("readFileSync"));
        assert_eq!(removed.len(), 1);
        assert!(
            removed[0]
                .text
                .contains("import { readFileSync, writeFileSync } from \"fs\"")
        );
    }

    // --- Export removal ---

    #[test]
    fn strips_export_from_function_declaration() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export function hello() { return 1; }\nhello();\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("function hello()"));
        assert!(!content.contains("export"));
        assert!(content.contains("hello();"));
    }

    #[test]
    fn strips_export_from_class_declaration() {
        let deps = vec![crate::utils::make_dep("src/a.ts", "export class Foo {}\n")];
        let (result, _) = remove_handler(deps);
        assert!(result[0].content.contains("class Foo {}"));
        assert!(!result[0].content.contains("export"));
    }

    #[test]
    fn strips_export_from_variable_declaration() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export const x = 42;\nconsole.log(x);\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("const x = 42;"));
        assert!(!content.contains("export"));
    }

    #[test]
    fn strips_export_from_interface() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export interface Foo { bar: number; }\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("interface Foo {"));
        assert!(!content.contains("export"));
    }

    #[test]
    fn strips_export_from_type_alias() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export type Bar = string;\n",
        )];
        let (result, _) = remove_handler(deps);
        assert!(result[0].content.contains("type Bar = string;"));
        assert!(!result[0].content.contains("export"));
    }

    #[test]
    fn strips_export_from_enum() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export enum Color { Red, Green, Blue }\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("enum Color {"));
        assert!(!content.contains("export"));
    }

    #[test]
    fn removes_export_named_specifiers() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "const foo = 1;\nexport { foo };\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("const foo = 1;"));
        // The `export { foo };` line should be gone.
        assert!(!content.contains("export {"));
    }

    #[test]
    fn removes_export_from_declaration() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export { foo } from \"./foo\";\nconst x = 1;\n",
        )];
        let (result, _) = remove_handler(deps);
        assert!(!result[0].content.contains("export"));
        assert!(result[0].content.contains("const x = 1;"));
    }

    #[test]
    fn removes_export_all_declaration() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export * from \"./utils\";\nconst x = 1;\n",
        )];
        let (result, _) = remove_handler(deps);
        assert!(!result[0].content.contains("export"));
        assert!(result[0].content.contains("const x = 1;"));
    }

    #[test]
    fn removes_export_default_identifier_reexport() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "const foo = 42;\nexport default foo;\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("const foo = 42;"));
        // The `export default foo;` line should be removed.
        assert!(!content.contains("export default"));
    }

    #[test]
    fn strips_export_default_from_function() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export default function hello() { return 1; }\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        // Should keep the function declaration but strip `export default `.
        assert!(content.contains("function hello()"));
        assert!(!content.contains("export default"));
    }

    #[test]
    fn strips_export_default_from_class() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "export default class Hello {}\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        assert!(content.contains("class Hello {}"));
        assert!(!content.contains("export default"));
    }

    // --- Combined ---

    #[test]
    fn removes_both_imports_and_exports() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import { foo } from \"./foo\";\nexport const bar = foo + 1;\nconsole.log(bar);\n",
        )];
        let (result, removed) = remove_handler(deps);
        let content = &result[0].content;
        assert!(!content.contains("import"));
        assert!(!content.contains("export"));
        assert!(content.contains("const bar = foo + 1;"));
        assert!(content.contains("console.log(bar);"));
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn preserves_exports_in_entry_file() {
        let entry = DepsFile {
            file: "src/index.ts".to_string(),
            content: "export const foo = 1;\nexport function bar() { return foo; }\n".to_string(),
            bytes: 0,
            module_type: crate::types::ModuleType::Esm,
            file_ext: crate::types::ValidExts::Ts,
            is_jsx: false,
            is_entry: true,
        };
        let (result, removed) = remove_handler(vec![entry]);
        let content = &result[0].content;
        // Exports should be preserved in entry files.
        assert!(content.contains("export const foo = 1;"));
        assert!(content.contains("export function bar()"));
        // No imports were removed.
        assert!(removed.is_empty());
    }

    #[test]
    fn removes_exports_in_non_entry_file() {
        let dep = DepsFile {
            file: "src/utils.ts".to_string(),
            content: "export const foo = 1;\nexport function bar() { return foo; }\n".to_string(),
            bytes: 0,
            module_type: crate::types::ModuleType::Esm,
            file_ext: crate::types::ValidExts::Ts,
            is_jsx: false,
            is_entry: false,
        };
        let (result, _) = remove_handler(vec![dep]);
        let content = &result[0].content;
        // Exports should be stripped in non-entry files.
        assert!(!content.contains("export"));
        assert!(content.contains("const foo = 1;"));
        assert!(content.contains("function bar()"));
    }

    #[test]
    fn no_changes_when_no_imports_or_exports() {
        let src = "const x = 1;\nfunction foo() { return x; }\nfoo();\n";
        let deps = vec![crate::utils::make_dep("src/a.ts", src)];
        let (result, removed) = remove_handler(deps);
        // strip_empty_lines trims trailing whitespace, so the result has no
        // trailing newline.
        assert_eq!(result[0].content, src.trim());
        assert!(removed.is_empty());
    }

    #[test]
    fn removes_multiple_imports() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import { a } from \"./a\";\nimport { b } from \"./b\";\nconst x = a + b;\n",
        )];
        let (result, removed) = remove_handler(deps);
        assert!(!result[0].content.contains("import"));
        assert_eq!(removed.len(), 2);
    }

    #[test]
    fn cleans_up_empty_lines() {
        let deps = vec![crate::utils::make_dep(
            "src/a.ts",
            "import { foo } from \"./foo\";\n\n\nconst x = foo;\n",
        )];
        let (result, _) = remove_handler(deps);
        let content = &result[0].content;
        // No multiple blank lines.
        assert!(!content.contains("\n\n\n"));
        assert!(content.contains("const x = foo;"));
    }
}
