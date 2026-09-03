//! Helpers for [`super`] — the five individual checks.
//!
//! Each check walks the `dep_files` of a [`DependenciesTree`] using the oxc
//! AST (via [`crate::core::susee_utils::with_parsed_program`]) and produces a
//! [`CheckReport`]. The checks are deliberately read-only — they never mutate
//! the tree. Line/column positions are computed from byte offsets with
//! [`byte_offset_to_line_col`].
//!
//! ## Note on the oxc version
//!
//! `Cargo.toml` pins `oxc = "0.147.0"`. The AST field names used here
//! (`func.return_type`, `param.pattern`, `param.type_annotation`,
//! `declarator.type_annotation`, `class.id`, `func.id`) match the API already
//! used throughout `susee_compiler/dts.rs` and `susee_utils/mod.rs`.

use std::collections::HashMap;
use std::path::Path;

use oxc::ast::ast::{
    BindingPattern, CallExpression, Declaration, ExportDefaultDeclaration,
    ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, Program, Statement,
    VariableDeclaration,
};
use oxc::ast_visit::Visit;
use oxc::span::{GetSpan, Span};

use crate::types::{DepsFile, ModuleType};
use crate::utils::with_parsed_program;

// ---------------------------------------------------------------------------
// Public report types
// ---------------------------------------------------------------------------

/// The five check categories. Order here matches the spec order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    Duplicates,
    Anonymous,
    ExportDefault,
    UndefinedUsage,
}

impl CheckKind {
    /// Return a human-readable label for this check category (e.g. `"check:Duplicates"`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Duplicates => "check:Duplicates",
            Self::Anonymous => "check:Anonymous",
            Self::ExportDefault => "check:ExportDefault",
            Self::UndefinedUsage => "check:UndefinedUsage",
        }
    }
}

/// A single issue found by a check.
#[derive(Debug, Clone)]
pub struct CheckItem {
    /// One-line summary (e.g. `Duplicate "shared" in src/a.ts:1 and src/b.ts:1`).
    pub message: String,
    /// Extra detail lines printed indented under the message.
    pub details: Vec<String>,
}

/// The result of running one check.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// The category of check that produced this report.
    pub kind: CheckKind,
    /// The list of issues found; empty when no issues were detected.
    pub items: Vec<CheckItem>,
}

impl CheckReport {
    fn new(kind: CheckKind) -> Self {
        Self {
            kind,
            items: Vec::new(),
        }
    }

