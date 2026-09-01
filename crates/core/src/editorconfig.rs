//! `.editorconfig` reading and application
//! (`docs/features/line-commands-and-editorconfig.md` §2.4, §3.6, §4.5).
//!
//! **Security-sensitive** (`CLAUDE.md`): [`resolve`] walks directories the
//! user did not explicitly open and reads whatever `.editorconfig` files it
//! finds there, so every one of them is untrusted input. The mitigations
//! this module implements, all required by §4.5: the walk never leaves
//! `root` (checked after canonicalizing both the root and the target file,
//! so a symlink cannot pull rules in from outside it), it is depth-capped
//! independently of `root = true`, each file is size- and section-capped,
//! the glob matcher backtracks on `*`/`**` at most once per star occurrence
//! (linear in `pattern.len() * path.len()`, never exponential), and a
//! parse failure narrows what a section matches rather than widening it.
//! Nothing here can name a path, run a program, or select an encoding the
//! buffer cannot represent -- the six properties in [`EditorConfig`] are the
//! entire vocabulary.

use std::path::{Path, PathBuf};

use crate::text::{Change, Transaction};

/// Largest `.editorconfig` that will be read.
pub const MAX_EDITORCONFIG_BYTES: u64 = 64 * 1024;
/// Most `[section]`s honoured in one file; the rest are ignored.
pub const MAX_EDITORCONFIG_SECTIONS: usize = 256;
/// Most directory levels walked upwards before giving up, independent of
/// `root = true`.
pub const MAX_EDITORCONFIG_DEPTH: usize = 64;

/// Most concrete patterns one `{...}` group (or composition of groups) in a
/// section header is allowed to expand into -- not part of the EditorConfig
/// spec, a local safety cap so a pathological header (`{1..999999}`) can't
/// blow up `parse`'s work per line.
const MAX_GLOB_EXPANSIONS: usize = 64;
/// Most `{...}` groups one section header may chain -- not part of the
/// EditorConfig spec. `expand_braces` recurses once per group; without this,
/// a header built from many groups (still well under
/// `MAX_EDITORCONFIG_BYTES`, since each group is only a few bytes) recurses
/// deep enough to overflow the stack before `MAX_GLOB_EXPANSIONS` is ever
/// checked -- an unrecoverable process abort, not a catchable error.
const MAX_GLOB_GROUPS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndOfLine {
    Lf,
    Crlf,
    Cr,
}

/// Only the encodings the editor can honour losslessly. `Utf8Bom` is
/// honoured on save (the BOM is written); the UTF-16 and Latin-1 spellings
/// are *recognised and reported*, never applied -- see [`save_charset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Utf8,
    Utf8Bom,
    Latin1,
    Utf16Le,
    Utf16Be,
}

/// The six properties `docs/roadmap.md`'s A4 row names, each `None` when no
/// matching section set it. Deliberately not a defaults-filled struct: the
/// caller distinguishes "the project says 2 spaces" from "the project says
/// nothing", and only the former should override an editor default.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EditorConfig {
    pub indent_style: Option<crate::text::IndentStyle>,
    pub indent_size: Option<usize>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    pub end_of_line: Option<EndOfLine>,
    pub charset: Option<Charset>,
}

/// The **only** way [`resolve`] fails. Everything else this module calls
/// "skipped" really is skipped and never reaches the caller: a file too
/// large to read, a directory that cannot be listed, a line that does not
/// parse and a section header that does not compile all narrow what
/// matches, they do not abort the walk.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditorConfigError {
    #[error("`{0}` is not inside the project root")]
    OutsideRoot(PathBuf),
}

