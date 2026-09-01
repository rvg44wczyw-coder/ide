use std::time::{Duration, Instant};

use super::*;
use crate::syntax::{tokenize, TokenKind, RUST};

fn rust() -> Option<&'static SyntaxRules> {
    Some(&RUST)
}

#[test]
fn new_starts_with_one_caret_and_a_clean_history() {
    let mut buffer = TextBuffer::new("hello", None);
    assert_eq!(buffer.len(), 5);
    assert!(!buffer.is_empty());
    assert_eq!(buffer.selections().len(), 1);
    assert_eq!(buffer.selections().primary(), Selection::caret(0));
    assert!(!buffer.undo());
    assert!(!buffer.redo());
}

#[test]
fn empty_buffer_is_one_empty_line() {
    let buffer = TextBuffer::new("", None);
    assert!(buffer.is_empty());
    assert_eq!(buffer.lines().line_count(), 1);
    assert_eq!(buffer.line_text(0), Some(""));
    assert_eq!(buffer.line_text(1), None);
}

#[test]
fn line_lookups_match_the_documented_example() {
    let buffer = TextBuffer::new("fn main() {\n    println!(\"hi\");\n}\n", None);
    assert_eq!(buffer.lines().line_count(), 4);
    assert_eq!(buffer.line_text(1), Some("    println!(\"hi\");"));
    assert_eq!(buffer.lines().position_at(12), (1, 0));
}

#[test]
fn apply_maps_the_caret_across_a_replacement() {
    let mut buffer = TextBuffer::new("hello world", None);
    buffer.apply(Transaction::replace(0..5, "goodbye"));
    assert_eq!(buffer.text(), "goodbye world");
    assert_eq!(buffer.selections().primary(), Selection::caret(7));
}

#[test]
fn apply_ignores_an_empty_transaction() {
    let mut buffer = TextBuffer::new("abc", None);
    buffer.apply(Transaction::default());
    assert_eq!(buffer.text(), "abc");
    assert!(!buffer.undo());
}

#[test]
fn apply_ignores_a_transaction_that_clamps_to_nothing() {
    let mut buffer = TextBuffer::new("abc", None);
    buffer.apply(Transaction::delete(9..12));
    assert_eq!(buffer.text(), "abc");
    assert!(!buffer.undo());
}

#[test]
fn apply_clamps_a_range_past_the_end_instead_of_panicking() {
    let mut buffer = TextBuffer::new("abc", None);
    buffer.apply(Transaction::replace(2..99, "Z"));
    assert_eq!(buffer.text(), "abZ");
}

#[test]
fn apply_clamps_to_a_char_boundary() {
    let mut buffer = TextBuffer::new("a\u{1F600}b", None);
    buffer.apply(Transaction::insert(3, "X"));
    assert_eq!(buffer.text(), "aX\u{1F600}b");
}

#[test]
fn multi_cursor_insert_is_one_undo_step() {
    let mut buffer = TextBuffer::new("one\ntwo\nthree", rust());
    buffer.set_selections(Selections::new(
        vec![
            Selection::caret(0),
            Selection::caret(4),
            Selection::caret(8),
        ],
        0,
    ));
    buffer.insert_at_selections("// ");
    assert_eq!(buffer.text(), "// one\n// two\n// three");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "one\ntwo\nthree");
    assert_eq!(buffer.selections().len(), 3);
    assert!(buffer.redo());
    assert_eq!(buffer.text(), "// one\n// two\n// three");
}

#[test]
fn insert_at_selections_leaves_carets_past_the_inserted_text() {
    let mut buffer = TextBuffer::new("ab", None);
    buffer.set_selections(Selections::new(
        vec![Selection::caret(0), Selection::caret(2)],
        0,
    ));
    buffer.insert_at_selections("-");
    assert_eq!(buffer.text(), "-ab-");
    assert_eq!(buffer.selections().all()[0], Selection::caret(1));
    assert_eq!(buffer.selections().all()[1], Selection::caret(4));
}