    /// Return `true` if this report contains any issues.
    pub fn has_issues(&self) -> bool {
        !self.items.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Convert a byte offset into a 1-based `(line, column)` pair.
///
/// `column` is also 1-based and counts characters (UTF-8 boundaries are
/// respected by scanning char-by-char up to the offset).
pub fn byte_offset_to_line_col(source: &str, offset: u32) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    let target = (offset as usize).min(source.len());
    for (i, ch) in source.char_indices() {
        if i >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Format a span as `file:line:column` using the file's source text.
#[allow(dead_code)]
fn fmt_loc(file: &str, source: &str, span: Span) -> String {
    let (line, col) = byte_offset_to_line_col(source, span.start);
    format!("{file}:{line}:{col}")
}

/// Return the file stem (basename without extension), mirroring the anonymous
/// hook's `file_stem`.
fn file_stem(file: &str) -> String {
    Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("mod")
        .to_string()
}

// ---------------------------------------------------------------------------
// Check 1 — Duplicated declarations
// ---------------------------------------------------------------------------

/// Detect top-level declaration names that appear in two or more `dep_files`.
///
/// Only true declarations are collected — import bindings are excluded so
/// that an `import { a } from './a'` in one file is not treated as a duplicate
/// of `export const a` in another file. This also covers CommonJS-style
/// imports (`const { Foo } = require('./foo')`, `import x = require('…')`)
/// which are imports, not declarations, even though they are syntactically
/// variable declarations. Each collision is reported with the declaration
/// name and every `(file, line:col)` location.
pub fn check_duplicates(dep_files: &[DepsFile]) -> CheckReport {
    let mut report = CheckReport::new(CheckKind::Duplicates);

    // name → Vec<(file, line, col)>
    let mut seen: HashMap<String, Vec<(String, usize, usize)>> = HashMap::new();

    for dep in dep_files {
        with_parsed_program(&dep.file, &dep.content, |program| {
            for (name, span) in collect_top_level_names(program) {
                let (line, col) = byte_offset_to_line_col(&dep.content, span.start);
                seen.entry(name)
                    .or_default()
                    .push((dep.file.clone(), line, col));
            }
        });
    }

    // Keep only names declared in 2+ files.
    let mut dupes: Vec<(String, Vec<(String, usize, usize)>)> = seen
        .into_iter()
        .filter(|(_, locs)| locs.len() > 1)
        .collect();
    dupes.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, locs) in dupes {
        report.items.push(CheckItem {
            message: format!(
                "Duplicate declaration \"{name}\" found in {} files",
                locs.len()
            ),
            details: {
                let mut d = vec![format!("  declaration: {name}")];
                for (f, l, c) in &locs {
                    d.push(format!("    at {f}:{l}:{c}"));
                }
                d.push(format!(
                    "  suggestion: rename one declaration to a unique name \
                     (e.g. {name}₂) before bundling"
                ));
                d
            },
        });
    }

    report
}

/// Collect `(name, span)` for every top-level declaration (excluding imports),
/// mirroring `collect_top_level_declaration_names` in `susee_utils`.
///
/// The following are **not** collected (they are imports, not declarations):
/// - `import … from "…"` (handled by oxc as `ImportDeclaration`, not matched here)
/// - `const x = require("…")` / `const { a, b } = require("…")` (CJS require)
/// - `const x = require("…").prop` (CJS require with member access)
/// - `import x = require("…")` (TS import-equals with external module reference)
fn collect_top_level_names(program: &Program<'_>) -> Vec<(String, Span)> {
    let mut out = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::VariableDeclaration(var) => {
                // Skip CJS require-imports: `const { Foo } = require("./foo")`
                // and `const x = require("./foo").prop`. These are imports,
                // not declarations, so they must not be treated as duplicates
                // of the exported name in the source module.
                if is_require_var(var) {
                    continue;
                }
                for decl in &var.declarations {
                    collect_binding_name(&decl.id, &mut out);
                }
            }
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    out.push((id.name.as_str().to_string(), id.span));
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    out.push((id.name.as_str().to_string(), id.span));
                }
            }
            Statement::TSTypeAliasDeclaration(t) => {
                out.push((t.id.name.as_str().to_string(), t.id.span));
            }
            Statement::TSInterfaceDeclaration(i) => {
                out.push((i.id.name.as_str().to_string(), i.id.span));
            }
            Statement::TSEnumDeclaration(e) => {
                out.push((e.id.name.as_str().to_string(), e.id.span));
            }
            // TS import-equals: `import x = require("…")` is an import, not a
            // declaration. Only the `ExternalModuleReference` variant is a
            // CJS require; `import x = foo.bar` is a type-only alias and is
            // also not a runtime declaration.
            Statement::TSImportEqualsDeclaration(_) => {}
            Statement::ExportDeclaration(exp) => {
                collect_decl_names_inner(&exp.declaration, &mut out);
            }
            _ => {}
        }
    }
    out
}

/// Check whether a `VariableDeclaration` is a CommonJS `require()` import:
/// `const x = require("…")`, `const { a, b } = require("…")`, or
/// `const x = require("…").prop`.
///
/// Mirrors the detection in `susee_tree/cjs_handler.rs::process_require_var`
/// and `susee_hooks/tree_hooks/remove.rs`.
fn is_require_var(var: &VariableDeclaration<'_>) -> bool {
    if var.declarations.len() != 1 {
        return false;
    }
    let Some(init) = &var.declarations[0].init else {
        return false;
    };
    is_require_expression(init)
}

