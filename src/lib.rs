mod bundler;
mod susee_log;
mod tree;
mod types;
mod unique_name;
mod utils;

use napi_derive::napi;

pub use bundler::BundleResult;
use colored::*;
use std::path::Path;
use std::time::Instant;
pub use tree::CheckOptions;

#[napi]
pub fn susee_bundler(
    entry: String,
    root: Option<String>,
    check_options: Option<CheckOptions>,
) -> BundleResult {
    let start = Instant::now();
    let dir_root = root
        .as_deref()
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."));
    let opts = check_options.unwrap_or(CheckOptions::default());
    let bundled = bundler::bundler(&entry, dir_root, opts)
        .unwrap_or_else(|_| panic!("{}", "Error when bundling".magenta()));
    susee_log::bundle_time(start);
    bundled
}
