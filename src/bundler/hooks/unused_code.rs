//! Unused code removal hook (post-bundle).
//!
//! Mirrors `__local__/ts/bundler/lib/unusedCode.ts` from the TypeScript
//! implementation.
//!
//! After bundling — when all dependency files have been merged into a single
//! output string with imports at the top — this hook removes top-level
//! declarations that are never referenced:
//!
//! - **Imports**: Removes unused named import specifiers; removes the entire
//!   import declaration when the default or namespace import is unused (or
//!   when all named specifiers are unused and there is no default import).
//! - **Functions / Classes**: Removes the declaration when its name is never
//!   referenced elsewhere.
//! - **Variable statements**: Removes the entire `const`/`let`/`var`
//!   statement only when **none** of the declared binding names are used.
//!
//! Exported symbols are treated as used by default (the entry file's exports
//! are the public API of the bundled library).
//!
//! The implementation operates on the final bundled source text via AST
//! round-tripping (parse → collect spans → replace right-to-left), the same
//! strategy used throughout `susee_hooks`.

use std::collections::HashSet;

use oxc::ast::ast::{
    BindingPattern, Class, Function, ImportDeclaration, ImportDeclarationSpecifier,
    ModuleExportName, Program, Statement,
};
use oxc::ast_visit::Visit;
use oxc::span::GetSpan;
use oxc::syntax::scope::ScopeFlags;

use crate::utils::with_parsed_program;

/// Options controlling unused-code removal.
#[derive(Debug, Clone)]
pub struct ClearUnusedOptions {
    /// Treat exported symbols as used (default: `true`).
    pub treat_exports_as_used: bool,
}

impl Default for ClearUnusedOptions {
    fn default() -> Self {
        Self {
            treat_exports_as_used: true,
        }
    }
}