/// Resolves the effective config for `file` by walking from its directory
/// upwards, stopping at `root` (inclusive), at a file declaring
/// `root = true`, or after [`MAX_EDITORCONFIG_DEPTH`] levels -- whichever
/// comes first. Nearer files win, and within one file a later matching
/// section wins.
///
/// `file` must be inside `root` after canonicalization; `OutsideRoot`
/// otherwise, which is what stops a symlinked file from pulling in a
/// `.editorconfig` from anywhere on the filesystem.
pub fn resolve(root: &Path, file: &Path) -> Result<EditorConfig, EditorConfigError> {
    let outside = || EditorConfigError::OutsideRoot(file.to_path_buf());
    let canonical_root = std::fs::canonicalize(root).map_err(|_| outside())?;
    let canonical_file = std::fs::canonicalize(file).map_err(|_| outside())?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err(outside());
    }

    let mut config = EditorConfig::default();
    let mut resolved = [false; 6];
    let mut dir = canonical_file.parent().map(Path::to_path_buf);
    let mut depth = 0usize;

    while let Some(d) = dir {
        if depth >= MAX_EDITORCONFIG_DEPTH {
            break;
        }
        depth += 1;

        let candidate = d.join(".editorconfig");
        let mut declares_root = false;
        match std::fs::metadata(&candidate) {
            Ok(meta) if meta.len() <= MAX_EDITORCONFIG_BYTES => {
                if let Ok(content) = std::fs::read_to_string(&candidate) {
                    if let Ok(relative) = canonical_file.strip_prefix(&d) {
                        let parsed = parse(&content, &path_to_slash(relative));
                        merge_nearer_wins(&mut config, &mut resolved, &parsed);
                    }
                    declares_root = declares_root_true(&content);
                }
            }
            Ok(_) => {} // too large: skip, keep walking up
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // no file here
            Err(_) => break, // unreadable directory: ends the walk
        }

        if declares_root || d == canonical_root {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }

    Ok(config)
}

fn merge_nearer_wins(config: &mut EditorConfig, resolved: &mut [bool; 6], parsed: &EditorConfig) {
    macro_rules! take {
        ($field:ident, $index:expr) => {
            if !resolved[$index] {
                if let Some(value) = parsed.$field {
                    config.$field = Some(value);
                    resolved[$index] = true;
                }
            }
        };
    }
    take!(indent_style, 0);
    take!(indent_size, 1);
    take!(trim_trailing_whitespace, 2);
    take!(insert_final_newline, 3);
    take!(end_of_line, 4);
    take!(charset, 5);
}

fn declares_root_true(content: &str) -> bool {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("root") && value.trim().eq_ignore_ascii_case("true")
            {
                return true;
            }
        }
    }
    false
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Parses one file's content, for tests and for [`resolve`]'s own use.
/// Sections are matched against `relative_path`, which must be relative and
/// use `/` separators regardless of platform. An unparsable line, an
/// unknown property and a section header that fails to compile are all
/// skipped rather than aborting the parse.
pub fn parse(content: &str, relative_path: &str) -> EditorConfig {
    let mut config = EditorConfig::default();
    let mut current_matches = false;
    let mut in_a_section = false;
    let mut sections = 0usize;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            sections += 1;
            if sections > MAX_EDITORCONFIG_SECTIONS {
                break;
            }
            in_a_section = true;
            current_matches = glob_matches(header, relative_path);
            continue;
        }
        if !in_a_section || !current_matches {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        apply_property(&mut config, key.trim(), value.trim());
    }
    config
}

