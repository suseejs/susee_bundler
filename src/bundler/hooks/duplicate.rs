//! Duplicate-declaration handler (proactive rename).
//!
//! This is the bundle-path counterpart of `check_duplicates` from
//! [`crate::tree::checks::helpers`]. Instead of *reporting* cross-file
//! duplicate top-level declarations and exiting, this handler **renames**
//! them to unique identifiers so the merged bundle has no collisions.
//!
//! ## What it does
//!
//! 1. Collect every top-level declaration name from all non-entry files
//!    (functions, classes, variables, type aliases, interfaces, enums —
//!    excluding import bindings and CJS `require()` vars).
//! 2. Find names that appear in **2 or more** files.
//! 3. For each duplicate name, assign a unique replacement using the
//!    [`UniqueName`] generator with the `"Duplicates"` category, producing
//!    names like `_uhelper$1`, `_uhelper$2`.
//! 4. In each file that has a duplicate, rename the **declaration** and
//!    **all references** to the new name.
//! 5. Also rename **import bindings** that import the duplicated name from
//!    an affected file, plus all references to those bindings.
//!
//! ## What it does NOT touch
//!
//! - **Entry files** — their declarations are left unchanged because the
//!   entry is the public API surface.
//! - **Exported declarations** (those with `export` keyword) — these are
//!   handled by [`export_default_handler`] (for default exports) and
//!   `remove_handler` (for named exports). Renaming exported names would
//!   break the public API contract.
//! - **Anonymous / export-default** — handled by [`anonymous_handler`] and
//!   [`export_default_handler`].
//!
//! ## Relationship to the old `check_duplicates`
//!
//! | Aspect | `check_duplicates` (lint) | `duplicate_handler` (bundle) |
//! |--------|---------------------------|-------------------------------|
//! | When | `susee_lint` command | During `bundler()` |
//! | Action | Report + exit | Rename + continue |
//! | Output | Diagnostics | Modified `DepsFile` content |
//! | Entry files | Reported | Skipped |
//! | Exported decls | Reported | Skipped (public API) |
//!
//! [`UniqueName`]: crate::unique_name::UniqueName
//! [`export_default_handler`]: super::export_default::export_default_handler
//! [`anonymous_handler`]: super::anonymous::anonymous_handler

use std::collections::HashMap;
use std::path::Path;

use oxc::ast::ast::{
    BindingPattern, Expression, ImportDeclarationSpecifier, ModuleExportName, Program, Statement,
    VariableDeclaration,
};
use oxc::ast_visit::Visit;
use oxc::span::Span;

use crate::types::DepsFile;
use crate::unique_name::{UniqueName, sigil};
use crate::utils::with_parsed_program;

use super::export_default::import_source;

/// The category key for duplicate-declaration renames, mirroring the TS
/// implementation (`uniqueName.setPrefix({ key: "Duplicates", ... })`).
const DUPLICATES_PREFIX_KEY: &str = "Duplicates";

// ---------------------------------------------------------------------------
// Data: name maps
// ---------------------------------------------------------------------------

/// A rename mapping for a single declaration in a single file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DuplicateRename {
    /// The original declaration name (e.g. `helper`).
    base: String,
    /// The generated unique name (e.g. `_uhelper$1`).
    new_name: String,
}

/// Mutable state for the duplicate handler.
struct DuplicateState {
    unique: UniqueName,
    /// Map: (file, original_name) → generated unique name.
    /// One entry per file that has a duplicated declaration.
    renames: HashMap<(String, String), String>,
    /// Set of base names that are duplicated across 2+ files.
    duplicated_names: std::collections::HashSet<String>,
    /// Map: (file, local_name) → new_name for import bindings that need
    /// renaming because they import a duplicated declaration.
    import_renames: Vec<ImportRenameEntry>,
}

#[derive(Debug, Clone)]
struct ImportRenameEntry {
    /// The file that contains the import.
    file: String,
    /// The original local binding name.
    base: String,
    /// The new name to use.
    new_name: String,
}

impl DuplicateState {
    fn new() -> Self {
        let mut unique = UniqueName::new();
        unique.set_prefix(DUPLICATES_PREFIX_KEY, sigil::DUPLICATE);
        Self {
            unique,
            renames: HashMap::new(),
            duplicated_names: std::collections::HashSet::new(),
            import_renames: Vec::new(),
        }
    }