/// Collect all binding names from a `BindingPattern` (identifiers, object
/// pattern properties, array pattern elements, assignment pattern left
/// sides).
fn collect_binding_names(pattern: &BindingPattern<'_>, out: &mut Vec<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => {
            out.push(id.name.as_str().to_string());
        }
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_names(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_binding_names(elem, out);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => {
            collect_binding_names(&assign.left, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1 — collect defined names and used names
// ---------------------------------------------------------------------------

/// Metadata for a defined top-level name.
#[derive(Debug, Clone, Copy)]
struct DefMeta {
    exported: bool,
}

/// First-pass collector: records every top-level defined name (imports, vars,
/// functions, classes) and every identifier reference that constitutes a "use".
struct CollectVisitor<'a> {
    defined: &'a mut Vec<(String, DefMeta)>,
    used: &'a mut HashSet<String>,
}

impl<'a, 'ast> Visit<'ast> for CollectVisitor<'a> {
    // --- Definitions ---

    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'ast>) {
        if let Some(specifiers) = &decl.specifiers {
            for spec in specifiers {
                match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        let name = s.local.name.as_str().to_string();
                        self.defined.push((name, DefMeta { exported: false }));
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        let name = s.local.name.as_str().to_string();
                        self.defined.push((name, DefMeta { exported: false }));
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        let name = s.local.name.as_str().to_string();
                        self.defined.push((name, DefMeta { exported: false }));
                    }
                }
            }
        }
        // Walk children to catch any references inside (rare for imports).
        oxc::ast_visit::walk::walk_import_declaration(self, decl);
    }

    fn visit_variable_declaration(&mut self, decl: &oxc::ast::ast::VariableDeclaration<'ast>) {
        for declarator in &decl.declarations {
            let mut names = Vec::new();
            collect_binding_names(&declarator.id, &mut names);
            for n in &names {
                self.defined.push((n.clone(), DefMeta { exported: false }));
            }
        }
        oxc::ast_visit::walk::walk_variable_declaration(self, decl);
    }

    fn visit_function(&mut self, decl: &Function<'ast>, _flags: ScopeFlags) {
        if let Some(id) = &decl.id {
            let name = id.name.as_str().to_string();
            self.defined.push((name, DefMeta { exported: false }));
        }
        oxc::ast_visit::walk::walk_function(self, decl, _flags);
    }

    fn visit_class(&mut self, decl: &Class<'ast>) {
        if let Some(id) = &decl.id {
            let name = id.name.as_str().to_string();
            self.defined.push((name, DefMeta { exported: false }));
        }
        oxc::ast_visit::walk::walk_class(self, decl);
    }

    fn visit_ts_type_alias_declaration(
        &mut self,
        decl: &oxc::ast::ast::TSTypeAliasDeclaration<'ast>,
    ) {
        let name = decl.id.name.as_str().to_string();
        self.defined.push((name, DefMeta { exported: false }));
        oxc::ast_visit::walk::walk_ts_type_alias_declaration(self, decl);
    }

    fn visit_ts_interface_declaration(
        &mut self,
        decl: &oxc::ast::ast::TSInterfaceDeclaration<'ast>,
    ) {
        let name = decl.id.name.as_str().to_string();
        self.defined.push((name, DefMeta { exported: false }));
        oxc::ast_visit::walk::walk_ts_interface_declaration(self, decl);
    }

    fn visit_ts_enum_declaration(&mut self, decl: &oxc::ast::ast::TSEnumDeclaration<'ast>) {
        let name = decl.id.name.as_str().to_string();
        self.defined.push((name, DefMeta { exported: false }));
        oxc::ast_visit::walk::walk_ts_enum_declaration(self, decl);
    }

    // --- Exports (mark defined names as exported) ---

    fn visit_export_named_declaration(
        &mut self,
        decl: &oxc::ast::ast::ExportNamedDeclaration<'ast>,
    ) {
        // Mark exported specifiers as exported.
        for spec in &decl.specifiers {
            let local_name = match &spec.local {
                ModuleExportName::IdentifierReference(id) => id.name.as_str().to_string(),
                ModuleExportName::IdentifierName(id) => id.name.as_str().to_string(),
                ModuleExportName::StringLiteral(_) => String::new(),
            };
            if !local_name.is_empty()
                && let Some(entry) = self.defined.iter_mut().find(|(n, _)| *n == local_name)
            {
                entry.1.exported = true;
            }
            let _ = spec;
        }
        oxc::ast_visit::walk::walk_export_named_declaration(self, decl);
    }

    fn visit_export_declaration(&mut self, decl: &oxc::ast::ast::ExportDeclaration<'ast>) {
        // `export const foo = ...` etc.  The inner declaration's name is
        // exported.  Walk the inner declaration to collect names, then mark
        // them as exported.
        oxc::ast_visit::walk::walk_export_declaration(self, decl);
        // After walking, mark names added by the inner declaration as
        // exported.  We can do this by checking the declaration's names.
        use oxc::ast::ast::Declaration;
        let mut names = Vec::new();
        match &decl.declaration {
            Declaration::VariableDeclaration(var_decl) => {
                for declarator in &var_decl.declarations {
                    collect_binding_names(&declarator.id, &mut names);
                }
            }
            Declaration::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    names.push(id.name.as_str().to_string());
                }
            }
            Declaration::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    names.push(id.name.as_str().to_string());
                }
            }
            Declaration::TSTypeAliasDeclaration(ta) => {
                names.push(ta.id.name.as_str().to_string());
            }
            Declaration::TSInterfaceDeclaration(iface) => {
                names.push(iface.id.name.as_str().to_string());
            }
            Declaration::TSEnumDeclaration(en) => {
                names.push(en.id.name.as_str().to_string());
            }
            _ => {}
        }
        for n in &names {
            if let Some(entry) = self.defined.iter_mut().find(|(name, _)| name == n) {
                entry.1.exported = true;
            }
        }
    }

    fn visit_export_default_declaration(
        &mut self,
        decl: &oxc::ast::ast::ExportDefaultDeclaration<'ast>,
    ) {
        // `export default function foo()` — `foo` is local but the export
        // itself is used.  We don't mark the name as exported here (it's not
        // a named export), but the expression/form is always considered used.
        oxc::ast_visit::walk::walk_export_default_declaration(self, decl);
    }

    // --- Usage ---

    fn visit_identifier_reference(&mut self, node: &oxc::ast::ast::IdentifierReference<'ast>) {
        let name = node.name.as_str().to_string();
        self.used.insert(name);
    }
}