fn apply_property(config: &mut EditorConfig, key: &str, value: &str) {
    let lower_key = key.to_ascii_lowercase();
    let lower_value = value.to_ascii_lowercase();
    match lower_key.as_str() {
        "indent_style" => match lower_value.as_str() {
            "space" => config.indent_style = Some(crate::text::IndentStyle::Spaces),
            "tab" => config.indent_style = Some(crate::text::IndentStyle::Tabs),
            _ => {}
        },
        "indent_size" => {
            if let Ok(n) = value.parse::<usize>() {
                config.indent_size = Some(n);
            }
        }
        "trim_trailing_whitespace" => {
            if let Some(b) = parse_bool(&lower_value) {
                config.trim_trailing_whitespace = Some(b);
            }
        }
        "insert_final_newline" => {
            if let Some(b) = parse_bool(&lower_value) {
                config.insert_final_newline = Some(b);
            }
        }
        "end_of_line" => match lower_value.as_str() {
            "lf" => config.end_of_line = Some(EndOfLine::Lf),
            "crlf" => config.end_of_line = Some(EndOfLine::Crlf),
            "cr" => config.end_of_line = Some(EndOfLine::Cr),
            _ => {}
        },
        "charset" => match lower_value.as_str() {
            "utf-8" => config.charset = Some(Charset::Utf8),
            "utf-8-bom" => config.charset = Some(Charset::Utf8Bom),
            "latin1" => config.charset = Some(Charset::Latin1),
            "utf-16le" => config.charset = Some(Charset::Utf16Le),
            "utf-16be" => config.charset = Some(Charset::Utf16Be),
            _ => {}
        },
        _ => {}
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The save-time half of the config, as the **minimal** edit that applies
/// it: one `Change` per affected span rather than one replacing the whole
/// buffer, so `Selections::map` carries the user's cursors through it.
///
/// Order is fixed: trailing whitespace is trimmed first, then the final
/// newline is inserted or removed, then line endings are normalised --
/// otherwise a trimmed trailing `\r` would be re-added by the newline rule,
/// and trimming after inserting the final newline would remove it again on
/// a whitespace-only buffer. `None` when nothing would change.
pub fn save_edit(text: &str, config: &EditorConfig) -> Option<Transaction> {
    if text.is_empty() {
        return None;
    }
    let bytes = text.as_bytes();

    // Where only newline characters remain, scanning back from the end.
    let content_tail_end = {
        let mut end = text.len();
        while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
            end -= 1;
        }
        end
    };
    // The same point, additionally walked back over the last line's own
    // trailing spaces/tabs when those are being trimmed -- computed
    // directly (not from Pass A's own changes) so Pass B's change can be
    // built, and ordered into `changes`, before Pass A's.
    let logical_end = {
        let mut end = content_tail_end;
        if config.trim_trailing_whitespace == Some(true) {
            while end > 0 && matches!(bytes[end - 1], b' ' | b'\t') {
                end -= 1;
            }
        }
        end
    };

    let mut changes: Vec<Change> = Vec::new();

    // Pass B first: when it inserts exactly at `logical_end`, it must sort
    // ahead of Pass A's trim of the same line so `Transaction::new` sees a
    // zero-width insert touching a deletion rather than the reverse (which
    // it rejects as overlapping).
    match config.insert_final_newline {
        Some(true) if content_tail_end == text.len() => {
            let eol = eol_str(config.end_of_line.unwrap_or(EndOfLine::Lf));
            changes.push(Change::new(logical_end..logical_end, eol));
        }
        Some(false) if content_tail_end < text.len() => {
            changes.push(Change::new(content_tail_end..text.len(), ""));
        }
        _ => {}
    }

    // Pass A: trim_trailing_whitespace, one Change per line whose content
    // (up to but not including its own terminator) has trailing spaces or
    // tabs.
    if config.trim_trailing_whitespace == Some(true) {
        let mut pos = 0usize;
        while pos < text.len() {
            let content_start = pos;
            while pos < text.len() && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            let content_end = pos;
            let content = &text[content_start..content_end];
            let trimmed_len = content.trim_end_matches([' ', '\t']).len();
            if trimmed_len < content.len() {
                changes.push(Change::new(content_start + trimmed_len..content_end, ""));
            }
            if pos < text.len() {
                pos += if bytes[pos] == b'\r' && bytes.get(pos + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
        }
    }

    // Pass C: normalise every remaining line ending, skipping any Pass B is
    // already deleting outright.
    if let Some(eol) = config.end_of_line {
        let target = eol_str(eol);
        let removing_tail =
            config.insert_final_newline == Some(false) && content_tail_end < text.len();
        let mut pos = 0usize;
        while pos < text.len() {
            if removing_tail && pos >= content_tail_end {
                break;
            }
            if bytes[pos] == b'\r' && bytes.get(pos + 1) == Some(&b'\n') {
                if target != "\r\n" {
                    changes.push(Change::new(pos..pos + 2, target));
                }
                pos += 2;
            } else if bytes[pos] == b'\r' {
                if target != "\r" {
                    changes.push(Change::new(pos..pos + 1, target));
                }
                pos += 1;
            } else if bytes[pos] == b'\n' {
                if target != "\n" {
                    changes.push(Change::new(pos..pos + 1, target));
                }
                pos += 1;
            } else {
                pos += 1;
            }
        }
    }

    if changes.is_empty() {
        return None;
    }
    Transaction::new(changes).ok()
}

fn eol_str(eol: EndOfLine) -> &'static str {
    match eol {
        EndOfLine::Lf => "\n",
        EndOfLine::Crlf => "\r\n",
        EndOfLine::Cr => "\r",
    }
}

/// The charset the file is written under. `None` when the config names no
/// charset or names one the buffer cannot represent.
pub fn save_charset(config: &EditorConfig) -> Option<Charset> {
    match config.charset {
        Some(Charset::Utf8) => Some(Charset::Utf8),
        Some(Charset::Utf8Bom) => Some(Charset::Utf8Bom),
        _ => None,
    }
}

// ---- glob matching --------------------------------------------------

/// A pattern with no `/` matches the file's name at any depth -- tried
/// anchored at the very start of `relative_path` and again right after
/// every `/` in it, rather than compiling a literal `**/` prefix (which
/// would wrongly *require* an actual `/` character, failing on a file
/// directly inside the matched directory with no subdirectory at all).
fn glob_matches(pattern: &str, relative_path: &str) -> bool {
    let Some(expansions) = expand_braces(pattern) else {
        return false;
    };
    for expansion in &expansions {
        let Some(segments) = compile(expansion) else {
            continue;
        };
        if expansion.contains('/') {
            if matches_segments(&segments, relative_path) {
                return true;
            }
            continue;
        }
        if matches_segments(&segments, relative_path) {
            return true;
        }
        for (i, c) in relative_path.char_indices() {
            if c == '/' && matches_segments(&segments, &relative_path[i + 1..]) {
                return true;
            }
        }
    }
    false
}

/// Expands the first top-level `{...}` group in `pattern` into the concrete
/// patterns it stands for -- comma alternation (`{a,b}`) or a numeric range
/// (`{1..9}`) -- recursing on the remainder so multiple groups compose.
/// `None` on anything this subset doesn't cover (nested braces, an
/// unterminated group, a malformed range/list, or more combinations than
/// `MAX_GLOB_EXPANSIONS`), which the caller treats as "matches nothing".
fn expand_braces(pattern: &str) -> Option<Vec<String>> {
    expand_braces_at_depth(pattern, 0)
}

fn expand_braces_at_depth(pattern: &str, depth: usize) -> Option<Vec<String>> {
    let Some(open) = pattern.find('{') else {
        return Some(vec![pattern.to_string()]);
    };
    if depth >= MAX_GLOB_GROUPS {
        return None;
    }
    let prefix = &pattern[..open];
    let rest = &pattern[open + 1..];
    let close_rel = rest.find('}')?;
    let inner = &rest[..close_rel];
    if inner.is_empty() || inner.contains('{') {
        return None;
    }
    let after = &rest[close_rel + 1..];

    let alternatives: Vec<String> = if let Some((lo, hi)) = parse_numeric_range(inner) {
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        let count = hi.checked_sub(lo).and_then(|d| d.checked_add(1))?;
        if count as usize > MAX_GLOB_EXPANSIONS {
            return None;
        }
        (lo..=hi).map(|n| n.to_string()).collect()
    } else {
        let parts: Vec<String> = inner.split(',').map(str::to_string).collect();
        if parts.len() < 2 {
            return None;
        }
        parts
    };

    let suffixes = expand_braces_at_depth(after, depth + 1)?;
    if alternatives.len().saturating_mul(suffixes.len()) > MAX_GLOB_EXPANSIONS {
        return None;
    }
    let mut result = Vec::with_capacity(alternatives.len() * suffixes.len());
    for alt in &alternatives {
        for suffix in &suffixes {
            result.push(format!("{prefix}{alt}{suffix}"));
        }
    }
    Some(result)
}

fn parse_numeric_range(inner: &str) -> Option<(i64, i64)> {
    let (lo, hi) = inner.split_once("..")?;
    Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?))
}

