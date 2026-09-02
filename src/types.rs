//! Share types for susee

use oxc::ast::ast::Expression;
use oxc::ast_visit::Visit;
use serde::{Deserialize, Serialize};

use napi_derive::napi;

/// File extensions considered valid for JS/TS/JSON modules.
///
/// Serialized as a string with a leading dot (e.g. `".ts"`) to match the
/// TypeScript `fileExt` field in `DepsFile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidExts {
    /// `.js` — CommonJS/ESM JavaScript.
    Js,
    /// `.cjs` — CommonJS JavaScript.
    Cjs,
    /// `.mjs` — ESM JavaScript.
    Mjs,
    /// `.ts` — TypeScript.
    Ts,
    /// `.cts` — CommonJS TypeScript.
    Cts,
    /// `.mts` — ESM TypeScript.
    Mts,
    /// `.tsx` — TypeScript with JSX.
    Tsx,
    /// `.jsx` — JavaScript with JSX.
    Jsx,
    /// `.json` — JSON module.
    Json,
}
impl Serialize for ValidExts {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_ext_str())
    }
}

impl<'de> Deserialize<'de> for ValidExts {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ValidExts::from_path_ext(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown file extension: {s}")))
    }
}
impl ValidExts {
    /// Parse an extension (without leading dot) into a [`ValidExts`].
    pub fn from_ext(ext: &str) -> Option<Self> {
        Some(match ext {
            "js" => Self::Js,
            "cjs" => Self::Cjs,
            "mjs" => Self::Mjs,
            "ts" => Self::Ts,
            "cts" => Self::Cts,
            "mts" => Self::Mts,
            "tsx" => Self::Tsx,
            "jsx" => Self::Jsx,
            "json" => Self::Json,
            _ => return None,
        })
    }

    /// Parse a file extension including the leading dot (e.g. `.ts`).
    pub fn from_path_ext(ext: &str) -> Option<Self> {
        let trimmed = ext.strip_prefix('.').unwrap_or(ext);
        Self::from_ext(trimmed)
    }

    /// Return the extension including the leading dot (e.g. `.ts`).
    pub fn as_ext_str(&self) -> &'static str {
        match self {
            Self::Js => ".js",
            Self::Cjs => ".cjs",
            Self::Mjs => ".mjs",
            Self::Ts => ".ts",
            Self::Cts => ".cts",
            Self::Mts => ".mts",
            Self::Tsx => ".tsx",
            Self::Jsx => ".jsx",
            Self::Json => ".json",
        }
    }
}
/// The module system a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleType {
    /// CommonJS module.
    Cjs,
    /// ECMAScript module.
    Esm,
    /// CommonJS TypeScript module (`.cts`).
    Cts,
    /// JSON module.
    Json,
}

impl ModuleType {
    /// Return the module type as a lowercase string (e.g. `cjs`, `esm`).
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cjs => "cjs",
            Self::Esm => "esm",
            Self::Cts => "cts",
            Self::Json => "json",
        }
    }
}
/// The type of project, determined by the languages used in its source files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectType {
    /// A purely TypeScript project.
    TS,
    /// A purely JavaScript project.
    JS,
    /// A project mixing both TypeScript and JavaScript.
    MIXED,
}

/// A single dependency file entry in the dependency tree.
///
/// All JSON fields use `snake_case` (e.g. `module_type`, `file_ext`,
/// `is_jsx`, `is_entry`) for a consistent naming convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepsFile {
    /// File path relative to the project root (using `/` separators).
    pub file: String,
    /// File contents as a UTF-8 string.
    pub content: String,
    /// File size in bytes.
    pub bytes: usize,
    /// Module format (cjs / esm / json).
    pub module_type: ModuleType,
    /// Resolved file extension.
    pub file_ext: ValidExts,
    /// Whether the file contains JSX syntax.
    pub is_jsx: bool,
    /// Whether this is the entry file.
    pub is_entry: bool,
}
/// The full dependency tree built from a project entry point.
///
/// All JSON fields use `snake_case` for a consistent naming convention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependenciesTree {
    /// The entry file path (relative to root).
    pub entry: String,
    /// NPM dependencies referenced by the entry and its dependencies.
    pub npm: Vec<String>,
    /// Node.js built-in modules referenced.
    pub nodes: Vec<String>,
    /// Unknown/unresolved module specifiers collected as warnings.
    pub warns: Vec<String>,
    /// The sorted list of dependency files.
    pub dep_files: Vec<DepsFile>,
    /// Type of the project
    pub project_type: ProjectType,
}

/// Detects the module system used by a source file by walking its AST.
///
/// The detector flags which module formats are present so callers can
/// classify a file as ESM, CommonJS, or CommonJS-with-types (CTS).
#[derive(Default)]
pub struct ModuleTypeDetector {
    /// `true` if the file contains ESM syntax (e.g. `import`/`export`).
    pub is_esm: bool,
    /// `true` if the file contains CommonJS syntax (e.g. `require`/`module.exports`).
    pub is_common_js: bool,
    /// `true` if the file is a TypeScript CommonJS file (`.cts`).
    pub is_cts: bool,
}

impl<'a> Visit<'a> for ModuleTypeDetector {
    // ESM: import declarations.
    fn visit_import_declaration(&mut self, _it: &oxc::ast::ast::ImportDeclaration<'a>) {
        self.is_esm = true;
        // Don't walk children — we only care about the declaration itself.
    }