#[test]
fn insert_at_selections_replaces_selected_ranges() {
    let mut buffer = TextBuffer::new("aaa bbb", None);
    buffer.set_selections(Selections::new(
        vec![Selection::new(0, 3), Selection::new(4, 7)],
        0,
    ));
    buffer.insert_at_selections("x");
    assert_eq!(buffer.text(), "x x");
}

#[test]
fn colliding_cursors_merge_before_the_edit() {
    let mut buffer = TextBuffer::new("abcdef", None);
    buffer.set_selections(Selections::new(
        vec![Selection::new(0, 4), Selection::new(2, 6)],
        0,
    ));
    assert_eq!(buffer.selections().len(), 1);
    buffer.insert_at_selections("Z");
    assert_eq!(buffer.text(), "Z");
}

#[test]
fn typed_runs_coalesce_but_applied_edits_do_not() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("", None);
    for ch in ["h", "e", "l", "l", "o"] {
        buffer.type_text_at(ch, now);
    }
    assert_eq!(buffer.text(), "hello");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "");

    let mut buffer = TextBuffer::new("", None);
    buffer.apply(Transaction::insert(0, "hello"));
    buffer.apply(Transaction::insert(5, " world"));
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "hello");
}

#[test]
fn a_typed_newline_breaks_the_run() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("", None);
    buffer.type_text_at("a", now);
    buffer.type_text_at("\n", now);
    buffer.type_text_at("b", now);
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "a\n");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "a");
}

#[test]
fn the_coalesce_timeout_breaks_the_run() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("", None);
    buffer.type_text_at("a", now);
    buffer.type_text_at("b", now + Duration::from_millis(600));
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "a");
}

#[test]
fn break_undo_group_breaks_the_run() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("", None);
    buffer.type_text_at("a", now);
    buffer.break_undo_group();
    buffer.type_text_at("b", now);
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "a");
}

#[test]
fn moving_the_caret_breaks_the_run() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("xy", None);
    buffer.type_text_at("a", now);
    buffer.set_selections(Selections::single(Selection::caret(3)));
    buffer.type_text_at("b", now);
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "axy");
}

#[test]
fn undo_and_redo_restore_selections_both_ways() {
    let now = Instant::now();
    let mut buffer = TextBuffer::new("ab", None);
    buffer.set_selections(Selections::single(Selection::caret(1)));
    buffer.type_text_at("X", now);
    assert_eq!(buffer.selections().primary(), Selection::caret(2));
    assert!(buffer.undo());
    assert_eq!(buffer.selections().primary(), Selection::caret(1));
    assert!(buffer.redo());
    assert_eq!(buffer.selections().primary(), Selection::caret(2));
}

#[test]
fn undo_to_the_bottom_restores_the_initial_text() {
    let mut buffer = TextBuffer::new("start", None);
    buffer.apply(Transaction::insert(5, "-more"));
    buffer.apply(Transaction::delete(0..2));
    while buffer.undo() {}
    assert_eq!(buffer.text(), "start");
    assert!(!buffer.undo());
}

#[test]
fn a_new_edit_clears_the_redo_stack() {
    let mut buffer = TextBuffer::new("", None);
    buffer.apply(Transaction::insert(0, "a"));
    buffer.undo();
    buffer.apply(Transaction::insert(0, "b"));
    assert!(!buffer.redo());
}

#[test]
fn undo_of_a_multi_change_transaction_restores_every_span() {
    let mut buffer = TextBuffer::new("aa bb cc", None);
    let transaction = Transaction::new(vec![
        Change::new(0..2, "XXXX"),
        Change::new(3..5, ""),
        Change::new(6..8, "Y"),
    ])
    .expect("non-overlapping");
    buffer.apply(transaction);
    assert_eq!(buffer.text(), "XXXX  Y");
    assert!(buffer.undo());
    assert_eq!(buffer.text(), "aa bb cc");
}