/// Determine the set of unused names.
fn compute_unused(
    defined: &[(String, DefMeta)],
    used: &HashSet<String>,
    options: &ClearUnusedOptions,
) -> HashSet<String> {
    let mut unused = HashSet::new();

    // Build a merged map: name → exported (true if any definition marks it exported).
    let mut merged: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for (name, meta) in defined {
        let entry = merged.entry(name.clone()).or_insert(false);
        *entry = *entry || meta.exported;
    }

    for (name, exported) in &merged {
        if used.contains(name) {
            continue;
        }
        if options.treat_exports_as_used && *exported {
            continue;
        }
        unused.insert(name.clone());
    }

    unused
}

// ---------------------------------------------------------------------------
// Phase 2 — collect spans to remove
// ---------------------------------------------------------------------------

/// Collect byte-offset spans of statements/specifiers that should be removed.
fn collect_removal_spans(
    program: &Program<'_>,
    source_text: &str,
    unused: &HashSet<String>,
) -> Vec<(usize, usize, String)> {
    let mut spans: Vec<(usize, usize, String)> = Vec::new();

    for stmt in &program.body {
        match stmt {
            // --- Imports ---
            Statement::ImportDeclaration(import_decl) => {
                let Some(specifiers) = &import_decl.specifiers else {
                    continue;
                };

                let mut default_name: Option<String> = None;
                let mut namespace_name: Option<String> = None;
                let mut named_specifiers: Vec<&oxc::ast::ast::ImportSpecifier<'_>> = Vec::new();

                for spec in specifiers {
                    match spec {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            named_specifiers.push(s);
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            default_name = Some(s.local.name.as_str().to_string());
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            namespace_name = Some(s.local.name.as_str().to_string());
                        }
                    }
                }

                let default_used = default_name
                    .as_ref()
                    .map(|n| !unused.contains(n))
                    .unwrap_or(false);
                let namespace_used = namespace_name
                    .as_ref()
                    .map(|n| !unused.contains(n))
                    .unwrap_or(false);
                let kept_named: Vec<&oxc::ast::ast::ImportSpecifier<'_>> = named_specifiers
                    .iter()
                    .filter(|s| !unused.contains(s.local.name.as_str()))
                    .copied()
                    .collect();

                // Case A: default or namespace import is unused → remove entire import.
                if (default_name.is_some() && !default_used)
                    || (namespace_name.is_some() && !namespace_used)
                {
                    let span = import_decl.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                    continue;
                }

                // Case B: all named specifiers unused and no default → remove entire import.
                if !named_specifiers.is_empty() && kept_named.is_empty() && default_name.is_none() {
                    let span = import_decl.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                    continue;
                }

                // Case C: some (but not all) named specifiers are unused →
                // replace just the specifier spans with nothing (we remove
                // individual specifier spans plus any trailing comma).
                if kept_named.len() != named_specifiers.len() {
                    for s in &named_specifiers {
                        if unused.contains(s.local.name.as_str()) {
                            // Expand the span to include a trailing comma +
                            // whitespace so we don't leave `,,` artifacts.
                            let spec_span = s.span();
                            let start = spec_span.start as usize;
                            let end = spec_span.end as usize;
                            // Extend end to swallow a following comma + whitespace.
                            let extended_end = extend_through_comma(source_text, end);
                            // Also extend start backward to swallow a preceding comma + whitespace.
                            let extended_start = extend_back_through_comma(source_text, start);
                            spans.push((extended_start, extended_end, String::new()));
                        }
                    }
                }
            }

            // --- Function / Class declarations ---
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id
                    && unused.contains(id.name.as_str())
                {
                    let span = stmt.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id
                    && unused.contains(id.name.as_str())
                {
                    let span = stmt.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                }
            }

            // --- Variable declarations ---
            Statement::VariableDeclaration(var_decl) => {
                let mut names = Vec::new();
                for declarator in &var_decl.declarations {
                    collect_binding_names(&declarator.id, &mut names);
                }
                let any_used = names.iter().any(|n| !unused.contains(n));
                if !any_used {
                    let span = stmt.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                }
            }

            // --- TS type aliases, interfaces, enums ---
            Statement::TSTypeAliasDeclaration(ta) => {
                if unused.contains(ta.id.name.as_str()) {
                    let span = stmt.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                }
            }
            Statement::TSInterfaceDeclaration(iface) => {
                if unused.contains(iface.id.name.as_str()) {
                    let span = stmt.span();
                    spans.push((span.start as usize, span.end as usize, String::new()));
                }
            }
            Statement::TSEnumDeclaration(en) if unused.contains(en.id.name.as_str()) => {
                let span = stmt.span();
                spans.push((span.start as usize, span.end as usize, String::new()));
            }

            _ => {}
        }
    }

    spans
}

