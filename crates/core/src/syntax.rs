//! Hand-rolled, pure-Rust syntax tokenizer. Covers twenty built-in
//! languages (see `BUILTINS`): JSON, YAML, systemd units, env files, INI,
//! Makefiles, Dockerfiles, Rust, TOML, Shell, Python, Go, JavaScript/
//! TypeScript, C/C++, Java, SQL, CSS, Markdown, XML/HTML, and gitignore
//! patterns. Single
//! left-to-right pass, never revisiting a byte position — a load-bearing
//! invariant (see `docs/features/syntax-highlighting.md` §4): it's what
//! keeps `tokenize` O(n) even under adversarial input shapes (e.g. a
//! multi-megabyte line with no terminator for any rule in progress), not
//! just a style choice.

use std::ops::Range;

/// How a single token should be colored. The first six variants are the
/// original data-language set; the rest were added by
/// `docs/features/richer-highlighting-and-usages-popup.md` so that a
/// programming-language file isn't mostly plain text — they're what make
/// identifiers visually separable from each other, not just from keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    String,
    Number,
    Comment,
    Punctuation,
    /// The key half of a `Key=Value` (systemd) or `key: value` (YAML)
    /// line — see `SyntaxRules::key_separator`. JSON has no bare keys
    /// (object keys are ordinary `String` tokens), so this never appears
    /// when tokenizing with `JSON`. Also produced by
    /// `SyntaxRules::attribute_names` for `name="value"` attributes.
    Key,
    /// An identifier immediately followed by `(` — a call or a definition;
    /// the tokenizer has no symbol table and deliberately doesn't try to
    /// tell those two apart.
    Function,
    /// A `type_keywords` match, a `capitalized_is_type` identifier, or a
    /// `sigil_words` entry that names one (an XML tag, a Rust lifetime).
    Type,
    /// A macro invocation (`name!(…)`), an attribute (`#[…]`), a decorator
    /// (`@name`), or a C preprocessor directive — all "this line is
    /// meta, not code" markers that share one color.
    Macro,
    /// A `SCREAMING_SNAKE_CASE` identifier, or a shell/Makefile `$VAR`.
    Constant,
    /// A single operator character (`=`, `+`, `<`, …). Distinct from
    /// `Punctuation` (brackets, separators), which stays the plain text
    /// color: operators are where the logic is, brackets are structure.
    Operator,
    /// A value binding the regex tokenizer cannot distinguish from a type
    /// by shape alone -- local variables, parameters, struct fields, enum
    /// members, event bindings. Never produced by `tokenize` itself (no
    /// `SyntaxRules` field maps to it); exists solely as a target for
    /// semantic-token classification (`docs/features/semantic-highlighting.md`
    /// §3.2), so that "is `foo` a type or a variable" -- a gap the regex
    /// tokenizer cannot close by itself -- has an answer once a language
    /// server is attached. Colored identically to plain text: real
    /// JetBrains IDEs don't give local variables a strikingly distinct
    /// color either, so this needs no new palette token.
    Variable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Byte range within the tokenized text.
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// A language's token rules, applied in a fixed priority order by
/// `tokenize` (see its doc comment). Every field is data — no per-language
/// code — so a new language is a new `SyntaxRules` value.
#[derive(Debug)]
pub struct SyntaxRules {
    pub name: &'static str,
    /// No leading `.` — matched against a path's extension the same way
    /// `ide_core::LanguageConfig::extension` is, but this list is fixed
    /// and not user-configurable.
    pub extensions: &'static [&'static str],
    /// A line starting with one of these (after leading whitespace, column
    /// position irrelevant otherwise) comments out the rest of the line.
    pub line_comment_prefixes: &'static [&'static str],
    /// `(start, end)` delimiter pair for a block comment.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Characters that open/close a string literal (each is both the open
    /// and close delimiter for itself, e.g. `'"'`). Escaped via `\`
    /// inside the literal; unterminated at end-of-line is treated as
    /// ending there.
    pub string_quotes: &'static [char],
    /// Case-sensitive exact-word matches.
    pub keywords: &'static [&'static str],
    /// Case-sensitive exact-word matches colored as `TokenKind::Type`
    /// regardless of their shape — primitive/standard type names that
    /// `capitalized_is_type` would miss because they're lowercase (`u32`,
    /// `int`, `string`).
    pub type_keywords: &'static [&'static str],
    /// Single characters tokenized individually as `Punctuation`.
    pub punctuation: &'static [char],
    /// Single characters tokenized individually as `Operator`. Checked
    /// before `punctuation`; the two sets are expected to be disjoint.
    pub operators: &'static [char],
    /// Exact bare-filename matches (the full `file_name()`, not an
    /// extension), checked case-sensitively against a short enumerated
    /// list of known spellings -- e.g. `["Makefile", "makefile",
    /// "GNUmakefile"]`. Deliberately exact-match rather than
    /// case-normalized: the realistic spelling variants are few and known
    /// in advance, so enumerating them is simpler and less surprising than
    /// a lowercasing pass. Empty for extension-only languages.
    pub filenames: &'static [&'static str],
    /// Prefix matches against a bare filename, for suffix-variant
    /// conventions like `Dockerfile.dev` or `.env.local`. Same
    /// case-sensitivity rationale as `filenames`.
    pub filename_prefixes: &'static [&'static str],
    /// Line-start prefixes that claim the whole rest of the line as a
    /// single token of the given kind -- e.g. `[("#", TokenKind::Keyword)]`
    /// for Markdown headings, `[("#[", TokenKind::Macro)]` for Rust
    /// attributes. Matched with `starts_with`, so `"#"` also covers
    /// `##`/`###`. Distinct from `line_comment_prefixes`, which hardcodes
    /// `TokenKind::Comment`: a heading is a comment-shaped rule with a
    /// different color, and there was no way to express that.
    pub line_prefix_tokens: &'static [(&'static str, TokenKind)],
    /// `(prefix, kind)` pairs where the prefix plus the identifier
    /// immediately after it form one token -- `<div` in XML, `'a` in Rust,
    /// `$PATH` in shell. Tried in order, so a longer prefix that shares a
    /// head with a shorter one (`"</"` before `"<"`) must come first. The
    /// rule fails (and the prefix falls through to the ordinary
    /// punctuation/operator path) when no identifier follows, which is
    /// what keeps `$(cmd)` and `<3` from matching.
    pub sigil_words: &'static [(&'static str, TokenKind)],
    /// If set, a line (after skipping leading whitespace) is checked for
    /// this character via a continuous forward scan from the line's first
    /// non-whitespace character (whitespace is not a stop condition —
    /// only a newline, a brace, a `string_quotes` char, or a
    /// `line_comment_prefixes` match stops it) — reaching the separator
    /// after consuming at least one character tokenizes the scanned span
    /// as `Key`. `None` for JSON (object keys are quoted strings, already
    /// covered by `string_quotes`).
    pub key_separator: Option<char>,
    /// An identifier starting with an uppercase letter is a `Type`.
    /// Checked *after* `upper_case_is_constant`, so `MAX_LEN` stays a
    /// constant in languages where both are on.
    pub capitalized_is_type: bool,
    /// A `SCREAMING_SNAKE_CASE` identifier is a `Constant`.
    pub upper_case_is_constant: bool,
    /// `name!(`, `name![`, `name!{` is a `Macro` (the `!` is included in
    /// the token). The delimiter check is what keeps `a != b` from
    /// reading as a macro invocation of `a`.
    pub macro_bang: bool,
    /// An identifier immediately followed by `=` is a `Key` -- XML/HTML
    /// attribute names. Off everywhere else, where `=` is assignment and
    /// the left-hand side is an ordinary variable.
    pub attribute_names: bool,
    /// Bracket pairs, `(open, close)`. Drives auto-closing, matching,
    /// auto-indent's block detection and Move Statement's balance test
    /// (`docs/features/smart-editing.md` §2.1). Empty for a language with
    /// no bracketing worth the behaviour.
    ///
    /// Each `open` must be distinct from every other `open` and from every
    /// `close`: a language using one character for both would break the
    /// depth scan in `TextBuffer::matching_bracket`. Quotes are *not*
    /// listed here -- they are `string_quotes`, matched by a different rule
    /// precisely because they are their own closer.
    pub brackets: &'static [(char, char)],
    /// A line whose trimmed content *ends* with one of these opens an
    /// indented block even though no bracket is left open -- Python's and
    /// YAML's `":"`. Combined with the bracket rule rather than added to
    /// it: a line that both opens a bracket and ends with a trigger still
    /// indents by exactly one level.
    pub indent_line_suffixes: &'static [&'static str],
}

pub const JSON: SyntaxRules = SyntaxRules {
    name: "JSON",
    extensions: &["json"],
    line_comment_prefixes: &[],
    block_comment: None,
    string_quotes: &['"'],
    keywords: &["true", "false", "null"],
    type_keywords: &[],
    punctuation: &['{', '}', '[', ']', ':', ','],
    operators: &[],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const YAML: SyntaxRules = SyntaxRules {
    name: "YAML",
    extensions: &["yaml", "yml"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &["true", "false", "null", "~"],
    type_keywords: &[],
    // ':' is deliberately included: re-matching a Key rule's own
    // separator as Punctuation right after it closes a Key span is the
    // same pattern systemd's '=' already relies on (see the worked
    // example in docs/features/syntax-highlighting.md §5) -- there is no
    // actual conflict to avoid by excluding it.
    punctuation: &['{', '}', '[', ']', ',', ':'],
    operators: &[],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: Some(':'),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('[', ']'), ('{', '}')],
    indent_line_suffixes: &[":"],
};

pub const SYSTEMD_UNIT: SyntaxRules = SyntaxRules {
    name: "systemd unit file",
    extensions: &[
        "service", "socket", "timer", "mount", "target", "slice", "path", "swap", "scope",
    ],
    line_comment_prefixes: &["#", ";"],
    block_comment: None,
    string_quotes: &[],
    keywords: &[],
    type_keywords: &[],
    punctuation: &['[', ']', '='],
    operators: &[],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: Some('='),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('[', ']')],
    indent_line_suffixes: &[],
};

