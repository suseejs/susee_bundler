//! Export-star re-export handler (`export * from "…"`).
//!
//! When the **entry file** re-exports a local module with a star:
//!
//! ```js
//! export * from "./foo.js";
//! ```
//!
//! the bundler inlines `./foo.js` and `remove_handler` strips every
//! import/export statement *except* in the entry file. Left as-is, the
//! star statement survives into the final bundle pointing at a module path
//! that no longer exists next to it — breaking the bundle's public API at
//! runtime.
//!
//! This handler runs after the rename hooks and before `remove_handler`.
//! It rewrites each entry-file star re-export of a **local** module into an
//! explicit named export:
//!
//! ```js
//! export { a, b as c };
//! ```
//!
//! The names are discovered by enumerating the target module's exports
//! (following chained `export * from "…"` re-exports recursively); every
//! name resolves to the local binding that will exist in the bundled output
//! once `export` keywords are stripped from dependency files.
//!
//! ## Semantics
//!
//! - `default` exports are never re-exported by `export *` (per the ES
//!   module spec) and are skipped.
//! - Names the entry file exports explicitly win over star re-exports.
//! - A name provided by several star re-exports backed by *different*
//!   local bindings is ambiguous and excluded everywhere (per the ES
//!   module spec). A name provided with the same local binding by several
//!   stars is re-exported once, by the first star.
//! - `export type * from "…"` expands to `export type { … };` containing
//!   only type-only exports.
//! - `export * as ns from "…"` cannot be represented in the flat bundle
//!   (no namespace object exists). The statement is kept unchanged and a
//!   warning is logged when it targets a local module.
//! - Stars targeting non-local modules (npm packages, node built-ins) are
//!   kept unchanged — those specifiers still resolve at runtime.
//! - Star statements in **non-entry** files are left untouched; they are
//!   removed entirely by `remove_handler` (existing behaviour).

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use oxc::ast::ast::{
    BindingPattern, Declaration, ImportOrExportKind, ModuleExportName, Statement,
};

use super::export_default::get_file_key;
use crate::susee_log;
use crate::types::DepsFile;
use crate::utils::with_parsed_program;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single name re-exported through `export * from "…"`, resolved to the
/// local binding that will exist in the bundled output once `export`
/// keywords are stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportPair {
    /// Local binding name in the bundle (e.g. `foo`).
    local: String,
    /// Public export name (e.g. `bar` in `export { foo as bar }`).
    exported: String,
    /// Whether the export name was written as a string literal
    /// (`export { foo as "n" }`).
    exported_is_string: bool,
    /// Whether this is a type-only export (interface / type alias /
    /// `export type { … }`).
    is_type: bool,
}

