//! CommonJS → ESM conversion handler.
//!
//! Mirrors `cjsHandler.ts` from the TypeScript implementation.
//!
//! Converts `require()` calls and `module.exports`/`exports.x` assignments
//! in CommonJS `.js`/`.cjs` files into ESM `import`/`export` syntax so they
//! can be merged with the rest of the dependency tree.
//!
//! # Pipeline
//!
//! 1. [`commonjs_imports_handler`] — Rewrite `const x = require("…")` →
//!    `import x from "…"` (also handles destructuring and member-access
//!    patterns like `require("mod").prop`).
//! 2. [`commonjs_exports_handler`] — Rewrite `module.exports = …` →
//!    `export default …` and `exports.foo = …` → `export const foo = …`.
//! 3. Rename `.cjs`/`.js` extensions to `.js`.
//! 4. Flip `module_type` to [`ModuleType::Esm`].

use std::collections::HashSet;

use oxc::ast::ast::{
    BindingPattern, Expression, Program, Statement, VariableDeclaration, VariableDeclarationKind,
};
use oxc::ast_visit::Visit;
use oxc::span::{GetSpan, Span};

use crate::types::{DepsFile, ModuleType, ValidExts};
use crate::utils::with_parsed_program;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Collect all identifier names used as the *root* of a property access
/// (e.g. `foo.bar` → `foo`), mirroring `utils.gen.findProperty` from
/// `utilities.ts`.
///
/// This is used to detect namespace-style usage of `require`-bound variables:
/// if `foo.bar` appears anywhere then `const foo = require("mod")` should
/// become `import * as foo from "mod"`.
fn collect_property_access_names(program: &Program<'_>) -> HashSet<String> {
    let mut collector = PropertyAccessCollector {
        names: HashSet::new(),
    };
    collector.visit_program(program);
    collector.names
}

struct PropertyAccessCollector {
    names: HashSet<String>,
}

impl<'a> Visit<'a> for PropertyAccessCollector {
    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'a>) {
        if let Expression::Identifier(ident) = &it.object {
            self.names.insert(ident.name.as_str().to_string());
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }
}

/// Check whether a file should be processed by the CommonJS handlers.
fn is_commonjs_js_or_cjs(dep: &DepsFile) -> bool {
    dep.module_type == ModuleType::Cjs
        && (dep.file_ext == ValidExts::Js || dep.file_ext == ValidExts::Cjs)
}

/// Strip lines that are just whitespace + semicolons (mirrors the TS
/// `_content.replace(/^s*;\s*$/gm, "").trim()` cleanup).  The TS regex
/// `/^s*;\s*$/gm` is actually `/^\s*;\s*$/gm` — lines containing only an
/// optional semicolon surrounded by whitespace.
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

// ---------------------------------------------------------------------------
// 1. CommonJS imports handler  (require → import)
// ---------------------------------------------------------------------------

/// A parsed `require("…")` binding.
struct RequireImport {
    /// The source module path, e.g. `./foo`.
    source: String,
    /// `import x from` — the local name when bound to a single identifier.
    imported_string: Option<String>,
    /// `import { a, b } from` — names when bound via object destructuring.
    imported_object: Option<Vec<String>>,
}

/// Extract the module source from a `require("…")` call expression.
///
/// Returns the string argument if `call` is `require("…")`, otherwise `None`.
fn extract_require_source(call: &oxc::ast::ast::CallExpression<'_>) -> Option<String> {
    let Expression::Identifier(ident) = &call.callee else {
        return None;
    };
    if ident.name.as_str() != "require" {
        return None;
    }
    let arg = call.arguments.first()?;
    let Expression::StringLiteral(s) = arg.as_expression()? else {
        return None;
    };
    Some(s.value.as_str().to_string())
}

