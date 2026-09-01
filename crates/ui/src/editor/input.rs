//! Keyboard and clipboard events become an `Intent` first, and only then a
//! `Transaction` or a selection change. The split is what makes input
//! testable without a live `egui::Context`: `intent_for` is pure, and
//! `apply_intent` needs no `Ui`
//! (`docs/features/code-editor-widget.md` §2.5).

use std::ops::Range;

use ide_core::{
    all_occurrences, newline_indent, next_occurrence, splits_a_pair, word_at, Buffer, Change,
    IndentUnit, LineDirection, Selection, Selections, SyntaxRules, TextBuffer, TokenKind,
    Transaction,
};

use super::folding::VisualLines;
use super::geometry::{byte_offset_in_line, char_index_in_line, column_of, word_range_at, Metrics};
use super::EditorState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Insert(String),
    Newline,
    DeleteBackward(Granularity),
    DeleteForward(Granularity),
    Move {
        direction: Direction,
        granularity: Granularity,
        extend: bool,
    },
    SelectAll,
    Copy,
    Cut,
    Paste(String),
    /// `⌃G`. The needle is resolved here, not by the caller: the primary
    /// selection's text, or the word under it when it is empty.
    AddNextOccurrence,
    /// `⌃⇧G`.
    UnselectOccurrence,
    /// `⌃⌘G`.
    SelectAllOccurrences,
    /// `⌘⇧8`.
    ToggleColumnMode,
    /// `⌥⌥`+`↑`/`↓`. Only `Up`/`Down` are ever constructed, and only by
    /// `handle_keys` rewriting a vertical `Move` -- `intent_for` cannot know
    /// whether the gesture is armed (doc §3.4).
    CloneCaret(Direction),
    /// `Esc`. Always produced; `apply_intent` no-ops on a single selection,
    /// which is what leaves `Esc` to the Usages popup (doc §3.6).
    CollapseSelections,
    /// `Tab` with a selection worth indenting. Never produced by
    /// `intent_for`: `Tab` is one keystroke with two meanings, so
    /// `Frame::rewrite` decides between this and `Insert`
    /// (`smart-editing.md` §2.6).
    Indent,
    /// `⇧Tab`.
    Outdent,
    /// No JetBrains binding exists for jump-to-match, so this ships with no
    /// default binding and is unreachable from the keyboard until the
    /// command registry lands (`CLAUDE.md`: register with none rather than
    /// invent one). Only tests construct it in this phase.
    #[allow(dead_code)]
    JumpToMatchingBracket,
    /// `⌘D`.
    DuplicateLines,
    /// `⌘⌫`.
    DeleteLines,
    /// `⌃⇧J`.
    JoinLines,
    /// `⌥⇧↑` / `⌥⇧↓`.
    MoveLines(LineDirection),
    /// `⌘⇧↑` / `⌘⇧↓`.
    MoveStatements(LineDirection),
    /// `⌘/`.
    ToggleLineComment,
    /// `⌘⌥/`.
    ToggleBlockComment,
    /// `⌥↑` unarmed -- only `Frame::rewrite` ever produces this, the same
    /// way it produces `CloneCaret` for an armed `⌥⌥` (doc §2.5, §1.2
    /// collision 1). `⌥↓` unarmed is `ShrinkSelection`.
    ExtendSelection,
    ShrinkSelection,
    /// `⌘⇧U`.
    ToggleCase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Character,
    Word,
    Line,
    Page,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// What applying an intent asks the caller to do. `Copy`/`Cut` need the
/// clipboard, which is `ui.ctx().copy_text` and therefore unavailable to a
/// function that deliberately takes no `Ui` -- so the text comes back out
/// and `show` performs the copy.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: bool,
    pub copy: Option<String>,
}

/// `None` for an event the editor does not claim, which leaves it for the
/// rest of the app -- so `Cmd+S`, `Cmd+Shift+F` and friends still work while
/// the editor has focus.
pub fn intent_for(event: &egui::Event) -> Option<Intent> {
    match event {
        // A modifier chord can also arrive as text on some platforms; only
        // bare (or shifted) text is insertion.
        egui::Event::Text(text) if !text.is_empty() && !text.chars().all(|c| c.is_control()) => {
            Some(Intent::Insert(text.clone()))
        }
        egui::Event::Paste(text) => Some(Intent::Paste(text.clone())),
        egui::Event::Copy => Some(Intent::Copy),
        egui::Event::Cut => Some(Intent::Cut),
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => key_intent(*key, *modifiers),
        _ => None,
    }
}

fn key_intent(key: egui::Key, modifiers: egui::Modifiers) -> Option<Intent> {
    use egui::Key;

    let extend = modifiers.shift;
    // The macOS system text-editing bindings, which the JetBrains macOS
    // keymap uses unchanged (doc §3.5). Nothing outside this table is
    // claimed here.
    let granularity = if modifiers.command {
        Granularity::Line
    } else if modifiers.alt {
        Granularity::Word
    } else {
        Granularity::Character
    };

    match key {
        Key::Enter if !modifiers.command => Some(Intent::Newline),
        Key::Tab if modifiers.shift && !modifiers.command && !modifiers.alt && !modifiers.ctrl => {
            Some(Intent::Outdent)
        }
        // Still A2's literal tab: what `Tab` means depends on the selection,
        // and this function cannot see it. `Frame::rewrite` rewrites this
        // into `Indent` or into one indent unit (`smart-editing.md` §2.6).
        Key::Tab if modifiers.is_none() => Some(Intent::Insert("\t".to_string())),
        // `⌘⌫` takes over Delete Line (doc §1.2 collision 3); A2's
        // delete-to-line-start binding is what loses this chord specifically,
        // not `Backspace` in general, so the plain fallback below still
        // handles every other modifier combination.
        Key::Backspace if modifiers.mac_cmd && !modifiers.shift && !modifiers.alt => {
            Some(Intent::DeleteLines)
        }
        Key::Backspace => Some(Intent::DeleteBackward(granularity)),
        Key::Delete if !modifiers.command => Some(Intent::DeleteForward(granularity)),
        // `⌘⌥←`/`⌘⌥→` are Back/Forward at the app level (`docs/features/
        // goto-definition.md` §3.5) -- unclaimed here (regardless of
        // `shift`, which JetBrains has no editor-local binding on either)
        // so they fall through to `handle_shortcuts`/the command registry
        // instead of resolving to Line-granularity motion the way a bare
        // `⌘←`/`⌘→` still does below. Adds no new editor behavior: `None`
        // means the editor widget does nothing with the chord at all.
        Key::ArrowLeft if modifiers.command && modifiers.alt => None,
        Key::ArrowRight if modifiers.command && modifiers.alt => None,
        Key::ArrowLeft => Some(Intent::Move {
            direction: Direction::Left,
            granularity,
            extend,
        }),
        Key::ArrowRight => Some(Intent::Move {
            direction: Direction::Right,
            granularity,
            extend,
        }),
        // Move Statement takes `⌘⇧↑`/`⌘⇧↓` over from A2's extend-to-document
        // (doc §1.2 collision 4; `⌘⇧Home`/`⌘⇧End` below replace it). Move
        // Line takes the unclaimed `⌥⇧↑`/`⌥⇧↓`. Neither needs `rewrite`:
        // unlike bare `⌥↑`/`⌥↓`, both require `shift`, so a pure predicate
        // already tells them apart from Clone Caret/Extend Selection.
        Key::ArrowUp if modifiers.mac_cmd && modifiers.shift && !modifiers.alt => {
            Some(Intent::MoveStatements(LineDirection::Up))
        }
        Key::ArrowDown if modifiers.mac_cmd && modifiers.shift && !modifiers.alt => {
            Some(Intent::MoveStatements(LineDirection::Down))
        }
        Key::ArrowUp if modifiers.alt && modifiers.shift && !modifiers.command => {
            Some(Intent::MoveLines(LineDirection::Up))
        }
        Key::ArrowDown if modifiers.alt && modifiers.shift && !modifiers.command => {
            Some(Intent::MoveLines(LineDirection::Down))
        }
        // Vertical motion has no word granularity; `Cmd` jumps to the
        // document's ends, everything else moves a row. A bare `⌥↑`/`⌥↓`
        // lands here too -- indistinguishable at this point from a plain
        // arrow press, which is exactly why `Frame::rewrite` (not this pure
        // function) decides whether it becomes Extend/Shrink Selection
        // (doc §2.5).
        Key::ArrowUp => Some(Intent::Move {
            direction: Direction::Up,
            granularity: vertical_granularity(modifiers),
            extend,
        }),
        Key::ArrowDown => Some(Intent::Move {
            direction: Direction::Down,
            granularity: vertical_granularity(modifiers),
            extend,
        }),
        // Replaces the extend-to-document capability collision 4 in A2 took
        // away from `⌘⇧↑`/`⌘⇧↓` -- the macOS system binding for the same
        // gesture, not an invented one (doc §1.2). Without `command`, Home/
        // End keep A2's line-granularity meaning unchanged.
        Key::Home if modifiers.command && modifiers.shift => Some(Intent::Move {
            direction: Direction::Up,
            granularity: Granularity::Document,
            extend: true,
        }),
        Key::End if modifiers.command && modifiers.shift => Some(Intent::Move {
            direction: Direction::Down,
            granularity: Granularity::Document,
            extend: true,
        }),
        Key::Home => Some(Intent::Move {
            direction: Direction::Left,
            granularity: Granularity::Line,
            extend,
        }),
        Key::End => Some(Intent::Move {
            direction: Direction::Right,
            granularity: Granularity::Line,
            extend,
        }),
        Key::PageUp => Some(Intent::Move {
            direction: Direction::Up,
            granularity: Granularity::Page,
            extend,
        }),
        Key::PageDown => Some(Intent::Move {
            direction: Direction::Down,
            granularity: Granularity::Page,
            extend,
        }),
        Key::A if modifiers.command => Some(Intent::SelectAll),
        // A4b's chords. `⌘⌥/` uses `mac_cmd` rather than `command` for the
        // same reason A3's occurrence chords do: `Ctrl+Alt+/` is not what
        // Windows/Linux JetBrains binds there (`Ctrl+Shift+/`), so `command
        // && alt` would fire on the wrong chord off macOS (doc §2.5).
        Key::Slash if modifiers.command && !modifiers.alt && !modifiers.shift => {
            Some(Intent::ToggleLineComment)
        }
        Key::Slash if modifiers.mac_cmd && modifiers.alt => Some(Intent::ToggleBlockComment),
        Key::D if modifiers.command && !modifiers.shift && !modifiers.alt => {
            Some(Intent::DuplicateLines)
        }
        Key::J if modifiers.ctrl && !modifiers.command && modifiers.shift => {
            Some(Intent::JoinLines)
        }
        Key::U if modifiers.command && modifiers.shift && !modifiers.alt => {
            Some(Intent::ToggleCase)
        }
        // A3's chords. `command` is Cmd on macOS but *Ctrl* elsewhere, so
        // `ctrl && command` would fire on a plain `Ctrl+G` off macOS, where
        // the JetBrains keymap binds something else entirely; `mac_cmd` is
        // never set off macOS, which is what keeps this phase's macOS-only
        // bindings macOS-only (doc §1.2, §2.2).
        Key::G if modifiers.ctrl && modifiers.mac_cmd => Some(Intent::SelectAllOccurrences),
        Key::G if modifiers.ctrl && !modifiers.command && modifiers.shift => {
            Some(Intent::UnselectOccurrence)
        }
        Key::G if modifiers.ctrl && !modifiers.command && !modifiers.shift => {
            Some(Intent::AddNextOccurrence)
        }
        Key::Num8 if modifiers.mac_cmd && modifiers.shift => Some(Intent::ToggleColumnMode),
        Key::Escape if modifiers.is_none() => Some(Intent::CollapseSelections),
        _ => None,
    }
}

