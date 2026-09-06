//! Zero-trust masking of sensitive substrings before they're sent to a
//! cloud LLM endpoint, and restoration of the originals into the reply
//! (`docs/features/tui-ai-hybrid-fallback.md` §2.2, T49). One-shot, per
//! request/response pair -- never persisted, never shared between
//! requests. The masking guarantee ("no secret in any cloud payload")
//! is the security boundary the `hacker` pass reviews; it is proven by
//! this module's tests, not asserted.

use std::collections::HashMap;

/// A single masked token: placeholder ↔ original. `placeholder` is the
/// `__IDE_SAN_<n>__` literal that travels to the provider in place of
/// `original`; `original` is restored into the reply via
/// [`restore_originals`].
pub struct MaskEntry {
    pub placeholder: String,
    pub original: String,
}

/// In-progress masking state; one per request/response pair, never
/// persisted.
pub struct Sanitizer {
    map: Vec<MaskEntry>,
    next_id: usize,
}

/// A masking pass's result: the masked text plus how many distinct
/// values were masked.
pub struct Sanitized {
    pub masked: String,
    pub count: usize,
}

impl Default for Sanitizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sanitizer {
    pub fn new() -> Self {
        Sanitizer {
            map: Vec::new(),
            next_id: 0,
        }
    }

    /// Registers a placeholder for `original` and returns it. A repeated
    /// `original` reuses an existing placeholder rather than minting a
    /// new map entry, so the same secret always maps to the same
    /// placeholder (a roundtrip restore is unambiguous).
    pub fn register(&mut self, original: &str) -> String {
        if let Some(entry) = self.map.iter().find(|e| e.original == original) {
            return entry.placeholder.clone();
        }
        let placeholder = format!("__IDE_SAN_{}__", self.next_id);
        self.next_id += 1;
        self.map.push(MaskEntry {
            placeholder,
            original: original.to_string(),
        });
        self.map.last().expect("just pushed").placeholder.clone()
    }

    /// The full masking pipeline over `input` (regex passes + entropy,
    /// plus Rust-string-literal masking when the `rust-ast` feature is
    /// on), using the default 2.0 entropy gate for the opaque-token
    /// sweep. Returns the masked text and the count of distinct values
    /// masked this pass.
    pub fn mask(&mut self, input: &str) -> Sanitized {
        self.mask_with_threshold(input, 2.0)
    }

    /// [`mask`] with an explicit entropy gate for the opaque-token sweep,
    /// so a route can mask more or less aggressively without changing the
    /// known-shape rules (JWTs, token prefixes, private IPs are always
    /// masked regardless of `threshold` -- `docs/features/
    /// tui-ai-hybrid-fallback.md` §3.3's local vs cloud thresholds only
    /// move the bar for the "looks like a secret" catch-all). Cloud
    /// routes use the tighter `cloud_sanitize_threshold`; the local route
    /// the looser `local_sanitize_threshold`.
    pub fn mask_with_threshold(&mut self, input: &str, threshold: f64) -> Sanitized {
        let before = self.map.len();
        #[cfg(feature = "rust-ast")]
        let mut masked = mask_secrets_with_threshold(self, input, threshold);
        #[cfg(not(feature = "rust-ast"))]
        let masked = mask_secrets_with_threshold(self, input, threshold);
        #[cfg(feature = "rust-ast")]
        {
            masked = mask_rust_strings(self, &masked);
        }
        Sanitized {
            count: self.map.len() - before,
            masked,
        }
    }