/// Format an import specifier, using `as` only when the local name differs
/// from the imported name.
fn format_import_specifier(imported: &str, local: &str) -> String {
    if imported == local {
        imported.to_string()
    } else {
        format!("{imported} as {local}")
    }
}

/// Try to interpret a `VariableDeclaration` as a `require()` call and return
/// the equivalent ESM import statement, if applicable.
///
/// Handles the following patterns:
/// - `const x = require("mod")` → `import x from "mod"`
/// - `const { a, b } = require("mod")` → `import { a, b } from "mod"`
/// - `const x = require("mod").prop` → `import { prop } from "mod"`
///   (or `import { prop as x } from "mod"` when `x != prop`)
/// - `const { a, b } = require("mod").prop` → `import { a, b } from "mod"`
fn process_require_var(
    var_decl: &VariableDeclaration<'_>,
    _properties: &HashSet<String>,
) -> Option<RequireImport> {
    // Handle `const`, `let`, and `var` require declarations. The TS
    // implementation only checked `const`, but `let x = require("mod")` and
    // `var x = require("mod")` are equally valid CJS patterns.
    if !matches!(
        var_decl.kind,
        VariableDeclarationKind::Const
            | VariableDeclarationKind::Let
            | VariableDeclarationKind::Var
    ) {
        return None;
    }
    let decl = var_decl.declarations.first()?;
    let initializer = decl.init.as_ref()?;

    // The require() call may appear directly or as the object of a member
    // expression (e.g. `require("mod").prop`).
    let (source, member_property): (String, Option<String>) = match initializer {
        Expression::CallExpression(call) => {
            let src = extract_require_source(call)?;
            (src, None)
        }
        // const x = require("mod").prop
        Expression::StaticMemberExpression(member) => {
            let Expression::CallExpression(call) = &member.object else {
                return None;
            };
            let src = extract_require_source(call)?;
            (src, Some(member.property.name.as_str().to_string()))
        }
        _ => return None,
    };

    match &decl.id {
        BindingPattern::BindingIdentifier(binding_id) => {
            let local_name = binding_id.name.as_str().to_string();
            if let Some(prop) = member_property {
                // const EventEmitter = require("node:events").EventEmitter
                // → import { EventEmitter } from "node:events"
                // const MyEv = require("node:events").EventEmitter
                // → import { EventEmitter as MyEv } from "node:events"
                Some(RequireImport {
                    source,
                    imported_string: None,
                    imported_object: Some(vec![format_import_specifier(&prop, &local_name)]),
                })
            } else {
                Some(RequireImport {
                    source,
                    imported_string: Some(local_name),
                    imported_object: None,
                })
            }
        }
        BindingPattern::ObjectPattern(obj) => {
            let names: Vec<String> = obj
                .properties
                .iter()
                .filter_map(|prop| {
                    if let BindingPattern::BindingIdentifier(id) = &prop.value {
                        Some(id.name.as_str().to_string())
                    } else {
                        None
                    }
                })
                .collect();
            if names.is_empty() {
                None
            } else {
                Some(RequireImport {
                    source,
                    imported_string: None,
                    imported_object: Some(names),
                })
            }
        }
        _ => None,
    }
}

/// Build the ESM import string from a parsed [`RequireImport`].
///
/// A `const x = require("mod")` binding always becomes a default import
/// (`import x from "mod"`), regardless of whether `x` is later used with
/// property access.  This matches the CommonJS interop semantics where
/// `require("mod")` returns `module.exports`, which ESM exposes as the
/// default export.
fn build_import_string(req: &RequireImport, _properties: &HashSet<String>) -> Option<String> {
    if let Some(name) = &req.imported_string {
        Some(format!("import {name} from \"{}\";", req.source))
    } else {
        req.imported_object.as_ref().map(|_names| {
            format!(
                "import {{ {} }} from \"{}\";",
                _names.join(", "),
                req.source
            )
        })
    }
}

