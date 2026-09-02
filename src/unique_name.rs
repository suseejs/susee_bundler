//! Unique name generator for bundler-generated identifiers.
//!
//! Produces short, deterministic, collision-free identifiers that follow
//! JavaScript identifier conventions and are unlikely to clash with user
//! code.
//!
//! ## Format
//!
//! ```text
//! _<sigil><input>$<count>
//! ```
//!
//! - `_` — leading underscore marks the identifier as tool-generated (a
//!   widely used convention for "internal" names; user code rarely starts
//!   an exported binding with `_`).
//! - `<sigil>` — a single lowercase letter identifying the category:
//!   - `a` — anonymous default export (e.g. `export default function () {}`)
//!   - `d` — named default export (e.g. `export default function hello() {}`)
//!   - `u` — cross-file duplicate top-level declaration
//!   - `j` — JSON module default export
//! - `<input>` — the meaningful base (file stem or symbol name), sanitized
//!   to a valid identifier (non-alphanumeric chars → `_`).
//! - `$<count>` — a per-category counter disambiguates names that share the
//!   same `<input>`. The `$` separator is JS-idiomatic for generated names
//!   and keeps the boundary between the base and the counter visible.
//!
//! ## Examples
//!
//! | Input (key, input)                | Generated name       |
//! |-----------------------------------|----------------------|
//! | `("AnonymousName", "unusedCode")`  | `_aunusedCode$1`     |
//! | `("ExportDefault", "hello")`      | `_dhello$1`          |
//! | `("Duplicates", "shared")`         | `_ushared$1`         |
//! | `("JsonModule", "config")`         | `_jconfig$1`         |

use std::collections::HashMap;

/// Single-letter category sigils, chosen to be short and mnemonic.
pub mod sigil {
    /// Anonymous default export (`a`nonymous).
    pub const ANONYMOUS: &str = "a";
    /// Named default export (`d`efault).
    pub const DEFAULT: &str = "d";
    /// Cross-file duplicate top-level declaration (`u`nique).
    #[allow(dead_code)]
    pub const DUPLICATE: &str = "u";
    /// JSON module default export (`j`son).
    #[allow(dead_code)]
    pub const JSON: &str = "j";
}

/// A unique name generator that produces deterministic, collision-free
/// identifiers of the form `_<sigil><input>$<count>`.
///
/// Each *category* (identified by a `key`) maintains its own monotonically
/// increasing counter, so names generated under different categories never
/// interfere with each other.
#[derive(Debug, Clone)]
pub struct UniqueName {
    /// Maps category `key` → `(sigil, count)`.
    categories: HashMap<String, (&'static str, usize)>,
}

impl Default for UniqueName {
    fn default() -> Self {
        Self::new()
    }
}

impl UniqueName {
    /// Create a new empty `UniqueName`.
    pub fn new() -> Self {
        Self {
            categories: HashMap::new(),
        }
    }

    /// Register a category `key` with a single-letter `sigil`.
    ///
    /// If the key already exists its counter is reset to `0` (and the sigil
    /// updated); otherwise a fresh entry is created. This mirrors the
    /// semantics of `setPrefix` in the TS implementation — callers register
    /// a category once before generating names under it.
    pub fn set_prefix(&mut self, key: &str, sigil: &'static str) -> &mut Self {
        debug_assert!(
            sigil.len() == 1 && sigil.chars().all(|c| c.is_ascii_alphabetic()),
            "sigil must be a single ASCII letter, got {sigil:?}"
        );
        self.categories.insert(key.to_string(), (sigil, 0));
        self
    }

    /// Generate a unique name for `key` using `input` as the base.
    ///
    /// The returned string has the form `_<sigil><sanitized_input>$<count>`,
    /// where `<sigil>` is the letter registered for `key` via
    /// [`set_prefix`](Self::set_prefix) (defaulting to `a` when the key was
    /// never registered), and `<count>` is a per-key counter starting at 1.
    ///
    /// `input` is sanitized into a valid JS identifier tail: characters that
    /// are not ASCII alphanumeric or `_` are replaced with `_`, and a leading
    /// `_` is added if the first character would not be a valid identifier
    /// start (digit or empty).
    pub fn get_name(&mut self, key: &str, input: &str) -> String {
        let (sigil, count) = match self.categories.get_mut(key) {
            Some((s, c)) => {
                *c += 1;
                (*s, *c)
            }
            // Auto-register with the anonymous sigil for unregistered keys,
            // matching the TS default-prefix behavior.
            None => {
                self.categories
                    .insert(key.to_string(), (sigil::ANONYMOUS, 1));
                (sigil::ANONYMOUS, 1)
            }
        };
        let sanitized = sanitize_identifier(input);
        format!("_{sigil}{sanitized}${count}")
    }