pub const ENV: SyntaxRules = SyntaxRules {
    name: "env file",
    extensions: &["env"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &[],
    type_keywords: &[],
    punctuation: &['='],
    operators: &[],
    filenames: &[".env"],
    filename_prefixes: &[".env."],
    line_prefix_tokens: &[],
    sigil_words: &[("$", TokenKind::Constant)],
    key_separator: Some('='),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[],
    indent_line_suffixes: &[],
};

pub const INI: SyntaxRules = SyntaxRules {
    name: "INI",
    extensions: &["ini", "cfg", "conf", "properties"],
    line_comment_prefixes: &["#", ";"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &["true", "false", "yes", "no", "on", "off"],
    type_keywords: &[],
    punctuation: &['[', ']', '='],
    operators: &[],
    filenames: &[".gitconfig", ".editorconfig", ".npmrc", ".flake8"],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: Some('='),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('[', ']')],
    indent_line_suffixes: &[],
};

pub const MAKEFILE: SyntaxRules = SyntaxRules {
    name: "Makefile",
    extensions: &["mk"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &[],
    keywords: &[
        "ifeq", "ifneq", "ifdef", "ifndef", "else", "endif", "include", "export", "unexport",
        "override", "define", "endef",
    ],
    type_keywords: &[],
    punctuation: &[':', '='],
    operators: &[],
    filenames: &["Makefile", "makefile", "GNUmakefile"],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    // `$(CC)`/`${CC}` deliberately fall through (no identifier directly
    // after the sigil); only the bare `$VAR` form is a Constant.
    sigil_words: &[("$", TokenKind::Constant)],
    // ':' over '=': key_separator holds a single char, and every Makefile
    // has at least one target line while plenty have no top-level variable
    // assignments -- see docs/features/syntax-highlighting-env-make-docker.md §3.
    key_separator: Some(':'),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('(', ')'), ('{', '}')],
    indent_line_suffixes: &[],
};

pub const DOCKERFILE: SyntaxRules = SyntaxRules {
    name: "Dockerfile",
    extensions: &["dockerfile"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &[
        "FROM",
        "RUN",
        "CMD",
        "LABEL",
        "MAINTAINER",
        "EXPOSE",
        "ENV",
        "ADD",
        "COPY",
        "ENTRYPOINT",
        "VOLUME",
        "USER",
        "WORKDIR",
        "ARG",
        "ONBUILD",
        "STOPSIGNAL",
        "HEALTHCHECK",
        "SHELL",
    ],
    type_keywords: &[],
    punctuation: &[],
    operators: &[],
    filenames: &["Dockerfile", "dockerfile"],
    filename_prefixes: &["Dockerfile.", "dockerfile."],
    line_prefix_tokens: &[],
    sigil_words: &[("$", TokenKind::Constant)],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('(', ')'), ('{', '}')],
    indent_line_suffixes: &[],
};

pub const RUST: SyntaxRules = SyntaxRules {
    name: "Rust",
    extensions: &["rs"],
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    // '\'' is deliberately excluded: Rust uses it for both char literals
    // and lifetimes, and treating it as a string quote would make every
    // `&'a str` open an unterminated string swallowing the rest of the
    // line -- see docs/features/syntax-highlighting-more-languages.md §3.
    // The lifetime half is covered by `sigil_words` instead.
    string_quotes: &['"'],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while",
    ],
    type_keywords: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
        "u16", "u32", "u64", "u128", "usize",
    ],
    punctuation: &['{', '}', '(', ')', '[', ']', ';', ',', ':'],
    operators: &[
        '&', '=', '<', '>', '+', '-', '*', '/', '%', '!', '|', '^', '?', '.',
    ],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[("#![", TokenKind::Macro), ("#[", TokenKind::Macro)],
    sigil_words: &[("'", TokenKind::Type)],
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: true,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const TOML: SyntaxRules = SyntaxRules {
    name: "TOML",
    extensions: &["toml"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &["true", "false"],
    type_keywords: &[],
    punctuation: &['[', ']', '=', ',', '.'],
    operators: &[],
    filenames: &["Cargo.lock"],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: Some('='),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('[', ']')],
    indent_line_suffixes: &[],
};

pub const SHELL: SyntaxRules = SyntaxRules {
    name: "Shell",
    extensions: &["sh", "bash", "zsh", "ksh"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "select", "return", "break", "continue", "local", "export", "readonly",
        "declare", "source", "exit", "shift", "trap", "set", "unset",
    ],
    type_keywords: &[],
    punctuation: &['(', ')', '{', '}', ';'],
    operators: &['|', '&', '<', '>', '='],
    filenames: &[
        ".bashrc",
        ".zshrc",
        ".bash_profile",
        ".zprofile",
        ".profile",
        ".bash_aliases",
    ],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[("$", TokenKind::Constant)],
    // None despite `VAR=value` being real shell: the Key rule scans a whole
    // line for its separator, so `for i in a=b; do` would emit a spurious
    // Key covering "for i in a" -- shell lines carry '=' in too many
    // non-assignment positions for the line-start heuristic to hold.
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('(', ')'), ('{', '}')],
    indent_line_suffixes: &[],
};

pub const PYTHON: SyntaxRules = SyntaxRules {
    name: "Python",
    extensions: &["py", "pyi"],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &['"', '\''],
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "False", "finally", "for", "from", "global", "if", "import",
        "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
        "try", "while", "with", "yield",
    ],
    type_keywords: &[
        "bool",
        "bytes",
        "dict",
        "float",
        "frozenset",
        "int",
        "list",
        "object",
        "set",
        "str",
        "tuple",
        "type",
    ],
    punctuation: &['(', ')', '[', ']', '{', '}', ':', ','],
    operators: &[
        '=', '<', '>', '+', '-', '*', '/', '%', '!', '|', '&', '^', '.',
    ],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[("@", TokenKind::Macro)],
    sigil_words: &[],
    // None: a `def f():` line would otherwise emit a Key for "def f()".
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[":"],
};

pub const GO: SyntaxRules = SyntaxRules {
    name: "Go",
    extensions: &["go"],
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    // Unlike Rust, '\'' is safe here: Go's single quotes are unambiguously
    // rune literals, with no lifetime syntax to collide with.
    string_quotes: &['"', '`', '\''],
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
        "nil",
        "true",
        "false",
    ],
    type_keywords: &[
        "any",
        "bool",
        "byte",
        "complex64",
        "complex128",
        "error",
        "float32",
        "float64",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uintptr",
    ],
    punctuation: &['{', '}', '(', ')', '[', ']', ';', ',', ':'],
    operators: &[
        '=', '<', '>', '&', '+', '-', '*', '/', '%', '!', '|', '^', '.',
    ],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const JAVASCRIPT: SyntaxRules = SyntaxRules {
    // One rule set for both: TypeScript is JavaScript plus type syntax,
    // and every construct this tokenizer can actually see (comments,
    // strings, keywords, calls) is shared between them.
    name: "JavaScript/TypeScript",
    extensions: &["js", "jsx", "mjs", "cjs", "ts", "tsx"],
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['"', '\'', '`'],
    keywords: &[
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ],
    type_keywords: &[
        "any", "bigint", "boolean", "never", "number", "object", "string", "symbol", "unknown",
    ],
    punctuation: &['{', '}', '(', ')', '[', ']', ';', ',', ':'],
    operators: &[
        '=', '<', '>', '+', '-', '*', '/', '%', '!', '&', '|', '^', '?', '.',
    ],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[("@", TokenKind::Macro)],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const C: SyntaxRules = SyntaxRules {
    name: "C/C++",
    extensions: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx"],
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['"', '\''],
    keywords: &[
        "alignas",
        "auto",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "constexpr",
        "continue",
        "decltype",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "explicit",
        "extern",
        "false",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "namespace",
        "new",
        "noexcept",
        "nullptr",
        "operator",
        "private",
        "protected",
        "public",
        "register",
        "restrict",
        "return",
        "sizeof",
        "static",
        "struct",
        "switch",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typedef",
        "typename",
        "union",
        "using",
        "virtual",
        "volatile",
        "while",
    ],
    type_keywords: &[
        "bool",
        "char",
        "double",
        "float",
        "int",
        "long",
        "ptrdiff_t",
        "short",
        "signed",
        "size_t",
        "ssize_t",
        "unsigned",
        "void",
        "wchar_t",
        "int8_t",
        "int16_t",
        "int32_t",
        "int64_t",
        "uint8_t",
        "uint16_t",
        "uint32_t",
        "uint64_t",
    ],
    punctuation: &['{', '}', '(', ')', '[', ']', ';', ',', ':'],
    operators: &[
        '=', '<', '>', '+', '-', '*', '/', '%', '!', '&', '|', '^', '?', '.', '~',
    ],
    filenames: &[],
    filename_prefixes: &[],
    // Preprocessor directives claim the whole line, the same shape Rust's
    // `#[…]` attributes use.
    line_prefix_tokens: &[("#", TokenKind::Macro)],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const JAVA: SyntaxRules = SyntaxRules {
    name: "Java",
    extensions: &["java"],
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['"', '\''],
    keywords: &[
        "abstract",
        "assert",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "interface",
        "native",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "record",
        "return",
        "sealed",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "true",
        "false",
        "try",
        "var",
        "volatile",
        "while",
        "yield",
    ],
    type_keywords: &[
        "boolean", "byte", "char", "double", "float", "int", "long", "short", "void",
    ],
    punctuation: &['{', '}', '(', ')', '[', ']', ';', ',', ':'],
    operators: &[
        '=', '<', '>', '+', '-', '*', '/', '%', '!', '&', '|', '^', '?', '.',
    ],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[("@", TokenKind::Macro)],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: true,
    upper_case_is_constant: true,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')'), ('[', ']')],
    indent_line_suffixes: &[],
};

