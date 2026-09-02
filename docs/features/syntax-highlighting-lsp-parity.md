# Syntax Highlighting: LSP-Parity Languages (Ruby, PHP, Swift, Kotlin, Lua, Zig, Haskell, Elixir, Dart)

Extends `docs/features/syntax-highlighting.md` (the base tokenizer),
`docs/features/syntax-highlighting-env-make-docker.md` (filename-based
lookup) and `docs/features/syntax-highlighting-more-languages.md` (the
`line_prefix_tokens` field and the Rust/TOML/Shell/Python/Go/Markdown/XML
batch) with nine more built-in `SyntaxRules`. No new tokenizer mechanism.

## 1. Purpose

`crates/core/src/language.rs`'s `LANGUAGE_MARKERS` already auto-detects a
project and launches a language server for nine languages that
`crates/core/src/syntax.rs`'s `BUILTINS` has no entry for at all: Ruby,
PHP, Swift, Kotlin, Lua, Zig, Haskell, Elixir, Dart. A file in any of these
today opens with a fully working LSP connection (diagnostics, completion,
hover) but zero fallback highlighting — no keywords, no strings, no
comments colored — until/unless semantic tokens arrive from the server.
That's the gap this doc closes, purely additively, the same shape as the
previous two batches: nine new `SyntaxRules` consts, nine new `BUILTINS`
entries, no struct field changes, no new `tokenize` branch.

| Language | Resolves via | LSP marker (`language.rs`) |
|---|---|---|
| Ruby | `.rb` | `Gemfile` |
| PHP | `.php` | `composer.json` |
| Swift | `.swift` | `Package.swift` |
| Kotlin | `.kt`, `.kts` | `build.gradle.kts` |
| Lua | `.lua` | (existing marker set) |
| Zig | `.zig` | `build.zig` |
| Haskell | `.hs`, `.lhs` | (existing marker set) |
| Elixir | `.ex`, `.exs` | `mix.exs` |
| Dart | `.dart` | (existing marker set) |

Every `extension`/`extra_extensions` pair below is copied verbatim from
`LANGUAGE_MARKERS` (verified against `crates/core/src/language.rs` at the
time of writing) — this doc invents no extension list of its own.

**Scope**: same non-goal as every prior syntax-highlighting doc. No
nesting, no cross-line state beyond what `LineState` already tracks
(unterminated block comments), no semantic awareness. Every limitation is
enumerated in §3.

## 2. Interface / API

### 2.1 `ide-core` (`crates/core/src/syntax.rs`)

No struct change. Nine new built-in rule sets:

```rust
pub const RUBY: SyntaxRules;
pub const PHP: SyntaxRules;
pub const SWIFT: SyntaxRules;
pub const KOTLIN: SyntaxRules;
pub const LUA: SyntaxRules;
pub const ZIG: SyntaxRules;
pub const HASKELL: SyntaxRules;
pub const ELIXIR: SyntaxRules;
pub const DART: SyntaxRules;
```

All nine are appended to `BUILTINS` (grows from 20 to 29 entries) — no
change to `syntax_for_path`, `syntax_for_extension`, or `tokenize`'s
signature or cascade. `lib.rs` re-exports all nine consts, matching how
every prior batch's consts are re-exported.

### 2.2 `ide-ui` / `ide-lsp`

**No change to either crate.** `Tab::syntax_for_buffer` already resolves
new languages automatically via `BUILTINS`, and `detect_language`/
`LanguageConfig` already handle these nine languages' LSP side —
`syntax.rs` and `language.rs` remain two independent lookup tables with no
shared code path, so adding a `SyntaxRules` entry cannot affect LSP
auto-detect and vice versa (same independence the previous batch's §6
already established).

Per the more-languages doc's post-merge correction, this is a
*behavioural* change for any test elsewhere in the workspace that asserts
one of these nine extensions currently resolves to no syntax/language —
checked in advance for this batch (see §6): none exists. `ide-ui`'s own
"unrecognized extension" fixture (`app.rs`,
`from_buffer_with_unrecognized_extension_has_no_syntax_or_tokens`) already
uses `logo.png`, and `ide-core`'s two equivalent tests
(`unrecognized_extension_returns_none`, and the `syntax_for_path` sibling)
use `bin`/`png`/`target/debug/ide`/`..`/`/` — none of the nine extensions
this doc claims. No cross-crate test repointing is required; `rust-core-dev`
alone is sufficient.