fn vertical_granularity(modifiers: egui::Modifiers) -> Granularity {
    if modifiers.command {
        Granularity::Document
    } else {
        Granularity::Character
    }
}

/// Applies `intent`. The only place in the widget that mutates the buffer,
/// and the only one that calls `Buffer::mark_dirty` -- on a real change,
/// never on access.
pub fn apply_intent(
    buffer: &mut Buffer,
    state: &mut EditorState,
    metrics: &Metrics,
    visual: &VisualLines,
    intent: Intent,
) -> Applied {
    let mut applied = Applied::default();
    // The type-over window is exactly one keystroke wide (doc §3.2), so
    // every intent consumes it and only an auto-close writes it back.
    let auto_closed = std::mem::take(&mut state.auto_closed);
    // Survives exactly one Extend/Shrink Selection run, same shape as
    // `auto_closed` above: cleared by anything else, including an edit or a
    // plain arrow-key move (`line-commands-and-editorconfig.md` §3.4). `Copy`
    // is excluded too: it only reads the selection via `selected_text` below,
    // never edits or moves it, so it is neither of the two clearing triggers
    // the doc names. Mouse clicks/drags don't route through here at all, so
    // `handle_mouse`'s own selection-mutating methods clear it themselves.
    if !matches!(
        intent,
        Intent::ExtendSelection | Intent::ShrinkSelection | Intent::Copy
    ) {
        state.shrink_stack.clear();
    }
    match intent {
        Intent::Insert(text) => {
            state.desired_column = None;
            applied.changed = insert_text(buffer, state, &text, &auto_closed);
        }
        Intent::Newline => {
            state.desired_column = None;
            applied.changed = insert_newline(buffer.text_buffer_mut(), state.indent());
        }
        Intent::Paste(text) => {
            state.desired_column = None;
            buffer.text_buffer_mut().insert_at_selections(&text);
            applied.changed = true;
        }
        Intent::DeleteBackward(granularity) => {
            state.desired_column = None;
            applied.changed = if granularity == Granularity::Character {
                delete_backward_character(buffer.text_buffer_mut(), metrics)
            } else {
                delete(buffer.text_buffer_mut(), metrics, granularity, true)
            };
        }
        Intent::DeleteForward(granularity) => {
            state.desired_column = None;
            applied.changed = delete(buffer.text_buffer_mut(), metrics, granularity, false);
        }
        Intent::Move {
            direction,
            granularity,
            extend,
        } => {
            move_carets(
                buffer.text_buffer_mut(),
                state,
                metrics,
                visual,
                direction,
                granularity,
                extend,
            );
        }
        Intent::SelectAll => {
            state.desired_column = None;
            let len = buffer.text_buffer().len();
            // Reveal, don't avoid, same as `goto_offset` (§2.6): a trailing
            // fold covering the buffer's last line would otherwise leave
            // the selection's head on a hidden line, invisible to
            // `paint_carets` even though the selected range itself (a plain
            // byte range, unaffected by folding) is already correct.
            let ranges = buffer.text_buffer().fold_ranges();
            let last_line = buffer.text_buffer().lines().line_at(len);
            state.reveal_line(&ranges, last_line);
            buffer
                .text_buffer_mut()
                .set_selections(Selections::single(Selection::new(0, len)));
        }
        Intent::AddNextOccurrence => {
            state.desired_column = None;
            add_next_occurrence(buffer.text_buffer_mut());
        }
        Intent::SelectAllOccurrences => {
            state.desired_column = None;
            select_all_occurrences(buffer.text_buffer_mut());
        }
        Intent::UnselectOccurrence => {
            let mut selections = buffer.text_buffer().selections().clone();
            if selections.remove_primary() {
                buffer.text_buffer_mut().set_selections(selections);
            }
        }
        Intent::CollapseSelections => {
            if buffer.text_buffer().selections().is_multiple() {
                let mut selections = buffer.text_buffer().selections().clone();
                selections.collapse_to_primary();
                buffer.text_buffer_mut().set_selections(selections);
            }
        }
        Intent::ToggleColumnMode => {
            state.column_mode = !state.column_mode;
            state.column_anchor = None;
        }
        // Cloning is not movement: it deliberately neither reads nor writes
        // the sticky column, so a later `↓` still aims where the user last
        // put the caret horizontally (doc §3.4).
        Intent::CloneCaret(direction) => clone_carets(buffer.text_buffer_mut(), direction),
        Intent::Indent => {
            state.desired_column = None;
            applied.changed = buffer
                .text_buffer_mut()
                .indent_selection_lines(state.indent());
        }
        Intent::Outdent => {
            state.desired_column = None;
            applied.changed = buffer
                .text_buffer_mut()
                .outdent_selection_lines(state.indent());
        }
        Intent::JumpToMatchingBracket => {
            state.desired_column = None;
            jump_to_match(buffer.text_buffer_mut());
        }
        Intent::DuplicateLines => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().duplicate_selection_lines();
        }
        Intent::DeleteLines => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().delete_selection_lines();
        }
        Intent::JoinLines => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().join_selection_lines();
        }
        Intent::MoveLines(direction) => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().move_selection_lines(direction);
        }
        Intent::MoveStatements(direction) => {
            state.desired_column = None;
            applied.changed = buffer
                .text_buffer_mut()
                .move_selection_statements(direction);
        }
        Intent::ToggleLineComment => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().toggle_line_comment(state.indent());
        }
        Intent::ToggleBlockComment => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().toggle_block_comment();
        }
        Intent::ToggleCase => {
            state.desired_column = None;
            applied.changed = buffer.text_buffer_mut().toggle_selection_case();
        }
        Intent::ExtendSelection => {
            state.desired_column = None;
            extend_selection(buffer.text_buffer_mut(), state);
        }
        Intent::ShrinkSelection => {
            state.desired_column = None;
            shrink_selection(buffer.text_buffer_mut(), state);
        }
        Intent::Copy => applied.copy = selected_text(buffer.text_buffer()),
        Intent::Cut => {
            applied.copy = selected_text(buffer.text_buffer());
            if applied.copy.is_some() {
                state.desired_column = None;
                applied.changed = delete(
                    buffer.text_buffer_mut(),
                    metrics,
                    Granularity::Character,
                    true,
                );
            }
        }
    }
    if applied.changed {
        buffer.mark_dirty();
    }
    applied
}

/// What typing `c` opens, if anything: a bracket from the language's own
/// table, or a quote, which is its own closer (doc §3.2).
fn closer_for(rules: &SyntaxRules, c: char) -> Option<char> {
    rules
        .brackets
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
        .or_else(|| rules.string_quotes.contains(&c).then_some(c))
}

/// Doc §3.2: a pair is opened only before end-of-line, whitespace or a
/// closer. Before an identifier the user is far likelier to be wrapping what
/// follows than opening an empty pair.
fn may_open_pair(text: &str, offset: usize, rules: &SyntaxRules) -> bool {
    match text[offset..].chars().next() {
        None => true,
        Some(next) => {
            next.is_whitespace() || rules.brackets.iter().any(|(_, close)| *close == next)
        }
    }
}

/// Whether `offset` sits in a `String` or `Comment` token -- the extra guard
/// quotes carry, so an apostrophe inside a comment does not become `''`.
/// A binary search into tokens the buffer already maintains.
fn is_quoted_or_commented(buffer: &TextBuffer, offset: usize) -> bool {
    let tokens = buffer.tokens();
    let index = tokens.partition_point(|token| token.range.end <= offset);
    tokens.get(index).is_some_and(|token| {
        token.range.start <= offset && matches!(token.kind, TokenKind::String | TokenKind::Comment)
    })
}

/// Typing one character: type-over, opening a delimiter (surround and/or
/// auto-close, per selection), or -- when neither applies -- A2's plain
/// insertion, which is the only path that still coalesces into one undo
/// step per typed run.
fn insert_text(
    buffer: &mut Buffer,
    state: &mut EditorState,
    text: &str,
    auto_closed: &[usize],
) -> bool {
    let mut chars = text.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        buffer.text_buffer_mut().type_text(text);
        return true;
    };
    let Some(rules) = buffer.text_buffer().syntax() else {
        buffer.text_buffer_mut().type_text(text);
        return true;
    };

    // Unconditional on `c` being an opener: type-over fires for a *closer*
    // the previous keystroke auto-inserted, and a closing character never
    // has a `closer_for` mapping of its own.
    if types_over(buffer.text_buffer(), c, auto_closed) {
        move_past(buffer.text_buffer_mut(), c);
        return false;
    }
    if let Some(close) = closer_for(rules, c) {
        state.auto_closed = open_delimiter(buffer.text_buffer_mut(), c, close, rules);
        return true;
    }
    buffer.text_buffer_mut().type_text(text);
    true
}

