mod anonymous;
mod export_default;
mod remove_handler;
mod unused_code;

/// Re-export of `anonymous_handler` — normalises anonymous exports/imports.
pub use anonymous::anonymous_handler;
/// Re-export of `export_default_handler` — handles named default exports.
pub use export_default::export_default_handler;
/// Re-export of `remove_handler` — removes processed import/export statements.
pub use remove_handler::remove_handler;
/// Re-export of `clean` — removes unused code post-bundle.
pub use unused_code::clean;