/// `true` when `expr` is a `require("…")` call, possibly wrapped in a static
/// member access (`require("…").prop`).
fn is_require_expression(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::CallExpression(call) => is_require_call(call),
        Expression::StaticMemberExpression(member) => is_require_expression(&member.object),
        _ => false,
    }
}

/// `true` when `call` is `require("…")` — callee is the identifier `require`.
fn is_require_call(call: &CallExpression<'_>) -> bool {
    matches!(&call.callee, Expression::Identifier(ident) if ident.name.as_str() == "require")
}

fn collect_decl_names_inner(decl: &Declaration<'_>, out: &mut Vec<(String, Span)>) {
    match decl {
        Declaration::VariableDeclaration(var) => {
            // Skip CJS require-imports even under `export`.
            if is_require_var(var) {
                return;
            }
            for d in &var.declarations {
                collect_binding_name(&d.id, out);
            }
        }
        Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                out.push((id.name.as_str().to_string(), id.span));
            }
        }
        Declaration::ClassDeclaration(cls) => {
            if let Some(id) = &cls.id {
                out.push((id.name.as_str().to_string(), id.span));
            }
        }
        Declaration::TSTypeAliasDeclaration(t) => {
            out.push((t.id.name.as_str().to_string(), t.id.span));
        }
        Declaration::TSInterfaceDeclaration(i) => {
            out.push((i.id.name.as_str().to_string(), i.id.span));
        }
        Declaration::TSEnumDeclaration(e) => {
            out.push((e.id.name.as_str().to_string(), e.id.span));
        }
        _ => {}
    }
}

