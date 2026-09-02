//! CTS (CommonJS-TypeScript) → ESM conversion handler.
//!
//! Mirrors the CTS handler from the TypeScript implementation.
//!
//! `.cts` files use TypeScript syntax with CommonJS semantics (`import x =
//! require("…")` and `export = expr`). This handler converts them to
//! ESM-style TypeScript so they can be bundled alongside regular `.ts`
//! files.
//!
//! # Conversions
//!
//! - `import x = require("mod")` → `import x from "mod"`
//! - `export = expr` → `export default expr;`
//! - `.cts` extension renamed to `.ts`
//! - `module_type` flipped to [`ModuleType::Esm`]
//!
//! Namespace aliases like `import x = foo.bar` are preserved unchanged.

use oxc::ast::ast::{Statement, TSExportAssignment, TSImportEqualsDeclaration, TSModuleReference};
use oxc::span::GetSpan;

use crate::types::{DepsFile, ModuleType, ValidExts};
use crate::utils::with_parsed_program;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Check whether a file should be processed by the CTS handler.
fn is_cts_file(dep: &DepsFile) -> bool {
    dep.module_type == ModuleType::Cts && dep.file_ext == ValidExts::Cts
}

/// Strip lines that are just whitespace + semicolons, matching the CJS handler
/// cleanup.
fn strip_empty_semicolon_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.is_empty() || trimmed == ";")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Replace a `.cts` extension in a file path with `.ts`.
fn rename_cts_to_ts(file: &str) -> String {
    if let Some(stem) = file.strip_suffix(".cts") {
        format!("{stem}.ts")
    } else {
        file.to_string()
    }
}

// ---------------------------------------------------------------------------
// 1. CTS imports handler  (import x = require("...") → import x from "...")
// ---------------------------------------------------------------------------

/// Convert a `TSImportEqualsDeclaration` (`import x = require("mod")`) into
/// an ESM import declaration (`import x from "mod"`).
///
/// Only handles the `ExternalModuleReference` variant (i.e. `require("...")`).
/// Namespace aliases like `import x = foo` or `import x = foo.bar` are left
/// unchanged — they reference TypeScript namespaces, not CommonJS modules.
fn process_cts_import(decl: &TSImportEqualsDeclaration<'_>) -> Option<String> {
    let TSModuleReference::ExternalModuleReference(ext_ref) = &decl.module_reference else {
        return None;
    };
    let source = ext_ref.expression.value.as_str().to_string();
    let local_name = decl.id.name.as_str().to_string();
    Some(format!("import {local_name} from \"{source}\";"))
}

// ---------------------------------------------------------------------------
// 2. CTS exports handler  (export = expr → export default expr)
// ---------------------------------------------------------------------------

/// Convert a `TSExportAssignment` (`export = expr`) into an ESM default export
/// (`export default expr`).
fn process_cts_export_assignment(
    export_assign: &TSExportAssignment<'_>,
    source_text: &str,
) -> String {
    let expr_text = export_assign.expression.span().source_text(source_text);
    format!("export default {expr_text};")
}

// ---------------------------------------------------------------------------
// 3. Combined handler
// ---------------------------------------------------------------------------

/// Convert a single CTS file's content from CTS syntax to ESM-style TS.
///
/// Handles:
/// - `import x = require("mod")` → `import x from "mod"`
/// - `export = expr` → `export default expr`
///
/// Other statements (including `import x = foo` namespace aliases) are
/// preserved as-is.
fn cts_file_handler(dep: &DepsFile) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;
        let mut result = String::with_capacity(dep.content.len());

        for stmt in &program.body {
            match stmt {
                Statement::TSImportEqualsDeclaration(import_decl) => {
                    if let Some(import_str) = process_cts_import(import_decl) {
                        result.push_str(&import_str);
                        result.push('\n');
                        continue;
                    }
                    // Namespace alias or other — keep original
                    let text = stmt.span().source_text(source_text);
                    result.push_str(text);
                    result.push('\n');
                }
                Statement::TSExportAssignment(export_assign) => {
                    let export_str = process_cts_export_assignment(export_assign, source_text);
                    result.push_str(&export_str);
                    result.push('\n');
                }
                _ => {
                    let text = stmt.span().source_text(source_text);
                    result.push_str(text);
                    result.push('\n');
                }
            }
        }

        result
    })
}

// ---------------------------------------------------------------------------
// 4. Public entry point
// ---------------------------------------------------------------------------

