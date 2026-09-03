//! # susee_bundler
//!
//! A TypeScript/JavaScript bundler that inline all local dependencies into a
//! single output file. It resolves CommonJS, ESM, CTS, and JSON modules,
//! normalizes anonymous/default exports, removes processed import/export
//! statements, and strips unused code.
//!
//! The crate is exposed as a Node.js native addon via N-API; see
//! [`susee_bundler`] for the main entry point.

mod bundler;
mod susee_fs;
mod susee_log;
mod tree;
mod types;
mod unique_name;
mod utils;

use napi_derive::napi;

/// Re-export of [`BundleResult`] from the bundler module.
pub use bundler::BundleResult;
use colored::*;
use std::path::Path;
use std::time::Instant;
pub use susee_fs::SuseeFs;
/// Re-export of [`CheckOptions`] from the tree module.
pub use tree::CheckOptions;

/// Print a formatted error message to stderr.
///
/// When `exist` is `Some(true)` the process will exit with code 1 after printing.
#[napi]
pub fn log_error(info: String, cause: String, exist: Option<bool>) {
    let e = exist.unwrap_or(false);
    susee_log::error(&info, &cause, e);
}
/// Print a formatted info message to stderr.
#[napi]
pub fn log_info(message: String) {
    susee_log::info(&message);
}
/// Print a formatted warning message to stderr.
#[napi]
pub fn log_warning(message: String) {
    susee_log::warning(&message);
}

/// A timer for measuring bundle and build elapsed time.
///
/// Construct a `LogTimer` at the start of an operation, then call
/// [`log_bundle_time`](Self::log_bundle_time) or [`log_build_time`](Self::log_build_time)
/// to print the elapsed duration.
#[napi]
pub struct LogTimer {
    start_time: Instant,
}
#[napi]
impl LogTimer {
    /// Create a new timer initialized to the current instant.
    #[napi(constructor)]
    pub fn new() -> Self {
        LogTimer {
            start_time: Instant::now(),
        }
    }

    /// Print the elapsed time since construction as a bundle-time measurement.
    #[napi]
    pub fn bundle_time(&self) {
        susee_log::bundle_time(self.start_time);
    }

    /// Print the elapsed time since construction as a build-time measurement.
    #[napi]
    pub fn build_time(&self) {
        susee_log::build_time(self.start_time);
    }
    /// Print the elapsed time
    #[napi]
    pub fn elapsed_time(&self, message: String) {
        let elapsed = self.start_time.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        eprintln!("{} : {}ms", &message.cyan().bold(), format!("{ms:.1}"));
    }
}

/// Bundle a TypeScript/JavaScript entry point into a single file.
///
/// `entry` is the relative path to the entry file. `root` optionally specifies
/// the project root (defaults to the current directory). `check_options`
/// controls optional pre-bundle checks; `None` uses the defaults.
///
/// Returns a [`BundleResult`] containing the bundled output and project type.
/// Panics if bundling fails.
#[napi]
pub fn susee_bundler(
    entry: String,
    root: Option<String>,
    check_options: Option<CheckOptions>,
) -> BundleResult {
    let dir_root = root
        .as_deref()
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    let opts = check_options.unwrap_or(CheckOptions::default());
    let bundled = bundler::bundler(&entry, dir_root, opts)
        .unwrap_or_else(|_| panic!("{}", format!("Error when bundling {entry}").magenta()));
    bundled
}