#[test]
fn tokens_track_an_edit_incrementally() {
    let mut buffer = TextBuffer::new("let a = 1;\nlet b = 2;\n", rust());
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
    buffer.apply(Transaction::insert(4, "bc"));
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
}

#[test]
fn an_unterminated_block_comment_recolors_the_rest_and_undo_restores_it() {
    let mut buffer = TextBuffer::new("let a = 1;\nlet b = 2;\n", rust());
    buffer.apply(Transaction::insert(0, "/*"));
    assert!(buffer.tokens().iter().all(|t| t.kind == TokenKind::Comment));
    assert_eq!(buffer.tokens().len(), 1);
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
    assert!(buffer.undo());
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
}

#[test]
fn closing_a_block_comment_restores_the_previous_coloring() {
    let source = "/*\nlet a = 1;\nlet b = 2;\n";
    let mut buffer = TextBuffer::new(source, rust());
    assert_eq!(buffer.tokens().len(), 1);
    buffer.apply(Transaction::insert(2, "*/"));
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
}

#[test]
fn tokens_in_lines_returns_only_that_viewport() {
    let buffer = TextBuffer::new("let a = 1;\nlet b = 2;\nlet c = 3;", rust());
    let slice = buffer.tokens_in_lines(1..2);
    assert!(!slice.is_empty());
    let line = buffer.lines().line_range(1, buffer.text()).unwrap();
    assert!(slice
        .iter()
        .all(|t| t.range.start >= line.start && t.range.end <= line.end));
    assert!(buffer.tokens_in_lines(99..100).is_empty());
    assert_eq!(buffer.tokens_in_lines(0..99).len(), buffer.tokens().len());
}

#[test]
fn set_syntax_retokenizes_from_scratch() {
    let mut buffer = TextBuffer::new("/* x */ let a = 1;", None);
    assert!(buffer.tokens().is_empty());
    buffer.set_syntax(rust());
    assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
    buffer.set_syntax(None);
    assert!(buffer.tokens().is_empty());
}

#[test]
fn a_text_over_the_highlight_cap_has_no_tokens() {
    let huge = "a".repeat(MAX_HIGHLIGHTED_FILE_BYTES + 1);
    let mut buffer = TextBuffer::new(huge, rust());
    assert!(buffer.tokens().is_empty());
    buffer.apply(Transaction::insert(0, "x"));
    assert!(buffer.tokens().is_empty());
}

/// The invariant the whole incremental design rests on: after an arbitrary
/// sequence of edits, both the line index and the token list must equal what
/// a full rebuild would produce.
#[test]
fn incremental_state_matches_a_full_rebuild() {
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    let inserts = [
        "x",
        "\n",
        "/*",
        "*/",
        "// c\n",
        "\"s\"",
        "fn f() {}",
        "",
        "é\n",
        "let",
    ];
    let mut buffer = TextBuffer::new("fn main() {\n    let x = 1;\n}\n", rust());

    for step in 0..400 {
        let len = buffer.len();
        let a = (next() as usize) % (len + 1);
        let b = (next() as usize) % (len + 1);
        let insert = inserts[(next() as usize) % inserts.len()];
        match step % 5 {
            0 => buffer.apply(Transaction::delete(a..b)),
            1 => buffer.apply(Transaction::insert(a, insert)),
            _ => buffer.apply(Transaction::replace(a..b, insert)),
        }
        assert_eq!(
            *buffer.lines(),
            LineIndex::new(buffer.text()),
            "line index diverged at step {step}"
        );
        assert_eq!(
            buffer.tokens(),
            tokenize(buffer.text(), &RUST),
            "tokens diverged at step {step} for text {:?}",
            buffer.text()
        );
    }

    while buffer.undo() {
        assert_eq!(*buffer.lines(), LineIndex::new(buffer.text()));
        assert_eq!(buffer.tokens(), tokenize(buffer.text(), &RUST));
    }
    assert_eq!(buffer.text(), "fn main() {\n    let x = 1;\n}\n");
}
