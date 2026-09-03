//! Build script for the susee_bundler crate.
//!
//! Invokes [`napi_build::setup`] to register the N-API build-time hooks.

fn main() {
    napi_build::setup();
}