/// Convert `require()` calls to ESM `import` declarations.
///
/// Mirrors `commonjsImportsHandler` from `commonjs_handler.ts`.
fn commonjs_imports_handler(dep: &DepsFile) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;
        let properties = collect_property_access_names(program);

        let mut imports = Vec::new();
        let mut result = String::with_capacity(dep.content.len());

        for stmt in &program.body {
            if let Statement::VariableDeclaration(var_decl) = stmt
                && let Some(req) = process_require_var(var_decl, &properties)
                && let Some(import_str) = build_import_string(&req, &properties)
            {
                imports.push(import_str);
                continue; // drop the `const x = require()` statement
            }

            let text = stmt.span().source_text(source_text);
            result.push_str(text);
            result.push('\n');
        }

        // Prepend collected imports, mirroring
        // `_content = removedStatements.join("\n") + "\n" + _content`.
        if imports.is_empty() {
            result
        } else {
            format!("{}\n{}", imports.join("\n"), result)
        }
    })
}

// ---------------------------------------------------------------------------
// 2. CommonJS exports handler  (exports.x / module.exports → export)
// ---------------------------------------------------------------------------

/// Classification of a single statement in the exports handler.
enum ExportKind {
    /// Move to the end of the file (default export).
    Default,
    /// Keep in place (named export / everything else).
    Normal,
}

/// Try to convert a `VariableDeclaration` of the form
/// `const foo = exports.foo = <expr>` into `export const foo = <expr>`.
///
/// Returns the replacement source text if the pattern matched.
fn try_exports_var_assignment(
    var_decl: &VariableDeclaration<'_>,
    source_text: &str,
) -> Option<(String, ExportKind)> {
    if var_decl.kind != VariableDeclarationKind::Const {
        return None;
    }
    let decl = var_decl.declarations.iter().find(|d| d.init.is_some())?;
    let init = decl.init.as_ref()?;
    let Expression::BinaryExpression(bin) = init else {
        return None;
    };
    // `exports.foo` must be a static member access with identifier object.
    let Expression::StaticMemberExpression(member) = &bin.left else {
        return None;
    };
    let Expression::Identifier(obj_ident) = &member.object else {
        return None;
    };
    if obj_ident.name.as_str() != "exports" {
        return None;
    }
    let prop_name = member.property.name.as_str();
    // The variable name must match the property name (const foo = exports.foo = expr).
    let BindingPattern::BindingIdentifier(name_ident) = &decl.id else {
        return None;
    };
    if name_ident.name.as_str() != prop_name {
        return None;
    }

    // Build `export const <name> = <right>;`
    let right_text = bin.right.span().source_text(source_text);
    Some((
        format!(
            "export const {} = {};",
            name_ident.name.as_str(),
            right_text
        ),
        ExportKind::Normal,
    ))
}

