//! Standalone lint module for the Susee bundler.
//!
//! This module exposes [`susee_lint`] — a checks-only entry point that runs
//! the same dependency-graph checks as the bundler pipeline but **without**
//! performing any bundling, module-type conversion, or code generation. It
//! returns structured [`LintResult`] diagnostics across the N-API boundary
//! so the main package (susee) can implement a `susee lint` CLI command.
//!
//! # Checks
//!
//! | Rule                | Gate | Description                                     |
//! |---------------------|------|-------------------------------------------------|
//! | `duplicate-decl`    | hard | Same top-level declaration name in 2+ files     |
//! | `undefined-usage`  | hard | Identifier reference never declared or imported |
//! | `export-default`   | opt  | Any `export default` statement                  |
//! | `anonymous-export` | opt  | Anonymous default export that is imported       |
//! | `npm-not-installed`| opt  | npm package imported but not in `node_modules`   |
//!
//! The first two are always run (hard gate). The remaining three are
//! controlled by [`LintOptions`].
//!
//! # Relationship to the bundler pipeline
//!
//! The bundler calls `run_default_check` (which prints to stderr and exits on
//! failure). This module instead collects the same [`CheckReport`]s from
//! [`crate::tree::checks::helpers`] and converts them into structured
//! [`LintDiagnostic`]s, giving the caller full control over formatting and
//! exit behavior.

use std::path::Path;

use napi_derive::napi;
use serde::{Deserialize, Serialize};

use crate::tree::checks::helpers::{
    CheckKind, CheckReport, check_anonymous, check_default_exports, check_duplicates,
    check_undefined_usage,
};
use crate::tree::checks::check_installed;
use crate::tree::index::get_deps;
use crate::tree::package_info::get_package_info;
use crate::types::DepsFile;

// ---------------------------------------------------------------------------
// N-API types
// ---------------------------------------------------------------------------

/// A single lint diagnostic, mapped from an internal [`CheckReport`] item.
#[napi(object)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LintDiagnostic {
    /// Severity: `"error"` for hard-gate checks, `"warning"` for optional ones.
    pub severity: String,
    /// Machine-readable rule name (e.g. `"duplicate-decl"`, `"undefined-usage"`).
    pub rule: String,
    /// File path relative to the project root.
    pub file: String,
    /// 1-based line number (0 when not applicable).
    pub line: u32,
    /// 1-based column number (0 when not applicable).
    pub column: u32,
    /// One-line human-readable message.
    pub message: String,
    /// Extra detail lines for context.
    pub details: Vec<String>,
}

/// Options controlling which optional checks [`susee_lint`] runs.
///
/// The two hard-gate checks (`duplicate-decl` and `undefined-usage`) always
/// run regardless of these options.
#[napi(object)]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LintOptions {
    /// When `Some(true)`, report every `export default` statement.
    pub check_default_exports: Option<bool>,
    /// When `Some(true)`, report anonymous default exports imported elsewhere.
    pub check_anonymous: Option<bool>,
    /// When `Some(true)`, verify npm packages are installed in `node_modules`.
    pub check_npm_installed: Option<bool>,
}

impl Default for LintOptions {
    /// All optional checks disabled — only the two hard-gate checks run.
    fn default() -> Self {
        Self {
            check_default_exports: Some(false),
            check_anonymous: Some(false),
            check_npm_installed: Some(false),
        }
    }
}

/// The structured result of a [`susee_lint`] run.
#[napi(object)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintResult {
    /// Diagnostics with `"error"` severity (hard-gate failures).
    pub errors: Vec<LintDiagnostic>,
    /// Diagnostics with `"warning"` severity (optional checks).
    pub warnings: Vec<LintDiagnostic>,
}

// ---------------------------------------------------------------------------
// CheckReport → LintDiagnostic conversion
// ---------------------------------------------------------------------------

/// Map a [`CheckKind`] to a machine-readable rule name.
fn kind_to_rule(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Duplicates => "duplicate-decl",
        CheckKind::UndefinedUsage => "undefined-usage",
        CheckKind::ExportDefault => "export-default",
        CheckKind::Anonymous => "anonymous-export",
    }
}

/// Whether a check kind is a hard gate (`"error"`) or optional (`"warning"`).
fn kind_severity(kind: CheckKind) -> &'static str {
    match kind {
        CheckKind::Duplicates | CheckKind::UndefinedUsage => "error",
        CheckKind::ExportDefault | CheckKind::Anonymous => "warning",
    }
}

/// Extract `(file, line, col)` from a `CheckItem`.
///
/// The `message` and `details` produced by the helpers embed location info
/// in varying formats. We try to parse `file:line:col` from the first detail
/// line; if that fails we fall back to `(file="", 0, 0)`.
fn extract_location(item: &crate::tree::checks::helpers::CheckItem) -> (String, u32, u32) {
    // Detail lines look like:
    //   "    at src/a.ts:3:5"
    //   "  location: src/a.ts:7:1"
    //   "  export:     src/b.ts:2:1  (`export default function`)"
    // Try to find the first `file:line:col` pattern.
    let lines = std::iter::once(&item.message)
        .chain(item.details.iter())
        .map(|s| s.as_str());

    for line in lines {
        if let Some(loc) = parse_file_line_col(line) {
            return loc;
        }
    }
    (String::new(), 0, 0)
}