/// Doc §3.2's type-over, gated on *every* selection qualifying: a mixed set
/// would have to insert at some carets and skip at others, and a keystroke
/// that half-inserts is worse than one that always inserts.
fn types_over(buffer: &TextBuffer, c: char, auto_closed: &[usize]) -> bool {
    !auto_closed.is_empty()
        && buffer.selections().all().iter().all(|selection| {
            selection.is_empty()
                && auto_closed.contains(&selection.head)
                && buffer.text()[selection.head..].starts_with(c)
        })
}

fn move_past(buffer: &mut TextBuffer, c: char) {
    let moved = buffer
        .selections()
        .all()
        .iter()
        .map(|selection| Selection::caret(selection.head + c.len_utf8()))
        .collect();
    let primary = buffer.selections().primary_index();
    buffer.set_selections(Selections::new(moved, primary));
}

/// What one selection does when `open` is typed over it (doc §3.2, §3.3).
enum Opening {
    /// Non-empty: wraps the selection, which keeps covering its original
    /// text -- `Wrap` rather than a call out to
    /// `ide_core::TextBuffer::surround_selections`, because that call
    /// commits its own transaction and can't be combined with what the
    /// other selections in this same keystroke need (see `open_delimiter`).
    Wrap,
    /// Empty, and the §3.2 guard admits a pair.
    AutoClose,
    /// Empty, and the guard doesn't -- a lone `open`, same as plain typing.
    Bare,
}

/// Typing an opening bracket or quote, across every selection in one
/// transaction: a non-empty selection is wrapped, an empty one becomes an
/// auto-closed pair or a bare `open` depending on the §3.2 guard. One
/// transaction regardless of the mix, so N cursors -- wrapping, closing, or
/// plain -- are still one undo step, and none of them is silently skipped
/// because a sibling cursor took a different path. Returns the offsets of
/// the closers this keystroke auto-inserted -- `EditorState::auto_closed`,
/// empty when nothing was auto-closed.
fn open_delimiter(
    buffer: &mut TextBuffer,
    open: char,
    close: char,
    rules: &SyntaxRules,
) -> Vec<usize> {
    let selections = buffer.selections().all().to_vec();
    let quote = rules.string_quotes.contains(&open);
    let text = buffer.text();
    let outcomes: Vec<Opening> = selections
        .iter()
        .map(|selection| {
            if !selection.is_empty() {
                Opening::Wrap
            } else if may_open_pair(text, selection.head, rules)
                && !(quote && is_quoted_or_commented(buffer, selection.head))
            {
                Opening::AutoClose
            } else {
                Opening::Bare
            }
        })
        .collect();

    let changes = selections
        .iter()
        .zip(&outcomes)
        .map(|(selection, outcome)| {
            let inserted = match outcome {
                Opening::Wrap => format!("{open}{}{close}", &text[selection.range()]),
                Opening::AutoClose => format!("{open}{close}"),
                Opening::Bare => open.to_string(),
            };
            Change::new(selection.range(), inserted)
        })
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        return Vec::new();
    };

    let mut shift = 0isize;
    let mut carets = Vec::with_capacity(selections.len());
    let mut closed = Vec::new();
    for (selection, outcome) in selections.iter().zip(&outcomes) {
        let start = (selection.start() as isize + shift) as usize;
        let original_len = selection.range().len();
        let inserted_len = match outcome {
            Opening::Wrap => open.len_utf8() + original_len + close.len_utf8(),
            Opening::AutoClose => open.len_utf8() + close.len_utf8(),
            Opening::Bare => open.len_utf8(),
        };
        shift += inserted_len as isize - original_len as isize;

        carets.push(match outcome {
            // Mirrors `ide_core::TextBuffer::surround_selections`: the
            // selection still covers the original text, direction
            // preserved, not the delimiters around it.
            Opening::Wrap => {
                let content = start + open.len_utf8()..start + open.len_utf8() + original_len;
                if selection.head < selection.anchor {
                    Selection::new(content.end, content.start)
                } else {
                    Selection::new(content.start, content.end)
                }
            }
            Opening::AutoClose => {
                let caret = start + open.len_utf8();
                closed.push(caret);
                Selection::caret(caret)
            }
            Opening::Bare => Selection::caret(start + open.len_utf8()),
        });
    }
    let primary = buffer.selections().primary_index();
    buffer.apply(transaction);
    buffer.set_selections(Selections::new(carets, primary));
    closed
}

/// `Enter`: one newline per selection, each carrying its own line's
/// indentation, and the `{|}` case carrying a second line for the closer
/// (doc §3.1). One transaction, so one undo step however many carets there
/// are.
fn insert_newline(buffer: &mut TextBuffer, unit: IndentUnit) -> bool {
    let selections = buffer.selections().all().to_vec();
    let rules = buffer.syntax();
    let text = buffer.text();
    let lines = buffer.lines();

    let inserts: Vec<(usize, String)> = selections
        .iter()
        .map(|selection| {
            let at = selection.start();
            let first = newline_indent(text, lines, at, rules, unit);
            if selection.is_empty() && splits_a_pair(text, at, rules) {
                // `None` rather than `rules`: the closer's line wants the
                // *current* line's indent verbatim, which is exactly what
                // this function does with nothing to reason about.
                let closer_line = newline_indent(text, lines, at, None, unit);
                let full = format!("{first}{closer_line}");
                (first.len(), full)
            } else {
                (first.len(), first)
            }
        })
        .collect();

    let changes = selections
        .iter()
        .zip(&inserts)
        .map(|(selection, (_, full))| Change::new(selection.range(), full.clone()))
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        return false;
    };

    let mut shift = 0isize;
    let mut carets = Vec::with_capacity(selections.len());
    for (selection, (first, full)) in selections.iter().zip(&inserts) {
        carets.push(Selection::caret(
            (selection.start() as isize + shift) as usize + first,
        ));
        shift += full.len() as isize - selection.range().len() as isize;
    }
    let primary = buffer.selections().primary_index();
    buffer.apply(transaction);
    buffer.set_selections(Selections::new(carets, primary));
    true
}

/// `Backspace` with `Granularity::Character`: the ordinary one-character (or
/// whole-selection) delete `delete()` already does, except an empty
/// selection sitting between a matching bracket pair deletes both halves
/// (doc §3.2). Per-selection, in one transaction -- a cursor next to a pair
/// and a sibling cursor that isn't both get the outcome each one earns,
/// which is what keeps this consistent with `open_delimiter` rather than
/// repeating type-over's all-or-nothing (a pair-delete has no "half-typed"
/// state to guard against; each cursor's outcome is independent of the
/// others').
fn delete_backward_character(buffer: &mut TextBuffer, metrics: &Metrics) -> bool {
    let rules = buffer.syntax();
    let selections = buffer.selections().all().to_vec();
    let text = buffer.text();
    let ranges: Vec<Range<usize>> = selections
        .iter()
        .map(|selection| {
            if !selection.is_empty() {
                return selection.range();
            }
            let head = selection.head;
            let pair = rules.and_then(|rules| {
                let before = text[..head].chars().next_back()?;
                let after = text[head..].chars().next()?;
                rules
                    .brackets
                    .contains(&(before, after))
                    .then(|| head - before.len_utf8()..head + after.len_utf8())
            });
            pair.unwrap_or_else(|| {
                let other = step(buffer, metrics, head, Granularity::Character, true);
                other.min(head)..other.max(head)
            })
        })
        .collect();
    if ranges.iter().all(Range::is_empty) {
        return false;
    }

    let changes = ranges
        .iter()
        .filter(|range| !range.is_empty())
        .cloned()
        .map(|range| Change::new(range, ""))
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        return false;
    };

    let mut shift = 0isize;
    let mut carets = Vec::with_capacity(selections.len());
    for range in &ranges {
        carets.push(Selection::caret((range.start as isize + shift) as usize));
        if !range.is_empty() {
            shift -= range.len() as isize;
        }
    }
    let primary = buffer.selections().primary_index();
    buffer.apply(transaction);
    buffer.set_selections(Selections::new(carets, primary));
    true
}

/// Moves the primary caret just past the bracket matching the one it
/// touches, collapsing to a single selection (doc §3.4).
fn jump_to_match(buffer: &mut TextBuffer) {
    let head = buffer.selections().primary().head;
    let Some(pair) = buffer.matching_bracket(head) else {
        return;
    };
    // The closer is tested first, mirroring `matching_bracket`'s own
    // after-the-caret-first rule: in `()` the two brackets share an offset.
    let target = if head == pair.close.start || head == pair.close.end {
        pair.open.end
    } else {
        pair.close.end
    };
    buffer.set_selections(Selections::single(Selection::caret(target)));
}

/// `⌥↑` unarmed: grows every selection one rung out via
/// `extended_selection`, pushing the pre-growth `Selections` onto the
/// shrink stack so `⌥↓` can walk back down the same path (doc §3.4).
fn extend_selection(buffer: &mut TextBuffer, state: &mut EditorState) {
    let current = buffer.selections().clone();
    let grown: Vec<Selection> = current
        .all()
        .iter()
        .map(|&selection| buffer.extended_selection(selection).unwrap_or(selection))
        .collect();
    let primary = current.primary_index();
    state.shrink_stack.push(current);
    buffer.set_selections(Selections::new(grown, primary));
}

/// `⌥↓`: pops the shrink stack if it isn't empty, or -- with nothing to pop
/// -- falls back to the word under each caret, then to a bare caret, never
/// to nothing (doc §3.4).
fn shrink_selection(buffer: &mut TextBuffer, state: &mut EditorState) {
    if let Some(previous) = state.shrink_stack.pop() {
        buffer.set_selections(previous);
        return;
    }
    let text = buffer.text().to_string();
    let current = buffer.selections().clone();
    let fallback: Vec<Selection> = current
        .all()
        .iter()
        .map(|&selection| {
            word_at(&text, selection.head)
                .map(|range| Selection::new(range.start, range.end))
                .unwrap_or(Selection::caret(selection.head))
        })
        .collect();
    let primary = current.primary_index();
    buffer.set_selections(Selections::new(fallback, primary));
}

/// The text `⌃⌘G` looks for: the primary selection, or the word under it
/// when it is a bare caret. `None` when the caret is not on a word --
/// including on a number literal, which `word_range_at` rejects.
fn needle_range(buffer: &TextBuffer) -> Option<std::ops::Range<usize>> {
    let primary = buffer.selections().primary();
    if primary.is_empty() {
        word_range_at(buffer.text(), primary.head)
    } else {
        Some(primary.range())
    }
}

