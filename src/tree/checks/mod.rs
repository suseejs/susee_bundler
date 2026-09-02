//! Dependency-graph pre-bundle checks.
//!
//! This module orchestrates the five individual checks implemented in
//! [`helpers`]. Each check walks the parsed [`DepsFile`]s of a dependency
//! tree (using the oxc AST) and produces a [`CheckReport`]. The checks are
//! read-only — they never mutate the tree — and are meant to surface issues
//! that should be fixed *before* bundling.
//!
//! # Check categories
//!
//! | Kind               | Gate | Description                                     |
//! |--------------------|------|-------------------------------------------------|
//! | Duplicates         | hard | Same declaration name in 2+ files               |
//! | MissingTypes       | hard | Declarations lacking type annotations/JSDoc      |
//! | UndefinedUsage     | hard | Use of an identifier that is never declared     |
//! | ExportDefault      | opt  | Any `export default` statement                  |
//! | Anonymous          | opt  | Anonymous default export that is imported       |
//!
//! The first three ("hard" gate) are run by [`run_default_check`]; the two
//! opt-in checks are run by [`run_check_opts_default_exports`] and
//! [`run_check_opts_anonymous`] respectively.
//!
//! # Reporting
//!
//! [`print_report`] renders a [`CheckReport`] to stderr with a colored
//! header, the issue count, and one indented block per issue (message plus
//! detail lines). A non-zero-style failure is reported through
//! [`susee_log::error`] when any hard-gate check finds issues.

mod helpers;

use crate::susee_log;
use crate::types::DepsFile;
use colored::Colorize;

use helpers::{
    CheckReport, check_anonymous, check_default_exports, check_duplicates, check_undefined_usage,
};

/// Run the three hard-gate checks ([`check_duplicates`],
/// [`check_missing_types`], [`check_undefined_usage`]) and aggregate their
/// reports.
///
/// Returns `Ok(())` when none of the three checks found issues, or `Err(())`
/// as soon as any one of them does. Each non-empty report is printed by
/// [`print_report`] before returning.
fn check_default(dep_files: &[DepsFile]) -> Result<(), ()> {
    let reports: Vec<CheckReport> = vec![
        check_duplicates(dep_files),
        check_undefined_usage(dep_files),
    ];

    let mut had_issue = false;
    for report in &reports {
        if report.has_issues() {
            print_report(report);
            had_issue = true;
        }
    }

    if had_issue { Err(()) } else { Ok(()) }
}
/// Run the optional [`check_default_exports`] check and print its report.
///
/// Returns `Ok(())` when no `export default` statements were found, or
/// `Err(())` when at least one was reported.
fn check_opts_default_exports(dep_files: &[DepsFile]) -> Result<(), ()> {
    let reports: Vec<CheckReport> = vec![check_default_exports(dep_files)];

    let mut had_issue = false;
    for report in &reports {
        if report.has_issues() {
            print_report(report);
            had_issue = true;
        }
    }

    if had_issue { Err(()) } else { Ok(()) }
}
/// Run the optional [`check_anonymous`] check and print its report.
///
/// Returns `Ok(())` when no anonymous default exports were imported, or
/// `Err(())` when at least one was reported.
fn check_opts_anonymous(dep_files: &[DepsFile]) -> Result<(), ()> {
    let reports: Vec<CheckReport> = vec![check_anonymous(dep_files)];

    let mut had_issue = false;
    for report in &reports {
        if report.has_issues() {
            print_report(report);
            had_issue = true;
        }
    }

    if had_issue { Err(()) } else { Ok(()) }
}

// ---------------------------------------------------------------------------
// Run-checks
// ---------------------------------------------------------------------------

/// Entry point for the default (hard-gate) check run.
///
/// Prints a banner, runs [`check_default`], and on success prints a green
/// confirmation. On failure it logs an error via [`susee_log::error`]
/// describing that the issues must be fixed (or the declaration renamed to a
/// named export) before bundling.
///
/// [`susee_log::error`]: susee_log::error
pub fn run_default_check(dep_files: &[DepsFile]) {
    println!("{}", "Susee running 3 default checks…".cyan().bold());
    match check_default(dep_files) {
        Ok(()) => {
            println!(
                "{}",
                "susee: no issues found in default checks ✓".green().bold()
            );
        }
        Err(()) => {
            let info = "Susee found issues that must be fixed before bundling.";
            let cause = "See the report above for file names, line positions, and \
                         suggested fixes. Each category that found issues must be \
                         resolved (or the declaration renamed to a named export).";
            susee_log::error(info, cause, true);
        }
    }
}

/// Entry point for the optional `export default` check.
///
/// Prints a banner, runs [`check_opts_default_exports`], and reports whether
/// any `export default` statements were found. On failure it logs an error
/// via [`susee_log::error`].
///
/// [`susee_log::error`]: susee_log::error
pub fn run_check_opts_default_exports(dep_files: &[DepsFile]) {
    println!("{}", "Susee running default_exports check…".cyan().bold());
    match check_opts_default_exports(dep_files) {
        Ok(()) => {
            println!("{}", "No default_exports found ✓".green().bold());
        }
        Err(()) => {
            let info = "Susee found default_exports that should be fixed before bundling.";
            let cause = "See the report above for file names, line positions, and suggested fixes.";
            susee_log::error(info, cause, true);
        }
    }
}

/// Entry point for the optional anonymous-exports check.
///
/// Prints a banner, runs [`check_opts_anonymous`], and reports whether any
/// anonymous default exports were imported. On failure it logs an error via
/// [`susee_log::error`].
///
/// [`susee_log::error`]: susee_log::error
pub fn run_check_opts_anonymous(dep_files: &[DepsFile]) {
    println!("{}", "Susee running default_exports check…".cyan().bold());
    match check_opts_anonymous(dep_files) {
        Ok(()) => {
            println!("{}", "No anonymous found ✓".green().bold());
        }
        Err(()) => {
            let info = "Susee found anonymous that should be fixed before bundling.";
            let cause = "See the report above for file names, line positions, and suggested fixes.";
            susee_log::error(info, cause, true);
        }
    }
}
// ---------------------------------------------------------------------------
// Pretty-printing
// ---------------------------------------------------------------------------

/// Pretty-print a single [`CheckReport`] to stderr.
///
/// The header line shows the check kind label (red, bold), a human-readable
/// header (yellow, bold), and the total issue count. Each [`CheckItem`] is
/// then printed as a bullet point with its one-line message followed by any
/// indented detail lines.
fn print_report(report: &CheckReport) {
    let header = match report.kind {
        helpers::CheckKind::Duplicates => "Duplicated declarations",
        helpers::CheckKind::Anonymous => "Anonymous imports/exports",
        helpers::CheckKind::ExportDefault => "export default usage",
        helpers::CheckKind::MissingTypes => "Missing type annotations",
        helpers::CheckKind::UndefinedUsage => "Undefined identifier usage",
    };

    eprintln!();
    eprintln!(
        "[{}] {} — {} issue(s)",
        report.kind.label().red().bold(),
        header.yellow().bold(),
        report.items.len()
    );
    for item in &report.items {
        eprintln!("  • {}", item.message);
        for detail in &item.details {
            eprintln!("      {}", detail);
        }
    }
}