/// An entry-file `export * from "…"`, plus the enumerated re-export
/// candidates discovered for it.
struct StarStatement {
    /// Byte offset where the statement starts in the entry content.
    start: usize,
    /// Byte offset where the statement (including any trailing `;`) ends.
    end: usize,
    /// Enumerated re-export candidates from the target module.
    pairs: Vec<ExportPair>,
    /// `true` for `export type * from "…"`.
    type_only: bool,
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Construct an export pair whose public name equals the local binding name.
fn identical_pair(name: String, is_type: bool) -> ExportPair {
    ExportPair {
        local: name.clone(),
        exported: name,
        exported_is_string: false,
        is_type,
    }
}

/// Extract the (name, was-string-literal) parts of a module export name.
fn module_export_name_parts(name: &ModuleExportName<'_>) -> (String, bool) {
    match name {
        ModuleExportName::IdentifierReference(id) => (id.name.as_str().to_string(), false),
        ModuleExportName::IdentifierName(id) => (id.name.as_str().to_string(), false),
        ModuleExportName::StringLiteral(s) => (s.value.as_str().to_string(), true),
    }
}

/// Escape a string value for re-emission inside a double-quoted string
/// literal.
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
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
// Module-specifier resolution
// ---------------------------------------------------------------------------

/// Lexically normalize a path: drop `.` components and resolve `..`
/// against the preceding components (without touching the filesystem).
fn lexical_normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p| p.as_str() != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(p) => parts.push(p.to_string_lossy().to_string()),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

/// Resolve a module specifier into the same normalized file key used by
/// [`get_file_key`] (`dir/name` without extension, `dir` for `index` files).
///
/// Returns `None` for non-relative specifiers (npm packages, node
/// built-ins) — local re-exports always use relative specifiers.
fn resolve_local_key(specifier: &str, containing_file: &str) -> Option<String> {
    if !(specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/'))
    {
        return None;
    }
    let base = Path::new(containing_file)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let joined: PathBuf = if specifier.starts_with('/') {
        PathBuf::from(specifier)
    } else {
        base.join(specifier)
    };
    let normalized = lexical_normalize(&joined);
    if normalized.is_empty() {
        return None;
    }
    let path = Path::new(&normalized);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        return None;
    }
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    Some(if stem == "index" {
        if dir.is_empty() {
            ".".to_string()
        } else {
            dir
        }
    } else if dir.is_empty() {
        stem.to_string()
    } else {
        format!("{dir}/{stem}")
    })
}

/// Resolve a module specifier into the index of the matching dependency
/// file, if it is a local module present in the bundle.
fn resolve_local_index(
    specifier: &str,
    containing_file: &str,
    deps_by_key: &HashMap<String, usize>,
) -> Option<usize> {
    let key = resolve_local_key(specifier, containing_file)?;
    deps_by_key.get(&key).copied()
}

// ---------------------------------------------------------------------------
// Export enumeration
// ---------------------------------------------------------------------------

/// Collect the export pairs introduced by an `export <declaration>`
/// statement — the export name always equals the binding name.
fn collect_declaration_export_pairs(decl: &Declaration<'_>, out: &mut Vec<ExportPair>) {
    match decl {
        // `export const a = 1` / `export let { a, b } = obj`
        Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                let mut names = Vec::new();
                collect_binding_names(&declarator.id, &mut names);
                for name in names {
                    out.push(identical_pair(name, false));
                }
            }
        }
        // `export function foo() {}`
        Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                out.push(identical_pair(id.name.as_str().to_string(), false));
            }
        }
        // `export class Foo {}`
        Declaration::ClassDeclaration(cls) => {
            if let Some(id) = &cls.id {
                out.push(identical_pair(id.name.as_str().to_string(), false));
            }
        }
        // `export type T = …`
        Declaration::TSTypeAliasDeclaration(type_alias) => {
            out.push(identical_pair(
                type_alias.id.name.as_str().to_string(),
                true,
            ));
        }
        // `export interface I { … }`
        Declaration::TSInterfaceDeclaration(interface) => {
            out.push(identical_pair(
                interface.id.name.as_str().to_string(),
                true,
            ));
        }
        // `export enum E { … }` (enums are runtime values)
        Declaration::TSEnumDeclaration(enum_decl) => {
            out.push(identical_pair(enum_decl.id.name.as_str().to_string(), false));
        }
        // Namespace/module declarations are not re-exported flatly.
        _ => {}
    }
}