#[derive(Debug, Clone)]
enum Segment {
    Star,
    DoubleStar,
    Question,
    Class { negate: bool, set: Vec<char> },
    Literal(char),
}

/// Compiles a single, brace-free glob into segments. `None` on a construct
/// this subset doesn't support (an unterminated `[...]`, a stray `{`/`}`
/// left over from a caller that skipped brace expansion).
fn compile(pattern: &str) -> Option<Vec<Segment>> {
    let mut segments = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    segments.push(Segment::DoubleStar);
                } else {
                    segments.push(Segment::Star);
                }
            }
            '?' => segments.push(Segment::Question),
            '[' => {
                let negate = chars.peek() == Some(&'!');
                if negate {
                    chars.next();
                }
                let mut set = Vec::new();
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        closed = true;
                        break;
                    }
                    set.push(c2);
                }
                if !closed || set.is_empty() {
                    return None;
                }
                segments.push(Segment::Class { negate, set });
            }
            '{' | '}' => return None,
            _ => segments.push(Segment::Literal(c)),
        }
    }
    Some(segments)
}

/// Iterative match with a single remembered backtrack point (the most
/// recent `*`/`**`), the standard linear-time wildcard algorithm: each
/// backtrack advances that star's consumed length by exactly one character,
/// so the total work across every backtrack is bounded by `path`'s length,
/// not exponential in the number of stars (§4.5).
fn matches_segments(segments: &[Segment], path: &str) -> bool {
    let chars: Vec<char> = path.chars().collect();
    let (mut si, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, bool)> = None;
    let mut star_pi = 0usize;

    loop {
        if si < segments.len() {
            if let Segment::Star | Segment::DoubleStar = segments[si] {
                star = Some((si, matches!(segments[si], Segment::DoubleStar)));
                star_pi = pi;
                si += 1;
                continue;
            }
        }
        let single_matches =
            si < segments.len() && pi < chars.len() && single_matches(&segments[si], chars[pi]);
        if single_matches {
            si += 1;
            pi += 1;
            continue;
        }
        if si == segments.len() && pi == chars.len() {
            return true;
        }
        match star {
            Some((star_si, allow_slash)) => {
                if !allow_slash && chars.get(star_pi) == Some(&'/') {
                    return false;
                }
                star_pi += 1;
                if star_pi > chars.len() {
                    return false;
                }
                si = star_si + 1;
                pi = star_pi;
            }
            None => return false,
        }
    }
}