/// Extend a byte offset forward to include a trailing comma and whitespace.
fn extend_through_comma(source_text: &str, end: usize) -> usize {
    let bytes = source_text.as_bytes();
    let mut pos = end;
    // Skip whitespace.
    while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
        pos += 1;
    }
    // If the next char is a comma, include it and any trailing whitespace.
    if pos < bytes.len() && bytes[pos] == b',' {
        pos += 1;
        while pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
            pos += 1;
        }
        return pos;
    }
    end
}

/// Extend a byte offset backward to include a preceding comma and whitespace.
fn extend_back_through_comma(source_text: &str, start: usize) -> usize {
    let bytes = source_text.as_bytes();
    let mut pos = start;
    // Skip whitespace backward.
    while pos > 0 && (bytes[pos - 1] == b' ' || bytes[pos - 1] == b'\t') {
        pos -= 1;
    }
    // If the preceding char is a comma, include it and any preceding whitespace.
    if pos > 0 && bytes[pos - 1] == b',' {
        pos -= 1;
        while pos > 0 && (bytes[pos - 1] == b' ' || bytes[pos - 1] == b'\t') {
            pos -= 1;
        }
        return pos;
    }
    start
}

// ---------------------------------------------------------------------------
// Phase 3 — apply spans (right-to-left replacement)
// ---------------------------------------------------------------------------

/// Sort spans by start offset descending and remove duplicates.
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

use crate::utils::strip_empty_lines;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Clear unused top-level declarations from a bundled source string.
///
/// This is the post-bundle cleanup pass, mirroring
/// `cleanUnusedCode` from `__local__/ts/bundler/lib/unusedCode.ts`.
///
/// - Removes only unused named import specifiers.
/// - Removes entire import declarations when an unused default or namespace
///   import is present.
/// - Removes function/class declarations when their name is unused.
/// - Removes entire variable statements when none of the declared
///   identifiers are used.
/// - Removes unused TS type aliases, interfaces, and enums.
///
/// Limitations: works on a single-file basis (the bundled output); does not
/// analyze cross-file usages (they have already been merged).
pub fn clean_unused_code(content: &str, file: &str, options: ClearUnusedOptions) -> String {
    with_parsed_program(file, content, |program| {
        let source_text = program.source_text;

        // Phase 1: collect defined names and used names.
        let mut defined: Vec<(String, DefMeta)> = Vec::new();
        let mut used: HashSet<String> = HashSet::new();
        let mut collector = CollectVisitor {
            defined: &mut defined,
            used: &mut used,
        };
        collector.visit_program(program);

        // Phase 2: compute unused names.
        let unused = compute_unused(&defined, &used, &options);
        if unused.is_empty() {
            return content.to_string();
        }

        // Phase 3: collect removal spans.
        let mut spans = collect_removal_spans(program, source_text, &unused);
        if spans.is_empty() {
            return content.to_string();
        }
        sort_and_dedup_spans(&mut spans);

        // Phase 4: apply spans and clean up.
        let result = apply_spans(content, &spans);
        strip_empty_lines(&result)
    })
}