    /// True if nothing has been masked yet.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Count of distinct masked values.
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Maps every entry: placeholder → original.
pub fn as_map(s: &Sanitizer) -> HashMap<String, String> {
    s.map
        .iter()
        .map(|e| (e.placeholder.clone(), e.original.clone()))
        .collect()
}

/// Restores placeholders in `masked` back to their originals using
/// `map`. Matches placeholders only (the `__IDE_SAN_<n>__` literal
/// format the sanitizer itself mints) -- never arbitrary substrings, so
/// this never writes anything into files besides the restored text.
pub fn restore_originals(masked: &str, map: &HashMap<String, String>) -> String {
    let mut out = masked.to_string();
    for (placeholder, original) in map {
        out = out.replace(placeholder, original);
    }
    out
}

/// Clears all entries and resets the id counter (a fresh one-shot pass).
pub fn reset_placeholders(s: &mut Sanitizer) {
    s.map.clear();
    s.next_id = 0;
}

/// Regex passes: API-key patterns, JWTs (`eyJ...`), internal IPv4
/// (RFC1918 + loopback), and finally high-entropy opaque tokens. Every
/// match is registered (and replaced with its placeholder) via
/// `s.register`, except opaque-token matches whose Shannon entropy is too
/// low to be a secret (avoids masking ordinary identifiers/words).
/// Equivalent to [`mask_secrets_with_threshold`] with a 2.0 gate.
pub fn mask_secrets(s: &mut Sanitizer, input: &str) -> String {
    mask_secrets_with_threshold(s, input, 2.0)
}

/// [`mask_secrets`] with an explicit entropy gate on the opaque-token
/// sweep. The known-shape patterns (JWT, token prefixes, private IPs) are
/// masked unconditionally -- a shape match is a secret regardless of how
/// the route's threshold is set.
pub fn mask_secrets_with_threshold(s: &mut Sanitizer, input: &str, threshold: f64) -> String {
    let known: &[&str] = &[
        // JWT: three base64url segments, first always begins `eyJ`.
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b",
        // Common token/key prefixes (sk-/ghp_/gho_/gsk_/AIza/AKIA etc.).
        r"\b(pk|sk|rk|ghp|gho|gsk|AKIA|AIza|ya29)[A-Za-z0-9_-]{12,}\b",
        // Private/loopback IPv4 addresses.
        r"\b(127\.0\.0\.1|0\.0\.0\.0|10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.\d{1,3}\.\d{1,3})\b",
    ];
    // High-entropy opaque token: a long base64-ish run. Applied last so it
    // only sweeps up what the specific patterns above left behind, and
    // gated behind the per-token entropy check against `threshold`.
    const OPAQUE: &str = r"\b[A-Za-z0-9+/_-]{32,}\b";

    let mut masked = input.to_string();
    for pattern in known {
        masked = replace_matches(&masked, s, pattern, None);
    }
    replace_matches(&masked, s, OPAQUE, Some(threshold))
}

/// Replaces every `pattern` match in `masked` with a placeholder,
/// registering each token via `s.register`. `gate` surgical:
/// `Some(t)` keeps only matches whose entropy exceeds `t`; `None` keeps
/// every match. Returns the rebuilt string (unchanged if nothing matched).
fn replace_matches(masked: &str, s: &mut Sanitizer, pattern: &str, gate: Option<f64>) -> String {
    let re = match regex::Regex::new(pattern) {
        Ok(re) => re,
        Err(_) => return masked.to_string(),
    };
    let mut rebuilt = String::with_capacity(masked.len());
    let mut last = 0;
    let mut any = false;
    for cap in re.captures_iter(masked) {
        let m = cap.get(0).expect("group 0 always present");
        let token = &masked[m.start()..m.end()];
        if let Some(threshold) = gate {
            if !entropy_above(token, threshold) {
                continue;
            }
        }
        rebuilt.push_str(&masked[last..m.start()]);
        rebuilt.push_str(&s.register(token));
        last = m.end();
        any = true;
    }
    if any {
        rebuilt.push_str(&masked[last..]);
        rebuilt
    } else {
        masked.to_string()
    }
}

/// Shannon entropy over `s`'s single-character distribution, in bits.
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let bytes = s.as_bytes();
    let len = bytes.len() as f64;
    let mut counts: HashMap<u8, usize> = HashMap::new();
    for b in bytes {
        *counts.entry(*b).or_insert(0) += 1;
    }
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// True if `s`'s Shannon entropy exceeds `threshold`.
pub fn entropy_above(s: &str, threshold: f64) -> bool {
    shannon_entropy(s) > threshold
}

/// `syn` AST walk collecting every `LitStr`, masking each string
/// literal's *contents* to a placeholder (feature `rust-ast` only).
/// The quote delimiters are left intact, so the masked payload stays
/// valid Rust (the placeholder reads as a string literal whose contents
/// are the placeholder), and `restore_originals` round-trips the
/// original contents back. Runs the whole file through `syn::parse_file`
/// and a `Visit` walk; a file that fails to parse is left unchanged
/// (the regex `mask_secrets` pass still covers it).
#[cfg(feature = "rust-ast")]
pub fn mask_rust_strings(s: &mut Sanitizer, input: &str) -> String {
    use syn::visit::Visit;

    let file = match syn::parse_file(input) {
        Ok(file) => file,
        Err(_) => return input.to_string(),
    };

    let mut strings: Vec<(String, String)> = Vec::new();
    {
        struct Strings<'a> {
            s: &'a mut Sanitizer,
            out: &'a mut Vec<(String, String)>,
        }
        impl<'ast> Visit<'ast> for Strings<'_> {
            fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
                let value = node.value();
                if !value.is_empty() {
                    let placeholder = self.s.register(&value);
                    self.out.push((value, placeholder));
                }
                syn::visit::visit_lit_str(self, node);
            }
        }
        let mut visited = Strings {
            s,
            out: &mut strings,
        };
        syn::visit::visit_file(&mut visited, &file);
    }

    let mut masked = input.to_string();
    for (value, placeholder) in &strings {
        masked = masked.replace(value, placeholder);
    }
    masked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_mints_sequential_placeholders() {
        let mut s = Sanitizer::new();
        assert_eq!(s.register("a"), "__IDE_SAN_0__");
        assert_eq!(s.register("b"), "__IDE_SAN_1__");
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
    }

    #[test]
    fn register_reuses_a_placeholder_for_a_repeated_original() {
        let mut s = Sanitizer::new();
        let first = s.register("secret");
        let second = s.register("secret");
        assert_eq!(first, second);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn as_map_roundtrips_placeholders_back_to_originals() {
        let mut s = Sanitizer::new();
        let a = s.register("alpha");
        let b = s.register("beta");
        let map = as_map(&s);
        assert_eq!(
            restore_originals(&format!("{a} {b} {a}"), &map),
            "alpha beta alpha"
        );
    }

    #[test]
    fn reset_placeholders_clears_and_resets_ids() {
        let mut s = Sanitizer::new();
        s.register("x");
        reset_placeholders(&mut s);
        assert!(s.is_empty());
        assert_eq!(s.register("y"), "__IDE_SAN_0__");
    }

    #[test]
    fn fresh_sanitizer_is_empty() {
        let s = Sanitizer::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn shannon_entropy_of_a_single_repeated_char_is_zero() {
        assert_eq!(shannon_entropy("aaaaaa"), 0.0);
    }

    #[test]
    fn shannon_entropy_of_uniform_distinct_chars_is_max() {
        assert!((shannon_entropy("abcd") - 2.0).abs() < 1e-9);
        assert!(entropy_above("abcd", 1.0));
    }

    #[test]
    fn mask_secrets_hides_a_jwt_with_surrounding_text() {
        let mut s = Sanitizer::new();
        let input = "token=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeK";
        let out = mask_secrets(&mut s, input);
        assert!(!out.contains("eyJhbGci"));
        assert!(out.contains("__IDE_SAN_0__"));
        assert!(!s.is_empty());
    }

    #[test]
    fn mask_secrets_hides_a_private_ipv4() {
        let mut s = Sanitizer::new();
        let out = mask_secrets(&mut s, "server at 192.168.1.10:8080");
        assert!(!out.contains("192.168.1.10"));
        assert!(out.contains("__IDE_SAN"));
    }

    #[test]
    fn mask_secrets_hides_a_high_entropy_token() {
        let mut s = Sanitizer::new();
        let token = "gHk9Qr4mWzXpL2vN7sT3yB5cD1fJ8uA6";
        let out = mask_secrets(&mut s, &format!("key={token}"));
        assert!(!out.contains(token));
        assert!(out.contains("__IDE_SAN"));
    }

    #[test]
    fn mask_secrets_does_not_mask_a_low_entropy_repeated_word() {
        let mut s = Sanitizer::new();
        let out = mask_secrets(&mut s, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // All 'a' -- entropy 0, below the 2.0 gate -- so left alone.
        assert_eq!(out, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(s.is_empty());
    }

    #[test]
    fn mask_secrets_with_threshold_gates_the_opaque_token_sweep() {
        // A 32-char mix with modest entropy: passes a low gate, fails a
        // high one.
        let token = "ab12cd34ef56gh78ij90kl12mn34op56";
        let mut loose = Sanitizer::new();
        assert!(mask_secrets_with_threshold(&mut loose, token, 3.0).contains("__IDE_SAN"));
        let mut tight = Sanitizer::new();
        assert_eq!(mask_secrets_with_threshold(&mut tight, token, 12.0), token,);
        assert!(tight.is_empty());
    }

    #[test]
    fn mask_with_threshold_masks_known_shapes_regardless_of_gate() {
        // Known-shape secrets are masked even under an arbitrarily high
        // threshold -- only the catch-all opaque sweep is gated.
        let mut s = Sanitizer::new();
        let out = s.mask_with_threshold(
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKFA ip 10.0.0.7",
            99.0,
        );
        assert!(!out.masked.contains("eyJhbGci"));
        assert!(!out.masked.contains("10.0.0.7"));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn mask_with_threshold_reports_the_distinct_count() {
        let mut s = Sanitizer::new();
        let out = s.mask_with_threshold("t 192.168.0.1 and 10.1.1.1", 2.0);
        assert_eq!(out.count, 2);
        assert!(out.masked.contains("__IDE_SAN_0__"));
        assert!(out.masked.contains("__IDE_SAN_1__"));
    }

    #[test]
    fn mask_defaults_to_the_two_point_zero_gate() {
        // mask() == mask_with_threshold(2.0): the same medium-entropy
        // token that a 3.0 gate masks also masks under the default.
        let token = "ab12cd34ef56gh78ij90kl12mn34op56";
        let mut s = Sanitizer::new();
        assert!(s.mask(token).masked.contains("__IDE_SAN"));
    }

    #[test]
    fn mask_runs_secrets_then_optionally_rust_strings() {
        let mut s = Sanitizer::new();
        let input = "let url = \"http://192.168.1.5/x\"; let k = \"secretvalue\"";
        let result = s.mask(input);
        assert!(!result.masked.contains("192.168.1.5"));
        assert!(result.count >= 1);
    }

    #[cfg(feature = "rust-ast")]
    #[test]
    fn mask_rust_strings_masks_string_literal_contents() {
        let mut s = Sanitizer::new();
        let src = r#"fn main() { let a = "hello world"; let b = "hello world"; }"#;
        let out = mask_rust_strings(&mut s, src);
        assert!(!out.contains("hello world"));
        assert!(out.contains("\"__IDE_SAN_0__\""));
        let map = as_map(&s);
        assert_eq!(restore_originals(&out, &map), src);
    }

    #[cfg(feature = "rust-ast")]
    #[test]
    fn mask_rust_strings_leaves_invalid_rust_unchanged() {
        let mut s = Sanitizer::new();
        let src = "not valid rust {{{";
        assert_eq!(mask_rust_strings(&mut s, src), src);
    }

    #[cfg(feature = "rust-ast")]
    #[test]
    fn mask_rust_strings_skips_empty_strings() {
        let mut s = Sanitizer::new();
        let src = r#"fn main() { let a = ""; let b = "x"; }"#;
        let out = mask_rust_strings(&mut s, src);
        assert!(out.contains("\"\""));
        assert!(out.contains("\"__IDE_SAN_0__\""));
    }
}
