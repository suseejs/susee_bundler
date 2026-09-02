//! Read and parse `package.json` dependency information.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::Value;

/// Information about a single dependency's entry in `node_modules/<dep>/package.json`.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct DepMeta {
    /// The `"type"` field (e.g. `"module"` or `"commonjs"`).
    pub r#type: Option<String>,
    /// The `"main"` field (CommonJS entry).
    pub main: Option<String>,
    /// The `"module"` field (ESM entry).
    pub module: Option<String>,
    /// The `"types"` field (TypeScript declarations).
    pub types: Option<String>,
    /// The `"exports"` field (conditional exports map).
    pub exports: Option<Value>,
}

/// Information about a `@types/*` dependency.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct TypeDepMeta {
    /// The `"types"` field.
    pub types: Option<String>,
    /// The `"exports"` field.
    pub exports: Option<Value>,
}

/// Parsed `package.json` info for the project.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PackageInfo {
    /// The project's own `"type"` field (e.g. `"module"`).
    pub r#type: String,
    /// Metadata for each non-`@types` dependency.
    pub deps: BTreeMap<String, DepMeta>,
    /// Metadata for each `@types/*` dependency.
    pub type_deps: BTreeMap<String, TypeDepMeta>,
    /// All dependency names (dependencies + devDependencies), including `@types/*`.
    pub all: Vec<String>,
}

impl PackageInfo {
    /// Check whether `name` is a known project dependency.
    #[allow(dead_code)]
    pub fn contains(&self, name: &str) -> bool {
        self.all.iter().any(|d| d == name)
    }
}

/// Read `package.json` from `root` and collect dependency metadata.
///
/// If `package.json` is missing or unreadable, returns an empty `PackageInfo`.
pub fn get_package_info(root: &Path) -> PackageInfo {
    let package_json_path = root.join("package.json");
    let node_modules_path = root.join("node_modules");

    let pkg: Value = match fs::read_to_string(&package_json_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => {
            return PackageInfo {
                r#type: String::new(),
                deps: BTreeMap::new(),
                type_deps: BTreeMap::new(),
                all: Vec::new(),
            };
        }
    };

    let pkg_type = pkg
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let deps_keys = collect_keys(&pkg, "dependencies");
    let dev_deps_keys = collect_keys(&pkg, "devDependencies");

    let mut all_deps: Vec<String> = deps_keys
        .iter()
        .chain(dev_deps_keys.iter())
        .cloned()
        .collect();
    all_deps.sort();
    all_deps.dedup();

    let dependencies: Vec<String> = all_deps
        .iter()
        .filter(|d| !d.starts_with("@types/"))
        .cloned()
        .collect();
    let types_dependencies: Vec<String> = all_deps
        .iter()
        .filter(|d| d.starts_with("@types/"))
        .cloned()
        .collect();

    let mut deps_map: BTreeMap<String, DepMeta> = BTreeMap::new();
    for dep in &dependencies {
        if dep == "typescript" {
            continue;
        }
        let pj_path = node_modules_path.join(dep).join("package.json");
        if let Some(dep_pkg) = read_json(&pj_path) {
            deps_map.insert(
                dep.clone(),
                DepMeta {
                    r#type: dep_pkg
                        .get("type")
                        .and_then(Value::as_str)
                        .map(String::from),
                    main: dep_pkg
                        .get("main")
                        .and_then(Value::as_str)
                        .map(String::from),
                    module: dep_pkg
                        .get("module")
                        .and_then(Value::as_str)
                        .map(String::from),
                    types: dep_pkg
                        .get("types")
                        .and_then(Value::as_str)
                        .map(String::from),
                    exports: dep_pkg.get("exports").cloned(),
                },
            );
        }
    }

    let mut type_deps_map: BTreeMap<String, TypeDepMeta> = BTreeMap::new();
    for dep in &types_dependencies {
        if dep == "@types/node" {
            continue;
        }
        let pj_path = node_modules_path.join(dep).join("package.json");
        if let Some(dep_pkg) = read_json(&pj_path) {
            type_deps_map.insert(
                dep.clone(),
                TypeDepMeta {
                    types: dep_pkg
                        .get("types")
                        .and_then(Value::as_str)
                        .map(String::from),
                    exports: dep_pkg.get("exports").cloned(),
                },
            );
        }
    }

    PackageInfo {
        r#type: pkg_type,
        deps: deps_map,
        type_deps: type_deps_map,
        all: all_deps,
    }
}

fn collect_keys(pkg: &Value, field: &str) -> Vec<String> {
    pkg.get(field)
        .and_then(Value::as_object)
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

fn read_json(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_pkg(root: &Path, json: &str) {
        fs::write(root.join("package.json"), json).unwrap();
    }

    #[test]
    fn missing_package_json_returns_empty() {
        let dir = tempdir().unwrap();
        let info = get_package_info(dir.path());
        assert!(info.all.is_empty());
        assert!(info.deps.is_empty());
        assert!(info.type_deps.is_empty());
        assert_eq!(info.r#type, "");
    }

    #[test]
    fn reads_type_field() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#"{"type":"module"}"#);
        let info = get_package_info(dir.path());
        assert_eq!(info.r#type, "module");
    }

    #[test]
    fn collects_dependencies_and_dev_dependencies() {
        let dir = tempdir().unwrap();
        write_pkg(
            dir.path(),
            r#"{"dependencies":{"react":"^18.0.0"},"devDependencies":{"@types/react":"^18.0.0","typescript":"^5.0.0"}}"#,
        );
        let info = get_package_info(dir.path());
        assert!(info.contains("react"));
        assert!(info.contains("@types/react"));
        // `all` includes both deps and devDeps
        assert!(info.all.iter().any(|d| d == "react"));
        assert!(info.all.iter().any(|d| d == "@types/react"));
    }

    #[test]
    fn contains_unknown_returns_false() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#"{"dependencies":{"react":"^18.0.0"}}"#);
        let info = get_package_info(dir.path());
        assert!(!info.contains("vue"));
    }

    #[test]
    fn deps_meta_read_from_node_modules() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), r#"{"dependencies":{"react":"^18.0.0"}}"#);
        // Create node_modules/react/package.json with a "main" field
        let nm = dir.path().join("node_modules").join("react");
        fs::create_dir_all(&nm).unwrap();
        fs::write(
            nm.join("package.json"),
            r#"{"main":"index.js","types":"index.d.ts"}"#,
        )
        .unwrap();

        let info = get_package_info(dir.path());
        let meta = info.deps.get("react").expect("react meta should be read");
        assert_eq!(meta.main.as_deref(), Some("index.js"));
        assert_eq!(meta.types.as_deref(), Some("index.d.ts"));
    }

    #[test]
    fn types_deps_excluded_from_deps_map() {
        let dir = tempdir().unwrap();
        write_pkg(
            dir.path(),
            r#"{"devDependencies":{"@types/react":"^18.0.0","@types/node":"^20.0.0"}}"#,
        );
        let info = get_package_info(dir.path());
        // @types/node is skipped per the implementation, @types/react goes to type_deps
        assert!(!info.type_deps.contains_key("@types/node"));
    }

    #[test]
    fn invalid_json_returns_empty() {
        let dir = tempdir().unwrap();
        write_pkg(dir.path(), "not json");
        let info = get_package_info(dir.path());
        assert!(info.all.is_empty());
    }
}