/// Try to convert an `ExpressionStatement` whose expression is an assignment
/// to `module.exports = <expr>` or `exports.foo = <expr>`.
///
/// Both `BinaryExpression` (e.g. `a = b` parsed as binary in some contexts)
/// and `AssignmentExpression` (the normal case for `exports.x = y`) are
/// handled, since oxc represents `=` assignments as
/// [`Expression::AssignmentExpression`].
///
/// Returns the replacement source text and whether it is a default export.
fn try_exports_expr_statement(
    expr_stmt: &oxc::ast::ast::ExpressionStatement<'_>,
    source_text: &str,
) -> Option<(String, ExportKind)> {
    // Extract the (left member, right expression) from either an
    // `AssignmentExpression` or a `BinaryExpression`.
    let (member, right): (&oxc::ast::ast::StaticMemberExpression<'_>, &Expression<'_>) =
        match &expr_stmt.expression {
            Expression::AssignmentExpression(assign) => {
                let oxc::ast::ast::AssignmentTarget::StaticMemberExpression(m) = &assign.left
                else {
                    return None;
                };
                (m, &assign.right)
            }
            Expression::BinaryExpression(bin) => {
                let Expression::StaticMemberExpression(m) = &bin.left else {
                    return None;
                };
                (m, &bin.right)
            }
            _ => return None,
        };

    let Expression::Identifier(obj_ident) = &member.object else {
        return None;
    };
    let obj_name = obj_ident.name.as_str();
    let prop_name = member.property.name.as_str();
    let right_text = right.span().source_text(source_text);

    if obj_name == "module" && prop_name == "exports" {
        // module.exports = <right>
        match right {
            // `module.exports = function () {}` → `export default function () {}`
            Expression::FunctionExpression(func) => {
                let asterisk = if func.r#async { "async " } else { "" };
                let body_text = func.span().source_text(source_text);
                // Re-wrap as default-exported function declaration.
                return Some((
                    format!("{asterisk}export default {body_text};"),
                    ExportKind::Default,
                ));
            }
            // `module.exports = class {}` → `export default class {}`
            Expression::ClassExpression(cls) => {
                let body_text = cls.span().source_text(source_text);
                return Some((format!("export default {body_text};"), ExportKind::Default));
            }
            // `module.exports = <expr>` → `export default <expr>`
            _ => {
                return Some((format!("export default {right_text};"), ExportKind::Default));
            }
        }
    }

    if obj_name == "exports" {
        // exports.foo = <right>
        match right {
            // `exports.foo = <identifier>` → `export { <identifier> as <prop> };`
            // (or `export { <identifier> };` when `prop == identifier`)
            Expression::Identifier(ident) => {
                let local = ident.name.as_str();
                let specifier = if local == prop_name {
                    local.to_string()
                } else {
                    format!("{local} as {prop_name}")
                };
                return Some((format!("export {{ {specifier} }};"), ExportKind::Normal));
            }
            Expression::FunctionExpression(func) => {
                // `exports.foo = function() {}` → `export function foo() {}`
                let body_text = func.span().source_text(source_text);
                let name = prop_name;
                // Strip a leading `function ` and insert the name, or just prefix.
                let decl = inject_function_name(body_text, name);
                return Some((format!("export {decl};"), ExportKind::Normal));
            }
            Expression::ClassExpression(cls) => {
                // `exports.foo = class {}` → `export class foo {}`
                let body_text = cls.span().source_text(source_text);
                let name = prop_name;
                let decl = inject_class_name(body_text, name);
                return Some((format!("export {decl};"), ExportKind::Normal));
            }
            // `exports.foo = <expr>` → `export const foo = <expr>;`
            _ => {
                return Some((
                    format!("export const {prop_name} = {right_text};"),
                    ExportKind::Normal,
                ));
            }
        }
    }

    None
}

/// Inject a name into a function expression source text.
/// `function() {}` → `function name() {}`
fn inject_function_name(func_text: &str, name: &str) -> String {
    if let Some(pos) = func_text.find("function") {
        let after = &func_text[pos + "function".len()..];
        format!("{}function {name}{}", &func_text[..pos], after)
    } else {
        func_text.to_string()
    }
}

/// Inject a name into a class expression source text.
/// `class {}` → `class name {}`
fn inject_class_name(cls_text: &str, name: &str) -> String {
    if let Some(pos) = cls_text.find("class") {
        let after = &cls_text[pos + "class".len()..];
        format!("{}class {name}{}", &cls_text[..pos], after)
    } else {
        cls_text.to_string()
    }
}

/// Classify a top-level statement for the exports handler.
///
/// Returns `(replacement_text, kind)` where `kind` indicates whether the
/// statement should be hoisted to the end (default export) or kept in place.
fn classify_statement(stmt: &Statement<'_>, source_text: &str) -> (String, ExportKind) {
    match stmt {
        Statement::VariableDeclaration(var_decl) => {
            if let Some((text, kind)) = try_exports_var_assignment(var_decl, source_text) {
                return (text, kind);
            }
        }
        Statement::ExpressionStatement(expr_stmt) => {
            if let Some((text, kind)) = try_exports_expr_statement(expr_stmt, source_text) {
                return (text, kind);
            }
        }
        _ => {}
    }

    // Keep the original source text.
    let text = stmt.span().source_text(source_text).to_string();
    (text, ExportKind::Normal)
}