    /// Get the current count for a key (the number of names generated so
    /// far under that key). Returns `None` if the key has never been seen.
    #[allow(dead_code)]
    pub fn get_prefix(&self, key: &str) -> Option<usize> {
        self.categories.get(key).map(|(_, c)| *c)
    }
}

/// Sanitize an arbitrary string into a valid JavaScript identifier tail
/// (the part after a leading `_` + sigil).
///
/// - ASCII alphanumeric chars and `_` are preserved.
/// - Every other char is replaced with `_`.
/// - An empty result (or one starting with a digit) is prefixed with `_`
///   so the whole identifier remains valid after the sigil letter.
fn sanitize_identifier(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let starts_valid = cleaned
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if starts_valid {
        cleaned
    } else if cleaned.is_empty() {
        "_".to_string()
    } else {
        format!("_{cleaned}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uses_sigil_and_dollar_counter() {
        let mut generator = UniqueName::new();
        generator.set_prefix("ExportDefault", sigil::DEFAULT);
        let n1 = generator.get_name("ExportDefault", "foo");
        assert_eq!(n1, "_dfoo$1");
    }

    #[test]
    fn counts_increment_per_key() {
        let mut generator = UniqueName::new();
        generator.set_prefix("AnonymousName", sigil::ANONYMOUS);
        let n1 = generator.get_name("AnonymousName", "foo");
        let n2 = generator.get_name("AnonymousName", "bar");
        let n3 = generator.get_name("AnonymousName", "foo");
        assert_eq!(n1, "_afoo$1");
        assert_eq!(n2, "_abar$2");
        assert_eq!(n3, "_afoo$3");
    }

    #[test]
    fn counters_are_independent_per_key() {
        let mut generator = UniqueName::new();
        generator.set_prefix("AnonymousName", sigil::ANONYMOUS);
        generator.set_prefix("Duplicates", sigil::DUPLICATE);
        let a1 = generator.get_name("AnonymousName", "x");
        let d1 = generator.get_name("Duplicates", "x");
        let a2 = generator.get_name("AnonymousName", "y");
        assert_eq!(a1, "_ax$1");
        assert_eq!(d1, "_ux$1");
        assert_eq!(a2, "_ay$2");
    }

    #[test]
    fn unregistered_key_defaults_to_anonymous_sigil() {
        let mut generator = UniqueName::new();
        let name = generator.get_name("SomeKey", "someInput");
        assert_eq!(name, "_asomeInput$1");
    }

    #[test]
    fn always_starts_with_underscore() {
        let mut generator = UniqueName::new();
        generator.set_prefix("AnonymousName", sigil::ANONYMOUS);
        let name = generator.get_name("AnonymousName", "anon");
        assert!(name.starts_with('_'), "name should start with '_': {name}");
    }

    #[test]
    fn sanitizes_non_identifier_chars() {
        let mut generator = UniqueName::new();
        generator.set_prefix("Duplicates", sigil::DUPLICATE);
        let name = generator.get_name("Duplicates", "src/a.ts");
        assert_eq!(name, "_usrc_a_ts$1");
    }

    #[test]
    fn sanitizes_leading_digit() {
        let mut generator = UniqueName::new();
        generator.set_prefix("JsonModule", sigil::JSON);
        let name = generator.get_name("JsonModule", "123config");
        assert_eq!(name, "_j_123config$1");
    }

    #[test]
    fn sanitizes_empty_input() {
        let mut generator = UniqueName::new();
        generator.set_prefix("JsonModule", sigil::JSON);
        let name = generator.get_name("JsonModule", "");
        assert_eq!(name, "_j_$1");
    }

    #[test]
    fn get_prefix_reports_count() {
        let mut generator = UniqueName::new();
        generator.set_prefix("ExportDefault", sigil::DEFAULT);
        generator.get_name("ExportDefault", "foo");
        generator.get_name("ExportDefault", "bar");
        assert_eq!(generator.get_prefix("ExportDefault"), Some(2));
        assert_eq!(generator.get_prefix("Unknown"), None);
    }
}