pub const SQL: SyntaxRules = SyntaxRules {
    // Keyword matching is case-sensitive by design (see `tokenize`), and
    // SQL is written in both cases, so each keyword is listed twice.
    name: "SQL",
    extensions: &["sql"],
    line_comment_prefixes: &["--"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['\'', '"'],
    keywords: &[
        "SELECT",
        "select",
        "FROM",
        "from",
        "WHERE",
        "where",
        "INSERT",
        "insert",
        "INTO",
        "into",
        "VALUES",
        "values",
        "UPDATE",
        "update",
        "SET",
        "set",
        "DELETE",
        "delete",
        "CREATE",
        "create",
        "TABLE",
        "table",
        "DROP",
        "drop",
        "ALTER",
        "alter",
        "ADD",
        "add",
        "INDEX",
        "index",
        "VIEW",
        "view",
        "JOIN",
        "join",
        "LEFT",
        "left",
        "RIGHT",
        "right",
        "INNER",
        "inner",
        "OUTER",
        "outer",
        "ON",
        "on",
        "AS",
        "as",
        "AND",
        "and",
        "OR",
        "or",
        "NOT",
        "not",
        "NULL",
        "null",
        "IS",
        "is",
        "IN",
        "in",
        "LIKE",
        "like",
        "ORDER",
        "order",
        "BY",
        "by",
        "GROUP",
        "group",
        "HAVING",
        "having",
        "LIMIT",
        "limit",
        "OFFSET",
        "offset",
        "DISTINCT",
        "distinct",
        "UNION",
        "union",
        "ALL",
        "all",
        "PRIMARY",
        "primary",
        "KEY",
        "key",
        "FOREIGN",
        "foreign",
        "REFERENCES",
        "references",
        "DEFAULT",
        "default",
        "UNIQUE",
        "unique",
        "CASE",
        "case",
        "WHEN",
        "when",
        "THEN",
        "then",
        "ELSE",
        "else",
        "END",
        "end",
        "BEGIN",
        "begin",
        "COMMIT",
        "commit",
        "ROLLBACK",
        "rollback",
        "WITH",
        "with",
        "RETURNING",
        "returning",
        "IF",
        "if",
        "EXISTS",
        "exists",
        "CONSTRAINT",
        "constraint",
        "CASCADE",
        "cascade",
    ],
    type_keywords: &[
        "INT",
        "int",
        "INTEGER",
        "integer",
        "BIGINT",
        "bigint",
        "SMALLINT",
        "smallint",
        "SERIAL",
        "serial",
        "VARCHAR",
        "varchar",
        "CHAR",
        "char",
        "TEXT",
        "text",
        "BOOLEAN",
        "boolean",
        "BOOL",
        "bool",
        "DATE",
        "date",
        "TIME",
        "time",
        "TIMESTAMP",
        "timestamp",
        "NUMERIC",
        "numeric",
        "DECIMAL",
        "decimal",
        "REAL",
        "real",
        "DOUBLE",
        "double",
        "JSON",
        "json",
        "JSONB",
        "jsonb",
        "UUID",
        "uuid",
        "BYTEA",
        "bytea",
    ],
    punctuation: &['(', ')', ',', ';', '.'],
    operators: &['=', '<', '>', '+', '-', '*', '/', '|'],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('(', ')')],
    indent_line_suffixes: &[],
};

pub const CSS: SyntaxRules = SyntaxRules {
    name: "CSS",
    extensions: &["css", "scss", "sass", "less"],
    // '//' is not CSS proper but is valid in every preprocessor dialect
    // this rule set also claims (scss/sass/less).
    line_comment_prefixes: &["//"],
    block_comment: Some(("/*", "*/")),
    string_quotes: &['"', '\''],
    keywords: &["and", "from", "not", "only", "to"],
    type_keywords: &[],
    punctuation: &['{', '}', '(', ')', ';', ',', ':'],
    operators: &['>', '~', '*', '='],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    // Selectors and at-rules: `.card`, `#main`, `@media`. A leading '.'
    // followed by a digit (`.5em`) has no identifier after it, so it falls
    // through to punctuation instead.
    sigil_words: &[
        ("@", TokenKind::Macro),
        (".", TokenKind::Type),
        ("#", TokenKind::Type),
    ],
    // Declarations (`color: red`) far outnumber pseudo-class selectors
    // (`a:hover`), so the line-start Key rule is worth the occasional
    // misfire on the latter.
    key_separator: Some(':'),
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('{', '}'), ('(', ')')],
    indent_line_suffixes: &[],
};

pub const MARKDOWN: SyntaxRules = SyntaxRules {
    name: "Markdown",
    extensions: &["md", "markdown"],
    line_comment_prefixes: &[],
    block_comment: None,
    string_quotes: &['`'],
    keywords: &[],
    type_keywords: &[],
    punctuation: &[],
    operators: &[],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[("#", TokenKind::Keyword), (">", TokenKind::Comment)],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[('[', ']'), ('(', ')')],
    indent_line_suffixes: &[],
};

pub const XML: SyntaxRules = SyntaxRules {
    // Named for the generic subset rather than HTML: HTML files resolve
    // here via `extensions`, but nothing in the rule set is HTML-specific.
    name: "XML",
    extensions: &["xml", "html", "htm", "xhtml", "svg"],
    line_comment_prefixes: &[],
    block_comment: Some(("<!--", "-->")),
    string_quotes: &['"', '\''],
    keywords: &[],
    type_keywords: &[],
    punctuation: &['<', '>', '/', '='],
    operators: &[],
    filenames: &[],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    // "</" must precede "<": the list is tried in order and both match a
    // closing tag's first byte.
    sigil_words: &[("</", TokenKind::Type), ("<", TokenKind::Type)],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: true,
    brackets: &[('<', '>')],
    indent_line_suffixes: &[],
};

pub const GITIGNORE: SyntaxRules = SyntaxRules {
    name: "gitignore",
    extensions: &[],
    line_comment_prefixes: &["#"],
    block_comment: None,
    string_quotes: &[],
    keywords: &[],
    type_keywords: &[],
    punctuation: &['/'],
    // `!` negates a pattern, `*`/`?` are globs -- the three characters
    // that actually change a pattern's meaning, distinct enough from a
    // literal path segment to earn a color. `[`/`]` (character classes)
    // stay plain text: unlike INI's use of the same characters, they're
    // not a block worth bracket-matching here.
    operators: &['!', '*', '?'],
    filenames: &[".gitignore"],
    filename_prefixes: &[],
    line_prefix_tokens: &[],
    sigil_words: &[],
    key_separator: None,
    capitalized_is_type: false,
    upper_case_is_constant: false,
    macro_bang: false,
    attribute_names: false,
    brackets: &[],
    indent_line_suffixes: &[],
};

/// Every built-in language, in lookup order -- the single list both
/// `syntax_for_path` and `syntax_for_extension` iterate, so adding a
/// language never leaves one of them stale.
const BUILTINS: &[&SyntaxRules] = &[
    &JSON,
    &YAML,
    &SYSTEMD_UNIT,
    &ENV,
    &INI,
    &MAKEFILE,
    &DOCKERFILE,
    &RUST,
    &TOML,
    &SHELL,
    &PYTHON,
    &GO,
    &JAVASCRIPT,
    &C,
    &JAVA,
    &SQL,
    &CSS,
    &MARKDOWN,
    &XML,
    &GITIGNORE,
];

/// Above this size, `tokenize` returns `Vec::new()` immediately without
/// scanning anything -- see `docs/features/syntax-highlighting.md` §4.
pub const MAX_HIGHLIGHTED_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Looks up a built-in `SyntaxRules` by file extension (case-insensitive,
/// no leading `.`) -- `None` if the extension isn't one of the built-in
/// languages'.
pub fn syntax_for_extension(extension: &str) -> Option<&'static SyntaxRules> {
    let lower = extension.to_ascii_lowercase();
    BUILTINS
        .iter()
        .copied()
        .find(|rules| rules.extensions.contains(&lower.as_str()))
}

/// Looks up a built-in `SyntaxRules` for `path`: filename match (exact or
/// prefix) first, then extension match via `syntax_for_extension` -- see
/// `docs/features/syntax-highlighting-env-make-docker.md` §3. The primary
/// entry point for callers holding a path; `None` if nothing matches.
pub fn syntax_for_path(path: &std::path::Path) -> Option<&'static SyntaxRules> {
    if let Some(file_name) = path.file_name().and_then(|name| name.to_str()) {
        let by_name = BUILTINS.iter().copied().find(|rules| {
            rules.filenames.contains(&file_name)
                || rules
                    .filename_prefixes
                    .iter()
                    .any(|prefix| file_name.starts_with(prefix))
        });
        if by_name.is_some() {
            return by_name;
        }
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(syntax_for_extension)
}

/// Tokenizes `text` per `rules` into a flat, non-overlapping,
/// position-ordered `Vec<Token>`. Untokenized regions (plain text between
/// tokens) are simply absent from the result. See
/// `docs/features/syntax-highlighting.md` §3 for the exact per-position
/// rule-matching order this implements: Key -> line prefix -> line comment
/// -> block comment -> string -> sigil word -> number -> word (classified
/// by `classify_word`) -> operator -> punctuation -> skip one char.
pub fn tokenize(text: &str, rules: &SyntaxRules) -> Vec<Token> {
    if text.len() > MAX_HIGHLIGHTED_FILE_BYTES {
        return Vec::new();
    }
    tokenize_span(text, rules, 0, text.len(), true, false).0
}