fn add_next_occurrence(buffer: &mut TextBuffer) {
    let primary = buffer.selections().primary();
    // The first press on a bare caret selects the word, which is what makes
    // the second press unambiguous -- the needle is now visible (doc §3.2).
    if primary.is_empty() {
        let Some(word) = word_range_at(buffer.text(), primary.head) else {
            return;
        };
        let index = buffer.selections().primary_index();
        let mut ranges = buffer.selections().all().to_vec();
        ranges[index] = Selection::new(word.start, word.end);
        buffer.set_selections(Selections::new(ranges, index));
        return;
    }

    let needle = buffer.text()[primary.range()].to_string();
    let Some(next) = next_occurrence(buffer.text(), &needle, primary.end()) else {
        return;
    };
    let mut selections = buffer.selections().clone();
    // `false` means the occurrence is one the user already has, which is the
    // natural stopping point once every one of them is selected.
    if selections.push_primary(Selection::new(next.start, next.end)) {
        buffer.set_selections(selections);
    }
}

fn select_all_occurrences(buffer: &mut TextBuffer) {
    let Some(needle_range) = needle_range(buffer) else {
        return;
    };
    // The needle's own start, not the caret's: `word_range_at` resolves the
    // identifier to the *left* of a boundary, so a caret sitting at the end
    // of a word is outside the very match it just named.
    let anchor = needle_range.start;
    let needle = buffer.text()[needle_range].to_string();
    let ranges: Vec<Selection> = all_occurrences(buffer.text(), &needle)
        .into_iter()
        .map(|range| Selection::new(range.start, range.end))
        .collect();
    if ranges.is_empty() {
        return;
    }
    // The match the old primary was in stays primary, so the view does not
    // jump to the top of the file.
    let index = ranges
        .iter()
        .position(|s| s.start() <= anchor && anchor < s.end())
        .unwrap_or(0);
    buffer.set_selections(Selections::new(ranges, index));
}

fn clone_carets(buffer: &mut TextBuffer, direction: Direction) {
    let line_count = buffer.lines().line_count();
    let cloned: Vec<Selection> = buffer
        .selections()
        .all()
        .iter()
        .filter_map(|selection| {
            let line = buffer.lines().line_at(selection.head);
            let target = match direction {
                Direction::Up => line.checked_sub(1)?,
                _ => (line + 1 < line_count).then_some(line + 1)?,
            };
            let column = column_of(buffer, selection.head);
            let range = buffer.lines().line_range(target, buffer.text())?;
            let line_text = &buffer.text()[range.clone()];
            Some(Selection::caret(
                range.start + byte_offset_in_line(line_text, column),
            ))
        })
        .collect();

    let mut selections = buffer.selections().clone();
    // `push`, not `push_primary`: cloning adds cursors without moving the
    // one the user is working from. A clone landing on an existing caret is
    // absorbed by normalisation, which is what makes holding the gesture at
    // the file's edge stop cleanly.
    for caret in cloned {
        selections.push(caret);
    }
    buffer.set_selections(selections);
}

fn selected_text(buffer: &TextBuffer) -> Option<String> {
    let text: String = buffer
        .selections()
        .all()
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| &buffer.text()[s.range()])
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Deletes every selection, or -- when they are all bare carets -- one
/// `granularity` step in the given direction from each.
fn delete(
    buffer: &mut TextBuffer,
    metrics: &Metrics,
    granularity: Granularity,
    backward: bool,
) -> bool {
    let ranges: Vec<std::ops::Range<usize>> = buffer
        .selections()
        .all()
        .iter()
        .map(|selection| {
            if !selection.is_empty() {
                return selection.range();
            }
            let head = selection.head;
            let other = step(buffer, metrics, head, granularity, backward);
            other.min(head)..other.max(head)
        })
        .filter(|range| !range.is_empty())
        .collect();
    if ranges.is_empty() {
        return false;
    }
    let changes = ranges
        .into_iter()
        .map(|range| ide_core::Change::new(range, ""))
        .collect();
    let Ok(transaction) = Transaction::new(changes) else {
        return false;
    };
    buffer.apply(transaction);
    true
}

fn move_carets(
    buffer: &mut TextBuffer,
    state: &mut EditorState,
    metrics: &Metrics,
    visual: &VisualLines,
    direction: Direction,
    granularity: Granularity,
    extend: bool,
) {
    let vertical = matches!(direction, Direction::Up | Direction::Down);
    // A horizontal move re-anchors the column a later vertical move aims at;
    // a vertical one keeps aiming at the column the caret last chose.
    let desired = if vertical { state.desired_column } else { None };

    let mut new_desired = None;
    let moved: Vec<Selection> = buffer
        .selections()
        .all()
        .iter()
        .map(|selection| {
            let head = if vertical {
                let (offset, column_x) = vertical_step(
                    buffer,
                    metrics,
                    visual,
                    selection.head,
                    direction,
                    granularity,
                    desired,
                );
                new_desired = Some(column_x);
                offset
            } else {
                let backward = direction == Direction::Left;
                // Collapsing a selection with an un-extended arrow key goes
                // to its edge, not one step past it -- what every editor does.
                let raw = if !extend && !selection.is_empty() {
                    if backward {
                        selection.start()
                    } else {
                        selection.end()
                    }
                } else {
                    step(buffer, metrics, selection.head, granularity, backward)
                };
                redirect_hidden(buffer, visual, raw, backward)
            };
            if extend {
                Selection::new(selection.anchor, head)
            } else {
                Selection::caret(head)
            }
        })
        .collect();

    state.desired_column = new_desired;
    let primary = buffer.selections().primary_index();
    buffer.set_selections(Selections::new(moved, primary));
}

/// If `offset` fell on a buffer line hidden by a collapsed fold, redirects
/// to the nearest visible boundary in the direction of travel -- forward,
/// the start of the row right after the fold that hides it; backward, the
/// end of that fold's own `start_line` text (`code-folding.md` §2.6/§3.4).
/// A no-op when `offset`'s line is already visible.
fn redirect_hidden(
    buffer: &TextBuffer,
    visual: &VisualLines,
    offset: usize,
    backward: bool,
) -> usize {
    let line = buffer.lines().line_at(offset);
    let hiding_row = visual.row_of(line);
    if visual.buffer_line(hiding_row) == line {
        return offset;
    }
    if backward {
        buffer
            .lines()
            .line_range(visual.buffer_line(hiding_row), buffer.text())
            .map_or(offset, |range| range.end)
    } else {
        buffer
            .lines()
            .line_start(visual.buffer_line(hiding_row + 1))
            .unwrap_or(offset)
    }
}

/// One `granularity` step from `offset`, horizontally.
fn step(
    buffer: &TextBuffer,
    metrics: &Metrics,
    offset: usize,
    granularity: Granularity,
    backward: bool,
) -> usize {
    let text = buffer.text();
    match granularity {
        Granularity::Character => {
            if backward {
                prev_boundary(text, offset)
            } else {
                next_boundary(text, offset)
            }
        }
        Granularity::Word => {
            if backward {
                word_start_before(text, offset)
            } else {
                word_end_after(text, offset)
            }
        }
        Granularity::Line => {
            let line = buffer.lines().line_at(offset);
            match buffer.lines().line_range(line, text) {
                Some(range) if backward => range.start,
                Some(range) => range.end,
                None => offset,
            }
        }
        Granularity::Page => {
            let line = buffer.lines().line_at(offset);
            let target = if backward {
                line.saturating_sub(metrics.page_rows)
            } else {
                (line + metrics.page_rows).min(buffer.lines().line_count() - 1)
            };
            buffer.lines().line_start(target).unwrap_or(offset)
        }
        Granularity::Document => {
            if backward {
                0
            } else {
                text.len()
            }
        }
    }
}

/// A vertical step, returning the new offset and the x it aimed at. With a
/// monospace font a column is `char_width` wide, so the "sticky x" of
/// `EditorState::desired_column` is a column count in disguise -- which is
/// what keeps this testable without laying anything out.
fn vertical_step(
    buffer: &TextBuffer,
    metrics: &Metrics,
    visual: &VisualLines,
    offset: usize,
    direction: Direction,
    granularity: Granularity,
    desired: Option<f32>,
) -> (usize, f32) {
    if granularity == Granularity::Document {
        // A separate early return, not row-based like the branch below: `Up`
        // (`0`) is always on line 0, which can never be hidden, but `Down`
        // (`buffer.len()`) sits on the buffer's true last line, which *is*
        // reachable as a hidden interior line when a collapsed fold's
        // `end_line` is that last line (`code-folding.md` §2.6 revision
        // note 6).
        let raw = if direction == Direction::Up {
            0
        } else {
            buffer.len()
        };
        // Always the backward/nearest-visible-before redirect, for both
        // directions: `Up`'s call is inert regardless (line 0 is always
        // visible, so `redirect_hidden` short-circuits before ever
        // consulting this flag), and `Down`'s hidden target -- when it has
        // one at all -- can only be hidden by a *trailing* fold (nothing
        // exists past the buffer's true last line), so the visible
        // position immediately after it never exists; landing at the end
        // of the nearest visible line before it is the only sound target
        // (`code-folding.md` §2.6).
        let offset = redirect_hidden(buffer, visual, raw, true);
        return (offset, column_x(buffer, metrics, offset));
    }

    // Steps the row, not the buffer line: a fold's interior has no row to
    // land on, which is what makes it unreachable by keyboard here without a
    // separate check (§3.4).
    let rows = if granularity == Granularity::Page {
        metrics.page_rows.max(1)
    } else {
        1
    };
    let line = buffer.lines().line_at(offset);
    let row = visual.row_of(line);
    let target_row = match direction {
        Direction::Up => row.saturating_sub(rows),
        _ => (row + rows).min(visual.row_count().saturating_sub(1)),
    };
    let target = visual.buffer_line(target_row);
    let x = desired.unwrap_or_else(|| column_x(buffer, metrics, offset));

    let Some(range) = buffer.lines().line_range(target, buffer.text()) else {
        return (offset, x);
    };
    let line_text = &buffer.text()[range.clone()];
    let column = if metrics.char_width > 0.0 {
        (x / metrics.char_width).round() as usize
    } else {
        0
    };
    (range.start + byte_offset_in_line(line_text, column), x)
}