    /// Look up the rename for a given (file, name) pair.
    fn get_rename(&self, file: &str, base: &str) -> Option<&str> {
        self.renames
            .get(&(file.to_string(), base.to_string()))
            .map(|s| s.as_str())
    }

    /// Check if a name is duplicated across files.
    /// Check if a name is duplicated across files.
    #[allow(dead_code)]
    fn is_duplicate(&self, base: &str) -> bool {
        self.duplicated_names.contains(base)
    }

    /// Look up the import rename for a given (file, base).
    fn find_import_rename(&self, file: &str, base: &str) -> Option<&str> {
        self.import_renames
            .iter()
            .find(|m| m.file == file && m.base == base)
            .map(|m| m.new_name.as_str())
    }
}

// ---------------------------------------------------------------------------
// Phase 1 — Collect duplicates
// ---------------------------------------------------------------------------

/// Collect all top-level declaration names from non-entry files and find
/// names that appear in 2+ files.
///
/// Excludes:
/// - Import bindings (ESM imports, CJS `require()` vars, TS import-equals)
/// - Exported declarations (those with `export` keyword) — these are the
///   public API and are handled by `export_default_handler` / `remove_handler`.
fn collect_duplicates(deps: &[DepsFile], state: &mut DuplicateState) {
    // name → list of files that declare it
    let mut seen: HashMap<String, Vec<String>> = HashMap::new();

    for dep in deps {
        if dep.is_entry {
            continue;
        }

        with_parsed_program(&dep.file, &dep.content, |program| {
            for (name, _span) in collect_non_exported_top_level_names(program) {
                seen.entry(name).or_default().push(dep.file.clone());
            }
        });
    }

    // For names appearing in 2+ files, assign a unique name per file.
    // Each file gets its own distinct `_u<name>$<n>` so there's no collision.
    for (name, files) in &seen {
        if files.len() >= 2 {
            state.duplicated_names.insert(name.clone());
            for file in files {
                let new_name = state.unique.get_name(DUPLICATES_PREFIX_KEY, name);
                state.renames.insert((file.clone(), name.clone()), new_name);
            }
        }
    }
}