/// Convert CTS dependency files to ESM-style TypeScript.
///
/// Mirrors the structure of `cjs_handler`:
/// 1. Process each CTS file, converting `import = require()` and `export =`
///    syntax to ESM equivalents.
/// 2. Flip `module_type` to `Esm` for every processed file.
/// 3. Rename `.cts` extension to `.ts`.
///
/// Non-CTS files are passed through unchanged.
pub fn cts_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    let mut result = Vec::with_capacity(deps.len());
    for dep in &deps {
        if is_cts_file(dep) {
            let content = strip_empty_semicolon_lines(&cts_file_handler(dep));
            let file = rename_cts_to_ts(&dep.file);
            let bytes = content.len();
            result.push(DepsFile {
                file,
                content,
                bytes,
                module_type: ModuleType::Esm,
                file_ext: ValidExts::Ts,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            });
        } else {
            result.push(dep.clone());
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Quick check helpers
// ---------------------------------------------------------------------------

/// Quick check whether source text contains a `TSImportEqualsDeclaration` with
/// a `require()` call or a `TSExportAssignment`.
// fn has_cts_syntax(content: &str) -> bool {
//     use oxc::ast_visit::Visit;

//     struct CtsDetector {
//         found: bool,
//     }

//     impl<'a> Visit<'a> for CtsDetector {
//         fn visit_ts_import_equals_declaration(&mut self, _it: &TSImportEqualsDeclaration<'a>) {
//             if let TSModuleReference::ExternalModuleReference(_) = &_it.module_reference {
//                 self.found = true;
//             }
//         }

//         fn visit_ts_export_assignment(&mut self, _it: &TSExportAssignment<'a>) {
//             self.found = true;
//         }
//     }

//     with_parsed_program("__probe.cts", content, |program| {
//         let mut det = CtsDetector { found: false };
//         det.visit_program(program);
//         det.found
//     })
// }

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cts_dep(file: &str, content: &str) -> DepsFile {
        DepsFile {
            file: file.to_string(),
            content: content.to_string(),
            bytes: content.len(),
            module_type: ModuleType::Cts,
            file_ext: ValidExts::Cts,
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
            file_ext: ValidExts::Ts,
            is_jsx: false,
            is_entry: false,
        }
    }

    // --- is_cts_file ---

    #[test]
    fn test_is_cts_file_true() {
        let dep = make_cts_dep("mod.cts", "export = {};");
        assert!(is_cts_file(&dep));
    }

    #[test]
    fn test_is_cts_file_false_esm() {
        let dep = make_esm_dep("mod.ts", "export const x = 1;");
        assert!(!is_cts_file(&dep));
    }

    // --- strip_empty_semicolon_lines ---

    #[test]
    fn test_strip_empty_semicolon_lines_removes_semicolon_only() {
        let result = strip_empty_semicolon_lines("const x = 1;\n;\nconst y = 2;");
        assert!(!result.contains("\n;\n"));
        assert!(result.contains("const x = 1;"));
        assert!(result.contains("const y = 2;"));
    }

    // --- rename_cts_to_ts ---

    #[test]
    fn test_rename_cts_to_ts() {
        assert_eq!(rename_cts_to_ts("src/mod.cts"), "src/mod.ts");
        assert_eq!(rename_cts_to_ts("mod.ts"), "mod.ts");
    }

    // --- has_cts_syntax ---

    // #[test]
    // fn test_has_cts_syntax_true_import_require() {
    //     assert!(has_cts_syntax("import fs = require(\"fs\");"));
    // }

    // #[test]
    // fn test_has_cts_syntax_true_export_assignment() {
    //     assert!(has_cts_syntax("export = { foo: 1 };"));
    // }

    // #[test]
    // fn test_has_cts_syntax_false_plain_esm() {
    //     assert!(!has_cts_syntax("export const x = 1;"));
    // }

    // --- cts_handler ---

    #[test]
    fn test_cts_handler_converts_import_require() {
        let dep = make_cts_dep("mod.cts", "import fs = require(\"fs\");\nexport = fs;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("import fs from \"fs\""));
        assert!(result[0].content.contains("export default"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
        assert_eq!(result[0].file_ext, ValidExts::Ts);
        assert_eq!(result[0].file, "mod.ts");
    }

    #[test]
    fn test_cts_handler_converts_export_assignment() {
        let dep = make_cts_dep("mod.cts", "const foo = 1;\nexport = foo;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export default foo;"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_cts_handler_converts_export_object() {
        let dep = make_cts_dep("mod.cts", "const foo = { bar: 1 };\nexport = foo;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export default foo;"));
    }

    #[test]
    fn test_cts_handler_preserves_namespace_alias() {
        let dep = make_cts_dep(
            "mod.cts",
            "import foo = require(\"foo\");\nimport bar = foo.bar;\nexport = bar;",
        );
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        // `import foo = require("foo")` should become `import foo from "foo"`
        assert!(result[0].content.contains("import foo from \"foo\""));
        // `import bar = foo.bar` is a namespace alias, should be preserved
        assert!(result[0].content.contains("import bar = foo.bar"));
    }

    #[test]
    fn test_cts_handler_passes_through_esm() {
        let dep = make_esm_dep("mod.ts", "export const x = 1;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "export const x = 1;");
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_cts_handler_renames_extension() {
        let dep = make_cts_dep("src/mod.cts", "export = 42;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result[0].file, "src/mod.ts");
        assert_eq!(result[0].file_ext, ValidExts::Ts);
    }

    #[test]
    fn test_cts_handler_preserves_other_statements() {
        let dep = make_cts_dep(
            "mod.cts",
            "const x = 1;\nimport fs = require(\"fs\");\nexport = x;",
        );
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("const x = 1;"));
        assert!(result[0].content.contains("import fs from \"fs\""));
        assert!(result[0].content.contains("export default x;"));
    }

    #[test]
    fn test_cts_handler_export_default_function() {
        let dep = make_cts_dep("mod.cts", "export = function() { return 1; };");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export default function"));
    }

    #[test]
    fn test_cts_handler_export_default_class() {
        let dep = make_cts_dep("mod.cts", "export = class { foo() {} };");
        let result = cts_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export default class"));
    }

    #[test]
    fn test_cts_handler_bytes_match_content_length() {
        // Bug fix: `bytes` was `dep.bytes` (stale) instead of `content.len()`.
        // The handler rewrites the content (e.g. `export = x` →
        // `export default x;`), so `bytes` must reflect the NEW content.
        let dep = make_cts_dep("mod.cts", "const x = 1;\nexport = x;");
        let result = cts_handler(vec![dep]);
        assert_eq!(result[0].bytes, result[0].content.len());
    }
}