/// Enumerate the exported names of a dependency file, resolving every name
/// to the local binding that will exist in the bundled output.
///
/// Follows chained `export * from "…"` re-exports recursively (with cycle
/// protection via `visited`) and resolves `export { a as b } from "…"`
/// re-export specifiers against the target module's own exports. The
/// `visited` set is path-scoped: a module is removed from it on the way
/// out so diamond dependencies are still enumerated through every path.
fn collect_export_pairs(
    dep: &DepsFile,
    deps: &[DepsFile],
    deps_by_key: &HashMap<String, usize>,
    visited: &mut HashSet<String>,
) -> Vec<ExportPair> {
    let key = get_file_key(&dep.file);
    if !visited.insert(key.clone()) {
        // Circular re-export — the cycle contributes no additional names.
        return Vec::new();
    }

    // Explicit exports of this module (declarations, local re-exports, and
    // re-export-from specifiers).
    let mut own: Vec<ExportPair> = Vec::new();
    // Names propagated from chained `export * from "…"` statements.
    let mut star: Vec<ExportPair> = Vec::new();

    with_parsed_program(&dep.file, &dep.content, |program| {
        for stmt in &program.body {
            match stmt {
                // `export const/function/class/interface/type/enum …`
                Statement::ExportDeclaration(export_decl) => {
                    collect_declaration_export_pairs(&export_decl.declaration, &mut own);
                }
                // `export { a, a as b }` (local re-exports).
                Statement::ExportNamedDeclaration(export_named) => {
                    let stmt_type = export_named.export_kind == ImportOrExportKind::Type;
                    for spec in &export_named.specifiers {
                        let (local, local_is_string) = module_export_name_parts(&spec.local);
                        // `default` cannot be re-exported through a flat
                        // bundle binding, and string-literal local names are
                        // only valid in `from` clauses.
                        if local == "default" || local_is_string {
                            continue;
                        }
                        let (exported, exported_is_string) =
                            module_export_name_parts(&spec.exported);
                        own.push(ExportPair {
                            local,
                            exported,
                            exported_is_string,
                            is_type: stmt_type
                                || spec.export_kind == ImportOrExportKind::Type,
                        });
                    }
                }
                // `export { a as b } from "…"` — resolve against the target.
                Statement::ExportFromDeclaration(export_from) => {
                    let source = export_from.source.value.as_str().to_string();
                    let Some(target) = resolve_local_index(&source, &dep.file, deps_by_key)
                    else {
                        continue;
                    };
                    let target_pairs =
                        collect_export_pairs(&deps[target], deps, deps_by_key, visited);
                    let stmt_type = export_from.export_kind == ImportOrExportKind::Type;
                    for spec in &export_from.specifiers {
                        let (imported, _) = module_export_name_parts(&spec.local);
                        let Some(found) = target_pairs.iter().find(|p| p.exported == imported)
                        else {
                            continue;
                        };
                        let (exported, exported_is_string) =
                            module_export_name_parts(&spec.exported);
                        own.push(ExportPair {
                            local: found.local.clone(),
                            exported,
                            exported_is_string,
                            is_type: stmt_type
                                || spec.export_kind == ImportOrExportKind::Type
                                || found.is_type,
                        });
                    }
                }
                // `export * from "…"` / `export * as ns from "…"`.
                Statement::ExportAllDeclaration(export_all) => {
                    // `export * as ns` cannot be represented in the flat
                    // bundle; the namespace name is simply not re-exported.
                    if export_all.exported.is_some() {
                        continue;
                    }
                    let source = export_all.source.value.as_str().to_string();
                    let Some(target) = resolve_local_index(&source, &dep.file, deps_by_key)
                    else {
                        continue;
                    };
                    let mut sub = collect_export_pairs(&deps[target], deps, deps_by_key, visited);
                    if export_all.export_kind == ImportOrExportKind::Type {
                        sub.retain(|p| p.is_type);
                    }
                    star.extend(sub);
                }
                _ => {}
            }
        }
    });

    // Explicit exports of this module win over its own star re-exports
    // (per ES module semantics).
    star.retain(|p| !own.iter().any(|o| o.exported == p.exported));

    // Allow diamond dependencies: remove the cycle marker on the way out so
    // the same module can still be enumerated through another path.
    visited.remove(&key);

    own.extend(star);
    own
}

// ---------------------------------------------------------------------------
// Entry-file rewriting
// ---------------------------------------------------------------------------

/// Render the expansion of one star statement as an `export` statement.
///
/// Returns an empty string when there is nothing to re-export (the star
/// statement is then removed entirely).
fn format_export_statement(pairs: &[ExportPair], type_only: bool) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let items: Vec<String> = pairs
        .iter()
        .map(|p| {
            if p.exported_is_string {
                format!("{} as \"{}\"", p.local, escape_js_string(&p.exported))
            } else if p.exported == p.local {
                p.local.clone()
            } else {
                format!("{} as {}", p.local, p.exported)
            }
        })
        .collect();
    if type_only {
        format!("export type {{ {} }};", items.join(", "))
    } else {
        format!("export {{ {} }};", items.join(", "))
    }
}

