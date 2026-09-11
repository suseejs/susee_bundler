//! Verify that collected npm module specifiers are actually installed.
//!
//! The check runs in two phases:
//!
//! 1. **package.json check** — every imported specifier must be listed in the
//!    project's `package.json` (`dependencies` or `devDependencies`). If a
//!    specifier is imported but not declared, an error is reported and the
//!    process exits. This catches transitive dependencies that happen to be
//!    in `node_modules/` because another package pulled them in — they must
//!    still be explicitly declared in `package.json`.
//!
//! 2. **node_modules check** — every declared specifier must have a
//!    corresponding entry under `node_modules/`. Symlinks are followed (via
//!    `std::fs::metadata`), so pnpm's symlinked `node_modules/` structure is
//!    handled transparently. If a specifier is declared but not installed, an
//!    error is reported and the process exits.
//!
//! Scoped packages (`@scope/name`) and subpath imports (`pkg/sub/path`) are
//! normalized to their root package name before checking.
use std::path::Path;

use colored::*;

use crate::tree::package_info::PackageInfo;

/// Normalize an npm specifier to its root package name.
///
/// Examples:
/// - `react` → `react`
/// - `@scope/pkg` → `@scope/pkg`
/// - `@scope/pkg/sub/path` → `@scope/pkg`
/// - `react/jsx-runtime` → `react`
fn root_package_name(specifier: &str) -> &str {
    let s = specifier.trim_start_matches('/');
    if s.starts_with('@') {
        // Scoped package: `@scope/name[/sub...]`
        // The root package name is everything up to (but not including) the
        // second `/`, or the whole string if there is no second slash.
        let after_at = &s[1..];
        match after_at.find('/') {
            Some(first_slash) => {
                let after_scope = &after_at[first_slash + 1..];
                match after_scope.find('/') {
                    Some(second_slash) => {
                        // Include `@scope` + `/` + name up to second slash.
                        &s[..1 + first_slash + 1 + second_slash]
                    }
                    None => s, // `@scope/name` with no subpath
                }
            }
            None => s, // Malformed `@scope` without a name; return as-is.
        }
    } else {
        // Unscoped package: `name[/sub...]`
        match s.find('/') {
            Some(idx) => &s[..idx],
            None => s,
        }
    }
}