fn collect_binding_name(pattern: &BindingPattern<'_>, out: &mut Vec<(String, Span)>) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => {
            out.push((id.name.as_str().to_string(), id.span));
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_name(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_name(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_binding_name(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_name(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => {
            collect_binding_name(&assign.left, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Check 2 — Anonymous imports/exports
// ---------------------------------------------------------------------------

/// Detect anonymous default exports (functions/classes without a name, or
/// expression default exports) that are imported by another file, and report
/// every usage of the imported binding.
pub fn check_anonymous(dep_files: &[DepsFile]) -> CheckReport {
    let mut report = CheckReport::new(CheckKind::Anonymous);

    // 1. Find files that export an anonymous default.
    //    Map: exporting file stem → (file, export line:col, kind label).
    let mut anon_exports: HashMap<String, (String, usize, usize, String)> = HashMap::new();
    for dep in dep_files {
        with_parsed_program(&dep.file, &dep.content, |program| {
            for stmt in &program.body {
                if let Statement::ExportDefaultDeclaration(decl) = stmt {
                    if let Some(label) = anonymous_kind_label(decl) {
                        let (line, col) = byte_offset_to_line_col(&dep.content, decl.span.start);
                        let stem = file_stem(&dep.file);
                        anon_exports.insert(stem, (dep.file.clone(), line, col, label));
                    }
                }
            }
        });
    }

    if anon_exports.is_empty() {
        return report;
    }

    // 2. Find default imports whose source file stem matches an anon export,
    //    and collect usage spans of the imported binding in that file.
    for dep in dep_files {
        with_parsed_program(&dep.file, &dep.content, |program| {
            let mut local_binding: Option<(String, String)> = None; // (local, source_file_of_export)

            for stmt in &program.body {
                if let Statement::ImportDeclaration(imp) = stmt {
                    let source = imp.source.value.as_str();
                    let stem = file_stem(source);
                    if let Some((exp_file, exp_line, exp_col, label)) = anon_exports.get(&stem) {
                        if let Some(specs) = &imp.specifiers {
                            for spec in specs {
                                if let ImportDeclarationSpecifier::ImportDefaultSpecifier(d) = spec
                                {
                                    let local = d.local.name.as_str().to_string();
                                    let imp_line =
                                        byte_offset_to_line_col(&dep.content, d.local.span.start).0;
                                    local_binding = Some((local.clone(), exp_file.clone()));

                                    report.items.push(CheckItem {
                                        message: format!(
                                            "Anonymous default export in {exp_file}:{exp_line}:{exp_col} \
                                             ({label}) imported by {file}:{line}",
                                            file = dep.file,
                                            line = imp_line
                                        ),
                                        details: vec![
                                            format!(
                                                "  export:     {exp_file}:{exp_line}:{exp_col}  \
                                                 (`export default {label}`)"
                                            ),
                                            format!(
                                                "  import:     {file}:{line}  (`import {local} from \"{source}\"`)",
                                                file = dep.file,
                                                line = imp_line,
                                                local = local,
                                                source = source,
                                            ),
                                        ],
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // 3. Collect usages of the imported binding (references after the
            //    import). We walk identifier references whose name matches the
            //    local binding and are not the import declaration itself.
            if let Some((local, _)) = &local_binding {
                let mut usages = Vec::new();
                let mut visitor = UsageCollector {
                    target: local,
                    source: &dep.content,
                    usages: &mut usages,
                };
                visitor.visit_program(program);

                let mut detail_lines = vec!["  usages:".to_string()];
                for (line, col) in &usages {
                    detail_lines.push(format!("    {file}:{line}:{col}", file = dep.file));
                }
                detail_lines.push(format!(
                    "  suggestion: use a named export (e.g. `export function {name}() {{…}}`) \
                     and import it by name",
                    name = local
                ));
                if !usages.is_empty() {
                    // Attach usages to the last reported item for this file.
                    if let Some(item) = report.items.last_mut() {
                        item.details.extend(detail_lines);
                    }
                }
            }
        });
    }

    report
}

/// Return a human label for an anonymous `ExportDefaultDeclaration`, or
/// `None` if the declaration is named (not anonymous).
fn anonymous_kind_label<'a>(decl: &ExportDefaultDeclaration<'a>) -> Option<String> {
    match &decl.declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
            f.id.is_none().then(|| "function".to_string())
        }
        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
            c.id.is_none().then(|| "class".to_string())
        }
        ExportDefaultDeclarationKind::ArrowFunctionExpression(_) => Some("arrow".to_string()),
        ExportDefaultDeclarationKind::ObjectExpression(_) => Some("object".to_string()),
        ExportDefaultDeclarationKind::ArrayExpression(_) => Some("array".to_string()),
        ExportDefaultDeclarationKind::StringLiteral(_) => Some("string".to_string()),
        ExportDefaultDeclarationKind::NumericLiteral(_) => Some("number".to_string()),
        ExportDefaultDeclarationKind::BooleanLiteral(_) => Some("boolean".to_string()),
        ExportDefaultDeclarationKind::NullLiteral(_) => Some("null".to_string()),
        ExportDefaultDeclarationKind::TemplateLiteral(_) => Some("template".to_string()),
        ExportDefaultDeclarationKind::FunctionExpression(_) => Some("function".to_string()),
        ExportDefaultDeclarationKind::ClassExpression(_) => Some("class".to_string()),
        _ => None,
    }
}

/// Collect line:col positions of identifier references matching `target`.
struct UsageCollector<'a> {
    target: &'a str,
    source: &'a str,
    usages: &'a mut Vec<(usize, usize)>,
}

impl<'a, 'ast> Visit<'ast> for UsageCollector<'a> {
    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'ast>) {
        if it.name.as_str() == self.target {
            let (l, c) = byte_offset_to_line_col(self.source, it.span.start);
            self.usages.push((l, c));
        }
    }
}

// ---------------------------------------------------------------------------
// Check 3 — export default usage
// ---------------------------------------------------------------------------

/// Report every `export default` statement (named or anonymous) and suggest
/// converting to a named export.
pub fn check_default_exports(dep_files: &[DepsFile]) -> CheckReport {
    let mut report = CheckReport::new(CheckKind::ExportDefault);

    for dep in dep_files {
        with_parsed_program(&dep.file, &dep.content, |program| {
            for stmt in &program.body {
                if let Statement::ExportDefaultDeclaration(decl) = stmt {
                    let (line, col) = byte_offset_to_line_col(&dep.content, decl.span.start);
                    let kind = export_default_kind_label(decl);
                    report.items.push(CheckItem {
                        message: format!(
                            "`export default` found in {file}:{line}:{col} ({kind})",
                            file = dep.file,
                            line = line,
                            col = col,
                            kind = kind
                        ),
                        details: vec![
                            format!(
                                "  location: {file}:{line}:{col}",
                                file = dep.file,
                                line = line,
                                col = col
                            ),
                            format!(
                                "  suggestion: use a named export instead, e.g.\n\
                                 \t    export function foo() {{…}}   // then `import {{ foo }} from \"…\"`"
                            ),
                        ],
                    });
                }
            }
        });
    }

    report
}

/// A short label describing the export-default declaration kind.
fn export_default_kind_label(decl: &ExportDefaultDeclaration<'_>) -> &'static str {
    match &decl.declaration {
        ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
            if f.id.is_some() {
                "named function"
            } else {
                "anonymous function"
            }
        }
        ExportDefaultDeclarationKind::ClassDeclaration(c) => {
            if c.id.is_some() {
                "named class"
            } else {
                "anonymous class"
            }
        }
        ExportDefaultDeclarationKind::ArrowFunctionExpression(_) => "arrow expression",
        ExportDefaultDeclarationKind::ObjectExpression(_) => "object expression",
        ExportDefaultDeclarationKind::ArrayExpression(_) => "array expression",
        ExportDefaultDeclarationKind::StringLiteral(_) => "string literal",
        ExportDefaultDeclarationKind::NumericLiteral(_) => "numeric literal",
        ExportDefaultDeclarationKind::BooleanLiteral(_) => "boolean literal",
        ExportDefaultDeclarationKind::NullLiteral(_) => "null literal",
        ExportDefaultDeclarationKind::TemplateLiteral(_) => "template literal",
        ExportDefaultDeclarationKind::FunctionExpression(_) => "function expression",
        ExportDefaultDeclarationKind::ClassExpression(_) => "class expression",
        ExportDefaultDeclarationKind::Identifier(_) => "identifier",
        ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => "interface",
        _ => "unknown",
    }
}
// ---------------------------------------------------------------------------
// Check 5 — Undefined identifier usage
// ---------------------------------------------------------------------------

/// Detect identifier references that are never declared or imported in their
/// file and are not known globals/built-ins.
pub fn check_undefined_usage(dep_files: &[DepsFile]) -> CheckReport {
    let mut report = CheckReport::new(CheckKind::UndefinedUsage);

    for dep in dep_files {
        let undefined = with_parsed_program(&dep.file, &dep.content, |program| {
            find_undefined_references(program, dep)
        });

        if undefined.is_empty() {
            continue;
        }

        let mut sorted = undefined;
        sorted.sort_by_key(|(_, l, c)| (*l, *c));
        sorted.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);

        let mut details = vec!["  undefined references:".to_string()];
        for (name, line, col) in &sorted {
            details.push(format!(
                "    {name}  at {file}:{line}:{col}",
                file = dep.file
            ));
        }
        details.push(format!(
            "  suggestion: declare, import, or alias `{name}` before using it",
            name = sorted.first().map(|(n, _, _)| n.as_str()).unwrap_or("it")
        ));
        report.items.push(CheckItem {
            message: format!(
                "{} undefined identifier(s) in {file}",
                sorted.len(),
                file = dep.file
            ),
            details,
        });
    }

    report
}

/// A set of identifiers that are always considered defined (JS globals,
/// Node.js globals, TS utility types, and common ambient declarations).
fn known_globals() -> &'static [&'static str] {
    &[
        // JS language globals
        "undefined",
        "null",
        "true",
        "false",
        "NaN",
        "Infinity",
        "globalThis",
        "this",
        "arguments",
        "super",
        "new",
        "console",
        "Math",
        "JSON",
        "Object",
        "Array",
        "String",
        "Number",
        "Boolean",
        "Symbol",
        "BigInt",
        "Date",
        "RegExp",
        "Error",
        "Function",
        "Promise",
        "Proxy",
        "Reflect",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "ArrayBuffer",
        "DataView",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
        "Symbol",
        "Intl",
        "decodeURIComponent",
        "decodeURI",
        "encodeURIComponent",
        "encodeURI",
        "eval",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
        "structuredClone",
        "queueMicrotask",
        "atob",
        "btoa",
        "fetch",
        "Request",
        "Response",
        "Headers",
        "URL",
        "URLSearchParams",
        "setTimeout",
        "setInterval",
        "clearTimeout",
        "clearInterval",
        "setImmediate",
        "clearImmediate",
        "AbortController",
        "AbortSignal",
        "Event",
        "EventTarget",
        "CustomEvent",
        "MessageEvent",
        "WebSocket",
        "localStorage",
        "sessionStorage",
        "document",
        "window",
        "navigator",
        "location",
        "history",
        "screen",
        "alert",
        "confirm",
        "prompt",
        "addEventListener",
        "removeEventListener",
        // Node.js globals
        "process",
        "Buffer",
        "global",
        "require",
        "module",
        "exports",
        "__dirname",
        "__filename",
        "NodeJS",
        "Buffer",
        // TS utility/ambient types
        "any",
        "unknown",
        "never",
        "void",
        "string",
        "number",
        "boolean",
        "object",
        "bigint",
        "symbol",
        "Array",
        "ReadonlyArray",
        "Record",
        "Partial",
        "Required",
        "Readonly",
        "Pick",
        "Omit",
        "Exclude",
        "Extract",
        "NonNullable",
        "Parameters",
        "ReturnType",
        "InstanceType",
        "ConstructorParameters",
        "Awaited",
        "Promise",
        "ReadonlyMap",
        "ReadonlySet",
        "Iterable",
        "Iterator",
        "Symbol",
        // Common ambient values
        "it",
        "describe",
        "test",
        "expect",
        "beforeEach",
        "afterEach",
        "beforeAll",
        "afterAll",
        "jest",
        "vi",
        "assert",
        "console",
        // added by me
        "PromiseFulfilledResult",
        "PromiseRejectedResult",
    ]
}

/// Find `(name, line, col)` for identifier references that are not bound in
/// the file's scopes and are not known globals.
fn find_undefined_references(program: &Program<'_>, dep: &DepsFile) -> Vec<(String, usize, usize)> {
    use oxc::semantic::SemanticBuilder;

    let semantic = SemanticBuilder::new().with_build_nodes(true).build(program);
    let scoping = semantic.semantic.scoping();

    let globals = known_globals();
    let mut out = Vec::new();

    // `root_unresolved_references` maps identifier name -> Vec<ReferenceId>.
    // These are references that could not be resolved to any binding in the
    // file (neither a local declaration nor an import).
    let unresolved = scoping.root_unresolved_references();
    for (name, ref_ids) in unresolved.iter() {
        let name = name.to_string();
        if globals.contains(&name.as_str()) {
            continue;
        }
        for ref_id in ref_ids {
            let reference = scoping.get_reference(*ref_id);
            // Skip type-level references (e.g. `const` in `as const`,
            // type names in type annotations).  The undefined-usage check is
            // concerned with value-level identifiers only.
            if reference.flags().is_type() {
                continue;
            }
            let node_id = reference.node_id();
            let node = semantic.semantic.nodes().get_node(node_id);
            let span = node.kind().span();
            let (line, col) = byte_offset_to_line_col(&dep.content, span.start);
            out.push((name.clone(), line, col));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Re-exports for the parent module
// ---------------------------------------------------------------------------

#[allow(unused_imports)]
use ModuleType as _ModuleType; // keep the import alive for is_ts_file context