/// Convert `exports.x` / `module.exports` patterns into ESM `export`
/// declarations, hoisting default exports to the end of the file.
///
/// Mirrors `commonjsExportsHandler` from `commonjs_handler.ts`.
fn commonjs_exports_handler(dep: &DepsFile) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let source_text = program.source_text;

        let mut normal: Vec<String> = Vec::new();
        let mut default_stmts: Vec<String> = Vec::new();

        for stmt in &program.body {
            let (text, kind) = classify_statement(stmt, source_text);
            match kind {
                ExportKind::Default => default_stmts.push(text),
                ExportKind::Normal => normal.push(text),
            }
        }

        // Default statements go to the end, mirroring the TS post-visitor
        // `factory.updateSourceFile(visited, [...nonDefault, ...default])`.
        normal.extend(default_stmts);
        normal.join("\n")
    })
}

// ---------------------------------------------------------------------------
// 3. Public entry point
// ---------------------------------------------------------------------------

/// Convert CommonJS dependency files to ESM.
///
/// Mirrors `commonjsHandler` from `commonjs_handler.ts`:
/// 1. Run the exports handler on each CJS file.
/// 2. Run the imports handler on each CJS file.
/// 3. Flip `module_type` to `Esm` for every processed file.
///
/// Non-CJS files are passed through unchanged.
pub fn cjs_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    // Phase 1: exports
    let mut phase1 = Vec::with_capacity(deps.len());
    for dep in &deps {
        if is_commonjs_js_or_cjs(dep) {
            let content = strip_empty_semicolon_lines(&commonjs_exports_handler(dep));
            let (file, file_ext) = if dep.file_ext == ValidExts::Cjs {
                (rename_cjs_to_js(&dep.file), ValidExts::Js)
            } else {
                (dep.file.clone(), dep.file_ext)
            };
            phase1.push(DepsFile {
                file,
                content,
                bytes: dep.bytes,
                module_type: ModuleType::Esm,
                file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            });
        } else {
            phase1.push(dep.clone());
        }
    }

    // Phase 2: imports
    let mut phase2 = Vec::with_capacity(phase1.len());
    for dep in &phase1 {
        // After phase 1 the module_type is already Esm, but the file still
        // uses CJS require() syntax — detect by re-parsing.
        if (dep.file_ext == ValidExts::Js || dep.file_ext == ValidExts::Cjs)
            && has_require_call(&dep.content)
        {
            let content = strip_empty_semicolon_lines(&commonjs_imports_handler(dep));
            let (file, file_ext) = if dep.file_ext == ValidExts::Cjs {
                (rename_cjs_to_js(&dep.file), ValidExts::Js)
            } else {
                (dep.file.clone(), dep.file_ext)
            };
            phase2.push(DepsFile {
                file,
                content,
                bytes: dep.bytes,
                module_type: ModuleType::Esm,
                file_ext,
                is_jsx: dep.is_jsx,
                is_entry: dep.is_entry,
            });
            continue;
        }
        phase2.push(dep.clone());
    }

    phase2
}

/// Replace a `.cjs` extension in a file path with `.js`.
fn rename_cjs_to_js(file: &str) -> String {
    if let Some(stem) = file.strip_suffix(".cjs") {
        format!("{stem}.js")
    } else {
        file.to_string()
    }
}