/// Check that every npm specifier in `npm_modules` is declared in
/// `package.json` **and** physically installed in `node_modules/`.
///
/// The check runs in two phases:
///
/// 1. **package.json** — each specifier (normalized to its root package name)
///    must appear in `pkg.all` (which includes both `dependencies` and
///    `devDependencies`). Specifiers not listed are collected into a
///    "not declared" error list. If any are found, an error is logged and the
///    process exits *before* proceeding to the node_modules check.
///
/// 2. **node_modules** — each specifier must have a corresponding directory (or
///    symlink target) under `node_modules/`. Symlinks are followed via
///    `std::fs::metadata` so pnpm's symlinked structure works correctly.
///    Missing entries are collected into a "not installed" error list.
///
/// `pkg` is the parsed `package.json` info for the project rooted at `root`.
/// `root` is the project root directory (where `node_modules/` lives).
///
/// Returns `Ok(())` if all specifiers pass both checks, or `Err(())` if at
/// least one fails. When failures are found, an error is logged via
/// [`susee_log::error`] with `exit = true`, which terminates the process with
/// exit code 1.
pub fn check_npm_installed(
    npm_modules: &[String],
    pkg: &PackageInfo,
    root: &Path,
) -> Result<(), ()> {
    println!("{}", "");
    println!("{}", "Running npm dependencies check…".cyan());

    let node_modules = root.join("node_modules");

    // -----------------------------------------------------------------------
    // Phase 1 — package.json declaration check
    // -----------------------------------------------------------------------
    // Every imported npm package must be explicitly listed in the project's
    // package.json. A package that is only present in node_modules because
    // another dependency pulled it in (a transitive / hoisted dependency) is
    // NOT sufficient — it must be declared explicitly.
    let mut not_declared: Vec<String> = Vec::new();

    for specifier in npm_modules {
        let root_name = root_package_name(specifier);
        if root_name.is_empty() {
            continue;
        }

        if !pkg.contains(root_name) {
            not_declared.push(specifier.clone());
        }
    }

    if !not_declared.is_empty() {
        let info = "Found npm dependencies that are imported but not declared in package.json.";
        let cause = format!(
            "The following npm packages are imported but not listed in \
             package.json dependencies or devDependencies. Add them to \
             package.json and run `npm install` (or `pnpm install`) to fix:\n{}",
            not_declared
                .iter()
                .map(|m| format!("  - {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        crate::susee_log::error(info, &cause, true);
        // Unreachable — `susee_log::error` with `exit = true` exits the process.
        return Err(());
    }

    // -----------------------------------------------------------------------
    // Phase 2 — node_modules installation check
    // -----------------------------------------------------------------------
    // After confirming every specifier is declared, verify that each one is
    // actually installed. We use `std::fs::metadata` (which follows symlinks)
    // instead of `Path::exists` so that pnpm's symlinked node_modules structure
    // is handled correctly — pnpm stores packages in a content-addressable
    // store and creates symlinks in node_modules/<pkg> pointing to the store.
    let mut not_installed: Vec<String> = Vec::new();

    for specifier in npm_modules {
        let root_name = root_package_name(specifier);
        if root_name.is_empty() {
            continue;
        }

        let pkg_path = node_modules.join(root_name);
        // `fs::metadata` follows symlinks — returns Ok only if the symlink
        // target exists. `Path::exists` also follows symlinks on most
        // platforms, but `fs::metadata` is explicit and portable.
        let is_installed = std::fs::metadata(&pkg_path).is_ok();

        if !is_installed {
            not_installed.push(specifier.clone());
        }
    }

    if not_installed.is_empty() {
        println!("{}", "All npm dependencies are installed ✓".green());
        println!("{}", "");
        Ok(())
    } else {
        let info = "Found npm dependencies that are declared but not installed.";
        let cause = format!(
            "The following npm packages are listed in package.json but not \
             installed in node_modules (run `npm install` or `pnpm install` \
             to fix):\n{}",
            not_installed
                .iter()
                .map(|m| format!("  - {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        crate::susee_log::error(info, &cause, true);
        // Unreachable — `susee_log::error` with `exit = true` exits the process.
        Err(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::package_info::get_package_info;
    use std::fs;
    use tempfile::tempdir;

    /// Helper: create a `node_modules/<name>/package.json` stub.
    fn install_pkg(root: &Path, name: &str) {
        let dir = root.join("node_modules").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"0.0.0"}}"#),
        )
        .unwrap();
    }

    /// Helper: write a `package.json` with the given dependencies JSON.
    fn write_pkg(root: &Path, deps_json: &str) {
        fs::write(
            root.join("package.json"),
            format!(r#"{{"dependencies":{deps_json}}}"#),
        )
        .unwrap();
    }

    // -- root_package_name unit tests (pure, no fs) --------------------------

    #[test]
    fn root_name_unscoped() {
        assert_eq!(root_package_name("react"), "react");
    }

    #[test]
    fn root_name_unscoped_subpath() {
        assert_eq!(root_package_name("react/jsx-runtime"), "react");
    }

    #[test]
    fn root_name_scoped() {
        assert_eq!(root_package_name("@scope/pkg"), "@scope/pkg");
    }

    #[test]
    fn root_name_scoped_subpath() {
        assert_eq!(root_package_name("@scope/pkg/sub/path"), "@scope/pkg");
    }

    // -- check_npm_installed integration tests -------------------------------
    //
    // NOTE: `susee_log::error` with `exit = true` calls `std::process::exit`,
    // which cannot be tested in-process. The tests below exercise only the
    // *success* path (all installed) and rely on the structure of
    // `check_npm_installed` returning `Ok(())` without triggering the error
    // branch.

    #[test]
    fn check_ok_when_all_listed_in_package_json_and_installed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"react":"^18.0.0"}"#);
        install_pkg(root, "react");
        let pkg = get_package_info(root);

        let npm = vec!["react".to_string()];
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }

    #[test]
    fn check_ok_when_all_in_node_modules_and_declared() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"react":"^18.0.0"}"#);
        install_pkg(root, "react");
        let pkg = get_package_info(root);

        let npm = vec!["react".to_string()];
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }

    #[test]
    fn check_ok_for_subpath_import_when_installed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"react":"^18.0.0"}"#);
        install_pkg(root, "react");
        let pkg = get_package_info(root);

        let npm = vec!["react/jsx-runtime".to_string()];
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }

    #[test]
    fn check_ok_for_scoped_subpath_when_installed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"@scope/pkg":"^1.0.0"}"#);
        install_pkg(root, "@scope/pkg");
        let pkg = get_package_info(root);

        let npm = vec!["@scope/pkg/sub/path".to_string()];
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }

    #[test]
    fn check_ok_empty_npm_list() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{}"#);
        let pkg = get_package_info(root);

        let npm: Vec<String> = Vec::new();
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }

    /// When a package is in node_modules but NOT declared in package.json
    /// (e.g. a transitive dependency hoisted by another package), the check
    /// should fail at the package.json phase. We replicate the internal logic
    /// to confirm `pkg.contains` returns false.
    #[test]
    fn check_fails_when_in_node_modules_but_not_in_package_json() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{}"#);
        install_pkg(root, "left-pad");
        let pkg = get_package_info(root);

        // The package is physically in node_modules...
        let in_node_modules = root.join("node_modules").join("left-pad").exists();
        assert!(in_node_modules);
        // ...but NOT declared in package.json.
        assert!(!pkg.contains("left-pad"));
    }

    /// When a package is listed in `package.json` but not physically installed
    /// in `node_modules`, the check should fail at the node_modules phase.
    /// We replicate the internal logic to confirm the directory check returns
    /// `false`.
    #[test]
    fn check_fails_when_listed_but_not_installed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"react":"^18.0.0"}"#);
        // Deliberately do NOT create node_modules/react.
        let pkg = get_package_info(root);

        // The internal directory check should report it as missing.
        let in_node_modules = root.join("node_modules").join("react").exists();
        assert!(!in_node_modules);
        // pkg.contains still returns true (listed in package.json).
        assert!(pkg.contains("react"));
    }

    /// When a pnpm-style symlink exists in node_modules, `fs::metadata`
    /// (which follows symlinks) should report it as installed.
    #[test]
    fn check_ok_with_pnpm_symlink() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write_pkg(root, r#"{"react":"^18.0.0"}"#);

        // Simulate pnpm: create a real package in a store, then symlink it.
        let store_dir = root.join(".pnpm-store");
        let real_pkg = store_dir.join("react");
        fs::create_dir_all(&real_pkg).unwrap();
        fs::write(
            real_pkg.join("package.json"),
            r#"{"name":"react","version":"18.0.0"}"#,
        )
        .unwrap();

        let nm_dir = root.join("node_modules");
        fs::create_dir_all(&nm_dir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_pkg, nm_dir.join("react")).unwrap();
        }
        // On non-Unix platforms, just create a regular directory (symlink
        // support varies). The test still validates the metadata-based check.
        #[cfg(not(unix))]
        {
            fs::create_dir_all(nm_dir.join("react")).unwrap();
            fs::write(
                nm_dir.join("react").join("package.json"),
                r#"{"name":"react","version":"18.0.0"}"#,
            )
            .unwrap();
        }

        let pkg = get_package_info(root);
        let npm = vec!["react".to_string()];
        let result = check_npm_installed(&npm, &pkg, root);
        assert!(result.is_ok());
    }
}