/// Rewrite the entry file's `export * from "…"` statements into explicit
/// `export { … }` statements.
fn process_entry(
    dep: &DepsFile,
    deps: &[DepsFile],
    deps_by_key: &HashMap<String, usize>,
) -> String {
    let entry_key = get_file_key(&dep.file);
    with_parsed_program(&dep.file, &dep.content, |program| {
        // Phase 1: Collect the entry's own explicit export names and every
        // star statement that targets a local module.
        let mut explicit: HashSet<String> = HashSet::new();
        let mut stars: Vec<StarStatement> = Vec::new();

        for stmt in &program.body {
            match stmt {
                Statement::ExportDeclaration(export_decl) => {
                    let mut pairs = Vec::new();
                    collect_declaration_export_pairs(&export_decl.declaration, &mut pairs);
                    explicit.extend(pairs.into_iter().map(|p| p.exported));
                }
                Statement::ExportNamedDeclaration(export_named) => {
                    for spec in &export_named.specifiers {
                        let (name, _) = module_export_name_parts(&spec.exported);
                        explicit.insert(name);
                    }
                }
                Statement::ExportFromDeclaration(export_from) => {
                    for spec in &export_from.specifiers {
                        let (name, _) = module_export_name_parts(&spec.exported);
                        explicit.insert(name);
                    }
                }
                Statement::ExportAllDeclaration(export_all) => {
                    let source = export_all.source.value.as_str().to_string();
                    match &export_all.exported {
                        // `export * as ns from "…"` — a namespace object
                        // cannot exist in the flat bundle. Keep the
                        // statement unchanged and warn when it targets a
                        // local (inlined) module.
                        Some(exported) => {
                            let (ns, _) = module_export_name_parts(exported);
                            explicit.insert(ns.clone());
                            if resolve_local_index(&source, &dep.file, deps_by_key).is_some() {
                                susee_log::warning(&format!(
                                    "`export * as {ns} from \"{source}\"` in the entry file \
                                     cannot be inlined into the bundle (no namespace object \
                                     exists); the statement is kept unchanged and may not \
                                     resolve at runtime."
                                ));
                            }
                        }
                        // `export * from "…"` — enumerate the target's exports.
                        None => {
                            let Some(target) =
                                resolve_local_index(&source, &dep.file, deps_by_key)
                            else {
                                // Non-local (npm / node built-in) or
                                // unresolved — leave the statement unchanged.
                                continue;
                            };
                            // Seed the visited set with the entry so circular
                            // re-exports (`export * from "./index"`) resolve
                            // to nothing instead of recursing forever.
                            let mut visited = HashSet::new();
                            visited.insert(entry_key.clone());
                            let mut pairs = collect_export_pairs(
                                &deps[target],
                                deps,
                                deps_by_key,
                                &mut visited,
                            );
                            let type_only = export_all.export_kind == ImportOrExportKind::Type;
                            if type_only {
                                pairs.retain(|p| p.is_type);
                            }
                            let mut end = export_all.span.end as usize;
                            // Statement spans may exclude the trailing `;`;
                            // consume it so the replacement fully replaces
                            // the original statement text.
                            if dep.content.as_bytes().get(end) == Some(&b';') {
                                end += 1;
                            }
                            stars.push(StarStatement {
                                start: export_all.span.start as usize,
                                end,
                                pairs,
                                type_only,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        if stars.is_empty() {
            return dep.content.clone();
        }

        // Phase 2: Resolve names across all star statements. The first
        // star providing a name owns it; the same name backed by a
        // *different* local binding elsewhere is ambiguous and excluded
        // everywhere (per ES module semantics).
        let mut owner: HashMap<String, (usize, String)> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for (i, star) in stars.iter().enumerate() {
            for pair in &star.pairs {
                if pair.exported == "default" || explicit.contains(&pair.exported) {
                    continue;
                }
                match owner.get(&pair.exported) {
                    Some((_, existing)) if *existing != pair.local => {
                        ambiguous.insert(pair.exported.clone());
                    }
                    Some(_) => {}
                    None => {
                        owner.insert(pair.exported.clone(), (i, pair.local.clone()));
                    }
                }
            }
        }

        // Phase 3: Build the replacement text for each star statement. Only
        // the star that owns a name re-exports it, so every name is
        // exported exactly once.
        let mut spans: Vec<(usize, usize, String)> = Vec::new();
        for (i, star) in stars.iter().enumerate() {
            let mut emitted: HashSet<String> = HashSet::new();
            let mut expansion: Vec<ExportPair> = Vec::new();
            for pair in &star.pairs {
                if pair.exported == "default"
                    || explicit.contains(&pair.exported)
                    || ambiguous.contains(&pair.exported)
                {
                    continue;
                }
                let Some((owner_idx, _)) = owner.get(&pair.exported) else {
                    continue;
                };
                if *owner_idx != i {
                    continue;
                }
                if !emitted.insert(pair.exported.clone()) {
                    continue;
                }
                expansion.push(pair.clone());
            }
            spans.push((
                star.start,
                star.end,
                format_export_statement(&expansion, star.type_only),
            ));
        }

        // Apply replacements right-to-left so earlier offsets stay valid.
        spans.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
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
// Public entry point
// ---------------------------------------------------------------------------

/// Expand entry-file `export * from "…"` re-exports of local modules into
/// explicit `export { … }` statements.
///
/// The re-export target is inlined into the bundle, so the original star
/// statement would reference a module path that no longer resolves at
/// runtime. Each star statement is replaced by the concrete list of names
/// its target exports (see the [module documentation](self) for the exact
/// semantics).
///
/// Stars targeting non-local modules are kept unchanged, and non-entry
/// files are passed through untouched — their star statements are removed
/// by [`remove_handler`](super::remove_handler) as before.
pub fn export_star_handler(deps: Vec<DepsFile>) -> Vec<DepsFile> {
    // Index every dependency file by its normalized file key so star
    // statements can resolve their target module.
    let mut deps_by_key: HashMap<String, usize> = HashMap::new();
    for (i, dep) in deps.iter().enumerate() {
        deps_by_key.entry(get_file_key(&dep.file)).or_insert(i);
    }

    deps.iter()
        .map(|dep| {
            if !dep.is_entry {
                return dep.clone();
            }
            let content = process_entry(dep, &deps, &deps_by_key);
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
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DepsFile;

    /// Build an entry-file `DepsFile`.
    fn make_entry(file: &str, content: &str) -> DepsFile {
        let mut dep = crate::utils::make_dep(file, content);
        dep.is_entry = true;
        dep
    }

    #[test]
    fn expands_star_reexport_in_entry() {
        let foo = crate::utils::make_dep(
            "src/foo.ts",
            "export const a = 1;\nexport function b() { return a; }\n",
        );
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(
            content.contains("export { a, b };"),
            "content was: {content}"
        );
        assert!(!content.contains("export *"), "content was: {content}");
    }

    #[test]
    fn excludes_default_export() {
        let foo = crate::utils::make_dep("src/foo.ts", "export const a = 1;\nexport default a;\n");
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(content.contains("export { a };"), "content was: {content}");
        assert!(!content.contains("default"), "content was: {content}");
    }

    #[test]
    fn follows_chained_star_reexports() {
        let leaf = crate::utils::make_dep("src/leaf.ts", "export const l = 2;\n");
        let mid = crate::utils::make_dep(
            "src/mid.ts",
            "export * from \"./leaf.js\";\nexport const m = 1;\n",
        );
        let entry = make_entry("src/index.ts", "export * from \"./mid.js\";\n");
        let result = export_star_handler(vec![leaf, mid, entry]);
        let content = &result[2].content;
        assert!(
            content.contains("export { m, l };"),
            "content was: {content}"
        );
        // `m` is `mid`'s own export; `l` is inherited via `mid`'s own
        // `export * from "./leaf.js"`.
        assert!(content.contains("m"), "content was: {content}");
        assert!(content.contains("l"), "content was: {content}");
    }

    #[test]
    fn expands_aliased_local_reexport() {
        let foo = crate::utils::make_dep("src/foo.ts", "const x = 1;\nexport { x as renamed };\n");
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(
            content.contains("export { x as renamed };"),
            "content was: {content}"
        );
    }

    #[test]
    fn resolves_reexport_from_specifiers() {
        let leaf = crate::utils::make_dep("src/leaf.ts", "export const l = 2;\n");
        let foo = crate::utils::make_dep("src/foo.ts", "export { l as fl } from \"./leaf.js\";\n");
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![leaf, foo, entry]);
        let content = &result[2].content;
        assert!(
            content.contains("export { l as fl };"),
            "content was: {content}"
        );
    }

    #[test]
    fn entry_explicit_exports_win() {
        let foo =
            crate::utils::make_dep("src/foo.ts", "export const a = 1;\nexport const b = 2;\n");
        let entry = make_entry("src/index.ts", "export const a = 10;\nexport * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(content.contains("export { b };"), "content was: {content}");
        // `a` is already exported explicitly and must not be re-exported
        // by the expansion.
        assert!(
            !content.contains("export { a,"),
            "content was: {content}"
        );
    }

    #[test]
    fn ambiguous_star_names_excluded() {
        let a = crate::utils::make_dep("src/a.ts", "export const x = 1;\n");
        let b = crate::utils::make_dep(
            "src/b.ts",
            "const x2 = 2;\nexport { x2 as x };\nexport const y = 3;\n",
        );
        let entry = make_entry(
            "src/index.ts",
            "export * from \"./a.js\";\nexport * from \"./b.js\";\n",
        );
        let result = export_star_handler(vec![a, b, entry]);
        let content = &result[2].content;
        // `x` is provided by two different bindings → ambiguous → excluded.
        assert!(
            !content.contains("export { x"),
            "content was: {content}"
        );
        assert!(content.contains("export { y };"), "content was: {content}");
        assert!(!content.contains("export *"), "content was: {content}");
    }

    #[test]
    fn same_binding_from_multiple_stars_exported_once() {
        let a = crate::utils::make_dep("src/a.ts", "export const x = 1;\n");
        let b = crate::utils::make_dep("src/b.ts", "export const x = 1;\nexport const y = 2;\n");
        let entry = make_entry(
            "src/index.ts",
            "export * from \"./a.js\";\nexport * from \"./b.js\";\n",
        );
        let result = export_star_handler(vec![a, b, entry]);
        let content = &result[2].content;
        // `x` is re-exported exactly once (by the first star), `y` by the
        // second.
        assert_eq!(content.matches("export { x };").count(), 1, "content was: {content}");
        assert!(content.contains("export { y };"), "content was: {content}");
    }

    #[test]
    fn non_local_star_left_unchanged() {
        let original = "export * from \"lodash-es\";\n";
        let entry = make_entry("src/index.ts", original);
        let result = export_star_handler(vec![entry]);
        assert_eq!(result[0].content, original);
    }

    #[test]
    fn namespace_star_kept_unchanged() {
        let foo = crate::utils::make_dep("src/foo.ts", "export const a = 1;\n");
        let original = "export * as ns from \"./foo.js\";\n";
        let entry = make_entry("src/index.ts", original);
        let result = export_star_handler(vec![foo, entry]);
        // The namespace re-export cannot be inlined; it is kept as-is
        // (a warning is logged).
        assert_eq!(result[1].content, original);
    }

    #[test]
    fn default_only_target_removes_statement() {
        let foo = crate::utils::make_dep("src/foo.ts", "const x = 1;\nexport default x;\n");
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(!content.contains("export"), "content was: {content}");
    }

    #[test]
    fn non_entry_star_left_unchanged() {
        let dep = crate::utils::make_dep("src/mid.ts", "export * from \"./leaf.js\";\n");
        let result = export_star_handler(vec![dep]);
        assert_eq!(
            result[0].content,
            "export * from \"./leaf.js\";\n"
        );
    }

    #[test]
    fn resolves_root_level_specifier() {
        // Entry at the project root; `./foo.js` must resolve to `foo.ts`
        // even though the joined path is `././foo.js`.
        let foo = crate::utils::make_dep("foo.ts", "export const a = 1;\n");
        let entry = make_entry("index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(content.contains("export { a };"), "content was: {content}");
    }

    #[test]
    fn resolves_directory_index_target() {
        let lib = crate::utils::make_dep("src/lib/index.ts", "export const a = 1;\n");
        let entry = make_entry("src/index.ts", "export * from \"./lib\";\n");
        let result = export_star_handler(vec![lib, entry]);
        let content = &result[1].content;
        assert!(content.contains("export { a };"), "content was: {content}");
    }

    #[test]
    fn type_only_star_expands_to_type_export() {
        let types = crate::utils::make_dep(
            "src/types.ts",
            "export interface I {}\nexport type T = number;\nexport const v = 1;\n",
        );
        let entry = make_entry("src/index.ts", "export type * from \"./types.js\";\n");
        let result = export_star_handler(vec![types, entry]);
        let content = &result[1].content;
        assert!(
            content.contains("export type { I, T };"),
            "content was: {content}"
        );
    }

    #[test]
    fn string_export_name_preserved() {
        let foo =
            crate::utils::make_dep("src/foo.ts", "const x = 1;\nexport { x as \"weird name\" };\n");
        let entry = make_entry("src/index.ts", "export * from \"./foo.js\";\n");
        let result = export_star_handler(vec![foo, entry]);
        let content = &result[1].content;
        assert!(
            content.contains("export { x as \"weird name\" };"),
            "content was: {content}"
        );
    }
}