use susee_bundler::{CheckOptions, bundler};

fn main() {
    let entry = "__local__/cjs/index.cjs";
    let opts = CheckOptions::default();
    let bdl = bundler(entry, ".", opts).expect("Error").bundled_code;
    std::fs::write("aa.js", bdl).ok();
}