## 3. Behaviour

### New `SyntaxRules` definitions

Every field not listed for a language below is empty/`None`/`false`
(`filenames: []`, `filename_prefixes: []`, `line_prefix_tokens: []`,
`sigil_words: []`, `key_separator: None`, `macro_bang: false`,
`attribute_names: false`, `indent_line_suffixes: []`) — stated once here,
per the more-languages doc's own convention, and called out explicitly
only where a language sets one.

**`RUBY`** — `extensions: ["rb"]`, `line_comment_prefixes: ["#"]`,
`block_comment: Some(("=begin", "=end"))`, `string_quotes: ['"', '\'']`,
`keywords:` (`alias`, `and`, `begin`, `break`, `case`, `class`, `def`,
`defined?`, `do`, `else`, `elsif`, `end`, `ensure`, `false`, `for`, `if`,
`in`, `module`, `next`, `nil`, `not`, `or`, `redo`, `rescue`, `retry`,
`return`, `self`, `super`, `then`, `true`, `undef`, `unless`, `until`,
`when`, `while`, `yield`), `type_keywords: []`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`line_prefix_tokens: [("@", TokenKind::Macro)]` (instance/class variable
sigil — reuses the same "line-start-only" caveat from Python's `@`, noted
as a limitation below since Ruby's `@foo` is valid mid-line too),
`capitalized_is_type: true` (Ruby constants and class names are both
capitalized; the model can't tell them apart, same acceptable blur as
every other language's `capitalized_is_type`), `upper_case_is_constant:
true`, `brackets: [('{','}'), ('(',')'), ('[',']')]`.

`=begin`/`=end` is real Ruby but only valid at column 0; the existing
`try_block_comment` has no column-position awareness (nothing in
`SyntaxRules` does), so `x =begin` mid-line would also open a block
comment. Documented as a limitation, not fixed — the same class of
deviation the more-languages doc accepted for Markdown headings.

**`PHP`** — `extensions: ["php"]`, `line_comment_prefixes: ["//", "#"]`,
`block_comment: Some(("/*", "*/"))`, `string_quotes: ['"', '\'']`,
`keywords:` (`abstract`, `and`, `array`, `as`, `break`, `case`, `catch`,
`class`, `clone`, `const`, `continue`, `declare`, `default`, `do`, `echo`,
`else`, `elseif`, `enddeclare`, `endfor`, `endforeach`, `endif`,
`endswitch`, `endwhile`, `enum`, `extends`, `final`, `finally`, `fn`,
`for`, `foreach`, `function`, `global`, `if`, `implements`, `include`,
`include_once`, `instanceof`, `insteadof`, `interface`, `match`,
`namespace`, `new`, `null`, `or`, `private`, `protected`, `public`,
`readonly`, `require`, `require_once`, `return`, `static`, `switch`,
`throw`, `trait`, `true`, `false`, `try`, `use`, `var`, `while`, `xor`,
`yield`), `type_keywords: ["bool", "int", "float", "string", "void",
"mixed", "object", "callable", "iterable", "self", "parent"]`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`sigil_words: [("$", TokenKind::Constant)]` (PHP variables are always
`$name` — reusing the sigil mechanism Rust's lifetimes use, colored as
`Constant` to match the codebase's existing convention for exactly this
shape: `SHELL`'s own `sigil_words` already maps `$` to `Constant`, and
`TokenKind::Constant`'s doc comment explicitly names "a shell/Makefile
`$VAR`" as a covered case), `capitalized_is_type: true`,
`upper_case_is_constant: true`,
`brackets: [('{','}'), ('(',')'), ('[',']')]`.

PHP's `<?php ... ?>` tag delimiters aren't modeled — a `.php` file mixing
HTML and PHP will highlight everything between (and outside) the tags as
plain PHP text/plain, since the tokenizer has no per-region language
switch. Same class of accepted gap as the more-languages doc's "no
embedded-language switching" absence for HTML `<script>` blocks (never
claimed there either).

**`SWIFT`** — `extensions: ["swift"]`, `line_comment_prefixes: ["//"]`,
`block_comment: Some(("/*", "*/"))`, `string_quotes: ['"']`,
`keywords:` (`associatedtype`, `break`, `case`, `catch`, `class`,
`continue`, `default`, `defer`, `deinit`, `do`, `else`, `enum`, `extension`,
`fallthrough`, `false`, `fileprivate`, `final`, `for`, `func`, `guard`,
`if`, `import`, `in`, `init`, `inout`, `internal`, `is`, `lazy`, `let`,
`mutating`, `nil`, `open`, `operator`, `private`, `protocol`, `public`,
`repeat`, `rethrows`, `return`, `self`, `Self`, `static`, `struct`,
`subscript`, `super`, `switch`, `throw`, `throws`, `true`, `try`,
`typealias`, `var`, `where`, `while`), `type_keywords: ["Int", "Double",
"Float", "Bool", "String", "Character", "Any", "AnyObject", "Void"]`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`sigil_words: [("#", TokenKind::Macro)]` (compiler directives like
`#available`/`#selector`), `capitalized_is_type: true`,
`upper_case_is_constant: true`,
`brackets: [('{','}'), ('(',')'), ('[',']')]`.

Swift string interpolation (`"\(expr)"`) is not specially handled — the
`\` before `(` is just an escape as far as `try_string` is concerned, so
`expr` inside the interpolation is highlighted as part of the string, not
as code. Same accepted-limitation shape as the more-languages doc's
Shell `$VAR`-in-string case.

**`KOTLIN`** — `extensions: ["kt", "kts"]`, `line_comment_prefixes:
["//"]`, `block_comment: Some(("/*", "*/"))`, `string_quotes: ['"']`,
`keywords:` (`as`, `break`, `class`, `continue`, `do`, `else`, `false`,
`for`, `fun`, `if`, `in`, `interface`, `is`, `null`, `object`, `package`,
`return`, `super`, `this`, `throw`, `true`, `try`, `typealias`, `typeof`,
`val`, `var`, `when`, `while`, `by`, `catch`, `constructor`, `data`,
`enum`, `finally`, `import`, `init`, `inline`, `internal`, `override`,
`private`, `protected`, `public`, `sealed`, `suspend`),
`type_keywords: ["Int", "Long", "Double", "Float", "Boolean", "Char",
"String", "Unit", "Any", "Nothing"]`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`line_prefix_tokens: [("@", TokenKind::Macro)]` (annotations),
`capitalized_is_type: true`, `upper_case_is_constant: true`,
`brackets: [('{','}'), ('(',')'), ('[',']')]`.

Kotlin's triple-quoted raw strings (`"""..."""`) suffer the same
empty-string-then-plain-text artifact the more-languages doc already
documented for Python — not re-litigated here, just inherited.

**`LUA`** — `extensions: ["lua"]`, `line_comment_prefixes: ["--"]`,
`block_comment: None` (see below — Lua's real `--[[ ]]` block comment
cannot be expressed with the existing cascade), `string_quotes: ['"', '\'']`,
`keywords:` (`and`, `break`, `do`, `else`, `elseif`, `end`, `false`, `for`,
`function`, `goto`, `if`, `in`, `local`, `nil`, `not`, `or`, `repeat`,
`return`, `then`, `true`, `until`, `while`), `type_keywords: []`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','~','#','.']`,
`capitalized_is_type: false` (Lua has no capitalization convention for
types — it's dynamically typed with no class keyword at the language
level; turning this on would just color arbitrary capitalized identifiers,
unlike every other language here where it tracks a real convention),
`upper_case_is_constant: true`, `brackets: [('{','}'), ('(',')'),
('[',']')]`.

**Lua block comments can't be represented, and that's `block_comment:
None` rather than `Some(("--[[", "]]"))`.** `tokenize_span` checks
`try_line_comment` *before* `try_block_comment` (`syntax-highlighting-
more-languages.md`'s own documented cascade order: line comment is step 3,
block comment step 4), and the check is a plain `starts_with` against
`line_comment_prefixes`. Lua's block-comment open, `--[[`, itself starts
with `--`, so `try_line_comment` always matches first and swallows the
rest of the line as an ordinary `Comment` token — `try_block_comment` is
never reached at all. A single-line `--[[ x ]]` looks right by accident
(the whole thing becomes one `Comment` token either way), but a real
multi-line block comment breaks: only its first line is treated as
comment; every following line is left as ordinary code, since
`LineState` never enters `InBlockComment`. This is the "one of the nine
genuinely needs something the existing fields can't express" case §1
flagged as a possibility — fixing it needs the cascade itself to check for
a longer, more-specific block-comment delimiter before falling back to a
plain line-comment prefix, which is a real (if small) `tokenize` change
and out of scope for this purely-additive-data doc. `block_comment: None`
is the honest, non-misleading data value until a future doc addresses the
general "line comment prefix is a prefix of my block comment open"
ordering problem — Lua source will highlight `--` line comments correctly
and simply show no dedicated coloring for `--[[ ]]` regions.

**`ZIG`** — `extensions: ["zig"]`, `line_comment_prefixes: ["//"]`,
`block_comment: None` (Zig has no block-comment syntax at all),
`string_quotes: ['"']` (`'` is excluded: like Rust, Zig uses `'` for
character/codepoint literals only — no lifetime collision here, but also
no payoff, so it's left out for the same "ambiguous with common non-string
use" caution rather than included and mismatching the doc's Rust
precedent for no reason), `keywords:` (`align`, `allowzero`, `and`,
`anyframe`, `anytype`, `asm`, `async`, `await`, `break`, `callconv`,
`catch`, `comptime`, `const`, `continue`, `defer`, `else`, `enum`,
`errdefer`, `error`, `export`, `extern`, `fn`, `for`, `if`, `inline`,
`noalias`, `noinline`, `nosuspend`, `opaque`, `or`, `orelse`, `packed`,
`pub`, `resume`, `return`, `linksection`, `struct`, `suspend`, `switch`,
`test`, `threadlocal`, `try`, `union`, `unreachable`, `usingnamespace`,
`var`, `volatile`, `while`, `true`, `false`, `null`, `undefined`),
`type_keywords: ["bool", "void", "type", "anyerror", "u8", "u16", "u32",
"u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f16",
"f32", "f64", "f128"]`, `punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`sigil_words: [("@", TokenKind::Macro)]` (Zig builtins like `@import`,
`@as` are always `@`-prefixed identifiers), `capitalized_is_type: true`,
`upper_case_is_constant: true`, `brackets: [('{','}'), ('(',')'),
('[',']')]`.

**`HASKELL`** — `extensions: ["hs", "lhs"]`, `line_comment_prefixes:
["--"]`, `block_comment: Some(("{-", "-}"))`, `string_quotes: ['"']`
(`'` deliberately excluded — Haskell overloads `'` for both character
literals and trailing identifier characters like `x'`/`n'`, and including
it as a string quote would make every primed identifier open an
unterminated string; the same "ambiguous, excluded" call as Rust's `'`),
`keywords:` (`case`, `class`, `data`, `default`, `deriving`, `do`, `else`,
`foreign`, `if`, `import`, `in`, `infix`, `infixl`, `infixr`, `instance`,
`let`, `module`, `newtype`, `of`, `then`, `type`, `where`),
`type_keywords: ["Int", "Integer", "Float", "Double", "Bool", "Char",
"String", "IO", "Maybe", "Either"]`,
`punctuation: ['(',')','[',']','{','}',',',';']`,
`operators: ['=','<','>','+','-','*','/','!','|','&','.','$',':','\\']`,
`capitalized_is_type: true`, `upper_case_is_constant: false` (Haskell's
`SCREAMING_SNAKE_CASE` constant convention is far weaker than C-family
languages' — top-level constants are ordinary `camelCase` bindings — so
this would mostly just miscolor uncommon identifiers; left off rather than
guessing), `brackets: [('{','}'), ('(',')'), ('[',']')]`.

Haskell's `--` line-comment rule has the same "prefix of a longer real
operator" hazard as Lua's block-comment open, but in the opposite
direction: an operator sequence like `-->` or a user-defined `-->>`
operator starting with `--` is swallowed as a comment from that point,
which is a real (if rare) Haskell operator-naming collision. Documented,
not fixed — resolving it needs lookahead the line-comment rule doesn't do
for any language.

**`ELIXIR`** — `extensions: ["ex", "exs"]`, `line_comment_prefixes:
["#"]`, `block_comment: None` (Elixir has no block-comment syntax),
`string_quotes: ['"', '\'']`, `keywords:` (`after`, `alias`, `and`, `case`,
`catch`, `cond`, `def`, `defexception`, `defguard`, `defimpl`, `defmacro`,
`defmodule`, `defp`, `defprotocol`, `defstruct`, `do`, `else`, `end`,
`false`, `fn`, `for`, `if`, `import`, `in`, `nil`, `not`, `or`, `quote`,
`raise`, `receive`, `require`, `rescue`, `true`, `try`, `unless`,
`unquote`, `use`, `when`, `with`), `type_keywords: []`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','!','|','&','.','^']`,
`sigil_words: [("@", TokenKind::Macro), (":", TokenKind::Type)]`
(`@moduledoc`/module attributes, and `:atom` literals — tried in the
listed order per `sigil_words`' documented "longer prefix first" rule;
`":"` doesn't collide with a longer prefix here since neither starts with
the other), `capitalized_is_type: true` (Elixir module names are
capitalized aliases), `upper_case_is_constant: true`,
`brackets: [('{','}'), ('(',')'), ('[',']')]`.

The `:atom` sigil rule interacts with `punctuation`'s existing `:`
(argument/keyword-list separator, e.g. `key: value`) the same way every
other `sigil_words` entry interacts with its base punctuation character:
`sigil_words` is tried before punctuation in the cascade (§3), so `:atom`
(identifier immediately follows) becomes one `Type` token, while `key:
value`'s `:` (whitespace follows, no identifier) falls through to a plain
`Punctuation` token exactly as the struct's own doc comment on
`sigil_words` describes for `$(cmd)`/`<3`.

**`DART`** — `extensions: ["dart"]`, `line_comment_prefixes: ["//"]`,
`block_comment: Some(("/*", "*/"))`, `string_quotes: ['"', '\'']`,
`keywords:` (`abstract`, `as`, `assert`, `async`, `await`, `break`, `case`,
`catch`, `class`, `const`, `continue`, `default`, `deferred`, `do`,
`dynamic`, `else`, `enum`, `export`, `extends`, `extension`, `factory`,
`false`, `final`, `finally`, `for`, `function`, `get`, `if`, `implements`,
`import`, `in`, `interface`, `is`, `late`, `library`, `mixin`, `new`,
`null`, `on`, `operator`, `part`, `required`, `rethrow`, `return`, `set`,
`static`, `super`, `switch`, `sync`, `this`, `throw`, `true`, `try`,
`typedef`, `var`, `void`, `while`, `with`, `yield`),
`type_keywords: ["int", "double", "num", "bool", "String", "Object",
"List", "Map", "Set", "Future"]`,
`punctuation: ['(',')','[',']','{','}',',',';',':']`,
`operators: ['=','<','>','+','-','*','/','%','!','|','&','^','.','?']`,
`line_prefix_tokens: [("@", TokenKind::Macro)]` (annotations, same
line-start-only caveat as Ruby/Kotlin's `@`), `capitalized_is_type: true`,
`upper_case_is_constant: true`, `brackets: [('{','}'), ('(',')'),
('[',']')]`.

Dart string interpolation (`"${expr}"`/`"$name"`) has the same
not-specially-handled status as Swift's — the whole string, interpolation
included, is one `String` token.

### `tokenize`'s rule order — unchanged

All nine languages exercise only cascade steps that already exist (line
comment, block comment, string, sigil word, keyword/type keyword, operator,
punctuation) — see `syntax-highlighting.md` §3 and
`syntax-highlighting-more-languages.md`'s "tokenize's rule order" section
for the authoritative ordering. No new step, no new `TokenKind` variant.

### Cross-cutting limitations (apply to all nine, stated once)

- **`@`/`#`/`$`/`:` line-prefix and sigil rules are line-start-only where
  implemented via `line_prefix_tokens`** (Ruby/Kotlin/Dart's `@`) —
  mid-line uses of the same character (a Ruby instance variable read
  inside an expression, a Kotlin annotation on the same line as code) fall
  through to plain `Punctuation`/text instead. Where the same concept is
  instead expressed via `sigil_words` (PHP's `$`, Zig's `@`, Elixir's `@`
  and `:`), it *does* work mid-line, since `sigil_words` has no
  line-position restriction — the split between the two mechanisms exists
  because `line_prefix_tokens` claims the rest of the line (right for an
  annotation that's the only thing on its line) while `sigil_words` claims
  just the sigil plus one identifier (right for a variable reference used
  inline). Each language's choice above matches its actual common usage
  pattern, not a rule applied mechanically.
- **No per-language function/type name highlighting** beyond
  `capitalized_is_type`/`upper_case_is_constant`'s blunt heuristics —
  consistent with every prior batch; the tokenizer has no declaration-vs-
  reference context anywhere.
- **No string interpolation awareness** (Swift `\(...)`, Dart `${...}`)
  — the whole string highlights as one token, interpolated expression
  included. Same class as the more-languages doc's Shell `$VAR` case.

## 4. Constraints & invariants

- Every base-doc invariant holds unchanged: O(n) tokenization, sorted
  non-overlapping output, UTF-8-boundary-safe ranges,
  `MAX_HIGHLIGHTED_FILE_BYTES` short-circuit, no I/O, no new dependency.
- `BUILTINS` grows from 20 to 29 entries; `syntax_for_path` and
  `syntax_for_extension` stay O(number of languages).
- Extension sets across all 29 languages must remain disjoint — the
  existing `builtin_extension_sets_are_disjoint` test enforces this
  mechanically and covers the nine new entries automatically with zero
  test-code changes. (Sanity-checked while writing this doc: none of
  `rb`/`php`/`swift`/`kt`/`kts`/`lua`/`zig`/`hs`/`lhs`/`ex`/`exs`/`dart`
  collides with any of the 20 existing languages' `extensions`.)
- Every `extensions`/`extra_extensions` pair matches `LANGUAGE_MARKERS`
  exactly (§1's table) — this is a hard requirement from the feature
  request, not just a nice-to-have, since the whole point is LSP/highlight
  parity per language.
- No `crates/lsp` or `crates/ui` change of any kind (§2.2).

## 5. Examples

**Ruby** (`def greet(name)\n  puts "hi"\n  # done\nend\n`), exercising the
`classify_word` "identifier immediately followed by `(`" rule the same way
the Rust worked example does:

```rust
let rules = ide_core::syntax_for_path(std::path::Path::new("app.rb")).unwrap();
let tokens = ide_core::tokenize("def greet(name)\n  puts \"hi\"\n  # done\nend\n", rules);
// Keyword("def") 0..3, Function("greet") 4..9, Punctuation('(') 9..10,
// Punctuation(')') 14..15, String("\"hi\"") 23..27, Comment("# done") 30..36,
// Keyword("end") 37..40
// -- "name"/"puts" are plain: "name" is followed by ')', not '(', and
// "puts" here is followed by a space, not '(' (this call has no parens).
```

**PHP** (a `$name` variable via the sigil rule, colored `Constant` to match
`SHELL`'s existing `$VAR` convention):

```rust
let rules = ide_core::syntax_for_path(std::path::Path::new("index.php")).unwrap();
let tokens = ide_core::tokenize("function greet($name) {\n    return $name;\n}\n", rules);
// Keyword("function") 0..8, Function("greet") 9..14, Punctuation('(') 14..15,
// Constant("$name") 15..20, Punctuation(')') 20..21, Punctuation('{') 22..23,
// Keyword("return") 28..34, Constant("$name") 35..40, Punctuation(';') 40..41,
// Punctuation('}') 42..43
```

**Zig** (`@import` builtin via `sigil_words`; `=` is `Operator`, not
`Punctuation`, since Zig's `operators` list includes it and `tokenize_span`
checks `operators` before `punctuation`):

```rust
let rules = ide_core::syntax_for_path(std::path::Path::new("main.zig")).unwrap();
let tokens = ide_core::tokenize("const std = @import(\"std\");\n", rules);
// Keyword("const") 0..5, Operator('=') 10..11,
// Macro("@import") 12..19, Punctuation('(') 19..20,
// String("\"std\"") 20..25, Punctuation(')') 25..26, Punctuation(';') 26..27
// -- "std" is plain (not a type_keyword, not followed by '(').
```

**Elixir** (`:atom` sigil vs. plain `:` punctuation, same line — this is
also why `:` must be in `punctuation`, not just reachable via the sigil):

```rust
let rules = ide_core::syntax_for_path(std::path::Path::new("lib/app.ex")).unwrap();
let tokens = ide_core::tokenize("def run(opts), do: :ok\n", rules);
// Keyword("def") 0..3, Function("run") 4..7, Punctuation('(') 7..8,
// Punctuation(')') 12..13, Punctuation(',') 13..14, Keyword("do") 15..17,
// Punctuation(':') 17..18, Type(":ok") 19..22
// -- "opts" is plain (followed by ')', not '('). The first ':' (after
// "do") is followed by a space, so try_sigil_word's identifier lookup
// fails and it falls through to the plain Punctuation rule; the second
// ':' is immediately followed by "ok" and becomes one Type token via
// sigil_words, tried before punctuation in the cascade.
```

**Lua** (line comment; block comment intentionally unsupported — see §3):

```rust
let rules = ide_core::syntax_for_path(std::path::Path::new("init.lua")).unwrap();
let tokens = ide_core::tokenize("-- setup\nlocal x = 42\n", rules);
// Comment("-- setup") 0..8, Keyword("local") 9..14, Operator('=') 17..18,
// Number("42") 19..21
// -- "x" is plain (followed by a space, not '(').
```

## 6. Dependencies & integration points

- `ide-core`: `syntax.rs` gains nine new consts and nine new `BUILTINS`
  entries. `lib.rs` re-exports all nine. No struct field change, no
  `tokenize` cascade change, no new dependency.
- `ide-ui`, `crates/lsp`: **no change** (§2.2) — verified in advance (this
  doc) that no existing test in either crate encodes one of these nine
  extensions as "resolves to no language."
- `crates/dap`: not applicable — unrelated to per-file highlighting.
- Required roles: `rust-core-dev` only. Not a declared security-sensitive
  path per `CLAUDE.md` — no `hacker` pass expected.

## 7. Diagrams

Skipped — nine additive data records over an already-diagrammed cascade
(`syntax-highlighting-more-languages-cascade.png` still applies verbatim,
since no step changes). Same "skip for a change too small to benefit" call
the dev-chain template allows.

## Revision notes

Round 1 review findings, all addressed in place:

1. **Lua's `block_comment: Some(("--[[", "]]"))` was unreachable.**
   `tokenize_span` checks `try_line_comment` before `try_block_comment`,
   and `--[[` starts with `--`, so the line-comment rule always wins —
   real multi-line block comments would have silently broken after their
   first line. Changed to `block_comment: None` with the reachability
   limitation documented in §3, and replaced the §5 Lua example (it had
   relied on the broken behavior).
2. **PHP's `$name` sigil used `TokenKind::Type`.** The codebase already
   has a convention for this exact shape — `SHELL`'s `sigil_words` maps
   `$` to `Constant`, and `TokenKind::Constant`'s own doc comment names
   "a shell/Makefile `$VAR`" as a covered case. Changed to `Constant` and
   updated the §5 PHP example.
3. **Elixir's `punctuation` list omitted `:`.** A `:` not immediately
   followed by an identifier (e.g. `do:`) fell through every rule
   unmatched and produced no token at all, contradicting the doc's own
   description of the interaction. Added `:` to `punctuation` and
   corrected the §5 example's token list.
4. **Three §5 examples (Ruby, PHP, Elixir) missed `classify_word`'s
   Function-detection rule** — an identifier immediately followed by `(`
   is a `Function` token (`greet`, `run`), the same rule the predecessor
   doc's own Rust example relies on. All three examples were re-traced
   byte-for-byte and corrected, including every downstream byte offset.
5. **The Zig example mislabeled `=` as `Punctuation`.** Zig's `operators`
   list includes `=`, and `operators` is checked before `punctuation` in
   the cascade, so it resolves to `Operator`. Corrected.