    // ESM: export named declarations.
    fn visit_export_named_declaration(&mut self, _it: &oxc::ast::ast::ExportNamedDeclaration<'a>) {
        self.is_esm = true;
    }

    // ESM: export default declarations.
    fn visit_export_default_declaration(
        &mut self,
        _it: &oxc::ast::ast::ExportDefaultDeclaration<'a>,
    ) {
        self.is_esm = true;
    }

    // ESM: export all declarations (`export * from "..."`).
    fn visit_export_all_declaration(&mut self, _it: &oxc::ast::ast::ExportAllDeclaration<'a>) {
        self.is_esm = true;
    }

    // TS import-equals (`import foo = require("...")`) counts as ESM.
    fn visit_ts_import_equals_declaration(
        &mut self,
        _it: &oxc::ast::ast::TSImportEqualsDeclaration<'a>,
    ) {
        self.is_esm = true;
        // `import foo = require("...")` is the CommonJS-style import syntax
        // used by `.cts` files.
        self.is_cts = true;
    }

    // TS export-assignment (`export = foo`) — the CommonJS-style export syntax
    // used by `.cts` files.
    fn visit_ts_export_assignment(&mut self, _it: &oxc::ast::ast::TSExportAssignment<'a>) {
        self.is_cts = true;
    }

    // CommonJS: `require(...)` calls.
    fn visit_call_expression(&mut self, it: &oxc::ast::ast::CallExpression<'a>) {
        if let Expression::Identifier(ident) = &it.callee
            && ident.name.as_str() == "require"
        {
            self.is_common_js = true;
            // Don't walk children — we already captured it.
            return;
        }
        // Otherwise walk normally to find nested require/import expressions.
        self.visit_expression(&it.callee);
        self.visit_arguments(&it.arguments);
    }

    // CommonJS: `module.exports` / `exports.x` static member access.
    fn visit_static_member_expression(&mut self, it: &oxc::ast::ast::StaticMemberExpression<'a>) {
        if let Expression::Identifier(ident) = &it.object {
            let name = ident.name.as_str();
            let prop = it.property.name.as_str();
            if (name == "module" && prop == "exports") || name == "exports" {
                self.is_common_js = true;
            }
        }
        // Walk children to find more.
        self.visit_expression(&it.object);
    }
}

/// Detects whether a JavaScript/TypeScript source contains JSX syntax.
///
/// The visitor sets [`JsxDetector::contains_jsx`] to `true` when it encounters
/// any [`JSXElement`] or [`JSXFragment`] node while walking the AST.
///
/// [`JSXElement`]: oxc::ast::ast::JSXElement
/// [`JSXFragment`]: oxc::ast::ast::JSXFragment
#[derive(Default)]
pub struct JsxDetector {
    /// Whether the visited AST contains at least one JSX element or fragment.
    pub contains_jsx: bool,
}

impl<'a> Visit<'a> for JsxDetector {
    fn visit_jsx_element(&mut self, _it: &oxc::ast::ast::JSXElement<'a>) {
        self.contains_jsx = true;
    }

    fn visit_jsx_fragment(&mut self, _it: &oxc::ast::ast::JSXFragment<'a>) {
        self.contains_jsx = true;
    }
}
/// Result returned by the dependency-collection pass.
///
/// It bundles together the discovered npm dependencies, the observed node
/// builtins/modules, any warnings emitted during analysis, and the per-file
/// dependency records ([`DepsFile`]).
#[derive(serde::Serialize)]
pub struct DepReturns {
    /// npm package dependencies discovered in the source.
    pub npm: Vec<String>,
    /// Node.js builtins or modules referenced by the source.
    pub nodes: Vec<String>,
    /// Warnings emitted while collecting dependencies.
    pub warns: Vec<String>,
    /// Per-file dependency records.
    pub dep_files: Vec<DepsFile>,
}

// ---------------------------------------------------------------------------
// susee_hooks
// ---------------------------------------------------------------------------

/// The module format used when emitting compiled output.
///
/// Controls both the emitted module file extension and the corresponding
/// type declaration file extension, as well as the runtime module system
/// (CommonJS `require`/`module.exports` vs. ESM `import`/`export`).
#[napi]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum OutputFormat {
    /// Emit a CommonJS module (`.cjs` file, `.d.cts` declaration).
    Commonjs,
    /// Emit an ES module (`.mjs` file, `.d.mts` declaration).
    #[default]
    Esm,
}

impl OutputFormat {
    /// Return the canonical string label used in logs and file extensions.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Commonjs => "commonjs",
            Self::Esm => "esm",
        }
    }

    /// The primary file extension used for the emitted module file:
    /// `.cjs` for CommonJS, `.mjs` for ESM.
    pub fn module_ext(&self) -> &'static str {
        match self {
            Self::Commonjs => ".cjs",
            Self::Esm => ".mjs",
        }
    }

    /// The extension used for the type declaration file:
    /// `.d.cts` for CommonJS, `.d.mts` for ESM.
    pub fn dts_ext(&self) -> &'static str {
        match self {
            Self::Commonjs => ".d.cts",
            Self::Esm => ".d.mts",
        }
    }

    /// The extension used for the source map file.
    pub fn map_ext(&self) -> &'static str {
        match self {
            Self::Commonjs => ".cjs.map",
            Self::Esm => ".mjs.map",
        }
    }
}