/// Quick check whether source text contains a top-level `require(` call.
fn has_require_call(content: &str) -> bool {
    use oxc::ast::ast::Expression;
    use oxc::ast_visit::Visit;

    struct RequireDetector {
        found: bool,
    }

    impl<'a> Visit<'a> for RequireDetector {
        fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'a>) {
            if let Expression::Identifier(ident) = &it.callee
                && ident.name.as_str() == "require"
            {
                self.found = true;
            }
            if !self.found {
                oxc::ast_visit::walk::walk_call_expression(self, it);
            }
        }
    }

    with_parsed_program("__probe.ts", content, |program| {
        let mut det = RequireDetector { found: false };
        det.visit_program(program);
        det.found
    })
}

// Spans are unused at the call sites above (we rely on `GetSpan::span()`),
// but oxc re-exports `Span` for downstream use.  Keep the import active so
// future AST-level replacements (à la `resolve_json.rs`) compile cleanly.
#[allow(dead_code)]
fn _span_type_marker() -> Span {
    Span::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cjs_dep(file: &str, content: &str) -> DepsFile {
        let ext = if file.ends_with(".cjs") {
            ValidExts::Cjs
        } else {
            ValidExts::Js
        };
        DepsFile {
            file: file.to_string(),
            content: content.to_string(),
            bytes: content.len(),
            module_type: ModuleType::Cjs,
            file_ext: ext,
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

    // --- is_commonjs_js_or_cjs ---

    #[test]
    fn test_is_commonjs_js_or_cjs_true_js() {
        let dep = make_cjs_dep("mod.js", "module.exports = {}");
        assert!(is_commonjs_js_or_cjs(&dep));
    }

    #[test]
    fn test_is_commonjs_js_or_cjs_true_cjs() {
        let dep = make_cjs_dep("mod.cjs", "module.exports = {}");
        assert!(is_commonjs_js_or_cjs(&dep));
    }

    #[test]
    fn test_is_commonjs_js_or_cjs_false_esm() {
        let dep = make_esm_dep("mod.ts", "export const x = 1;");
        assert!(!is_commonjs_js_or_cjs(&dep));
    }

    #[test]
    fn test_is_commonjs_js_or_cjs_false_cjs_with_ts_ext() {
        let dep = DepsFile {
            file: "mod.ts".to_string(),
            content: "module.exports = {}".to_string(),
            bytes: 20,
            module_type: ModuleType::Cjs,
            file_ext: ValidExts::Ts,
            is_jsx: false,
            is_entry: false,
        };
        assert!(!is_commonjs_js_or_cjs(&dep));
    }

    // --- strip_empty_semicolon_lines ---

    #[test]
    fn test_strip_empty_semicolon_lines_removes_semicolon_only() {
        let result = strip_empty_semicolon_lines("const x = 1;\n;\nconst y = 2;");
        assert!(!result.contains("\n;\n"));
        assert!(result.contains("const x = 1;"));
        assert!(result.contains("const y = 2;"));
    }

    #[test]
    fn test_strip_empty_semicolon_lines_removes_empty_lines() {
        let result = strip_empty_semicolon_lines("const x = 1;\n\n\nconst y = 2;");
        assert!(!result.contains("\n\n"));
    }

    #[test]
    fn test_strip_empty_semicolon_lines_removes_whitespace_only() {
        let result = strip_empty_semicolon_lines("const x = 1;\n   \nconst y = 2;");
        assert!(!result.contains("   "));
    }

    #[test]
    fn test_strip_empty_semicolon_lines_trims() {
        let result = strip_empty_semicolon_lines("  const x = 1;  ");
        assert_eq!(result, "const x = 1;");
    }

    #[test]
    fn test_strip_empty_semicolon_lines_keeps_code_with_semicolons() {
        let result = strip_empty_semicolon_lines("const x = 1;");
        assert_eq!(result, "const x = 1;");
    }

    // --- format_import_specifier ---

    #[test]
    fn test_format_import_specifier_same_name() {
        assert_eq!(format_import_specifier("foo", "foo"), "foo");
    }

    #[test]
    fn test_format_import_specifier_different_name() {
        assert_eq!(format_import_specifier("foo", "bar"), "foo as bar");
    }

    // --- has_require_call ---

    #[test]
    fn test_has_require_call_true() {
        assert!(has_require_call("const fs = require(\"fs\");"));
    }

    #[test]
    fn test_has_require_call_false() {
        assert!(!has_require_call("const x = 1;"));
    }

    #[test]
    fn test_has_require_call_false_for_other_function() {
        assert!(!has_require_call("const x = myFunc(\"fs\");"));
    }

    // --- commonjs_handler ---

    #[test]
    fn test_commonjs_handler_converts_module_exports() {
        let dep = make_cjs_dep("mod.js", "module.exports = { foo: 1 };");
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export default"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_commonjs_handler_converts_exports_property() {
        let dep = make_cjs_dep("mod.js", "exports.foo = 1;");
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("export"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_commonjs_handler_converts_require() {
        let dep = make_cjs_dep(
            "mod.js",
            "const fs = require(\"node:fs\");\nmodule.exports = { read: fs.readFileSync };",
        );
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("import"));
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_commonjs_handler_passes_through_esm() {
        let dep = make_esm_dep("mod.ts", "export const x = 1;");
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "export const x = 1;");
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_commonjs_handler_passes_through_json() {
        let dep = DepsFile {
            file: "data.json".to_string(),
            content: "{\"x\":1}".to_string(),
            bytes: 7,
            module_type: ModuleType::Json,
            file_ext: ValidExts::Json,
            is_jsx: false,
            is_entry: false,
        };
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "{\"x\":1}");
        assert_eq!(result[0].module_type, ModuleType::Json);
    }

    #[test]
    fn test_commonjs_handler_require_object_destructuring() {
        let dep = make_cjs_dep(
            "mod.js",
            "const { readFileSync } = require(\"fs\");\nmodule.exports = { readFileSync };",
        );
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("import"));
    }

    #[test]
    fn test_commonjs_handler_default_export_hoisted_to_end() {
        let dep = make_cjs_dep("mod.js", "module.exports = 42;\nexports.foo = 1;");
        let result = cjs_handler(vec![dep]);
        // Default export (module.exports = ...) should be after named export
        let default_pos = result[0].content.find("export default");
        let named_pos = result[0].content.find("export const foo");
        if let (Some(d), Some(n)) = (default_pos, named_pos) {
            assert!(d > n, "default export should be hoisted to end");
        }
    }

    #[test]
    fn test_commonjs_handler_converts_let_require() {
        // Bug fix: `let x = require("mod")` was not converted to ESM.
        let dep = make_cjs_dep(
            "mod.js",
            "let fs = require(\"node:fs\");\nmodule.exports = { read: fs };",
        );
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].content.contains("import"),
            "let require should be converted, got: {}",
            result[0].content
        );
        assert_eq!(result[0].module_type, ModuleType::Esm);
    }

    #[test]
    fn test_commonjs_handler_converts_var_require() {
        // Bug fix: `var x = require("mod")` was not converted to ESM.
        let dep = make_cjs_dep(
            "mod.js",
            "var fs = require(\"node:fs\");\nmodule.exports = { read: fs };",
        );
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].content.contains("import"),
            "var require should be converted, got: {}",
            result[0].content
        );
    }

    #[test]
    fn test_commonjs_handler_converts_let_require_destructuring() {
        // Bug fix: `let { a } = require("mod")` was not converted to ESM.
        let dep = make_cjs_dep(
            "mod.js",
            "let { readFileSync } = require(\"fs\");\nmodule.exports = { readFileSync };",
        );
        let result = cjs_handler(vec![dep]);
        assert_eq!(result.len(), 1);
        assert!(
            result[0].content.contains("import"),
            "let destructuring require should be converted, got: {}",
            result[0].content
        );
    }
}