/// What the tokenizer carries across a position boundary. Strings stop at a
/// newline (`try_string`) and line comments end at one by definition, so an
/// unterminated block comment is the only construct that can be "open" at a
/// line start -- which is what makes one two-variant enum enough state for
/// incremental retokenization (`docs/features/editor-engine.md` §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineState {
    #[default]
    Normal,
    InBlockComment,
}

/// Tokenizes `text[range]`, entering in `state`, returning the tokens (with
/// offsets absolute in `text`) and the state at `range.end`.
///
/// A token is never clipped: one that starts inside `range` and runs past
/// its end is emitted whole, which is what keeps a multi-line block comment
/// a single token no matter how the range is sliced. Unlike `tokenize`,
/// this has no size threshold -- it works span-by-span and has nothing to
/// bail out of -- so the identity `tokenize(t, r) ==
/// tokenize_range(t, r, 0..t.len(), LineState::Normal).0` holds only for
/// texts at or under `MAX_HIGHLIGHTED_FILE_BYTES`, above which `tokenize`
/// deliberately returns nothing.
pub fn tokenize_range(
    text: &str,
    rules: &SyntaxRules,
    range: Range<usize>,
    state: LineState,
) -> (Vec<Token>, LineState) {
    let start = clamp_to_boundary(text, range.start.min(range.end));
    let end = clamp_to_boundary(text, range.start.max(range.end));
    let at_line_start = start == 0 || text[..start].ends_with('\n');
    tokenize_span(
        text,
        rules,
        start,
        end,
        at_line_start,
        state == LineState::InBlockComment,
    )
}

fn clamp_to_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Position just past the block comment that is already open at `from`, or
/// `text.len()` when it never closes. Mirrors `try_block_comment`'s tail
/// for the case where the opening delimiter lies before the span.
fn open_block_comment_end(text: &str, from: usize, rules: &SyntaxRules) -> usize {
    let Some((_, end_delim)) = rules.block_comment else {
        return from;
    };
    match text[from..].find(end_delim) {
        Some(rel) => from + rel + end_delim.len(),
        None => text.len(),
    }
}

fn tokenize_span(
    text: &str,
    rules: &SyntaxRules,
    start: usize,
    end: usize,
    at_line_start: bool,
    in_block_comment: bool,
) -> (Vec<Token>, LineState) {
    let mut tokens = Vec::new();
    let mut pos = start;
    let mut at_line_start = at_line_start;
    let mut open_at_end = false;

    if in_block_comment && pos < end {
        let close = open_block_comment_end(text, pos, rules);
        if close > pos {
            tokens.push(Token {
                range: pos..close,
                kind: TokenKind::Comment,
            });
            at_line_start = false;
            if close > end {
                open_at_end = true;
            }
            pos = close;
        }
    }

    while pos < end {
        let ch = text[pos..]
            .chars()
            .next()
            .expect("pos is always advanced to a valid char boundary");
        let ch_len = ch.len_utf8();

        if at_line_start && ch != '\n' && ch.is_whitespace() {
            pos += ch_len;
            continue;
        }

        if at_line_start {
            at_line_start = false;
            if let Some(sep) = rules.key_separator {
                if let Some((key_end, sep_pos)) = try_key(text, pos, sep, rules) {
                    tokens.push(Token {
                        range: pos..key_end,
                        kind: TokenKind::Key,
                    });
                    pos = sep_pos;
                    continue;
                }
            }
            if let Some((end, kind)) = try_line_prefix(text, pos, rules) {
                tokens.push(Token {
                    range: pos..end,
                    kind,
                });
                pos = end;
                continue;
            }
        }

        if let Some(end) = try_line_comment(text, pos, rules) {
            tokens.push(Token {
                range: pos..end,
                kind: TokenKind::Comment,
            });
            pos = end;
            continue;
        }
        if let Some(comment_end) = try_block_comment(text, pos, rules) {
            tokens.push(Token {
                range: pos..comment_end,
                kind: TokenKind::Comment,
            });
            if comment_end > end {
                open_at_end = true;
            }
            pos = comment_end;
            continue;
        }
        if let Some(end) = try_string(text, pos, rules) {
            tokens.push(Token {
                range: pos..end,
                kind: TokenKind::String,
            });
            pos = end;
            continue;
        }
        if let Some((end, kind)) = try_sigil_word(text, pos, rules) {
            tokens.push(Token {
                range: pos..end,
                kind,
            });
            pos = end;
            continue;
        }
        if let Some(end) = try_number(text, pos) {
            tokens.push(Token {
                range: pos..end,
                kind: TokenKind::Number,
            });
            pos = end;
            continue;
        }
        if let Some(word_end) = try_word(text, pos) {
            match classify_word(text, pos, word_end, rules) {
                Some((kind, token_end)) => {
                    tokens.push(Token {
                        range: pos..token_end,
                        kind,
                    });
                    pos = token_end;
                }
                None => pos = word_end,
            }
            continue;
        }
        if rules.operators.contains(&ch) {
            tokens.push(Token {
                range: pos..pos + ch_len,
                kind: TokenKind::Operator,
            });
            pos += ch_len;
            continue;
        }
        if rules.punctuation.contains(&ch) {
            tokens.push(Token {
                range: pos..pos + ch_len,
                kind: TokenKind::Punctuation,
            });
            pos += ch_len;
            continue;
        }

        if ch == '\n' {
            at_line_start = true;
        }
        pos += ch_len;
    }

    let state = if open_at_end {
        LineState::InBlockComment
    } else {
        LineState::Normal
    };
    (tokens, state)
}

/// Decides what an identifier spanning `start..end` should be colored as,
/// and how far the resulting token reaches -- normally `end`, but one byte
/// further for a `macro_bang` match, which swallows the `!`. `None` means
/// "leave it plain"; the caller still advances past `end` either way, so
/// this never costs a rescan.
fn classify_word(
    text: &str,
    start: usize,
    end: usize,
    rules: &SyntaxRules,
) -> Option<(TokenKind, usize)> {
    let word = &text[start..end];
    if rules.keywords.contains(&word) {
        return Some((TokenKind::Keyword, end));
    }
    if rules.type_keywords.contains(&word) {
        return Some((TokenKind::Type, end));
    }

    let next = text[end..].chars().next();
    if rules.macro_bang
        && next == Some('!')
        && matches!(text[end + 1..].chars().next(), Some('(' | '[' | '{'))
    {
        return Some((TokenKind::Macro, end + 1));
    }
    if next == Some('(') {
        return Some((TokenKind::Function, end));
    }
    if rules.attribute_names && next == Some('=') {
        return Some((TokenKind::Key, end));
    }

    // Constant before type: SCREAMING_SNAKE_CASE also starts with an
    // uppercase letter, and "constant" is the more specific reading.
    if rules.upper_case_is_constant && is_screaming_case(word) {
        return Some((TokenKind::Constant, end));
    }
    if rules.capitalized_is_type && word.starts_with(char::is_uppercase) {
        return Some((TokenKind::Type, end));
    }
    None
}

