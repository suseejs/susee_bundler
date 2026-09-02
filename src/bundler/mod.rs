mod hooks;

use crate::tree::susee_tree;
use crate::types::ProjectType;
use crate::utils::{is_non_local_import, merge_content, merge_imports_statement};
use hooks::{anonymous_handler, clean, export_default_handler, remove_handler};
use std::path::Path;

use napi_derive::napi;

#[napi]
/// The output of a successful bundle operation.
///
/// Contains the final bundled source code and the detected `ProjectType`
/// (TS, JS, or MIXED) of the project.

pub struct BundleResult {
    /// The final bundled source code, pretty-printed via oxc's codegen.
    pub bundled_code: String,
    /// The detected project type (TS / JS / MIXED).
    pub project_type: ProjectType,
}

/// Bundle a TypeScript/JavaScript project entry point into a single file.
///
/// This is the main entry point of the `susee_bundler` module. It takes an
/// entry file path and a project root, resolves the full dependency tree,
/// applies tree-shaking hooks, strips and re-emits non-local imports,
/// concatenates all local source, removes unused code, and pretty-prints
/// the result.
///
/// # Arguments
///
/// * `entry` — The entry file path, relative to `root` (e.g. `"src/index.ts"`).
/// * `root` — The project root directory. Anything implementing [`AsRef<Path>`]
///   is accepted (e.g. `&str`, `PathBuf`, `&Path`).
/// * `opts` — [`CheckOptions`](crate::tree::CheckOptions) controlling the
///   optional checks (npm, default exports, anonymous). When a check field
///   is `Some(true)`, the corresponding *check* (diagnostics only) runs instead
///   of the *handler* (which renames/normalizes). When `None` or `Some(false)`,
///   the handler runs during normal bundling.
///
/// # Returns
///
/// `Ok(BundleResult)` on success, or `Err(io::Error)` if the entry file or any
/// dependency cannot be read.
///
/// # Pipeline
///
/// 1. `susee_tree` — resolve the dependency tree and detect `ProjectType`.
/// 2. [`export_default_handler`] — normalize named default exports (skipped
///    when `opts.check_default_exports == Some(true)`).
/// 3. [`anonymous_handler`] — name anonymous default exports (skipped when
///    `opts.check_anonymous == Some(true)`).
/// 4. [`remove_handler`] — strip all import/export syntax; collect removed
///    import text for re-emission.
/// 5. Filter removed imports to non-local only (npm packages, node built-ins)
///    via [`is_non_local_import`], then merge duplicates via
///    [`merge_imports_statement`].
/// 6. [`merge_content`] — concatenate all dependency file contents with
///    `//path` separator comments.
/// 7. Remove empty lines and stray `;`-only lines left by import removal.
/// 8. [`clean`] — tree-shake unused declarations from the concatenated bundle.
/// 9. [`pretty_print`] — round-trip through oxc's codegen for normalized
///    formatting.
///
/// # Errors
///
/// Returns `Err` if `susee_tree` fails to resolve the entry file or any of
/// its dependencies.
///
pub fn bundler<P: AsRef<Path>>(
    entry: &str,
    root: P,
    opts: crate::tree::CheckOptions,
) -> std::io::Result<BundleResult> {
    let tree = susee_tree(entry, root, Some(opts.clone()))?;
    let project_type = tree.project_type;
    let mut dep_files: Vec<super::types::DepsFile> = tree.dep_files;
    let cde = opts.check_default_exports.unwrap_or(false);
    let ca = opts.check_anonymous.unwrap_or(false);
    if !cde {
        dep_files = export_default_handler(dep_files);
    }
    if !ca {
        dep_files = anonymous_handler(dep_files);
    }
    let (deps_files, removed_imports) = remove_handler(dep_files);
    let removed_statements: Vec<String> = removed_imports.into_iter().map(|r| r.text).collect();
    let mut removed_stats = removed_statements;
    removed_stats.retain(|s| is_non_local_import(s));
    removed_stats = merge_imports_statement(&removed_stats);
    let import_statements = removed_stats.join("\n").trim().to_string();
    let (dep_files_content, main_file_content) = merge_content(&deps_files);

    let mut content = format!("{import_statements}\n{dep_files_content}\n{main_file_content}");

    // Remove empty lines and lines that start with ";" that remain after
    // removing imports.
    //
    // IMPORTANT: only trim inside the filter predicate — do NOT `.map(|line|
    // line.trim())`, which would corrupt multi-line template literals and
    // other source where indentation is significant.
    //
    // Use `&&` so that BOTH conditions must hold for a line to be kept
    // (the previous `||` kept everything — see Bug 1 in repo memory).
    content = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with(';')
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    let file = if project_type == ProjectType::JS {
        "bundle.js"
    } else {
        // Both MIXED and TS parse as TypeScript.
        "bundle.ts"
    };
    content = clean(&content, file);
    // Pretty-print the bundled output by round-tripping it through oxc's
    // codegen, which re-indents and normalizes formatting.
    content = pretty_print(&content, file);
    Ok(BundleResult {
        bundled_code: content,
        project_type,
    })
}

/// Pretty-print a JS/TS source string by parsing it and regenerating it with
/// oxc's codegen.
///
/// Comments are preserved (`normal`, `jsdoc`, `annotation` all enabled) so
/// that file-separator comments (`// path.ts`) and JSDoc blocks survive the
/// round-trip. Legal comments are inlined.
///
/// # Arguments
///
/// * `content` — The source string to pretty-print.
/// * `file` — A file name used only to determine the [`SourceType`] for
///   parsing. `"bundle.js"` is treated as TypeScript (`"bundle.ts"`) so that
///   any remaining TS syntax is handled; pure JS is a subset of TS so this is
///   safe.
///
/// # Fallback
///
/// If parsing fails (panicked or has diagnostics), the original `content` is
/// returned unchanged rather than producing empty or partial output.
///
/// [`SourceType`]: oxc::span::SourceType
fn pretty_print(content: &str, file: &str) -> String {
    use oxc::allocator::Allocator;
    use oxc::codegen::{Codegen, CodegenOptions, CommentOptions};
    use oxc::parser::Parser;
    use oxc::span::SourceType;

    let ts_file = if file == "bundle.js" {
        "bundle.ts"
    } else {
        file
    };
    let path = std::path::Path::new(ts_file);
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parser_return = Parser::new(&allocator, content, source_type).parse();
    if !parser_return.panicked && parser_return.diagnostics.is_empty() {
        let options = CodegenOptions {
            single_quote: true,
            comments: CommentOptions {
                normal: true,
                jsdoc: true,
                annotation: true,
                legal: oxc::codegen::LegalComment::Inline,
            },
            ..CodegenOptions::default()
        };
        Codegen::new()
            .with_options(options)
            .build(&parser_return.program)
            .code
    } else {
        // If parsing fails (e.g. invalid syntax), keep the original content
        // rather than dropping the bundle on the floor.
        content.to_string()
    }
}
