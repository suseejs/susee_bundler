//! Colored console output utilities for SuSee.
//!
//! Provides [`error`], [`info`], and [`warning`] helpers that print
//! formatted messages to stderr. [`error`] with `e = true` calls
//! `std::process::exit(1)` after printing.
//!
//! [`build_time`] and [`bundle_time`] measure and print elapsed time
//! for the build and bundle operations respectively.

use colored::*;
use std::time::Instant;

/// Print a formatted error message to stderr.
///
/// When `e` is `true` the process exits with code 1 after printing.
pub fn error(info: &str, cause: &str, e: bool) {
    eprintln!("[{}]", "error".red().bold());
    eprintln!(" info  : {}", info);
    eprintln!(" cause : {}", cause);
    if e {
        std::process::exit(1)
    }
}
/// Print a formatted info message to stderr.
pub fn info(message: &str) {
    eprintln!("[{}]", "info".green().bold());
    eprintln!(" {}", message);
}

/// Print a formatted warning message to stderr.
pub fn warning(message: &str) {
    eprintln!("[{}]", "warning".yellow().bold());
    eprintln!(" {}", message);
}
/// Print the elapsed time since `start` as a bundle-time measurement in milliseconds.
pub fn bundle_time(start: Instant) {
    let elapsed = start.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    eprintln!("[{}] : {}ms", "Bundled".cyan().bold(), format!("{ms:.1}"));
}

/// Print the elapsed time since `start` as a build-time measurement in milliseconds.
pub fn build_time(start: Instant) {
    let elapsed = start.elapsed();
    let ms = elapsed.as_secs_f64() * 1000.0;
    eprintln!("[{}] : {}ms", "Build".cyan().bold(), format!("{ms:.1}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn bundle_time_does_not_panic() {
        let start = Instant::now();
        // Just verify it doesn't panic and produces output.
        bundle_time(start);
    }

    #[test]
    fn warning_does_not_panic() {
        warning("test warning message");
    }

    #[test]
    fn info_does_not_panic() {
        info("test info message");
    }

    // Note: `error(info, cause, true)` calls `std::process::exit(1)` which
    // cannot be tested in-process. We test only the non-exiting variant.
    #[test]
    fn error_non_exit_does_not_panic() {
        error("test info", "test cause", false);
    }
}