/// A single uppercase letter is deliberately excluded: `T`/`K`/`V` are
/// generic type parameters far more often than one-letter constants, and
/// `capitalized_is_type` gives them the better reading.
fn is_screaming_case(word: &str) -> bool {
    word.len() > 1
        && word.chars().any(|c| c.is_ascii_uppercase())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Returns `(trimmed_key_end, separator_pos)` on success. Bounded to a
/// single forward scan of the current line (stops at the first newline at
/// the latest), so this runs at most once per line -- `tokenize`'s main
/// loop never re-enters it at a position this scan already passed over.
fn try_key(text: &str, start: usize, sep: char, rules: &SyntaxRules) -> Option<(usize, usize)> {
    let mut pos = start;
    let mut trimmed_end = start;
    let mut consumed_any = false;

    loop {
        if pos >= text.len() {
            return None;
        }
        let ch = text[pos..].chars().next().unwrap();
        if ch == sep {
            return if consumed_any {
                Some((trimmed_end, pos))
            } else {
                None
            };
        }
        // '{'/'}' abort the scan: a line that opens a block before its
        // separator is structure, not a `key: value` pair -- without this,
        // a CSS rule head (`.card { color: red; }`) and a YAML flow mapping
        // (`{a: 1}`) both hand the whole span to the Key rule.
        if ch == '\n' || ch == '{' || ch == '}' || rules.string_quotes.contains(&ch) {
            return None;
        }
        if rules
            .line_comment_prefixes
            .iter()
            .any(|prefix| text[pos..].starts_with(prefix))
        {
            return None;
        }
        consumed_any = true;
        pos += ch.len_utf8();
        if !ch.is_whitespace() {
            trimmed_end = pos;
        }
    }
}

/// Returns `(end, kind)` for the first `line_prefix_tokens` entry matching
/// at `pos`. Same forward-only shape as `try_line_comment`; the kind comes
/// from the rule rather than being hardcoded to `Comment`.
fn try_line_prefix(text: &str, pos: usize, rules: &SyntaxRules) -> Option<(usize, TokenKind)> {
    rules
        .line_prefix_tokens
        .iter()
        .find(|(prefix, _)| text[pos..].starts_with(prefix))
        .map(|&(_, kind)| {
            let end = match text[pos..].find('\n') {
                Some(rel) => pos + rel,
                None => text.len(),
            };
            (end, kind)
        })
}

/// Returns `(end, kind)` for the first `sigil_words` entry whose prefix
/// matches at `pos` *and* is followed by an identifier -- the token covers
/// prefix and identifier together. Forward-only and bounded by the
/// identifier's own length, so it preserves `tokenize`'s single-pass
/// invariant.
fn try_sigil_word(text: &str, pos: usize, rules: &SyntaxRules) -> Option<(usize, TokenKind)> {
    rules.sigil_words.iter().find_map(|&(prefix, kind)| {
        if !text[pos..].starts_with(prefix) {
            return None;
        }
        try_word(text, pos + prefix.len()).map(|end| (end, kind))
    })
}

fn try_line_comment(text: &str, pos: usize, rules: &SyntaxRules) -> Option<usize> {
    rules
        .line_comment_prefixes
        .iter()
        .find(|prefix| text[pos..].starts_with(**prefix))
        .map(|_| match text[pos..].find('\n') {
            Some(rel) => pos + rel,
            None => text.len(),
        })
}

fn try_block_comment(text: &str, pos: usize, rules: &SyntaxRules) -> Option<usize> {
    let (start_delim, end_delim) = rules.block_comment?;
    if !text[pos..].starts_with(start_delim) {
        return None;
    }
    let search_from = pos + start_delim.len();
    Some(match text[search_from..].find(end_delim) {
        Some(rel) => search_from + rel + end_delim.len(),
        None => text.len(),
    })
}

fn try_string(text: &str, pos: usize, rules: &SyntaxRules) -> Option<usize> {
    let quote = text[pos..].chars().next()?;
    if !rules.string_quotes.contains(&quote) {
        return None;
    }

    let mut cursor = pos + quote.len_utf8();
    loop {
        if cursor >= text.len() {
            return Some(text.len());
        }
        let ch = text[cursor..].chars().next().unwrap();
        if ch == '\\' {
            cursor += ch.len_utf8();
            match text[cursor..].chars().next() {
                Some(escaped) => cursor += escaped.len_utf8(),
                None => return Some(text.len()),
            }
            continue;
        }
        if ch == '\n' {
            return Some(cursor);
        }
        cursor += ch.len_utf8();
        if ch == quote {
            return Some(cursor);
        }
    }
}

fn try_number(text: &str, pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = pos;

    if bytes[cursor] == b'-' || bytes[cursor] == b'+' {
        if cursor + 1 >= text.len() || !bytes[cursor + 1].is_ascii_digit() {
            return None;
        }
        cursor += 1;
    } else if !bytes[cursor].is_ascii_digit() {
        return None;
    }

    while cursor < text.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }

    let mut seen_dot = false;
    let mut seen_exp = false;
    loop {
        if !seen_dot
            && cursor < text.len()
            && bytes[cursor] == b'.'
            && cursor + 1 < text.len()
            && bytes[cursor + 1].is_ascii_digit()
        {
            seen_dot = true;
            cursor += 1;
            while cursor < text.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            continue;
        }
        if !seen_exp && cursor < text.len() && (bytes[cursor] == b'e' || bytes[cursor] == b'E') {
            let mut peek = cursor + 1;
            if peek < text.len() && (bytes[peek] == b'-' || bytes[peek] == b'+') {
                peek += 1;
            }
            if peek < text.len() && bytes[peek].is_ascii_digit() {
                seen_exp = true;
                cursor = peek;
                while cursor < text.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                continue;
            }
        }
        break;
    }

    Some(cursor)
}