/// Try to parse a `file:line:col` triple from a string.
///
/// Matches the first occurrence of `<path>:<digits>:<digits>`.
fn parse_file_line_col(s: &str) -> Option<(String, u32, u32)> {
    // Find the last `:` followed by digits, then the second-to-last `:`.
    let bytes = s.as_bytes();
    let mut colon_positions: Vec<usize> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            colon_positions.push(i);
        }
    }
    if colon_positions.len() < 2 {
        return None;
    }
    // Try the last two colons: line:col
    let n = colon_positions.len();
    let col_start = colon_positions[n - 1] + 1;
    let line_start = colon_positions[n - 2] + 1;
    let line_str = &s[line_start..colon_positions[n - 1]];
    let col_str = &s[col_start..];
    let line: u32 = line_str.trim().parse().ok()?;
    let col: u32 = col_str.trim().parse().ok()?;
    // The file path is everything before the line colon, trimmed of
    // whitespace and common prefixes like "at " or "location: ".
    let file_part = s[..colon_positions[n - 2]].trim();
    // Strip leading labels: "at ", "location: ", "export: ", "import: "
    let file = file_part
        .strip_prefix("at ")
        .or_else(|| file_part.strip_prefix("location: "))
        .unwrap_or(file_part)
        .trim()
        .to_string();
    Some((file, line, col))
}

/// Convert a single [`CheckReport`] into a vector of [`LintDiagnostic`]s.
fn report_to_diagnostics(report: &CheckReport) -> Vec<LintDiagnostic> {
    let rule = kind_to_rule(report.kind);
    let severity = kind_severity(report.kind);

    report
        .items
        .iter()
        .map(|item| {
            let (file, line, column) = extract_location(item);
            LintDiagnostic {
                severity: severity.to_string(),
                rule: rule.to_string(),
                file,
                line,
                column,
                message: item.message.clone(),
                details: item.details.clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run lint checks on a dependency tree without bundling.
///
/// Builds the dependency graph rooted at `entry` (resolved relative to
/// `root`), runs the two hard-gate checks (`duplicate-decl`,
/// `undefined-usage`) and any optional checks enabled in `options`, then
/// returns structured diagnostics.
///
/// # Arguments
///
/// * `entry` — Path to the entry file, relative to `root`.
/// * `root` — Optional project root (defaults to `"."`).
/// * `options` — [`LintOptions`] controlling optional checks. `None` uses
///   the defaults (only hard-gate checks).
///
/// # Returns
///
/// A [`LintResult`] with `errors` (hard-gate) and `warnings` (optional)
/// vectors. The function never exits the process — the caller decides how
/// to handle the diagnostics.
#[napi]
pub fn susee_lint(
    entry: String,
    root: Option<String>,
    options: Option<LintOptions>,
) -> LintResult {
    let dir_root = root
        .as_deref()
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    let opts = options.unwrap_or(LintOptions::default());

    let deps = match get_deps(&entry, dir_root, opts.check_npm_installed.unwrap_or(false)) {
        Ok(d) => d,
        Err(e) => {
            return LintResult {
                errors: vec![LintDiagnostic {
                    severity: "error".to_string(),
                    rule: "io-error".to_string(),
                    file: entry.clone(),
                    line: 0,
                    column: 0,
                    message: format!("Failed to read dependency tree: {e}"),
                    details: vec![],
                }],
                warnings: vec![],
            };
        }
    };

    let dep_files: &[DepsFile] = &deps.dep_files;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // --- Hard-gate checks (always run) ---
    let hard_reports = [check_duplicates(dep_files), check_undefined_usage(dep_files)];
    for report in &hard_reports {
        if report.has_issues() {
            errors.extend(report_to_diagnostics(report));
        }
    }

    // --- Optional: export default ---
    if opts.check_default_exports.unwrap_or(false) {
        let report = check_default_exports(dep_files);
        if report.has_issues() {
            warnings.extend(report_to_diagnostics(&report));
        }
    }

    // --- Optional: anonymous exports ---
    if opts.check_anonymous.unwrap_or(false) {
        let report = check_anonymous(dep_files);
        if report.has_issues() {
            warnings.extend(report_to_diagnostics(&report));
        }
    }

    // --- Optional: npm installed ---
    if opts.check_npm_installed.unwrap_or(false) {
        let pkg = get_package_info(dir_root);
        let npm = &deps.npm;
        let node_modules = dir_root.join("node_modules");
        for specifier in npm {
            let root_name = root_package_name(specifier);
            if root_name.is_empty() {
                continue;
            }
            if !node_modules.join(root_name).exists() {
                errors.push(LintDiagnostic {
                    severity: "error".to_string(),
                    rule: "npm-not-installed".to_string(),
                    file: String::new(),
                    line: 0,
                    column: 0,
                    message: format!("npm package \"{specifier}\" is not installed in node_modules"),
                    details: vec![
                        format!("  package: {specifier}"),
                        format!("  suggestion: run `npm install` to install missing packages"),
                    ],
                });
            }
        }
        // Silence unused variable warning when pkg is not otherwise used.
        let _ = &pkg;
    }

    LintResult { errors, warnings }
}

/// Normalize an npm specifier to its root package name.
///
/// Mirrors [`check_installed::root_package_name`] but that function is
/// private, so we re-implement it here.
fn root_package_name(specifier: &str) -> &str {
    let s = specifier.trim_start_matches('/');
    if s.starts_with('@') {
        let after_at = &s[1..];
        match after_at.find('/') {
            Some(first_slash) => {
                let after_scope = &after_at[first_slash + 1..];
                match after_scope.find('/') {
                    Some(second_slash) => &s[..1 + first_slash + 1 + second_slash],
                    None => s,
                }
            }
            None => s,
        }
    } else {
        match s.find('/') {
            Some(idx) => &s[..idx],
            None => s,
        }
    }
}

// Silence unused import in some configurations.
#[allow(unused_imports)]
use check_installed as _check_installed;