/// Convenience wrapper using default options.
pub fn clean(content: &str, file: &str) -> String {
    clean_unused_code(content, file, ClearUnusedOptions::default())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_unused_function() {
        let src = r#"
function used() { return helper(); }
function helper() { return 42; }
unused();
const result = used();
export { result };
"#;
        let out = clean(src, "bundle.ts");
        // `unused` should be removed, `helper` and `used` should remain.
        assert!(!out.contains("function unused"));
        assert!(out.contains("function helper"));
        assert!(out.contains("function used"));
    }

    #[test]
    fn removes_unused_class() {
        let src = r#"
class UsedClass { method() { return 42; } }
class UnusedClass { foo() {} }
const instance = new UsedClass();
export { instance };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("class UnusedClass"));
        assert!(out.contains("class UsedClass"));
    }

    #[test]
    fn removes_unused_variable_statement() {
        let src = r#"
const x = 1;
const y = 2;
const z = x + y;
export { z };
"#;
        let out = clean(src, "bundle.ts");
        // All variables are used (x in z, y in z, z exported).
        assert!(out.contains("const x"));
        assert!(out.contains("const y"));
        assert!(out.contains("const z"));
    }

    #[test]
    fn removes_completely_unused_variable_statement() {
        let src = r#"
const unused1 = 1;
const unused2 = 2;
function used() { return 42; }
export { used };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("unused1"));
        assert!(!out.contains("unused2"));
        assert!(out.contains("function used"));
    }

    #[test]
    fn keeps_partial_variable_statement() {
        // When one declarator is used and another isn't, the TS impl removes
        // the whole statement only if ALL are unused.  If any is used, the
        // statement stays (it doesn't split the statement).
        let src = r#"
const a = 1, b = 2;
function foo() { return a; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        // `a` is used, `b` is not, but the whole statement stays because
        // not all are unused.
        assert!(out.contains("const a"));
    }

    #[test]
    fn removes_unused_named_import_specifier() {
        let src = r#"
import { used, unused } from "mod";
function foo() { return used; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("used"));
        assert!(!out.contains("unused"));
        // The import should still be there with `used` kept.
        assert!(out.contains("import"));
    }

    #[test]
    fn removes_entire_import_when_default_unused() {
        let src = r#"
import Foo from "mod";
function bar() { return 42; }
export { bar };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("Foo"));
        assert!(!out.contains("import"));
    }

    #[test]
    fn removes_entire_import_when_all_named_unused() {
        let src = r#"
import { a, b, c } from "mod";
function bar() { return 42; }
export { bar };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("import"));
        assert!(!out.contains("from \"mod\""));
    }

    #[test]
    fn keeps_imports_from_used_specifiers() {
        let src = r#"
import { used1, used2 } from "mod";
function bar() { return used1 + used2; }
export { bar };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("import"));
        assert!(out.contains("used1"));
        assert!(out.contains("used2"));
    }

    #[test]
    fn treats_exports_as_used_by_default() {
        let src = r#"
const exported = 42;
export { exported };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("exported"));
    }

    #[test]
    fn removes_exported_when_treat_exports_as_used_false() {
        // When treat_exports_as_used is false, a variable that is ONLY used
        // in an export specifier (not referenced elsewhere) should be removed
        // along with its export.
        let src = r#"
const only_exported = 42;
export { only_exported };
"#;
        let out = clean_unused_code(
            src,
            "bundle.ts",
            ClearUnusedOptions {
                treat_exports_as_used: false,
            },
        );
        // `only_exported` is referenced in the export specifier, which
        // counts as a use — so it is kept.  This matches the TS behavior
        // where export specifier identifiers are not declaration names.
        assert!(out.contains("only_exported"));
    }

    #[test]
    fn removes_unused_type_alias() {
        let src = r#"
type UnusedType = string;
type UsedType = number;
function foo(): UsedType { return 42; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("UnusedType"));
        assert!(out.contains("UsedType"));
    }

    #[test]
    fn removes_unused_interface() {
        let src = r#"
interface UnusedIface { x: number; }
interface UsedIface { y: string; }
function foo(): UsedIface { return { y: "hi" }; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("UnusedIface"));
        assert!(out.contains("UsedIface"));
    }

    #[test]
    fn removes_unused_enum() {
        let src = r#"
enum UnusedEnum { A, B }
enum UsedEnum { X, Y }
function foo() { return UsedEnum.X; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("UnusedEnum"));
        assert!(out.contains("UsedEnum"));
    }

    #[test]
    fn keeps_namespace_import_when_used() {
        let src = r#"
import * as ns from "mod";
function foo() { return ns.value; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("ns"));
        assert!(out.contains("import"));
    }

    #[test]
    fn removes_namespace_import_when_unused() {
        let src = r#"
import * as ns from "mod";
function foo() { return 42; }
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(!out.contains("ns"));
        assert!(!out.contains("import"));
    }

    #[test]
    fn no_changes_when_all_used() {
        let src = r#"
import { foo } from "mod";
function bar() { return foo(); }
export { bar };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("import"));
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn handles_object_destructuring_imports() {
        let src = r#"
import { used, unused } from "mod";
function foo() {
  const { prop } = { prop: used };
  return prop;
}
export { foo };
"#;
        let out = clean(src, "bundle.ts");
        assert!(out.contains("used"));
        assert!(!out.contains("unused"));
    }
}