/// Collect `(name, span)` for every non-exported top-level declaration.
///
/// This is similar to `collect_top_level_names` in `checks/helpers.rs` but
/// **excludes** exported declarations (those inside `ExportDeclaration` or
/// `ExportDefaultDeclaration`). Only standalone declarations are collected,
/// because exported ones are the public API and handled separately.
fn collect_non_exported_top_level_names(program: &Program<'_>) -> Vec<(String, Span)> {
    let mut out = Vec::new();
    for stmt in &program.body {
        match stmt {
            // Variable declarations — skip CJS require-imports.
            Statement::VariableDeclaration(var) => {
                if is_require_var(var) {
                    continue;
                }
                for decl in &var.declarations {
                    collect_binding_name(&decl.id, &mut out);
                }
            }
            // Function / class declarations.
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
            // TS declarations.
            Statement::TSTypeAliasDeclaration(t) => {
                out.push((t.id.name.as_str().to_string(), t.id.span));
            }
            Statement::TSInterfaceDeclaration(i) => {
                out.push((i.id.name.as_str().to_string(), i.id.span));
            }
            Statement::TSEnumDeclaration(e) => {
                out.push((e.id.name.as_str().to_string(), e.id.span));
            }
            // Skip: ImportDeclaration, ExportDeclaration,
            // ExportDefaultDeclaration, TSImportEqualsDeclaration, etc.
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Phase 2 — Rename declarations + references in each file
// ---------------------------------------------------------------------------

/// Collect byte-offset spans to replace for the duplicate local handler.
///
/// This rewrites the declaration name and all references to it within the
/// file. For each duplicate name in `state.renames`, we find the declaration
/// and all identifier references.
fn collect_duplicate_local_spans(
    program: &Program<'_>,
    file: &str,
    state: &DuplicateState,
) -> Vec<(usize, usize, String)> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        match stmt {
            // --- Declarations to rename ---
            Statement::VariableDeclaration(var) => {
                if is_require_var(var) {
                    continue;
                }
                for decl in &var.declarations {
                    if let BindingPattern::BindingIdentifier(id) = &decl.id {
                        if let Some(new_name) = state.get_rename(file, id.name.as_str()) {
                            spans.push((
                                id.span.start as usize,
                                id.span.end as usize,
                                new_name.to_string(),
                            ));
                        }
                    }
                }
            }
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id
                    && let Some(new_name) = state.get_rename(file, id.name.as_str())
                {
                    spans.push((
                        id.span.start as usize,
                        id.span.end as usize,
                        new_name.to_string(),
                    ));
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id
                    && let Some(new_name) = state.get_rename(file, id.name.as_str())
                {
                    spans.push((
                        id.span.start as usize,
                        id.span.end as usize,
                        new_name.to_string(),
                    ));
                }
            }
            Statement::TSTypeAliasDeclaration(t) => {
                if let Some(new_name) = state.get_rename(file, t.id.name.as_str()) {
                    spans.push((
                        t.id.span.start as usize,
                        t.id.span.end as usize,
                        new_name.to_string(),
                    ));
                }
            }
            Statement::TSInterfaceDeclaration(i) => {
                if let Some(new_name) = state.get_rename(file, i.id.name.as_str()) {
                    spans.push((
                        i.id.span.start as usize,
                        i.id.span.end as usize,
                        new_name.to_string(),
                    ));
                }
            }
            Statement::TSEnumDeclaration(e) => {
                if let Some(new_name) = state.get_rename(file, e.id.name.as_str()) {
                    spans.push((
                        e.id.span.start as usize,
                        e.id.span.end as usize,
                        new_name.to_string(),
                    ));
                }
            }
            // --- Imports to rename ---
            // If a file imports a name from another file that has a duplicate
            // declaration, we need to rename the import binding too.
            // We handle this in phase 3 (import handler) to avoid double-
            // counting spans here.
            _ => {}
        }
    }

    // Collect all identifier references that match any renamed name.
    let mut ref_collector = DuplicateRefCollector {
        state,
        file,
        spans: Vec::new(),
    };
    ref_collector.visit_program(program);

    for (span, name) in &ref_collector.spans {
        spans.push((span.start as usize, span.end as usize, name.clone()));
    }

    // Sort right-to-left and dedup.
    spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    spans.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    spans
}

/// Collect all identifier reference spans matching any renamed name.
struct DuplicateRefCollector<'a, 'b> {
    state: &'a DuplicateState,
    file: &'b str,
    spans: Vec<(Span, String)>,
}

impl<'a, 'b, 'ast> Visit<'ast> for DuplicateRefCollector<'a, 'b> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self.state.get_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.object
            && let Some(new_name) = self.state.get_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self.state.get_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_new_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'ast>) {
        if let Some(new_name) = self.state.get_rename(self.file, it.name.as_str()) {
            self.spans.push((it.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_identifier_reference(self, it);
    }
}

/// Process a single file for duplicate local renames (declaration + references).
fn duplicate_local_handler(dep: &DepsFile, state: &DuplicateState) -> String {
    if dep.is_entry {
        return dep.content.clone();
    }

    with_parsed_program(&dep.file, &dep.content, |program| {
        let spans = collect_duplicate_local_spans(program, &dep.file, state);

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
// Phase 3 — Rename import bindings + references in importing files
// ---------------------------------------------------------------------------

/// Collect import mappings for this file.
///
/// If a file imports a named binding from another file that has a duplicate
/// declaration, we need to rename the import binding and all its references.
/// The new name is looked up from the **source file's** rename entry.
fn collect_import_renames(dep: &DepsFile, state: &mut DuplicateState, deps: &[DepsFile]) {
    with_parsed_program(&dep.file, &dep.content, |program| {
        for stmt in &program.body {
            if let Statement::ImportDeclaration(import_decl) = stmt {
                let source = import_source(import_decl);
                // Resolve the import source to a file path in dep_files.
                let source_file = resolve_import_source(&source, &dep.file, deps);
                if let Some(specifiers) = &import_decl.specifiers {
                    for spec in specifiers {
                        match spec {
                            ImportDeclarationSpecifier::ImportSpecifier(imp_spec) => {
                                // import { helper } from "./mod"
                                // or import { helper as h } from "./mod"
                                let imported_name = match &imp_spec.imported {
                                    ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                                    ModuleExportName::IdentifierName(id) => id.name.as_str(),
                                    ModuleExportName::StringLiteral(_) => continue,
                                };
                                // Only rename if the imported name is a
                                // duplicate and the source file has a rename
                                // for it.
                                if let Some(src_file) = &source_file {
                                    if let Some(new_name) =
                                        state.get_rename(src_file, imported_name)
                                    {
                                        let local_name = imp_spec.local.name.as_str().to_string();
                                        state.import_renames.push(ImportRenameEntry {
                                            file: dep.file.clone(),
                                            base: local_name,
                                            new_name: new_name.to_string(),
                                        });
                                    }
                                }
                            }
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                                // Default imports are handled by
                                // export_default_handler, not here.
                            }
                            ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                                // Namespace imports (`import * as ns`) are
                                // not affected by declaration renames.
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Process a single file for import renames + reference updates.
fn duplicate_import_handler(dep: &DepsFile, state: &DuplicateState, deps: &[DepsFile]) -> String {
    with_parsed_program(&dep.file, &dep.content, |program| {
        let mut import_spans: Vec<(usize, usize, String)> = Vec::new();

        // Collect import binding spans to rename.
        for stmt in &program.body {
            if let Statement::ImportDeclaration(import_decl) = stmt {
                let source = import_source(import_decl);
                let source_file = resolve_import_source(&source, &dep.file, deps);
                if let Some(specifiers) = &import_decl.specifiers {
                    for spec in specifiers {
                        if let ImportDeclarationSpecifier::ImportSpecifier(imp_spec) = spec {
                            let imported_name = match &imp_spec.imported {
                                ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                                ModuleExportName::IdentifierName(id) => id.name.as_str(),
                                ModuleExportName::StringLiteral(_) => continue,
                            };
                            if let Some(src_file) = &source_file {
                                if let Some(new_name) = state.get_rename(src_file, imported_name) {
                                    import_spans.push((
                                        imp_spec.local.span.start as usize,
                                        imp_spec.local.span.end as usize,
                                        new_name.to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Collect all reference spans for renamed import bindings.
        let mut ref_collector = DuplicateImportRefCollector {
            state,
            file: &dep.file,
            spans: Vec::new(),
        };
        ref_collector.visit_program(program);

        let mut all_spans: Vec<(usize, usize, String)> = import_spans;
        for (span, new_name) in &ref_collector.spans {
            all_spans.push((span.start as usize, span.end as usize, new_name.clone()));
        }

        if all_spans.is_empty() {
            return dep.content.clone();
        }

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
/// they refer to a renamed import binding.
struct DuplicateImportRefCollector<'a, 'b> {
    state: &'a DuplicateState,
    file: &'b str,
    spans: Vec<(Span, String)>,
}

impl<'a, 'b, 'ast> Visit<'ast> for DuplicateImportRefCollector<'a, 'b> {
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self
                .state
                .find_import_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }

    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.object
            && let Some(new_name) = self
                .state
                .find_import_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_static_member_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &oxc::ast::ast::NewExpression<'ast>) {
        if let Expression::Identifier(ident) = &it.callee
            && let Some(new_name) = self
                .state
                .find_import_rename(self.file, ident.name.as_str())
        {
            self.spans.push((ident.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_new_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &oxc::ast::ast::IdentifierReference<'ast>) {
        if let Some(new_name) = self.state.find_import_rename(self.file, it.name.as_str()) {
            self.spans.push((it.span, new_name.to_string()));
        }
        oxc::ast_visit::walk::walk_identifier_reference(self, it);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (mirrors checks/helpers.rs)
// ---------------------------------------------------------------------------

/// Resolve an import specifier to a file path in `dep_files`.
///
/// Given `import { helper } from "./mod"` in `src/a.ts`, this resolves
/// `"./mod"` to a file path like `"src/mod.ts"` by checking which file in
/// `deps` matches the resolved path. Returns `None` for non-relative
/// specifiers (npm packages, node built-ins).
fn resolve_import_source(
    specifier: &str,
    containing_file: &str,
    deps: &[DepsFile],
) -> Option<String> {
    if !specifier.starts_with('.') && !specifier.starts_with('/') {
        return None;
    }

    let base_dir = Path::new(containing_file)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let resolved = base_dir.join(specifier);

    // Try matching against dep files with various extensions.
    let resolved_str = resolved.to_string_lossy().replace('\\', "/");
    for dep in deps {
        // Exact path match (with extension).
        let dep_normalized = dep.file.replace('\\', "/");
        if dep_normalized == resolved_str {
            return Some(dep.file.clone());
        }
        // Try adding extensions (e.g. "./mod" → "src/mod.ts").
        for ext in &[".ts", ".tsx", ".js", ".jsx", ".cjs", ".mjs", ".cts", ".mts"] {
            let candidate = format!("{resolved_str}{ext}");
            if dep_normalized == candidate {
                return Some(dep.file.clone());
            }
        }
        // Try matching with /index (e.g. "./mod" → "src/mod/index.ts").
        for ext in &[".ts", ".tsx", ".js", ".jsx"] {
            let candidate = format!("{resolved_str}/index{ext}");
            if dep_normalized == candidate {
                return Some(dep.file.clone());
            }
        }
    }

    None
}

/// Check whether a `VariableDeclaration` is a CommonJS `require()` import.
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
fn is_require_call(call: &oxc::ast::ast::CallExpression<'_>) -> bool {
    matches!(&call.callee, Expression::Identifier(ident) if ident.name.as_str() == "require")
}

/// Recursively collect binding names from a `BindingPattern`.
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
// Public entry point
// ---------------------------------------------------------------------------

/// Rename cross-file duplicate top-level declarations to unique names.
///
/// This is the bundle-path counterpart of `check_duplicates`. Instead of
/// reporting duplicates and exiting, it proactively renames non-exported
/// declarations that collide across files to unique identifiers
/// (`_u<name>$<n>`), preventing scope collisions in the merged bundle.
///
/// ## Pipeline
///
/// 1. `collect_duplicates` — Scan all non-entry files for non-exported
///    top-level declarations; assign unique names to names appearing in 2+ files.
/// 2. `duplicate_local_handler` (per file) — Rename the declaration + all
///    local references.
/// 3. `collect_import_renames` + `duplicate_import_handler` (per file) —
///    Rename import bindings that reference duplicated names + all references.
///
/// ## What is skipped
///
/// - **Entry files** — left unchanged (public API surface).
/// - **Exported declarations** — handled by `export_default_handler` and
///   `remove_handler`. Renaming them would break the public API.
/// - **Import bindings** (CJS `require()` vars, TS import-equals) — these are
///   imports, not declarations, and are handled by `remove_handler`.
///
/// ## When to run
///
/// This should run **before** `export_default_handler` and
/// `anonymous_handler`, because those handlers also rename declarations,
/// and running duplicate_handler first ensures the names it generates don't
/// collide with the export-default renames (they use different sigils:
/// `_u` for duplicates vs `_d` for default exports).
pub fn duplicate_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    let mut state = DuplicateState::new();

    // Phase 1: Collect duplicate names across all files.
    collect_duplicates(&deps, &mut state);

    // If no duplicates, fast-path — return unchanged.
    if state.renames.is_empty() {
        return deps;
    }

    // Phase 2: Rename declarations + local references in each file.
    let phase2: Vec<DepsFile> = deps
        .iter()
        .map(|dep| {
            let content = duplicate_local_handler(dep, &state);
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

    // Phase 3: Collect import renames, then apply them.
    // Pass `deps` so we can resolve import specifiers to source files.
    for dep in &phase2 {
        collect_import_renames(dep, &mut state, &deps);
    }

    let phase3: Vec<DepsFile> = phase2
        .iter()
        .map(|dep| {
            let content = duplicate_import_handler(dep, &state, &deps);
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
    use crate::types::ModuleType;

    fn make_dep(file: &str, content: &str) -> DepsFile {
        DepsFile {
            file: file.to_string(),
            content: content.to_string(),
            bytes: content.len(),
            module_type: ModuleType::Esm,
            file_ext: crate::types::ValidExts::Ts,
            is_jsx: false,
            is_entry: false,
        }
    }

    #[test]
    fn renames_duplicate_function() {
        let deps = vec![
            make_dep(
                "src/a.ts",
                "function helper() { return 1; }\nexport default helper;",
            ),
            make_dep(
                "src/b.ts",
                "function helper() { return 2; }\nexport default helper;",
            ),
        ];

        let result = duplicate_handler(deps);

        // Both should be renamed with _u prefix
        assert!(result[0].content.contains("_uhelper$1"));
        assert!(result[1].content.contains("_uhelper$2"));
    }

    #[test]
    fn does_not_rename_single_declaration() {
        let deps = vec![
            make_dep("src/a.ts", "function helper() { return 1; }"),
            make_dep("src/b.ts", "function other() { return 2; }"),
        ];

        let result = duplicate_handler(deps);

        // No duplicates — should be unchanged
        assert!(result[0].content.contains("function helper()"));
        assert!(result[1].content.contains("function other()"));
    }

    #[test]
    fn skips_entry_files() {
        let deps = vec![
            DepsFile {
                file: "src/index.ts".to_string(),
                content: "function helper() { return 0; }".to_string(),
                bytes: 28,
                module_type: ModuleType::Esm,
                file_ext: crate::types::ValidExts::Ts,
                is_jsx: false,
                is_entry: true,
            },
            make_dep("src/a.ts", "function helper() { return 1; }"),
            make_dep("src/b.ts", "function helper() { return 2; }"),
        ];

        let result = duplicate_handler(deps);

        // Entry file should be unchanged
        assert!(result[0].content.contains("function helper()"));
        assert!(!result[0].content.contains("_uhelper"));
        // Non-entry files should be renamed
        assert!(result[1].content.contains("_uhelper$1"));
        assert!(result[2].content.contains("_uhelper$2"));
    }

    #[test]
    fn renames_references_to_duplicate() {
        let deps = vec![
            make_dep(
                "src/a.ts",
                "function helper() { return 1; }\nconst x = helper();",
            ),
            make_dep(
                "src/b.ts",
                "function helper() { return 2; }\nconst y = helper();",
            ),
        ];

        let result = duplicate_handler(deps);

        // References should be renamed too
        assert!(result[0].content.contains("_uhelper$1()"));
        assert!(result[1].content.contains("_uhelper$2()"));
    }

    #[test]
    fn does_not_rename_exported_declarations() {
        let deps = vec![
            make_dep("src/a.ts", "export function helper() { return 1; }"),
            make_dep("src/b.ts", "export function helper() { return 2; }"),
        ];

        let result = duplicate_handler(deps);

        // Exported declarations should NOT be renamed by duplicate_handler
        // (they are handled by remove_handler / export_default_handler)
        assert!(result[0].content.contains("export function helper()"));
        assert!(result[1].content.contains("export function helper()"));
    }

    #[test]
    fn renames_variables_and_types() {
        let deps = vec![
            make_dep("src/a.ts", "const config = 1;\ntype Result = string;"),
            make_dep("src/b.ts", "const config = 2;\ntype Result = number;"),
        ];

        let result = duplicate_handler(deps);

        // Both `config` and `Result` are duplicated across 2 files.
        // Each file should get a unique `_u`-prefixed name for each.
        // We don't assert specific counter values since HashMap iteration
        // order is non-deterministic and the counter is shared per category.
        assert!(
            result[0].content.contains("_uconfig$"),
            "expected _uconfig$ in {}",
            result[0].content
        );
        assert!(
            result[0].content.contains("_uResult$"),
            "expected _uResult$ in {}",
            result[0].content
        );
        assert!(
            result[1].content.contains("_uconfig$"),
            "expected _uconfig$ in {}",
            result[1].content
        );
        assert!(
            result[1].content.contains("_uResult$"),
            "expected _uResult$ in {}",
            result[1].content
        );
        // Ensure the two files got different names (no collision).
        assert_ne!(
            extract_u_name(&result[0].content, "config"),
            extract_u_name(&result[1].content, "config"),
        );
    }

    /// Extract the first `_u<base>$N` identifier from `content`.
    fn extract_u_name(content: &str, base: &str) -> String {
        let prefix = format!("_u{base}$");
        content
            .split_whitespace()
            .find(|tok| tok.starts_with(&prefix))
            .map(|s| {
                let end = s
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
                    .unwrap_or(s.len());
                s[..end].to_string()
            })
            .unwrap_or_default()
    }
}