fn column_x(buffer: &TextBuffer, metrics: &Metrics, offset: usize) -> f32 {
    let line = buffer.lines().line_at(offset);
    let Some(range) = buffer.lines().line_range(line, buffer.text()) else {
        return 0.0;
    };
    let column = char_index_in_line(
        &buffer.text()[range.clone()],
        offset.saturating_sub(range.start),
    );
    column as f32 * metrics.char_width
}

fn prev_boundary(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_boundary(text: &str, offset: usize) -> usize {
    text[offset.min(text.len())..]
        .chars()
        .next()
        .map(|c| offset + c.len_utf8())
        .unwrap_or(text.len())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_start_before(text: &str, offset: usize) -> usize {
    let mut cursor = offset.min(text.len());
    while cursor > 0 {
        let prev = prev_boundary(text, cursor);
        if text[prev..cursor].chars().all(|c| !is_word(c)) {
            cursor = prev;
        } else {
            break;
        }
    }
    while cursor > 0 {
        let prev = prev_boundary(text, cursor);
        if text[prev..cursor].chars().all(is_word) {
            cursor = prev;
        } else {
            break;
        }
    }
    cursor
}

fn word_end_after(text: &str, offset: usize) -> usize {
    let mut cursor = offset.min(text.len());
    while cursor < text.len() {
        let next = next_boundary(text, cursor);
        if text[cursor..next].chars().all(|c| !is_word(c)) {
            cursor = next;
        } else {
            break;
        }
    }
    while cursor < text.len() {
        let next = next_boundary(text, cursor);
        if text[cursor..next].chars().all(is_word) {
            cursor = next;
        } else {
            break;
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Spacing;

    fn metrics() -> Metrics {
        Metrics::new(
            18.0,
            8.0,
            2,
            10,
            false,
            &Spacing {
                xs: 2.0,
                sm: 4.0,
                md: 8.0,
                lg: 12.0,
                xl: 16.0,
            },
        )
    }

    fn with_text(text: &str) -> Buffer {
        let mut buffer = Buffer::untitled();
        buffer.insert(0, text);
        buffer
    }

    fn rust(text: &str) -> Buffer {
        let mut buffer = with_text(text);
        buffer.set_syntax(Some(&ide_core::RUST));
        buffer
    }

    fn caret(buffer: &Buffer) -> usize {
        buffer.text_buffer().selections().primary().head
    }

    fn set_caret(buffer: &mut Buffer, offset: usize) {
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::caret(offset)));
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn go(buffer: &mut Buffer, state: &mut EditorState, intent: Intent) -> Applied {
        let line_count = buffer.text_buffer().lines().line_count();
        let ranges = buffer.text_buffer().fold_ranges();
        let visual = state.visual_lines(line_count, &ranges);
        apply_intent(buffer, state, &metrics(), &visual, intent)
    }

    fn moving(direction: Direction, granularity: Granularity, extend: bool) -> Intent {
        Intent::Move {
            direction,
            granularity,
            extend,
        }
    }

    // ---- intent_for ----

    #[test]
    fn text_events_become_insertions() {
        assert_eq!(
            intent_for(&egui::Event::Text("x".into())),
            Some(Intent::Insert("x".into()))
        );
        assert_eq!(intent_for(&egui::Event::Text("\u{7}".into())), None);
        assert_eq!(intent_for(&egui::Event::Text(String::new())), None);
    }

    #[test]
    fn clipboard_events_are_claimed() {
        assert_eq!(intent_for(&egui::Event::Copy), Some(Intent::Copy));
        assert_eq!(intent_for(&egui::Event::Cut), Some(Intent::Cut));
        assert_eq!(
            intent_for(&egui::Event::Paste("v".into())),
            Some(Intent::Paste("v".into()))
        );
    }

    #[test]
    fn the_binding_table_maps_to_the_documented_intents() {
        let none = egui::Modifiers::NONE;
        let cmd = egui::Modifiers::COMMAND;
        let alt = egui::Modifiers::ALT;
        let shift = egui::Modifiers::SHIFT;

        assert_eq!(
            intent_for(&key(egui::Key::Enter, none)),
            Some(Intent::Newline)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Tab, none)),
            Some(Intent::Insert("\t".into()))
        );
        assert_eq!(
            intent_for(&key(egui::Key::Backspace, none)),
            Some(Intent::DeleteBackward(Granularity::Character))
        );
        assert_eq!(
            intent_for(&key(egui::Key::Backspace, alt)),
            Some(Intent::DeleteBackward(Granularity::Word))
        );
        assert_eq!(
            intent_for(&key(egui::Key::Backspace, cmd)),
            Some(Intent::DeleteBackward(Granularity::Line))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, none)),
            Some(moving(Direction::Left, Granularity::Character, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, shift)),
            Some(moving(Direction::Right, Granularity::Character, true))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, alt)),
            Some(moving(Direction::Left, Granularity::Word, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, cmd)),
            Some(moving(Direction::Right, Granularity::Line, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowUp, cmd)),
            Some(moving(Direction::Up, Granularity::Document, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowDown, none)),
            Some(moving(Direction::Down, Granularity::Character, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::Home, none)),
            Some(moving(Direction::Left, Granularity::Line, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::End, none)),
            Some(moving(Direction::Right, Granularity::Line, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::PageDown, none)),
            Some(moving(Direction::Down, Granularity::Page, false))
        );
        assert_eq!(intent_for(&key(egui::Key::A, cmd)), Some(Intent::SelectAll));
        assert_eq!(
            intent_for(&key(egui::Key::Tab, shift)),
            Some(Intent::Outdent)
        );
    }

    #[test]
    fn only_the_bare_shift_chord_claims_tab_as_outdent() {
        let combos = [
            egui::Modifiers {
                shift: true,
                ctrl: true,
                ..Default::default()
            },
            egui::Modifiers {
                shift: true,
                alt: true,
                ..Default::default()
            },
            egui::Modifiers {
                shift: true,
                command: true,
                ..Default::default()
            },
        ];
        for modifiers in combos {
            assert_eq!(intent_for(&key(egui::Key::Tab, modifiers)), None);
        }
    }

    #[test]
    fn the_a3_chords_map_to_their_intents() {
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let ctrl_shift = egui::Modifiers {
            shift: true,
            ..ctrl
        };
        // On macOS `⌃⌘G` sets ctrl, mac_cmd and command all at once.
        let ctrl_cmd = egui::Modifiers {
            ctrl: true,
            mac_cmd: true,
            command: true,
            ..Default::default()
        };
        let cmd_shift_mac = egui::Modifiers {
            mac_cmd: true,
            command: true,
            shift: true,
            ..Default::default()
        };

        assert_eq!(
            intent_for(&key(egui::Key::G, ctrl)),
            Some(Intent::AddNextOccurrence)
        );
        assert_eq!(
            intent_for(&key(egui::Key::G, ctrl_shift)),
            Some(Intent::UnselectOccurrence)
        );
        assert_eq!(
            intent_for(&key(egui::Key::G, ctrl_cmd)),
            Some(Intent::SelectAllOccurrences)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Num8, cmd_shift_mac)),
            Some(Intent::ToggleColumnMode)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Escape, egui::Modifiers::NONE)),
            Some(Intent::CollapseSelections)
        );
    }

    #[test]
    fn the_near_misses_of_the_a3_chords_stay_unclaimed() {
        // `⌘G` is not `⌃G`.
        assert_eq!(
            intent_for(&key(egui::Key::G, egui::Modifiers::COMMAND)),
            None
        );
        // A plain `Ctrl+G` off macOS sets both `ctrl` and `command`, and the
        // JetBrains keymap binds something else entirely there (doc §1.2).
        let ctrl_g_on_windows = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(intent_for(&key(egui::Key::G, ctrl_g_on_windows)), None);
        // ...and `⌘⇧8` off macOS, where `command` is Ctrl but `mac_cmd` is
        // never set.
        let ctrl_shift_8_on_windows = egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            intent_for(&key(egui::Key::Num8, ctrl_shift_8_on_windows)),
            None
        );
        // Escape with a modifier is not ours.
        assert_eq!(
            intent_for(&key(egui::Key::Escape, egui::Modifiers::SHIFT)),
            None
        );
    }

    #[test]
    fn add_next_occurrence_selects_the_word_then_adds_occurrences() {
        let mut buffer = with_text("count = count + counter");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);

        // First press: the word under the caret.
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 1);
        assert_eq!(&buffer.text()[selections.primary().range()], "count");

        // Second: the next occurrence, which becomes primary.
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 2);
        assert_eq!(selections.primary().range(), 8..13);

        // Third: "count" inside "counter" -- a plain substring match.
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        assert_eq!(buffer.text_buffer().selections().len(), 3);

        // Fourth: everything is already selected, so it is a no-op.
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        assert_eq!(buffer.text_buffer().selections().len(), 3);
    }

    #[test]
    fn add_next_occurrence_on_a_number_is_a_no_op() {
        let mut buffer = with_text("x = 42;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 5);
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        assert!(buffer.text_buffer().selections().primary().is_empty());
    }

    #[test]
    fn select_all_occurrences_selects_every_match_and_keeps_the_primary() {
        let mut buffer = with_text("a bb a bb a");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 5);
        go(&mut buffer, &mut state, Intent::SelectAllOccurrences);
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 3);
        assert_eq!(selections.primary().range(), 5..6);
        assert!(selections
            .all()
            .iter()
            .all(|s| &buffer.text()[s.range()] == "a"));
    }

    #[test]
    fn select_all_occurrences_keeps_the_primary_from_a_caret_at_a_word_end() {
        let mut buffer = with_text("count = count");
        let mut state = EditorState::default();
        // `word_range_at` resolves leftwards, so the caret is outside the
        // match it names -- the primary must still follow the needle.
        set_caret(&mut buffer, 13);
        go(&mut buffer, &mut state, Intent::SelectAllOccurrences);
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 2);
        assert_eq!(selections.primary().range(), 8..13);
    }

    #[test]
    fn unselect_occurrence_removes_the_last_added_and_stops_at_one() {
        let mut buffer = with_text("a a a");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        go(&mut buffer, &mut state, Intent::AddNextOccurrence);
        assert_eq!(buffer.text_buffer().selections().len(), 2);

        go(&mut buffer, &mut state, Intent::UnselectOccurrence);
        assert_eq!(buffer.text_buffer().selections().len(), 1);
        go(&mut buffer, &mut state, Intent::UnselectOccurrence);
        assert_eq!(buffer.text_buffer().selections().len(), 1);
    }

    #[test]
    fn collapse_selections_leaves_the_primary_and_no_ops_at_one() {
        let mut buffer = with_text("a a");
        let mut state = EditorState::default();
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::caret(0), Selection::caret(2)],
            1,
        ));
        go(&mut buffer, &mut state, Intent::CollapseSelections);
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 1);
        assert_eq!(selections.primary(), Selection::caret(2));

        go(&mut buffer, &mut state, Intent::CollapseSelections);
        assert_eq!(buffer.text_buffer().selections().len(), 1);
    }

    #[test]
    fn toggle_column_mode_flips_and_drops_the_anchor() {
        let mut buffer = with_text("abc");
        let mut state = EditorState {
            column_anchor: Some((1, 2)),
            ..Default::default()
        };
        go(&mut buffer, &mut state, Intent::ToggleColumnMode);
        assert!(state.column_mode);
        assert_eq!(state.column_anchor, None);
        go(&mut buffer, &mut state, Intent::ToggleColumnMode);
        assert!(!state.column_mode);
    }

    #[test]
    fn clone_caret_adds_a_cursor_on_the_next_line_at_the_same_column() {
        let mut buffer = with_text("alpha\nbravo\ncharlie");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 3);

        go(&mut buffer, &mut state, Intent::CloneCaret(Direction::Down));
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.len(), 2);
        assert_eq!(selections.all()[1].head, 9);

        // Both carets clone again, and the sticky column stays untouched.
        go(&mut buffer, &mut state, Intent::CloneCaret(Direction::Down));
        assert_eq!(buffer.text_buffer().selections().len(), 3);
        assert!(state.desired_column.is_none());
    }

    #[test]
    fn clone_caret_clamps_to_a_short_line_and_stops_at_the_edges() {
        let mut buffer = with_text("longest line\nab");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 8);
        go(&mut buffer, &mut state, Intent::CloneCaret(Direction::Down));
        // Column 8 does not exist on "ab": clamped to its end.
        assert_eq!(buffer.text_buffer().selections().all()[1].head, 15);

        // Nothing above the first line, so the clone is simply absorbed.
        let mut buffer = with_text("only");
        set_caret(&mut buffer, 2);
        go(&mut buffer, &mut state, Intent::CloneCaret(Direction::Up));
        assert_eq!(buffer.text_buffer().selections().len(), 1);
    }

    #[test]
    fn the_apps_own_shortcuts_are_left_unclaimed() {
        let cmd = egui::Modifiers::COMMAND;
        assert_eq!(intent_for(&key(egui::Key::S, cmd)), None);
        assert_eq!(
            intent_for(&key(egui::Key::F, cmd | egui::Modifiers::SHIFT)),
            None
        );
        assert_eq!(intent_for(&key(egui::Key::B, cmd)), None);
        // Undo/redo stay with the app's own shortcut handler, so they work
        // before the editor has ever been focused (doc §3.5, parity item 6).
        assert_eq!(intent_for(&key(egui::Key::Z, cmd)), None);
        assert_eq!(
            intent_for(&key(egui::Key::Z, cmd | egui::Modifiers::SHIFT)),
            None
        );
        assert_eq!(intent_for(&key(egui::Key::F7, egui::Modifiers::ALT)), None);
        assert_eq!(
            intent_for(&egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: cmd,
            }),
            None
        );
    }

    /// `docs/features/goto-definition.md` §3.5: `⌘⌥←`/`⌘⌥→` (Back/Forward
    /// at the app level) must be left entirely unclaimed by the editor, in
    /// every `shift` combination -- but every other `⌘`/`⌥` arrow-key
    /// combination the editor already handles must be completely
    /// unaffected by that fix.
    #[test]
    fn cmd_option_arrow_keys_are_left_unclaimed_for_back_forward() {
        let cmd_alt = egui::Modifiers::COMMAND | egui::Modifiers::ALT;
        let cmd_alt_shift = cmd_alt | egui::Modifiers::SHIFT;
        assert_eq!(intent_for(&key(egui::Key::ArrowLeft, cmd_alt)), None);
        assert_eq!(intent_for(&key(egui::Key::ArrowRight, cmd_alt)), None);
        assert_eq!(intent_for(&key(egui::Key::ArrowLeft, cmd_alt_shift)), None);
        assert_eq!(intent_for(&key(egui::Key::ArrowRight, cmd_alt_shift)), None);
    }

    #[test]
    fn plain_cmd_and_alt_arrow_keys_are_unchanged_by_the_collision_fix() {
        let cmd = egui::Modifiers::COMMAND;
        let alt = egui::Modifiers::ALT;
        let cmd_shift = cmd | egui::Modifiers::SHIFT;
        let alt_shift = alt | egui::Modifiers::SHIFT;

        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, cmd)),
            Some(moving(Direction::Left, Granularity::Line, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, cmd)),
            Some(moving(Direction::Right, Granularity::Line, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, alt)),
            Some(moving(Direction::Left, Granularity::Word, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, alt)),
            Some(moving(Direction::Right, Granularity::Word, false))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, cmd_shift)),
            Some(moving(Direction::Left, Granularity::Line, true))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, cmd_shift)),
            Some(moving(Direction::Right, Granularity::Line, true))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowLeft, alt_shift)),
            Some(moving(Direction::Left, Granularity::Word, true))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowRight, alt_shift)),
            Some(moving(Direction::Right, Granularity::Word, true))
        );
    }

    // ---- apply_intent ----

    #[test]
    fn typing_marks_dirty_and_coalesces_into_one_undo_step() {
        let mut buffer = Buffer::untitled();
        let mut state = EditorState::default();
        for ch in ["a", "b", "c"] {
            assert!(go(&mut buffer, &mut state, Intent::Insert(ch.into())).changed);
        }
        assert_eq!(buffer.text(), "abc");
        assert!(buffer.is_dirty());

        assert!(buffer.text_buffer_mut().undo());
        assert_eq!(buffer.text(), "");
    }

    #[test]
    fn a_delete_between_typed_runs_is_its_own_undo_step() {
        let mut buffer = Buffer::untitled();
        let mut state = EditorState::default();
        go(&mut buffer, &mut state, Intent::Insert("ab".into()));
        go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Character),
        );
        go(&mut buffer, &mut state, Intent::Insert("c".into()));
        assert_eq!(buffer.text(), "ac");

        assert!(buffer.text_buffer_mut().undo());
        assert_eq!(buffer.text(), "a");
        assert!(buffer.text_buffer_mut().undo());
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn delete_removes_a_selection_rather_than_a_character() {
        let mut buffer = with_text("hello world");
        let mut state = EditorState::default();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 6)));
        assert!(
            go(
                &mut buffer,
                &mut state,
                Intent::DeleteBackward(Granularity::Character)
            )
            .changed
        );
        assert_eq!(buffer.text(), "world");
    }

    #[test]
    fn delete_at_the_edges_is_a_no_op() {
        let mut buffer = with_text("ab");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        assert!(
            !go(
                &mut buffer,
                &mut state,
                Intent::DeleteBackward(Granularity::Character)
            )
            .changed
        );
        set_caret(&mut buffer, 2);
        assert!(
            !go(
                &mut buffer,
                &mut state,
                Intent::DeleteForward(Granularity::Character)
            )
            .changed
        );
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn word_and_line_deletion_span_the_right_range() {
        let mut buffer = with_text("let value = 1;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 9);
        go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Word),
        );
        assert_eq!(buffer.text(), "let  = 1;");

        let mut buffer = with_text("first\nsecond");
        set_caret(&mut buffer, 9);
        go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Line),
        );
        assert_eq!(buffer.text(), "first\nond");
    }

    #[test]
    fn forward_delete_removes_the_newline_joining_two_lines() {
        let mut buffer = with_text("a\nb");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        go(
            &mut buffer,
            &mut state,
            Intent::DeleteForward(Granularity::Character),
        );
        assert_eq!(buffer.text(), "ab");
    }

    #[test]
    fn horizontal_movement_walks_characters_words_and_lines() {
        let mut buffer = with_text("one two\nthree");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 1);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Word, false),
        );
        assert_eq!(caret(&buffer), 3);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Line, false),
        );
        assert_eq!(caret(&buffer), 7);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Left, Granularity::Line, false),
        );
        assert_eq!(caret(&buffer), 0);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Document, false),
        );
        assert_eq!(caret(&buffer), buffer.text().len());
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Left, Granularity::Document, false),
        );
        assert_eq!(caret(&buffer), 0);
    }

    #[test]
    fn movement_over_a_multibyte_character_stays_on_boundaries() {
        let mut buffer = with_text("a\u{1F600}b");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 5);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Left, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 1);
    }

    #[test]
    fn shift_extends_and_a_bare_arrow_collapses_to_the_edge() {
        let mut buffer = with_text("abcdef");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 2);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Character, true),
        );
        let selection = buffer.text_buffer().selections().primary();
        assert_eq!((selection.anchor, selection.head), (2, 3));

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Left, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 2);
        assert!(buffer.text_buffer().selections().primary().is_empty());
    }

    #[test]
    fn the_sticky_column_survives_a_short_line() {
        let mut buffer = with_text("longest line\nx\nanother long line");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 8);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 14, "clamped to the short line's end");
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Character, false),
        );
        assert_eq!(caret(&buffer), 23, "back out to the original column");
    }

    #[test]
    fn a_horizontal_move_resets_the_sticky_column() {
        let mut buffer = with_text("longest line\nx\nanother long line");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 8);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Character, false),
        );
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Character, false),
        );
        assert!(state.desired_column.is_none());
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Character, false),
        );
        assert_eq!(
            caret(&buffer),
            15,
            "column 0 of the third line, not column 8"
        );
    }

    #[test]
    fn page_movement_jumps_by_the_viewport_height() {
        let text = (0..40)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut buffer = with_text(&text);
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Page, false),
        );
        let line = buffer.text_buffer().lines().line_at(caret(&buffer));
        assert_eq!(line, 10);
        go(
            &mut buffer,
            &mut state,
            moving(Direction::Up, Granularity::Page, false),
        );
        assert_eq!(caret(&buffer), 0);
    }

    #[test]
    fn select_all_spans_the_buffer() {
        let mut buffer = with_text("abc");
        let mut state = EditorState::default();
        go(&mut buffer, &mut state, Intent::SelectAll);
        assert_eq!(buffer.text_buffer().selections().primary().range(), 0..3);
    }

    #[test]
    fn select_all_reveals_a_trailing_fold_its_selection_would_otherwise_hide() {
        let mut buffer = rust("fn f() {\n    a();\n    b();\n}");
        let mut state = EditorState::default();
        state.collapse_all(&buffer.text_buffer().fold_ranges());
        assert!(state.is_folded(0));

        go(&mut buffer, &mut state, Intent::SelectAll);

        assert!(!state.is_folded(0));
        assert_eq!(
            buffer.text_buffer().selections().primary().range(),
            0..buffer.text().len()
        );
    }

    #[test]
    fn horizontal_step_forward_skips_a_collapsed_folds_interior() {
        let mut buffer = rust("fn f() {\n    a();\n    b();\n}\nfn g() {}\n");
        let mut state = EditorState::default();
        state.toggle_fold(0);
        let end_of_line0 = buffer
            .text_buffer()
            .lines()
            .line_range(0, buffer.text())
            .unwrap()
            .end;
        set_caret(&mut buffer, end_of_line0);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Right, Granularity::Character, false),
        );

        let line = buffer.text_buffer().lines().line_at(caret(&buffer));
        assert_eq!(line, 4);
        assert_eq!(
            caret(&buffer),
            buffer.text_buffer().lines().line_start(4).unwrap()
        );
    }

    #[test]
    fn horizontal_step_backward_lands_at_the_end_of_a_collapsed_folds_start_line() {
        let mut buffer = rust("fn f() {\n    a();\n    b();\n}\nfn g() {}\n");
        let mut state = EditorState::default();
        state.toggle_fold(0);
        let line4_start = buffer.text_buffer().lines().line_start(4).unwrap();
        set_caret(&mut buffer, line4_start);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Left, Granularity::Character, false),
        );

        let line = buffer.text_buffer().lines().line_at(caret(&buffer));
        assert_eq!(line, 0);
        let end_of_line0 = buffer
            .text_buffer()
            .lines()
            .line_range(0, buffer.text())
            .unwrap()
            .end;
        assert_eq!(caret(&buffer), end_of_line0);
    }

    #[test]
    fn vertical_line_step_skips_a_collapsed_folds_interior_rows() {
        let mut buffer = rust("fn f() {\n    a();\n    b();\n}\nfn g() {}\n");
        let mut state = EditorState::default();
        state.toggle_fold(0);
        set_caret(&mut buffer, 0);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Line, false),
        );

        let line = buffer.text_buffer().lines().line_at(caret(&buffer));
        assert_eq!(line, 4);
    }

    #[test]
    fn document_down_into_a_trailing_collapsed_fold_lands_at_its_start_lines_end() {
        let mut buffer = rust("fn f() {\n    a();\n    b();\n}");
        let mut state = EditorState::default();
        state.collapse_all(&buffer.text_buffer().fold_ranges());
        set_caret(&mut buffer, 0);

        go(
            &mut buffer,
            &mut state,
            moving(Direction::Down, Granularity::Document, false),
        );

        let line = buffer.text_buffer().lines().line_at(caret(&buffer));
        assert_eq!(line, 0);
        let end_of_line0 = buffer
            .text_buffer()
            .lines()
            .line_range(0, buffer.text())
            .unwrap()
            .end;
        assert_eq!(caret(&buffer), end_of_line0);
    }

    #[test]
    fn copy_returns_the_selection_and_cut_also_removes_it() {
        let mut buffer = with_text("hello world");
        let mut state = EditorState::default();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 5)));

        let applied = go(&mut buffer, &mut state, Intent::Copy);
        assert_eq!(applied.copy.as_deref(), Some("hello"));
        assert!(!applied.changed);
        assert_eq!(buffer.text(), "hello world");

        let applied = go(&mut buffer, &mut state, Intent::Cut);
        assert_eq!(applied.copy.as_deref(), Some("hello"));
        assert!(applied.changed);
        assert_eq!(buffer.text(), " world");
    }

    #[test]
    fn copy_with_no_selection_yields_nothing() {
        let mut buffer = with_text("abc");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        let applied = go(&mut buffer, &mut state, Intent::Copy);
        assert_eq!(applied.copy, None);
        let applied = go(&mut buffer, &mut state, Intent::Cut);
        assert!(!applied.changed);
        assert_eq!(buffer.text(), "abc");
    }

    #[test]
    fn paste_replaces_every_selection() {
        let mut buffer = with_text("aaa bbb");
        let mut state = EditorState::default();
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::new(0, 3), Selection::new(4, 7)],
            0,
        ));
        go(&mut buffer, &mut state, Intent::Paste("x".into()));
        assert_eq!(buffer.text(), "x x");
    }

    #[test]
    fn multi_cursor_typing_edits_every_caret_in_one_step() {
        let mut buffer = with_text("one\ntwo\nthree");
        let mut state = EditorState::default();
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![
                Selection::caret(0),
                Selection::caret(4),
                Selection::caret(8),
            ],
            0,
        ));
        go(&mut buffer, &mut state, Intent::Insert("// ".into()));
        assert_eq!(buffer.text(), "// one\n// two\n// three");
        assert!(buffer.text_buffer_mut().undo());
        assert_eq!(buffer.text(), "one\ntwo\nthree");
    }

    #[test]
    fn auto_close_fires_before_eol_whitespace_or_a_closer() {
        let mut buffer = rust("");
        let mut state = EditorState::default();
        go(&mut buffer, &mut state, Intent::Insert("(".into()));
        assert_eq!(buffer.text(), "()");
        assert_eq!(caret(&buffer), 1);

        let mut buffer = rust(" x");
        set_caret(&mut buffer, 0);
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert("(".into()),
        );
        assert_eq!(buffer.text(), "() x");

        let mut buffer = rust(")");
        set_caret(&mut buffer, 0);
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert("(".into()),
        );
        assert_eq!(buffer.text(), "())");
    }

    #[test]
    fn auto_close_does_not_fire_before_an_identifier() {
        let mut buffer = rust("x");
        set_caret(&mut buffer, 0);
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert("(".into()),
        );
        assert_eq!(buffer.text(), "(x");
        assert_eq!(caret(&buffer), 1);
    }

    #[test]
    fn a_quote_does_not_auto_close_inside_a_string_or_a_comment() {
        let mut buffer = rust(r#""s" x"#);
        // Offset 1 is inside the string token `"s"`.
        set_caret(&mut buffer, 1);
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert("\"".into()),
        );
        assert_eq!(buffer.text(), r#"""s" x"#);

        let mut buffer = rust("// x");
        set_caret(&mut buffer, 3);
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert("\"".into()),
        );
        assert_eq!(buffer.text(), "// \"x");
    }

    #[test]
    fn type_over_skips_a_closer_the_previous_keystroke_inserted() {
        let mut buffer = rust("");
        let mut state = EditorState::default();
        go(&mut buffer, &mut state, Intent::Insert("(".into()));
        assert_eq!(buffer.text(), "()");

        let applied = go(&mut buffer, &mut state, Intent::Insert(")".into()));
        assert_eq!(buffer.text(), "()", "no new character should be inserted");
        assert!(!applied.changed);
        assert_eq!(caret(&buffer), 2);
    }

    #[test]
    fn type_over_does_not_skip_a_closer_the_user_typed_earlier() {
        let mut buffer = rust("()");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        // A fresh `EditorState` has nothing in `auto_closed`, which is what
        // this closer being "the user's own" means operationally.
        let applied = go(&mut buffer, &mut state, Intent::Insert(")".into()));
        assert_eq!(buffer.text(), "())");
        assert!(applied.changed);
    }

    #[test]
    fn paired_backspace_deletes_both_halves_in_one_transaction() {
        let mut buffer = rust("(x())y");
        set_caret(&mut buffer, 3);
        let mut state = EditorState::default();
        let applied = go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Character),
        );
        assert!(applied.changed);
        assert_eq!(buffer.text(), "(x)y");
        assert!(buffer.text_buffer_mut().undo());
        assert_eq!(buffer.text(), "(x())y");
    }

    #[test]
    fn backspace_with_no_pair_removes_one_character_as_before() {
        let mut buffer = rust("ab");
        set_caret(&mut buffer, 1);
        let mut state = EditorState::default();
        go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Character),
        );
        assert_eq!(buffer.text(), "b");
    }

    #[test]
    fn paired_backspace_is_independent_per_cursor_in_a_mixed_set() {
        // One caret sits between a pair, the other doesn't -- each must get
        // its own correct outcome in the same keystroke, not the whole
        // batch falling back to a plain single-character delete.
        let mut buffer = rust("() y");
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::caret(1), Selection::caret(4)],
            0,
        ));
        let mut state = EditorState::default();
        let applied = go(
            &mut buffer,
            &mut state,
            Intent::DeleteBackward(Granularity::Character),
        );
        assert!(applied.changed);
        assert_eq!(buffer.text(), " ");
        let selections = buffer.text_buffer().selections();
        assert_eq!(selections.all(), [Selection::caret(0), Selection::caret(1)]);
    }

    #[test]
    fn typing_an_opening_delimiter_with_a_selection_surrounds_it() {
        let mut buffer = rust("alpha bravo");
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::new(0, 5), Selection::new(6, 11)],
            0,
        ));
        let mut state = EditorState::default();
        let applied = go(&mut buffer, &mut state, Intent::Insert("\"".into()));
        assert!(applied.changed);
        assert_eq!(buffer.text(), r#""alpha" "bravo""#);
    }

    #[test]
    fn opening_a_delimiter_with_a_mixed_selection_set_handles_every_cursor() {
        // One cursor is a selection (gets wrapped), the other a bare caret
        // right before an identifier (gets a lone opener, per §3.2) -- both
        // must happen in this same keystroke, not just the selection.
        let mut buffer = rust("word x");
        buffer.text_buffer_mut().set_selections(Selections::new(
            vec![Selection::new(0, 4), Selection::caret(5)],
            0,
        ));
        let mut state = EditorState::default();
        let applied = go(&mut buffer, &mut state, Intent::Insert("(".into()));
        assert!(applied.changed);
        assert_eq!(buffer.text(), "(word) (x");
        let selections = buffer.text_buffer().selections();
        assert_eq!(
            selections.all(),
            [Selection::new(1, 5), Selection::caret(8)]
        );
    }

    #[test]
    fn a_closing_delimiter_with_a_selection_still_replaces_it() {
        let mut buffer = rust("alpha");
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 5)));
        go(
            &mut buffer,
            &mut EditorState::default(),
            Intent::Insert(")".into()),
        );
        assert_eq!(buffer.text(), ")");
    }

    #[test]
    fn indent_and_outdent_intents_delegate_to_the_buffer_operation() {
        let mut buffer = with_text("a\nb\n");
        let mut state = EditorState::default();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 3)));
        assert!(go(&mut buffer, &mut state, Intent::Indent).changed);
        assert_eq!(buffer.text(), "    a\n    b\n");
        assert!(go(&mut buffer, &mut state, Intent::Outdent).changed);
        assert_eq!(buffer.text(), "a\nb\n");
    }

    #[test]
    fn jump_to_matching_bracket_collapses_just_past_the_match() {
        let mut buffer = rust("f(x)");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        go(&mut buffer, &mut state, Intent::JumpToMatchingBracket);
        assert_eq!(caret(&buffer), 4);
        assert!(!buffer.text_buffer().selections().is_multiple());

        set_caret(&mut buffer, 4);
        go(&mut buffer, &mut state, Intent::JumpToMatchingBracket);
        assert_eq!(caret(&buffer), 2);
    }

    #[test]
    fn jump_to_matching_bracket_with_no_match_is_a_no_op() {
        let mut buffer = with_text("abc");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 1);
        go(&mut buffer, &mut state, Intent::JumpToMatchingBracket);
        assert_eq!(caret(&buffer), 1);
    }

    // ---- A4b: line commands, comments, case, extend/shrink selection ----

    #[test]
    fn the_a4b_binding_table_maps_to_the_documented_intents() {
        let cmd = egui::Modifiers::COMMAND;
        let mac_cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..Default::default()
        };
        let mac_cmd_shift = egui::Modifiers {
            mac_cmd: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        let alt_shift = egui::Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        let ctrl_shift = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        let mac_cmd_alt = egui::Modifiers {
            mac_cmd: true,
            command: true,
            alt: true,
            ..Default::default()
        };
        let cmd_shift = egui::Modifiers {
            command: true,
            shift: true,
            ..Default::default()
        };

        assert_eq!(
            intent_for(&key(egui::Key::D, cmd)),
            Some(Intent::DuplicateLines)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Backspace, mac_cmd)),
            Some(Intent::DeleteLines)
        );
        assert_eq!(
            intent_for(&key(egui::Key::J, ctrl_shift)),
            Some(Intent::JoinLines)
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowUp, alt_shift)),
            Some(Intent::MoveLines(LineDirection::Up))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowDown, alt_shift)),
            Some(Intent::MoveLines(LineDirection::Down))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowUp, mac_cmd_shift)),
            Some(Intent::MoveStatements(LineDirection::Up))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowDown, mac_cmd_shift)),
            Some(Intent::MoveStatements(LineDirection::Down))
        );
        assert_eq!(
            intent_for(&key(egui::Key::Slash, cmd)),
            Some(Intent::ToggleLineComment)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Slash, mac_cmd_alt)),
            Some(Intent::ToggleBlockComment)
        );
        assert_eq!(
            intent_for(&key(egui::Key::U, cmd_shift)),
            Some(Intent::ToggleCase)
        );
        assert_eq!(
            intent_for(&key(egui::Key::Home, cmd_shift)),
            Some(moving(Direction::Up, Granularity::Document, true))
        );
        assert_eq!(
            intent_for(&key(egui::Key::End, cmd_shift)),
            Some(moving(Direction::Down, Granularity::Document, true))
        );
    }

    #[test]
    fn documented_near_misses_never_fire_the_new_a4b_intent_off_macos() {
        // Off macOS, `command` mirrors `ctrl` but `mac_cmd` stays false --
        // the modifiers an off-mac Ctrl press actually carries (doc §6 item
        // 13). None of the three `mac_cmd`-gated predicates fire, so each
        // falls through to whatever the chord already meant rather than
        // inventing the Windows/Linux binding (doc §1.2, §2.7 revision note
        // 4) -- `⌘⌥/`'s off-mac equivalent really is unclaimed, but `⌘⌫` and
        // `⌘⇧↑` fall back to A2's pre-existing intents, not to a bare `None`.
        let off_mac = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let off_mac_shift = egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..Default::default()
        };
        let off_mac_alt = egui::Modifiers {
            ctrl: true,
            command: true,
            alt: true,
            ..Default::default()
        };

        assert_eq!(
            intent_for(&key(egui::Key::Backspace, off_mac)),
            Some(Intent::DeleteBackward(Granularity::Line))
        );
        assert_eq!(
            intent_for(&key(egui::Key::ArrowUp, off_mac_shift)),
            Some(moving(Direction::Up, Granularity::Document, true))
        );
        // `Ctrl+Alt+/` is not a JetBrains Windows/Linux binding at all
        // (that keymap uses `Ctrl+Shift+/`), so this one is unclaimed.
        assert_eq!(intent_for(&key(egui::Key::Slash, off_mac_alt)), None);
    }

    #[test]
    fn duplicate_lines_delegates_to_the_buffer_operation() {
        let mut buffer = rust("abc\ndef\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        assert!(go(&mut buffer, &mut state, Intent::DuplicateLines).changed);
        assert_eq!(buffer.text(), "abc\nabc\ndef\n");
    }

    #[test]
    fn delete_lines_delegates_to_the_buffer_operation() {
        let mut buffer = rust("a\nb\nc\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 2);
        assert!(go(&mut buffer, &mut state, Intent::DeleteLines).changed);
        assert_eq!(buffer.text(), "a\nc\n");
    }

    #[test]
    fn join_lines_delegates_to_the_buffer_operation() {
        let mut buffer = rust("abc\ndef\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        assert!(go(&mut buffer, &mut state, Intent::JoinLines).changed);
        assert_eq!(buffer.text(), "abc def\n");
    }

    #[test]
    fn move_lines_delegates_to_the_buffer_operation() {
        let mut buffer = rust("a\nb\nc\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        assert!(
            go(
                &mut buffer,
                &mut state,
                Intent::MoveLines(LineDirection::Down)
            )
            .changed
        );
        assert_eq!(buffer.text(), "b\na\nc\n");
    }

    #[test]
    fn move_statements_delegates_to_the_buffer_operation() {
        let mut buffer = with_text("a\nb\nc\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 2);
        assert!(
            go(
                &mut buffer,
                &mut state,
                Intent::MoveStatements(LineDirection::Up)
            )
            .changed
        );
        assert_eq!(buffer.text(), "b\na\nc\n");
    }

    #[test]
    fn toggle_line_comment_round_trips() {
        let mut buffer = rust("let a = 1;\n");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 0);
        assert!(go(&mut buffer, &mut state, Intent::ToggleLineComment).changed);
        assert_eq!(buffer.text(), "// let a = 1;\n");
        assert!(go(&mut buffer, &mut state, Intent::ToggleLineComment).changed);
        assert_eq!(buffer.text(), "let a = 1;\n");
    }

    #[test]
    fn toggle_block_comment_round_trips() {
        let mut buffer = rust("x");
        let mut state = EditorState::default();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 1)));
        assert!(go(&mut buffer, &mut state, Intent::ToggleBlockComment).changed);
        assert_eq!(buffer.text(), "/*x*/");
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 5)));
        assert!(go(&mut buffer, &mut state, Intent::ToggleBlockComment).changed);
        assert_eq!(buffer.text(), "x");
    }

    #[test]
    fn toggle_case_delegates_to_the_buffer_operation() {
        let mut buffer = rust("abc");
        let mut state = EditorState::default();
        buffer
            .text_buffer_mut()
            .set_selections(Selections::single(Selection::new(0, 3)));
        assert!(go(&mut buffer, &mut state, Intent::ToggleCase).changed);
        assert_eq!(buffer.text(), "ABC");
    }

    #[test]
    fn extend_selection_grows_and_pushes_onto_the_shrink_stack() {
        let mut buffer = rust("let a = 1;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 4); // inside "a"
        go(&mut buffer, &mut state, Intent::ExtendSelection);
        assert_eq!(
            buffer.text_buffer().selections().primary(),
            Selection::new(4, 5)
        );
        assert_eq!(state.shrink_stack.len(), 1);
    }

    #[test]
    fn shrink_selection_pops_the_stack_back_to_the_pre_growth_selection() {
        let mut buffer = rust("let a = 1;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 4);
        go(&mut buffer, &mut state, Intent::ExtendSelection);
        go(&mut buffer, &mut state, Intent::ShrinkSelection);
        assert_eq!(caret(&buffer), 4);
        assert!(state.shrink_stack.is_empty());
    }

    #[test]
    fn shrink_selection_with_an_empty_stack_falls_back_to_the_word_then_the_caret() {
        let mut buffer = with_text("abc  def");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 6); // inside "def"
        go(&mut buffer, &mut state, Intent::ShrinkSelection);
        assert_eq!(
            buffer.text_buffer().selections().primary(),
            Selection::new(5, 8)
        );

        set_caret(&mut buffer, 4); // the gap between the two words
        go(&mut buffer, &mut state, Intent::ShrinkSelection);
        assert_eq!(caret(&buffer), 4);
    }

    #[test]
    fn any_intent_other_than_extend_or_shrink_clears_the_shrink_stack() {
        let mut buffer = rust("let a = 1;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 4);
        go(&mut buffer, &mut state, Intent::ExtendSelection);
        assert_eq!(state.shrink_stack.len(), 1);
        go(&mut buffer, &mut state, Intent::Insert("x".into()));
        assert!(state.shrink_stack.is_empty());
    }

    #[test]
    fn copying_does_not_clear_the_shrink_stack() {
        let mut buffer = rust("let a = 1;");
        let mut state = EditorState::default();
        set_caret(&mut buffer, 4);
        go(&mut buffer, &mut state, Intent::ExtendSelection);
        assert_eq!(state.shrink_stack.len(), 1);

        let applied = go(&mut buffer, &mut state, Intent::Copy);
        assert_eq!(applied.copy.as_deref(), Some("a"));
        assert_eq!(state.shrink_stack.len(), 1);

        go(&mut buffer, &mut state, Intent::ShrinkSelection);
        assert_eq!(caret(&buffer), 4);
        assert!(state.shrink_stack.is_empty());
    }
}