fn single_matches(segment: &Segment, c: char) -> bool {
    match segment {
        Segment::Literal(l) => c == *l,
        Segment::Question => c != '/',
        Segment::Class { negate, set } => c != '/' && (set.contains(&c) != *negate),
        Segment::Star | Segment::DoubleStar => unreachable!("handled before single_matches"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{IndentStyle, Selection, Selections, TextBuffer};

    // ---- parse ----

    #[test]
    fn parse_reads_every_property() {
        let config = parse(
            "root = true\n\n[*.rs]\nindent_style = space\nindent_size = 4\n\
             trim_trailing_whitespace = true\ninsert_final_newline = true\n\
             end_of_line = lf\ncharset = utf-8\n",
            "src/main.rs",
        );
        assert_eq!(config.indent_style, Some(IndentStyle::Spaces));
        assert_eq!(config.indent_size, Some(4));
        assert_eq!(config.trim_trailing_whitespace, Some(true));
        assert_eq!(config.insert_final_newline, Some(true));
        assert_eq!(config.end_of_line, Some(EndOfLine::Lf));
        assert_eq!(config.charset, Some(Charset::Utf8));
    }

    #[test]
    fn parse_ignores_a_non_matching_section() {
        let config = parse("[*.py]\nindent_size = 2\n", "src/main.rs");
        assert_eq!(config.indent_size, None);
    }

    #[test]
    fn parse_lets_a_later_matching_section_win() {
        let config = parse("[*.rs]\nindent_size = 2\n[*.rs]\nindent_size = 8\n", "a.rs");
        assert_eq!(config.indent_size, Some(8));
    }

    #[test]
    fn parse_skips_malformed_lines_and_unknown_properties() {
        let config = parse(
            "[*.rs]\nnot a property line\nfrobnicate = yes\nindent_size = 2\n",
            "a.rs",
        );
        assert_eq!(config.indent_size, Some(2));
    }

    #[test]
    fn parse_skips_a_section_header_that_fails_to_parse() {
        // Unterminated brace: `compile`/`expand_braces` reject it, and a
        // failed header must match nothing, not everything.
        let config = parse("[{unterminated]\nindent_size = 2\n", "a.rs");
        assert_eq!(config.indent_size, None);
    }

    #[test]
    fn parse_truncates_after_the_section_cap() {
        // A section past MAX_EDITORCONFIG_SECTIONS must never be read, not
        // merely have its properties ignored -- the walk itself stops.
        let mut content = String::new();
        for i in 0..MAX_EDITORCONFIG_SECTIONS + 5 {
            content.push_str(&format!("[file{i}.rs]\nindent_size = {i}\n"));
        }
        let target = format!("file{}.rs", MAX_EDITORCONFIG_SECTIONS + 2);
        let config = parse(&content, &target);
        assert_eq!(config.indent_size, None);
    }

    // ---- glob subset ----

    #[test]
    fn glob_star_matches_within_one_path_segment() {
        assert!(glob_matches("*.rs", "src/main.rs"));
        assert!(!glob_matches("*.rs", "main.py"));
    }

    #[test]
    fn glob_double_star_crosses_path_segments() {
        assert!(glob_matches("src/**/mod.rs", "src/a/b/mod.rs"));
        assert!(!glob_matches("src/*/mod.rs", "src/a/b/mod.rs"));
    }

    #[test]
    fn glob_question_matches_exactly_one_character() {
        assert!(glob_matches("?.rs", "a.rs"));
        assert!(!glob_matches("?.rs", "ab.rs"));
    }

    #[test]
    fn glob_character_class_and_negation() {
        assert!(glob_matches("[abc].rs", "a.rs"));
        assert!(!glob_matches("[abc].rs", "d.rs"));
        assert!(glob_matches("[!abc].rs", "d.rs"));
        assert!(!glob_matches("[!abc].rs", "a.rs"));
    }

    #[test]
    fn glob_brace_alternation_and_numeric_range() {
        assert!(glob_matches("*.{rs,toml}", "Cargo.toml"));
        assert!(!glob_matches("*.{rs,toml}", "a.py"));
        assert!(glob_matches("v{1..9}.txt", "v7.txt"));
        assert!(!glob_matches("v{1..9}.txt", "v10.txt"));
    }

    #[test]
    fn glob_a_malformed_header_matches_nothing() {
        assert!(!glob_matches("[unterminated", "a.rs"));
        assert!(!glob_matches("{unterminated", "a.rs"));
    }

    #[test]
    fn glob_pathological_pattern_completes_quickly() {
        let pattern = "*".repeat(200) + "x";
        let path = "a".repeat(5000);
        let start = std::time::Instant::now();
        assert!(!glob_matches(&pattern, &path));
        assert!(start.elapsed().as_secs_f64() < 1.0);
    }

    #[test]
    fn glob_many_chained_brace_groups_does_not_overflow_the_stack() {
        // Well under MAX_EDITORCONFIG_BYTES (each group is 5 bytes), but far
        // more groups than MAX_GLOB_GROUPS allows to recurse through -- must
        // be rejected, not blow the stack (docs/security-findings/
        // editorconfig-glob-braces-2026-08-18.md, finding 1).
        let pattern = "{a,b}".repeat(10_000) + ".txt";
        assert!(!glob_matches(&pattern, "a.txt"));
    }

    #[test]
    fn glob_extreme_numeric_range_does_not_panic() {
        // i64 endpoints chosen so both the subtraction and the +1 would
        // overflow i64 if done with raw arithmetic (finding 2 in the same
        // doc). Must be rejected, not panic.
        assert!(!glob_matches("v{0..9223372036854775807}", "v1"));
        assert!(!glob_matches(
            "v{-9223372036854775808..9223372036854775807}",
            "v1"
        ));
    }

    // ---- resolve ----

    #[test]
    fn resolve_nearer_file_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(
            dir.path().join("sub/.editorconfig"),
            "[*.rs]\nindent_size = 8\n",
        )
        .unwrap();
        let file = dir.path().join("sub/a.rs");
        std::fs::write(&file, "").unwrap();

        let config = resolve(dir.path(), &file).unwrap();
        assert_eq!(config.indent_size, Some(8));
    }

    #[test]
    fn resolve_root_true_stops_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(
            dir.path().join("sub/.editorconfig"),
            "root = true\n[*.rs]\nindent_style = tab\n",
        )
        .unwrap();
        let file = dir.path().join("sub/a.rs");
        std::fs::write(&file, "").unwrap();

        let config = resolve(dir.path(), &file).unwrap();
        assert_eq!(config.indent_style, Some(IndentStyle::Tabs));
        // The outer .editorconfig's indent_size must never be seen.
        assert_eq!(config.indent_size, None);
    }

    #[test]
    fn resolve_rejects_a_file_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("a.rs");
        std::fs::write(&file, "").unwrap();

        let err = resolve(root.path(), &file).unwrap_err();
        assert_eq!(err, EditorConfigError::OutsideRoot(file));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_a_symlink_escaping_root() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "").unwrap();
        let link = root.path().join("escape.rs");
        symlink(outside.path().join("secret.rs"), &link).unwrap();

        assert!(resolve(root.path(), &link).is_err());
    }

    #[test]
    fn resolve_refuses_a_file_over_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "a\n".repeat((MAX_EDITORCONFIG_BYTES as usize / 2) + 10);
        std::fs::write(
            dir.path().join(".editorconfig"),
            format!("[*.rs]\nindent_size = 2\n# {huge}"),
        )
        .unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "").unwrap();

        let config = resolve(dir.path(), &file).unwrap();
        assert_eq!(config.indent_size, None);
    }

    #[test]
    fn resolve_stops_at_the_depth_cap() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..MAX_EDITORCONFIG_DEPTH + 5 {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .unwrap();
        let file = deep.join("a.rs");
        std::fs::write(&file, "").unwrap();

        // Must not hang or error -- exceeding the depth cap simply means
        // the far-away root .editorconfig is never reached.
        let config = resolve(dir.path(), &file).unwrap();
        assert_eq!(config.indent_size, None);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_ends_the_walk_at_an_unreadable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".editorconfig"),
            "[*.rs]\nindent_size = 2\n",
        )
        .unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let file = locked.join("a.rs");

        let err = resolve(dir.path(), &file);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Can't even canonicalize a file inside a locked directory -- this
        // resolves as OutsideRoot rather than hanging or panicking.
        assert!(err.is_err());
    }

    // ---- save_edit / save_charset ----

    #[test]
    fn save_edit_doc_example() {
        let config = parse(
            "root = true\n\n[*.rs]\nindent_style = space\nindent_size = 4\n\
             trim_trailing_whitespace = true\ninsert_final_newline = true\n",
            "src/main.rs",
        );
        assert_eq!(config.indent_size, Some(4));
        let edit = save_edit("fn main() {}   ", &config).unwrap();
        assert_eq!(edit.changes().len(), 2);
    }

    #[test]
    fn save_edit_trims_trailing_whitespace_alone() {
        let config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            ..Default::default()
        };
        let mut buffer = TextBuffer::new("a  \nb\t\n", None);
        buffer.apply(save_edit(buffer.text(), &config).unwrap());
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn save_edit_inserts_final_newline_alone() {
        let config = EditorConfig {
            insert_final_newline: Some(true),
            ..Default::default()
        };
        assert_eq!(
            save_edit("a", &config).map(|t| apply_str("a", t)),
            Some("a\n".to_string())
        );
    }

    #[test]
    fn save_edit_removes_trailing_newlines_when_false() {
        let config = EditorConfig {
            insert_final_newline: Some(false),
            ..Default::default()
        };
        assert_eq!(
            save_edit("a\n\n\n", &config).map(|t| apply_str("a\n\n\n", t)),
            Some("a".to_string())
        );
    }

    #[test]
    fn save_edit_normalizes_line_endings_alone() {
        let config = EditorConfig {
            end_of_line: Some(EndOfLine::Crlf),
            ..Default::default()
        };
        assert_eq!(
            save_edit("a\nb\n", &config).map(|t| apply_str("a\nb\n", t)),
            Some("a\r\nb\r\n".to_string())
        );
    }

    #[test]
    fn save_edit_applies_all_three_in_the_documented_order() {
        let config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(true),
            end_of_line: Some(EndOfLine::Crlf),
            ..Default::default()
        };
        let original = "a  \r\nb   ";
        let result = apply_str(original, save_edit(original, &config).unwrap());
        assert_eq!(result, "a\r\nb\r\n");
    }

    #[test]
    fn save_edit_is_none_when_nothing_changes() {
        let config = EditorConfig::default();
        assert_eq!(save_edit("a\n", &config), None);
        let trimmed_config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            ..Default::default()
        };
        assert_eq!(save_edit("a\nb\n", &trimmed_config), None);
    }

    #[test]
    fn save_edit_keeps_every_caret_across_a_multi_caret_buffer() {
        let config = EditorConfig {
            trim_trailing_whitespace: Some(true),
            ..Default::default()
        };
        let mut buffer = TextBuffer::new("a  \nb  \n", None);
        buffer.set_selections(Selections::new(
            vec![Selection::caret(1), Selection::caret(6)],
            0,
        ));
        let edit = save_edit(buffer.text(), &config).unwrap();
        buffer.apply(edit);
        assert_eq!(buffer.text(), "a\nb\n");
        // Both carets survive as carets (not collapsed by a whole-buffer
        // replace), each still inside its own (now-shorter) line.
        assert_eq!(buffer.selections().len(), 2);
    }

    #[test]
    fn save_charset_only_utf8_variants_are_applied() {
        assert_eq!(
            save_charset(&EditorConfig {
                charset: Some(Charset::Utf8Bom),
                ..Default::default()
            }),
            Some(Charset::Utf8Bom)
        );
        assert_eq!(
            save_charset(&EditorConfig {
                charset: Some(Charset::Latin1),
                ..Default::default()
            }),
            None
        );
        assert_eq!(save_charset(&EditorConfig::default()), None);
    }

    fn apply_str(text: &str, transaction: Transaction) -> String {
        let mut buffer = TextBuffer::new(text, None);
        buffer.apply(transaction);
        buffer.text().to_string()
    }
}