fn try_word(text: &str, pos: usize) -> Option<usize> {
    let first = text[pos..].chars().next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }
    let mut cursor = pos + first.len_utf8();
    while let Some(ch) = text[cursor..].chars().next() {
        if ch.is_alphanumeric() || ch == '_' {
            cursor += ch.len_utf8();
        } else {
            break;
        }
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tok(range: Range<usize>, kind: TokenKind) -> Token {
        Token { range, kind }
    }

    #[test]
    fn json_worked_example_matches_doc() {
        let tokens = tokenize(r#"{"ok": true, "n": 42}"#, &JSON);
        assert_eq!(
            tokens,
            vec![
                tok(0..1, TokenKind::Punctuation),
                tok(1..5, TokenKind::String),
                tok(5..6, TokenKind::Punctuation),
                tok(7..11, TokenKind::Keyword),
                tok(11..12, TokenKind::Punctuation),
                tok(13..16, TokenKind::String),
                tok(16..17, TokenKind::Punctuation),
                tok(18..20, TokenKind::Number),
                tok(20..21, TokenKind::Punctuation),
            ]
        );
    }

    #[test]
    fn yaml_worked_example_matches_doc() {
        let text = "key: value\n- item\n# note\n";
        let tokens = tokenize(text, &YAML);
        assert_eq!(
            tokens,
            vec![
                tok(0..3, TokenKind::Key),
                tok(3..4, TokenKind::Punctuation),
                tok(18..24, TokenKind::Comment),
            ]
        );
        assert_eq!(&text[18..24], "# note");
    }

    #[test]
    fn yaml_list_item_is_not_misread_as_a_key_line() {
        // The Key rule's forward scan hits the line's '\n' before finding
        // ':' anywhere on "- item", so it must fail and fall through --
        // this is the doc's central worked-example claim for YAML.
        let tokens = tokenize("- item\n", &YAML);
        assert!(!tokens.iter().any(|t| t.kind == TokenKind::Key));
    }

    #[test]
    fn systemd_worked_example_matches_doc() {
        let text = "[Unit]\nExecStart=/usr/bin/foo # launch\n";
        let tokens = tokenize(text, &SYSTEMD_UNIT);
        assert_eq!(
            tokens,
            vec![
                tok(0..1, TokenKind::Punctuation),
                tok(5..6, TokenKind::Punctuation),
                tok(7..16, TokenKind::Key),
                tok(16..17, TokenKind::Punctuation),
                tok(30..38, TokenKind::Comment),
            ]
        );
        assert_eq!(&text[7..16], "ExecStart");
        assert_eq!(&text[30..38], "# launch");
    }

    #[test]
    fn unrecognized_extension_returns_none() {
        assert!(syntax_for_extension("bin").is_none());
        assert!(syntax_for_extension("png").is_none());
    }

    #[test]
    fn syntax_for_extension_is_case_insensitive() {
        assert_eq!(syntax_for_extension("JSON").unwrap().name, "JSON");
        assert_eq!(syntax_for_extension("Yml").unwrap().name, "YAML");
        assert_eq!(
            syntax_for_extension("SERVICE").unwrap().name,
            "systemd unit file"
        );
    }

    #[test]
    fn byte_cap_returns_empty_without_scanning() {
        // A shape that a full scan would classify very differently
        // (either a single giant String token if scanned, or nothing) --
        // asserting an empty Vec either way only proves the *output*
        // looks capped. To prove it's a genuine early exit rather than
        // collect-then-truncate, assert the exact boundary: one byte over
        // the cap is empty, exactly at the cap is not.
        let over = "\"".to_string() + &"a".repeat(MAX_HIGHLIGHTED_FILE_BYTES);
        assert!(over.len() > MAX_HIGHLIGHTED_FILE_BYTES);
        assert_eq!(tokenize(&over, &JSON), Vec::new());

        let at_cap = "a".repeat(MAX_HIGHLIGHTED_FILE_BYTES);
        assert_eq!(at_cap.len(), MAX_HIGHLIGHTED_FILE_BYTES);
        // Not capped: a lone run of "a" chars with no keyword match
        // produces no tokens anyway, but the call must not panic/bail --
        // this is what actually distinguishes "capped" from "scanned and
        // happened to find nothing" for this input shape, so pair it
        // with a shape that WOULD produce a token at exactly the cap.
        let at_cap_number = "9".repeat(MAX_HIGHLIGHTED_FILE_BYTES);
        let tokens = tokenize(&at_cap_number, &JSON);
        assert_eq!(
            tokens,
            vec![tok(0..MAX_HIGHLIGHTED_FILE_BYTES, TokenKind::Number)]
        );

        let over_cap_number = "9".repeat(MAX_HIGHLIGHTED_FILE_BYTES + 1);
        assert_eq!(tokenize(&over_cap_number, &JSON), Vec::new());
    }

    #[test]
    fn output_is_sorted_and_non_overlapping() {
        let text = r#"{"a": 1, "b": [true, false, null], "c": "x\"y"}"#;
        let tokens = tokenize(text, &JSON);
        for pair in tokens.windows(2) {
            assert!(pair[0].range.end <= pair[1].range.start);
        }
    }

    #[test]
    fn token_ranges_respect_utf8_char_boundaries() {
        // "é" is 2 bytes; make sure it doesn't get split when it sits
        // right next to punctuation/string tokens.
        let text = "\"héllo\": 1";
        let tokens = tokenize(text, &YAML);
        for t in &tokens {
            assert!(text.is_char_boundary(t.range.start));
            assert!(text.is_char_boundary(t.range.end));
        }
        // The string token should include the full multi-byte content.
        let string_tok = tokens.iter().find(|t| t.kind == TokenKind::String).unwrap();
        assert_eq!(&text[string_tok.range.clone()], "\"héllo\"");
    }

    #[test]
    fn yaml_flow_mapping_at_line_start_is_not_misread_as_a_key() {
        // Was a documented v1 limitation ('{' wasn't a stop condition, so
        // "{a" was handed to the Key rule); the brace stop condition added
        // for CSS fixes it here too.
        let tokens = tokenize("{a: 1}\n", &YAML);
        assert!(!tokens.iter().any(|t| t.kind == TokenKind::Key));
        assert_eq!(tokens[0], tok(0..1, TokenKind::Punctuation));
    }

    #[test]
    fn escaped_quote_inside_string_does_not_end_it() {
        let text = r#""a\"b""#;
        let tokens = tokenize(text, &JSON);
        assert_eq!(tokens, vec![tok(0..6, TokenKind::String)]);
    }

    #[test]
    fn unterminated_string_ends_at_newline() {
        let text = "\"abc\ndef";
        let tokens = tokenize(text, &JSON);
        assert_eq!(tokens, vec![tok(0..4, TokenKind::String)]);
    }

    #[test]
    fn unterminated_string_ends_at_end_of_text() {
        let text = "\"abc";
        let tokens = tokenize(text, &JSON);
        assert_eq!(tokens, vec![tok(0..4, TokenKind::String)]);
    }

    #[test]
    fn unterminated_block_comment_ends_at_end_of_text() {
        let rules = SyntaxRules {
            block_comment: Some(("/*", "*/")),
            ..SYSTEMD_UNIT
        };
        let text = "/* never closes";
        let tokens = tokenize(text, &rules);
        assert_eq!(tokens, vec![tok(0..text.len(), TokenKind::Comment)]);
    }

    #[test]
    fn number_edge_cases() {
        assert_eq!(tokenize("-1", &JSON), vec![tok(0..2, TokenKind::Number)]);
        assert_eq!(tokenize("3.14", &JSON), vec![tok(0..4, TokenKind::Number)]);
        assert_eq!(tokenize("1e10", &JSON), vec![tok(0..4, TokenKind::Number)]);
        assert_eq!(
            tokenize("1.5e-3", &JSON),
            vec![tok(0..6, TokenKind::Number)]
        );
        // A lone '-' with no following digit is not a number -- and JSON
        // doesn't list '-' in punctuation, so it's an untokenized gap.
        assert_eq!(tokenize("-", &JSON), Vec::new());
        // A trailing '.' with no digit after it is not part of the number.
        assert_eq!(tokenize("1.", &JSON), vec![tok(0..1, TokenKind::Number)]);
    }

    #[test]
    fn empty_query_and_empty_text_produce_no_tokens() {
        assert_eq!(tokenize("", &JSON), Vec::new());
    }

    #[test]
    fn non_keyword_word_is_not_tokenized() {
        assert_eq!(tokenize("hello", &JSON), Vec::new());
    }

    #[test]
    fn env_worked_example_matches_doc() {
        let text = "FOO=bar\n# comment\n";
        let rules = syntax_for_path(Path::new(".env")).unwrap();
        let tokens = tokenize(text, rules);
        assert_eq!(
            tokens,
            vec![
                tok(0..3, TokenKind::Key),
                tok(3..4, TokenKind::Punctuation),
                tok(8..17, TokenKind::Comment),
            ]
        );
        assert_eq!(&text[0..3], "FOO");
        assert_eq!(&text[8..17], "# comment");
    }

    #[test]
    fn env_quoted_value_is_a_string_token() {
        let tokens = tokenize("TOKEN=\"abc\"\n", &ENV);
        assert_eq!(
            tokens,
            vec![
                tok(0..5, TokenKind::Key),
                tok(5..6, TokenKind::Punctuation),
                tok(6..11, TokenKind::String),
            ]
        );
    }

    #[test]
    fn makefile_worked_example_matches_doc() {
        let text = "build: main.o\n\tgcc -o build main.o\n";
        let rules = syntax_for_path(Path::new("Makefile")).unwrap();
        let tokens = tokenize(text, rules);
        assert_eq!(
            tokens,
            vec![tok(0..5, TokenKind::Key), tok(5..6, TokenKind::Punctuation),]
        );
        assert_eq!(&text[0..5], "build");
    }

    #[test]
    fn makefile_directive_is_a_keyword() {
        let text = "ifeq ($(OS),Linux)\nendif\n";
        let tokens = tokenize(text, &MAKEFILE);
        assert_eq!(tokens[0], tok(0..4, TokenKind::Keyword));
        assert!(tokens.contains(&tok(19..24, TokenKind::Keyword)));
        assert_eq!(&text[19..24], "endif");
    }

    #[test]
    fn makefile_recipe_line_colon_misfires_as_documented_limitation() {
        // Documented v1 limitation: `tokenize` carries no cross-line
        // state, so a tab-indented recipe line containing a ':' is
        // indistinguishable from a target line and picks up a spurious
        // Key token. Asserted here as intended behavior, not a bug.
        let text = "build: all\n\techo note: done\n";
        let tokens = tokenize(text, &MAKEFILE);
        assert!(tokens.contains(&tok(12..21, TokenKind::Key)));
        assert_eq!(&text[12..21], "echo note");
    }

    #[test]
    fn makefile_assignment_styles_are_asymmetric_as_documented() {
        // ':=' assignments get a Key (the ':' is the key_separator);
        // a bare '=' assignment does not. Documented v1 limitation.
        assert_eq!(
            tokenize("VAR := value\n", &MAKEFILE)[0],
            tok(0..3, TokenKind::Key)
        );
        assert!(!tokenize("VAR = value\n", &MAKEFILE)
            .iter()
            .any(|t| t.kind == TokenKind::Key));
    }

    #[test]
    fn dockerfile_worked_example_matches_doc() {
        let text = "FROM ubuntu:22.04\nRUN echo hi\n";
        let rules = syntax_for_path(Path::new("Dockerfile")).unwrap();
        let tokens = tokenize(text, rules);
        assert_eq!(
            tokens,
            vec![
                tok(0..4, TokenKind::Keyword),
                tok(12..17, TokenKind::Number),
                tok(18..21, TokenKind::Keyword),
            ]
        );
        assert_eq!(&text[12..17], "22.04");
        assert_eq!(&text[18..21], "RUN");
    }

    #[test]
    fn dockerfile_lowercase_instruction_is_not_a_keyword_as_documented() {
        // `tokenize`'s keyword rule is case-sensitive, so lowercase
        // instructions (valid Docker syntax) don't highlight. Documented
        // v1 limitation, consistent with the base tokenizer's design.
        assert!(!tokenize("from ubuntu\n", &DOCKERFILE)
            .iter()
            .any(|t| t.kind == TokenKind::Keyword));
    }

    #[test]
    fn gitignore_worked_example_tokenizes_comments_and_glob_operators() {
        let text = "# comment\n!keep.log\n*.tmp\nbuild/\n";
        let rules = syntax_for_path(Path::new(".gitignore")).unwrap();
        let tokens = tokenize(text, rules);
        assert_eq!(
            tokens,
            vec![
                tok(0..9, TokenKind::Comment),
                tok(10..11, TokenKind::Operator),
                tok(20..21, TokenKind::Operator),
                tok(31..32, TokenKind::Punctuation),
            ]
        );
        assert_eq!(&text[0..9], "# comment");
        assert_eq!(&text[10..11], "!");
        assert_eq!(&text[20..21], "*");
        assert_eq!(&text[31..32], "/");
    }

    #[test]
    fn gitignore_matches_by_bare_filename_regardless_of_directory() {
        assert_eq!(
            syntax_for_path(Path::new("nested/dir/.gitignore"))
                .unwrap()
                .name,
            "gitignore"
        );
    }

    #[test]
    fn syntax_for_path_matches_exact_filenames() {
        for (name, expected) in [
            (".env", "env file"),
            ("Makefile", "Makefile"),
            ("makefile", "Makefile"),
            ("GNUmakefile", "Makefile"),
            ("Dockerfile", "Dockerfile"),
            ("dockerfile", "Dockerfile"),
            (".gitignore", "gitignore"),
        ] {
            let rules = syntax_for_path(Path::new(name))
                .unwrap_or_else(|| panic!("{name} should match a builtin"));
            assert_eq!(rules.name, expected, "for {name}");
        }
    }

    #[test]
    fn syntax_for_path_matches_filename_prefixes() {
        assert_eq!(
            syntax_for_path(Path::new(".env.local")).unwrap().name,
            "env file"
        );
        assert_eq!(
            syntax_for_path(Path::new("/srv/app/.env.production"))
                .unwrap()
                .name,
            "env file"
        );
        assert_eq!(
            syntax_for_path(Path::new("Dockerfile.dev")).unwrap().name,
            "Dockerfile"
        );
        assert_eq!(
            syntax_for_path(Path::new("dockerfile.prod")).unwrap().name,
            "Dockerfile"
        );
    }

    #[test]
    fn bare_dockerfile_is_an_exact_match_not_a_prefix_match() {
        // "Dockerfile" has no trailing '.', so it cannot satisfy the
        // "Dockerfile." prefix -- the two checks can't disagree here even
        // though they live in the same pass.
        assert!(DOCKERFILE
            .filename_prefixes
            .iter()
            .all(|prefix| !"Dockerfile".starts_with(prefix)));
        assert!(DOCKERFILE.filenames.contains(&"Dockerfile"));
    }

    #[test]
    fn syntax_for_path_falls_back_to_extension() {
        assert_eq!(
            syntax_for_path(Path::new("app.env")).unwrap().name,
            "env file"
        );
        assert_eq!(
            syntax_for_path(Path::new("rules.mk")).unwrap().name,
            "Makefile"
        );
        assert_eq!(
            syntax_for_path(Path::new("build.dockerfile")).unwrap().name,
            "Dockerfile"
        );
        assert_eq!(
            syntax_for_path(Path::new("/etc/app/config.json"))
                .unwrap()
                .name,
            "JSON"
        );
        assert_eq!(
            syntax_for_path(Path::new("deploy.YML")).unwrap().name,
            "YAML"
        );
        assert_eq!(
            syntax_for_path(Path::new("/etc/systemd/system/foo.service"))
                .unwrap()
                .name,
            "systemd unit file"
        );
    }

    #[test]
    fn syntax_for_path_returns_none_when_nothing_matches() {
        assert!(syntax_for_path(Path::new("logo.png")).is_none());
        assert!(syntax_for_path(Path::new("target/debug/ide")).is_none());
        // No file name at all (`file_name()` is None) must not panic --
        // it just skips the filename dimension.
        assert!(syntax_for_path(Path::new("..")).is_none());
        assert!(syntax_for_path(Path::new("/")).is_none());
    }

    #[test]
    fn existing_languages_gained_no_filename_matching() {
        for rules in [&JSON, &YAML, &SYSTEMD_UNIT] {
            assert!(rules.filenames.is_empty(), "{}", rules.name);
            assert!(rules.filename_prefixes.is_empty(), "{}", rules.name);
        }
        // ...and their extension lookup is unchanged.
        assert_eq!(syntax_for_extension("json").unwrap().name, "JSON");
        assert_eq!(syntax_for_extension("yaml").unwrap().name, "YAML");
        assert_eq!(
            syntax_for_extension("timer").unwrap().name,
            "systemd unit file"
        );
    }

    #[test]
    fn builtin_extension_sets_are_disjoint() {
        let mut seen: Vec<&str> = Vec::new();
        for rules in BUILTINS {
            for ext in rules.extensions {
                assert!(!seen.contains(ext), "duplicate extension {ext}");
                seen.push(ext);
            }
        }
    }

    #[test]
    fn rust_worked_example_matches_doc() {
        let text = "fn main() { let x = 42; } // done";
        let rules = syntax_for_path(Path::new("src/main.rs")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..2, TokenKind::Keyword),
                // `main` is followed by '(' -- Function, not plain text.
                tok(3..7, TokenKind::Function),
                tok(7..8, TokenKind::Punctuation),
                tok(8..9, TokenKind::Punctuation),
                tok(10..11, TokenKind::Punctuation),
                tok(12..15, TokenKind::Keyword),
                // '=' moved from `punctuation` to `operators`.
                tok(18..19, TokenKind::Operator),
                tok(20..22, TokenKind::Number),
                tok(22..23, TokenKind::Punctuation),
                tok(24..25, TokenKind::Punctuation),
                tok(26..33, TokenKind::Comment),
            ]
        );
        assert_eq!(&text[3..7], "main");
        assert_eq!(&text[26..33], "// done");
    }

    #[test]
    fn rust_identifier_shapes_get_distinct_kinds() {
        let text = "let n: u32 = MAX_LEN; HashMap::new(); println!(\"x\");";
        let tokens = tokenize(text, &RUST);
        let kinds: Vec<(&str, TokenKind)> = tokens
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    TokenKind::Type
                        | TokenKind::Constant
                        | TokenKind::Function
                        | TokenKind::Macro
                        | TokenKind::Keyword
                )
            })
            .map(|t| (&text[t.range.clone()], t.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("let", TokenKind::Keyword),
                ("u32", TokenKind::Type),
                ("MAX_LEN", TokenKind::Constant),
                ("HashMap", TokenKind::Type),
                ("new", TokenKind::Function),
                ("println!", TokenKind::Macro),
            ]
        );
    }

    #[test]
    fn rust_not_equals_is_not_a_macro_invocation() {
        // `macro_bang` requires a delimiter right after the '!' -- without
        // that check, every `a != b` would read as an invocation of `a`.
        let tokens = tokenize("a != b", &RUST);
        assert!(!tokens.iter().any(|t| t.kind == TokenKind::Macro));
    }

    #[test]
    fn rust_attribute_line_is_a_macro_token() {
        let text = "#[derive(Debug)]\nstruct S;\n";
        let tokens = tokenize(text, &RUST);
        assert_eq!(tokens[0], tok(0..16, TokenKind::Macro));
        assert_eq!(&text[0..16], "#[derive(Debug)]");
    }

    #[test]
    fn single_uppercase_letter_is_a_type_not_a_constant() {
        // `T`/`K`/`V` are generic parameters far more often than
        // one-letter constants -- see `is_screaming_case`.
        assert_eq!(tokenize("T", &RUST), vec![tok(0..1, TokenKind::Type)]);
        assert_eq!(tokenize("TT", &RUST), vec![tok(0..2, TokenKind::Constant)]);
    }

    #[test]
    fn toml_worked_example_matches_doc() {
        let text = "[package]\nname = \"ide\"\n";
        let rules = syntax_for_path(Path::new("Cargo.toml")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..1, TokenKind::Punctuation),
                tok(8..9, TokenKind::Punctuation),
                tok(10..14, TokenKind::Key),
                tok(15..16, TokenKind::Punctuation),
                tok(17..22, TokenKind::String),
            ]
        );
        // The "[package]" line has no '=', so the Key rule's forward scan
        // hits '\n' and correctly fails -- same as SYSTEMD_UNIT's "[Unit]".
        assert_eq!(&text[10..14], "name");
    }

    #[test]
    fn markdown_worked_example_matches_doc() {
        let text = "# Title\n\nSome `code` here\n";
        let rules = syntax_for_path(Path::new("README.md")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..7, TokenKind::Keyword),
                tok(14..20, TokenKind::String),
            ]
        );
        assert_eq!(&text[0..7], "# Title");
        assert_eq!(&text[14..20], "`code`");
    }

    #[test]
    fn go_worked_example_matches_doc() {
        let text = "package main /* x */\nvar s = `raw`\n";
        let rules = syntax_for_path(Path::new("main.go")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..7, TokenKind::Keyword),
                tok(13..20, TokenKind::Comment),
                tok(21..24, TokenKind::Keyword),
                tok(27..28, TokenKind::Operator),
                tok(29..34, TokenKind::String),
            ]
        );
        assert_eq!(&text[13..20], "/* x */");
        assert_eq!(&text[29..34], "`raw`");
    }

    #[test]
    fn python_worked_example_matches_doc() {
        let text = "def main():\n    return None\n";
        let rules = syntax_for_path(Path::new("app.py")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..3, TokenKind::Keyword),
                tok(4..8, TokenKind::Function),
                tok(8..9, TokenKind::Punctuation),
                tok(9..10, TokenKind::Punctuation),
                tok(10..11, TokenKind::Punctuation),
                tok(16..22, TokenKind::Keyword),
                tok(23..27, TokenKind::Keyword),
            ]
        );
        assert_eq!(&text[4..8], "main");
        assert_eq!(&text[16..22], "return");
    }

    #[test]
    fn shell_worked_example_matches_doc() {
        let text = "# rc\nexport PATH=\"/bin\"\n";
        let rules = syntax_for_path(Path::new("/home/u/.bashrc")).unwrap();
        assert_eq!(rules.name, "Shell");
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..4, TokenKind::Comment),
                tok(5..11, TokenKind::Keyword),
                tok(12..16, TokenKind::Constant),
                tok(16..17, TokenKind::Operator),
                tok(17..23, TokenKind::String),
            ]
        );
        assert_eq!(&text[12..16], "PATH");
        assert_eq!(&text[17..23], "\"/bin\"");
    }

    #[test]
    fn shell_dollar_variable_is_a_constant_but_a_subshell_is_not() {
        let text = "echo $HOME $(date)\n";
        let tokens = tokenize(text, &SHELL);
        let constants: Vec<&str> = tokens
            .iter()
            .filter(|t| t.kind == TokenKind::Constant)
            .map(|t| &text[t.range.clone()])
            .collect();
        // `$(` has no identifier after the sigil, so the rule declines it.
        assert_eq!(constants, vec!["$HOME"]);
    }

    #[test]
    fn xml_worked_example_matches_doc() {
        let text = "<!-- c -->\n<a href=\"x\">t</a>\n";
        let rules = syntax_for_path(Path::new("index.html")).unwrap();
        assert_eq!(
            tokenize(text, rules),
            vec![
                tok(0..10, TokenKind::Comment),
                tok(11..13, TokenKind::Type),
                tok(14..18, TokenKind::Key),
                tok(18..19, TokenKind::Punctuation),
                tok(19..22, TokenKind::String),
                tok(22..23, TokenKind::Punctuation),
                tok(24..27, TokenKind::Type),
                tok(27..28, TokenKind::Punctuation),
            ]
        );
        assert_eq!(&text[0..10], "<!-- c -->");
        assert_eq!(&text[11..13], "<a");
        assert_eq!(&text[14..18], "href");
        assert_eq!(&text[24..27], "</a");
        assert_eq!(&text[27..28], ">");
    }

    #[test]
    fn rust_lifetime_is_not_swallowed_as_a_string() {
        // The whole reason '\'' is not a Rust string quote: if it were,
        // this would open an unterminated string eating the rest of the
        // line. Documented design choice, asserted as behavior.
        let text = "&'a str\n";
        let tokens = tokenize(text, &RUST);
        assert_eq!(
            tokens,
            vec![
                tok(0..1, TokenKind::Operator),
                // The lifetime is picked up by `sigil_words` instead.
                tok(1..3, TokenKind::Type),
                tok(4..7, TokenKind::Type),
            ]
        );
        assert_eq!(&text[1..3], "'a");
        assert!(!tokens.iter().any(|t| t.kind == TokenKind::String));
    }

    #[test]
    fn rust_nested_block_comment_ends_at_first_terminator() {
        // Documented v1 limitation: Rust permits nesting, the scan doesn't
        // count depth, so the Comment token stops at the first "*/".
        let text = "/* a /* b */ c */";
        let tokens = tokenize(text, &RUST);
        assert_eq!(tokens[0], tok(0..12, TokenKind::Comment));
        assert_eq!(&text[0..12], "/* a /* b */");
        // What follows the premature terminator is scanned as ordinary
        // code -- here the trailing "*/" as two operator characters.
        assert!(tokens[1..].iter().all(|t| t.kind == TokenKind::Operator));
    }

    #[test]
    fn python_triple_quotes_tokenize_as_three_strings_as_documented() {
        // Documented v1 limitation: no multi-character string delimiters,
        // so `"""doc"""` is an empty string, then "doc", then an empty
        // string -- cosmetic, and still sorted/non-overlapping.
        let tokens = tokenize("\"\"\"doc\"\"\"\n", &PYTHON);
        assert_eq!(
            tokens,
            vec![
                tok(0..2, TokenKind::String),
                tok(2..7, TokenKind::String),
                tok(7..9, TokenKind::String),
            ]
        );
        for pair in tokens.windows(2) {
            assert!(pair[0].range.end <= pair[1].range.start);
        }
    }

    #[test]
    fn indented_markdown_heading_still_highlights_as_documented() {
        // `tokenize` skips a line's leading whitespace before reaching the
        // line-start rules, so an indented '#' is treated as a heading even
        // though real Markdown requires column 0.
        assert_eq!(
            tokenize("  # x\n", &MARKDOWN),
            vec![tok(2..5, TokenKind::Keyword)]
        );
    }

    #[test]
    fn xml_tag_names_are_highlighted_via_the_sigil_rule() {
        let tokens = tokenize("<div>\n", &XML);
        assert_eq!(
            tokens,
            vec![
                tok(0..4, TokenKind::Type),
                tok(4..5, TokenKind::Punctuation),
            ]
        );
    }

    #[test]
    fn xml_closing_tag_prefix_wins_over_the_opening_one() {
        // "</" is listed before "<" precisely so a closing tag doesn't
        // tokenize as '<' + a nonexistent identifier starting with '/'.
        let text = "</div>";
        assert_eq!(
            tokens_of(text, &XML),
            vec![("</div", TokenKind::Type), (">", TokenKind::Punctuation)]
        );
    }

    #[test]
    fn line_prefix_rule_only_fires_at_line_start() {
        // A '#' mid-line is not a heading -- Markdown has no other rule
        // claiming it, so it stays an untokenized gap.
        assert_eq!(tokenize("text # not a heading\n", &MARKDOWN), Vec::new());
    }

    #[test]
    fn syntax_for_path_resolves_every_new_language() {
        for (path, expected) in [
            ("src/main.rs", "Rust"),
            ("Cargo.toml", "TOML"),
            ("Cargo.lock", "TOML"),
            ("scripts/deploy.sh", "Shell"),
            ("/home/u/.zshrc", "Shell"),
            ("/home/u/.profile", "Shell"),
            ("app.py", "Python"),
            ("types.pyi", "Python"),
            ("cmd/main.go", "Go"),
            ("README.md", "Markdown"),
            ("notes.markdown", "Markdown"),
            ("index.html", "XML"),
            ("data.xml", "XML"),
            ("logo.svg", "XML"),
        ] {
            let rules =
                syntax_for_path(Path::new(path)).unwrap_or_else(|| panic!("{path} should resolve"));
            assert_eq!(rules.name, expected, "for {path}");
        }
    }

    #[test]
    fn existing_languages_gained_no_line_prefix_rules() {
        for rules in [&JSON, &YAML, &SYSTEMD_UNIT, &ENV, &MAKEFILE, &DOCKERFILE] {
            assert!(rules.line_prefix_tokens.is_empty(), "{}", rules.name);
        }
        // Regression: the pre-existing worked examples still tokenize the
        // same way with the new cascade step in place.
        assert_eq!(
            tokenize("key: value\n", &YAML),
            vec![tok(0..3, TokenKind::Key), tok(3..4, TokenKind::Punctuation),]
        );
    }

    fn tokens_of<'a>(text: &'a str, rules: &SyntaxRules) -> Vec<(&'a str, TokenKind)> {
        tokenize(text, rules)
            .into_iter()
            .map(|t| (&text[t.range], t.kind))
            .collect()
    }

    #[test]
    fn syntax_for_path_resolves_every_language_added_for_richer_highlighting() {
        for (path, expected) in [
            ("app.js", "JavaScript/TypeScript"),
            ("app.tsx", "JavaScript/TypeScript"),
            ("main.c", "C/C++"),
            ("engine.hpp", "C/C++"),
            ("Main.java", "Java"),
            ("schema.sql", "SQL"),
            ("site.css", "CSS"),
            ("theme.scss", "CSS"),
            ("app.ini", "INI"),
            ("nginx.conf", "INI"),
            (".editorconfig", "INI"),
        ] {
            let rules =
                syntax_for_path(Path::new(path)).unwrap_or_else(|| panic!("{path} should resolve"));
            assert_eq!(rules.name, expected, "for {path}");
        }
    }

    #[test]
    fn javascript_call_and_type_shapes() {
        let text = "const x = new Foo(); render(y);";
        assert_eq!(
            tokens_of(text, &JAVASCRIPT)
                .into_iter()
                .filter(|(_, k)| matches!(k, TokenKind::Type | TokenKind::Function))
                .collect::<Vec<_>>(),
            vec![
                ("Foo", TokenKind::Function),
                ("render", TokenKind::Function)
            ]
        );
    }

    #[test]
    fn c_preprocessor_directive_claims_the_line() {
        let text = "#include <stdio.h>\nint main() {}\n";
        let tokens = tokenize(text, &C);
        assert_eq!(tokens[0], tok(0..18, TokenKind::Macro));
        assert_eq!(&text[0..18], "#include <stdio.h>");
        assert_eq!(tokens[1], tok(19..22, TokenKind::Type));
    }

    #[test]
    fn sql_keywords_match_in_either_case() {
        assert_eq!(
            tokens_of("select id from t", &SQL)
                .into_iter()
                .filter(|(_, k)| *k == TokenKind::Keyword)
                .collect::<Vec<_>>(),
            vec![("select", TokenKind::Keyword), ("from", TokenKind::Keyword)]
        );
        assert_eq!(
            tokens_of("SELECT id FROM t", &SQL)
                .into_iter()
                .filter(|(_, k)| *k == TokenKind::Keyword)
                .collect::<Vec<_>>(),
            vec![("SELECT", TokenKind::Keyword), ("FROM", TokenKind::Keyword)]
        );
    }

    #[test]
    fn css_selectors_and_at_rules_use_the_sigil_rule() {
        let text = "@media screen {\n  .card {\n    color: red;\n  }\n}\n";
        let kinds = tokens_of(text, &CSS);
        assert!(kinds.contains(&("@media", TokenKind::Macro)));
        assert!(kinds.contains(&(".card", TokenKind::Type)));
        assert!(kinds.contains(&("color", TokenKind::Key)));
    }

    #[test]
    fn css_decimal_length_is_not_mistaken_for_a_class_selector() {
        // ".5em" has no identifier right after the '.', so the sigil rule
        // declines it rather than swallowing "5em" as a selector.
        assert!(!tokens_of("a { margin: .5em; }", &CSS)
            .iter()
            .any(|(text, kind)| *kind == TokenKind::Type && text.starts_with('.')));
    }

    #[test]
    fn ini_section_and_key_shapes() {
        let text = "[core]\neditor = vim\n; note\n";
        let kinds = tokens_of(text, &INI);
        assert!(kinds.contains(&("editor", TokenKind::Key)));
        assert!(kinds.contains(&("; note", TokenKind::Comment)));
    }

    #[test]
    fn every_builtin_keeps_operators_and_punctuation_disjoint() {
        // `tokenize` checks operators first, so an overlap would silently
        // make the punctuation entry unreachable.
        for rules in BUILTINS {
            for op in rules.operators {
                assert!(
                    !rules.punctuation.contains(op),
                    "{}: '{op}' is in both sets",
                    rules.name
                );
            }
        }
    }

    #[test]
    fn sigil_word_prefixes_are_ordered_longest_first_within_a_shared_head() {
        // The list is tried in order, so a prefix that is itself a prefix
        // of an earlier entry would be unreachable.
        for rules in BUILTINS {
            for (i, (prefix, _)) in rules.sigil_words.iter().enumerate() {
                for (earlier, _) in &rules.sigil_words[..i] {
                    assert!(
                        !prefix.starts_with(earlier),
                        "{}: \"{prefix}\" is shadowed by \"{earlier}\"",
                        rules.name
                    );
                }
            }
        }
    }

    #[test]
    fn token_kind_variable_is_constructible_copyable_and_distinct() {
        let a = TokenKind::Variable;
        let b = a;
        assert_eq!(a, b);
        assert_eq!(TokenKind::Variable, TokenKind::Variable);
        for other in [
            TokenKind::Keyword,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Punctuation,
            TokenKind::Key,
            TokenKind::Function,
            TokenKind::Type,
            TokenKind::Macro,
            TokenKind::Constant,
            TokenKind::Operator,
        ] {
            assert_ne!(TokenKind::Variable, other);
        }
    }

    /// No `SyntaxRules` field maps to `Variable` -- it's reachable only via
    /// external (semantic-token) classification, never from `tokenize`
    /// itself. Runs `tokenize` against every built-in language's own
    /// existing worked-example-shaped inputs and confirms none of them ever
    /// produce it, so this invariant stays true as languages are added.
    #[test]
    fn tokenize_never_produces_variable_for_any_builtin_language() {
        for rules in BUILTINS {
            let sample = [
                rules.keywords.first().copied().unwrap_or("word"),
                rules.type_keywords.first().copied().unwrap_or("Word"),
                "identifier_like_a_variable",
                "SCREAMING_CONSTANT",
            ]
            .join(" ");
            let tokens = tokenize(&sample, rules);
            assert!(
                tokens.iter().all(|t| t.kind != TokenKind::Variable),
                "{}: tokenize produced a Variable token from {sample:?}",
                rules.name
            );
        }
    }
}
