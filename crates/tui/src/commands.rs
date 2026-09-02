//! `T1`'s minimal command registry (`docs/features/tui-shell-and-editor.md`
//! §2.4), extended by `T2` (`docs/features/tui-multi-buffer-tabs.md` §2.2)
//! with `NextTab`/`PreviousTab`/`CloseTab`, by `T4`'s find bar
//! (`docs/features/tui-find.md` §2.3) with `Find`, by `T5`'s replace
//! extension (`docs/features/tui-replace.md` §2.3) with `Replace`, and by
//! `docs/features/tui-goto-and-usages.md` §2.3 with `GoToDeclaration`/
//! `FindUsages`/`ToggleNotifications`, by `docs/features/tui-problems.md`
//! (`T9`) with `ToggleProblems`, by `docs/features/tui-cargo-panel.md`
//! (`T10`) with `ToggleCargoPanel`, by `docs/features/
//! tui-hover-and-inlay-hints.md` (`T12`) with `QuickDocumentation`, by
//! `docs/features/tui-find-in-path.md` (`T15`) with `FindInPath`, by
//! `docs/features/tui-code-actions-and-rename.md` (`T13`) with
//! `ShowIntentionActions`/`Rename`, by `docs/features/
//! tui-git-panel.md` (`T11`) with `ToggleGitPanel`, by `docs/features/
//! tui-smart-editing.md` (`T18a`) with `JumpToMatchingBracket`, and by
//! `docs/features/tui-line-commands-and-editorconfig.md` (`T18b`) with
//! `DuplicateLines`/`DeleteLines`/`JoinLines`/`MoveLinesUp`/
//! `MoveLinesDown`/`MoveStatementsUp`/`MoveStatementsDown`/
//! `ToggleLineComment`/`ToggleBlockComment`/`ExtendSelection`/
//! `ShrinkSelection`/`ToggleCase`. Every binding here is the
//! `Ctrl`-translated form of an id `ide-ui`'s own
//! `crates/ui/src/command.rs` registry already assigns (`Exit`,
//! `ToggleNotifications`, `ToggleCargoPanel`, `ToggleGitPanel`, and
//! `JumpToMatchingBracket` are the five exceptions -- no safe translation
//! exists for any of them, so all five are registered with no default
//! binding, reachable only via the palette;
//! `ToggleCargoPanel`'s own reasoning is in `tui-cargo-panel.md` §2.3,
//! `ToggleGitPanel`'s in `tui-git-panel.md` §1/§2.3,
//! `JumpToMatchingBracket`'s in `tui-smart-editing.md` §2.2/§2.6).
//! `T18b` adds a sixth kind of exception, not a lack of binding but a
//! lack of translation: `JoinLines` (`⌃⇧J`, already `Ctrl`-based in
//! JetBrains' own macOS keymap), `MoveLinesUp`/`MoveLinesDown` (`⌥⇧↑`/
//! `⌥⇧↓`) and `ExtendSelection`/`ShrinkSelection` (`⌥↑`/`⌥↓`) never start
//! from a `Cmd`/`Ctrl` chord in `ide-ui` either, so all five are used
//! literally, the same as `ShowIntentionActions`/`Rename` below
//! (`tui-line-commands-and-editorconfig.md` §2.2).
//! `QuickDocumentation` is a
//! fourth, different kind of exception: `ide-ui`'s own binding is a genuine
//! `{ mac: F1, other: Ctrl+Q }` split (not `Binding::same`), so there is no
//! single "the mac binding" to translate -- `F1` is used literally instead
//! (see `tui-hover-and-inlay-hints.md` §3.1 for why `F1` needs no `Ctrl`
//! masking or Kitty-protocol disambiguation the way every `Ctrl+<letter>`
//! chord in this table does). `ShowIntentionActions`/`Rename` join
//! `QuickDocumentation` in this same no-translation-needed category, for a
//! simpler reason than `QuickDocumentation`'s own mac/other split: `ide-ui`'s
//! `⌥Enter`/`⇧F6` are both genuine `Binding::same` (identical on every
//! platform), and neither ever starts from a `Cmd`/`Ctrl` chord in the first
//! place, so there is nothing to mask or disambiguate for either one
//! (`docs/features/tui-code-actions-and-rename.md` §2.3). `ide-ui`'s
//! `FindNext`/`FindPrevious` (`⌘G`/`⌘⇧G`) and Replace All (no binding at
//! all in `ide-ui`) are deliberately **not** here -- `tui-find.md` §4.2
//! and `tui-replace.md` §1/§4.2 explain why, the same way the palette's
//! own `Up`/`Down`/`Enter` navigation never appears in this table either.
//!
//! `ToggleProjectToolWindow` is the other exception to the direct
//! `Ctrl`-translation rule: `ide-ui`'s `⌘1` naively translates to `Ctrl+1`,
//! but a bare terminal has no C0 control code for `Ctrl`+digit -- masking
//! `'1'`'s low 5 bits (the same scheme every `Ctrl+<letter>` byte in this
//! table relies on) produces `0x11`, the identical byte `Ctrl+Q` produces.
//! `main.rs` now opts into the Kitty/CSI-u keyboard protocol when the
//! terminal supports it (see its own doc comment), which *would* actually
//! disambiguate this -- but `Ctrl+T` ("Tree") stays the binding anyway,
//! since it works identically on every terminal, protocol or not, and
//! there is no reason to make this one command's reachability depend on
//! an optional capability when an unused, unambiguous `Ctrl+<letter>` is
//! free.
//!
//! `Redo`'s `Ctrl+Shift+Z` binding uses a **lowercase** `'z'`, not `'Z'`:
//! `crossterm`'s Kitty/CSI-u decoder (`main.rs`'s protocol opt-in) reports
//! the key's base/unshifted codepoint plus a separate `SHIFT` modifier bit
//! for a held Shift, rather than folding case into the char the way a
//! plain (non-`Ctrl`) keystroke does -- so the terminal-produced `KeyEvent`
//! for a real `Ctrl+Shift+Z` press is `(CONTROL | SHIFT, Char('z'))`, never
//! `Char('Z')`. Without the enhanced protocol (an unsupporting terminal),
//! this binding stays exactly as unreachable as it was before -- no
//! regression, just no fix for that case either. The same lowercase rule
//! applies to `handle_key`'s inline `Ctrl+Shift+A` check and
//! `handle_find_key`'s inline `Ctrl+Shift+G`/`Ctrl+Shift+R` checks in
//! `app.rs`. `NextTab`/`PreviousTab`'s `Ctrl+Shift+[`/`Ctrl+Shift+]` are
//! unaffected by this -- brackets aren't letters, so `crossterm` never
//! case-folds them regardless of protocol, and their literal `'['`/`']'`
//! chars were already correct.
//!
//! `FindUsages`'s `Ctrl+U` and `ToggleProblems`'s `Ctrl+P` are two more
//! departures from a literal translation, for the same reason
//! `ToggleProjectToolWindow` already is: `ide-ui`'s own bindings for these
//! (`⌥F7` for Find Usages, `⌘6` for the Problems tool window) either don't
//! start from a `Cmd` chord at all or would naively translate to another
//! `Ctrl`+digit collision. Rather than invent a translation this crate has
//! no established convention for, each picks an unused, unambiguous
//! `Ctrl+<letter>` instead -- see `docs/features/tui-goto-and-usages.md`
//! §2.3 and `docs/features/tui-problems.md` for the reasoning spelled out
//! per binding.
//!
//! `T19` (`docs/features/tui-code-folding.md` §2.3) adds
//! `CollapseFold`/`ExpandFold`/`CollapseAllFolds`/`ExpandAllFolds`,
//! `Ctrl`-translated from `⌘−`/`⌘+`/`⌘⇧−`/`⌘⇧+`. `-`/`+` are used as
//! literal chars (the same way `/` already is for `ToggleLineComment`/
//! `ToggleBlockComment`, not the letter/bracket "base codepoint + explicit
//! `SHIFT` bit" convention above) since neither has an established
//! shifted/unshifted pairing in this table the way a letter or bracket
//! does; `SHIFT` is still added explicitly for the two "All" variants,
//! purely to distinguish them from their singular counterparts at the
//! modifier level, the same distinguishing role `SHIFT` plays for
//! `JoinLines`/`NextTab` elsewhere in this table.
//!
//! `T20` (`docs/features/tui-multiple-cursors.md` §1.1) adds
//! `AddNextOccurrence`/`UnselectOccurrence` (`⌃G`/`⌃⇧G`, already
//! `Ctrl`-based in JetBrains' own macOS keymap, used literally -- the
//! same category `JoinLines` is already in) and `SelectAllOccurrences`, a
//! new kind of exception: its mac binding (`⌃⌘G`) needs a `⌘` a terminal
//! cannot deliver, but substituting `Ctrl` for `⌘` the way every other
//! `⌘`-chord in this table is translated would collide with
//! `AddNextOccurrence`'s own `Ctrl+G` -- so this one command alone uses
//! the real Windows/Linux JetBrains binding for the same action
//! (`Ctrl+Alt+Shift+J`) instead, the `{mac, other}` interpretation
//! `tui-shell-and-editor.md` §2.4 already established for this crate as a
//! whole, applied to one binding rather than the whole table. `⌃G`/`⌃⇧G`
//! are only reachable while the find bar is closed
//! (`handle_key`'s `self.find.is_some()` check still runs first, per
//! `tui-find.md` §4.2) -- `CollapseSelections` (`Esc`) has no such
//! caveat and no prior binding to share it with.
//!
//! `T16` (`docs/features/tui-go-to-file-and-symbol.md` §1.2) adds
//! `GoToFile`/`GoToSymbol`, both `Ctrl`-translated from `ide-ui`'s own
//! `other`-keymap bindings (`Ctrl+Shift+N`/`Ctrl+Alt+Shift+N`) rather than
//! their mac bindings (`⌘⇧O`/`⌘⌥O`) -- the usual "mac `Cmd` is unreachable
//! in a terminal" substitution this table has followed since `T1`, not a
//! new kind of exception.
//!
//! `T22` (`docs/features/tui-keymap.md` §2.4) adds the `"FindAction"` id
//! (`Action::OpenPalette` -- named `OpenPalette` rather than `FindAction`
//! to avoid clippy's `enum_variant_names` lint on `Action::FindAction`;
//! the *id string* still matches `ide-ui`'s own registry entry for the
//! identical action) -- folding the pre-existing hardcoded `Ctrl+Shift+A`
//! -> `open_palette()` special case in `handle_key` into a real registry
//! entry, so it becomes rebindable through the new `keymap::
//! KeymapOverlay` like everything else -- plus `ToggleKeymapSettings`/
//! `ResetAllKeybindings`, both palette-only (no JetBrains keymap
//! window/reset-all shortcut exists to translate, same reasoning already
//! established for `ToggleGitPanel`/`ToggleTodoPanel`).
//!
//! `T23` (`docs/features/tui-scratch-files.md` §2.2) adds
//! `NewScratchFile`/`ScratchFiles`, both palette-only for the same
//! reason -- no default keybinding for either action exists in the
//! tracked JetBrains macOS keymap table.
//!
//! `T27` (`docs/features/tui-debugger.md` §2.7) adds `Debug`/
//! `ResumeProgram`/`StepOver`/`StepInto`/`StepOut`/`ToggleLineBreakpoint`/
//! `StopDebugging`/`PauseProgram`/`ToggleDebugPanel`/
//! `ConfigureDebugAdapter` -- the same real JetBrains Windows/Linux
//! debugger bindings `debugger.md` §3's own keymap table already
//! specifies for `ide-ui`'s identical actions, used verbatim (no
//! translation needed: none of them start from a `Cmd`/`Ctrl` chord).
//! `PauseProgram`/`ToggleDebugPanel`/`ConfigureDebugAdapter` are
//! palette-only, the first two for the same "not in the reference keymap
//! either" reason `ide-ui`'s own copy of this table already has, the
//! third because it's an `ide-tui`-only command with no `ide-ui`
//! analogue to translate a binding from.

#[cfg(test)]
use crossterm::event::KeyEvent;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SaveActive,
    Undo,
    Redo,
    ToggleTreeFocus,
    NextTab,
    PreviousTab,
    CloseTab,
    Find,
    Replace,
    GoToDeclaration,
    FindUsages,
    ToggleNotifications,
    ToggleProblems,
    ToggleCargoPanel,
    QuickDocumentation,
    FindInPath,
    ShowIntentionActions,
    Rename,
    ToggleGitPanel,
    JumpToMatchingBracket,
    DuplicateLines,
    DeleteLines,
    JoinLines,
    MoveLinesUp,
    MoveLinesDown,
    MoveStatementsUp,
    MoveStatementsDown,
    ToggleLineComment,
    ToggleBlockComment,
    ExtendSelection,
    ShrinkSelection,
    ToggleCase,
    CollapseFold,
    ExpandFold,
    CollapseAllFolds,
    ExpandAllFolds,
    AddNextOccurrence,
    UnselectOccurrence,
    SelectAllOccurrences,
    CollapseSelections,
    GoToFile,
    GoToSymbol,
    RecentFiles,
    ToggleBookmark,
    ShowBookmarks,
    ToggleTodoPanel,
    ReloadFromDisk,
    DismissExternalChange,
    OpenPalette,
    ToggleKeymapSettings,
    ResetAllKeybindings,
    NewScratchFile,
    ToggleScratchFiles,
    ToggleClaudePanel,
    ToggleDockerPanel,
    ToggleK8sPanel,
    Debug,
    ResumeProgram,
    StepOver,
    StepInto,
    StepOut,
    ToggleLineBreakpoint,
    StopDebugging,
    PauseProgram,
    ToggleDebugPanel,
    ConfigureDebugAdapter,
    GitBranches,
    ShowFileHistory,
    Exit,
}

pub struct Command {
    pub id: &'static str,
    pub title: &'static str,
    pub binding: Option<(KeyModifiers, KeyCode)>,
    pub action: Action,
}

pub fn commands() -> &'static [Command] {
    const COMMANDS: &[Command] = &[
        Command {
            id: "SaveAll",
            title: "Save",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('s'))),
            action: Action::SaveActive,
        },
        Command {
            id: "Undo",
            title: "Undo",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('z'))),
            action: Action::Undo,
        },
        Command {
            id: "Redo",
            title: "Redo",
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('z'),
            )),
            action: Action::Redo,
        },
        Command {
            id: "ToggleProjectToolWindow",
            title: "Project",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('t'))),
            action: Action::ToggleTreeFocus,
        },
        Command {
            id: "NextTab",
            title: "Next Tab",
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char(']'),
            )),
            action: Action::NextTab,
        },
        Command {
            id: "PreviousTab",
            title: "Previous Tab",
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('['),
            )),
            action: Action::PreviousTab,
        },
        Command {
            id: "CloseTab",
            title: "Close Tab",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('w'))),
            action: Action::CloseTab,
        },
        Command {
            id: "Find",
            title: "Find",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('f'))),
            action: Action::Find,
        },
        Command {
            id: "Replace",
            title: "Replace",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('r'))),
            action: Action::Replace,
        },
        Command {
            id: "GoToDeclaration",
            title: "Go to Declaration",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('b'))),
            action: Action::GoToDeclaration,
        },
        Command {
            id: "FindUsages",
            title: "Find Usages",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('u'))),
            action: Action::FindUsages,
        },
        Command {
            id: "ToggleNotifications",
            title: "Notifications",
            binding: None,
            action: Action::ToggleNotifications,
        },
        Command {
            id: "ToggleProblems",
            title: "Problems",
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('p'))),
            action: Action::ToggleProblems,
        },
        Command {
            id: "ToggleCargoPanel",
            title: "Cargo",
            binding: None,
            action: Action::ToggleCargoPanel,
        },
        Command {
            id: "QuickDocumentation",
            title: "Quick Documentation",
            // Literal `F1`, not a `Ctrl`-translated letter -- `ide-ui`'s
            // own binding is `{ mac: F1, other: Ctrl+Q }`, the one case
            // CLAUDE.md's keyboard-shortcuts section already names as a
            // genuine mac/other split. A function key needs no `Ctrl`
            // masking or Kitty-protocol disambiguation on any terminal,
            // so it's used exactly as the mac binding reads rather than
            // picking an unrelated free letter the way `ToggleProblems`/
            // `FindUsages` had to (`docs/features/
            // tui-hover-and-inlay-hints.md` §3.1).
            binding: Some((KeyModifiers::NONE, KeyCode::F(1))),
            action: Action::QuickDocumentation,
        },
        Command {
            id: "FindInPath",
            title: "Find in Path",
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('f'),
            )),
            action: Action::FindInPath,
        },
        Command {
            id: "ShowIntentionActions",
            title: "Show Intention Actions",
            // Literal `Alt+Enter`, not a `Ctrl`-translation -- `ide-ui`'s
            // own binding, `⌥↩`, is a genuine `Binding::same` (identical on
            // every JetBrains keymap variant) that never starts from a
            // `Cmd`/`Ctrl` chord in the first place, so this table's usual
            // `Ctrl`-masking/Kitty-protocol disambiguation concern doesn't
            // apply here, the same reasoning `QuickDocumentation`'s literal
            // `F1` already established for a different kind of exception
            // (`docs/features/tui-code-actions-and-rename.md` §2.3).
            binding: Some((KeyModifiers::ALT, KeyCode::Enter)),
            action: Action::ShowIntentionActions,
        },
        Command {
            id: "Rename",
            title: "Rename",
            // Literal `Shift+F6`, same reasoning as `ShowIntentionActions`
            // above -- `ide-ui`'s own `⇧F6` is a genuine `Binding::same`
            // that doesn't start from `Cmd`/`Ctrl` either.
            binding: Some((KeyModifiers::SHIFT, KeyCode::F(6))),
            action: Action::Rename,
        },
        Command {
            id: "ToggleGitPanel",
            title: "Git",
            // Palette-only, no default binding -- `ide-ui`'s own "Editor"/
            // "Source Control" toggle is toolbar-click-only, never bound to
            // a key in any JetBrains keymap this project tracks either
            // (`docs/roadmap.md`'s keymap table binds `⌘9` to the VCS
            // *tool window*, a different, broader action than this v1
            // feature's narrower graph+diff+conflict view). Per `CLAUDE.md`'s
            // "never invent a binding" rule, this joins `ToggleNotifications`/
            // `ToggleCargoPanel` in the no-binding category rather than
            // being translated from anything (`docs/features/
            // tui-git-panel.md` §1/§2.3).
            binding: None,
            action: Action::ToggleGitPanel,
        },
        Command {
            id: "JumpToMatchingBracket",
            title: "Jump to Matching Bracket",
            // Palette-only, no default binding -- `smart-editing.md` §2.6
            // itself notes no JetBrains macOS keymap entry exists for this
            // action, so per `CLAUDE.md`'s "never invent a binding" rule it
            // joins `ToggleNotifications`/`ToggleCargoPanel`/`ToggleGitPanel`
            // in the no-binding category (`docs/features/
            // tui-smart-editing.md` §2.2).
            binding: None,
            action: Action::JumpToMatchingBracket,
        },
        Command {
            id: "DuplicateLines",
            title: "Duplicate Line or Selection",
            // `⌘D` translated -- `line-commands-and-editorconfig.md` §1.2.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('d'))),
            action: Action::DuplicateLines,
        },
        Command {
            id: "DeleteLines",
            title: "Delete Line",
            // `⌘⌫` translated. `ide-tui` has no delete-to-line-start
            // binding to take this chord from (no A2-equivalent
            // `Granularity` system) -- `Ctrl+Backspace` was free.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Backspace)),
            action: Action::DeleteLines,
        },
        Command {
            id: "JoinLines",
            title: "Join Lines",
            // `⌃⇧J` -- already `Ctrl`-based in JetBrains' own macOS
            // keymap, so used literally, not translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('j'),
            )),
            action: Action::JoinLines,
        },
        Command {
            id: "MoveLinesUp",
            title: "Move Line Up",
            // `⌥⇧↑` -- not a `Cmd`/`Ctrl` chord to begin with, used
            // literally (same precedent as `Alt+Enter`/`Shift+F6`).
            binding: Some((KeyModifiers::ALT.union(KeyModifiers::SHIFT), KeyCode::Up)),
            action: Action::MoveLinesUp,
        },
        Command {
            id: "MoveLinesDown",
            title: "Move Line Down",
            binding: Some((KeyModifiers::ALT.union(KeyModifiers::SHIFT), KeyCode::Down)),
            action: Action::MoveLinesDown,
        },
        Command {
            id: "MoveStatementsUp",
            title: "Move Statement Up",
            // `⌘⇧↑` translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Up,
            )),
            action: Action::MoveStatementsUp,
        },
        Command {
            id: "MoveStatementsDown",
            title: "Move Statement Down",
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Down,
            )),
            action: Action::MoveStatementsDown,
        },
        Command {
            id: "ToggleLineComment",
            title: "Comment with Line Comment",
            // `⌘/` translated.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('/'))),
            action: Action::ToggleLineComment,
        },
        Command {
            id: "ToggleBlockComment",
            title: "Comment with Block Comment",
            // `⌘⌥/` translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::ALT),
                KeyCode::Char('/'),
            )),
            action: Action::ToggleBlockComment,
        },
        Command {
            id: "ExtendSelection",
            title: "Extend Selection",
            // `⌥↑` -- not a `Cmd`/`Ctrl` chord, used literally.
            // `ide-tui` has no Clone Caret / multiple-cursors feature to
            // disambiguate against (`docs/features/
            // tui-line-commands-and-editorconfig.md` §1), so this maps
            // unconditionally, unlike `ide-ui`'s `Frame::rewrite`-gated
            // version.
            binding: Some((KeyModifiers::ALT, KeyCode::Up)),
            action: Action::ExtendSelection,
        },
        Command {
            id: "ShrinkSelection",
            title: "Shrink Selection",
            binding: Some((KeyModifiers::ALT, KeyCode::Down)),
            action: Action::ShrinkSelection,
        },
        Command {
            id: "ToggleCase",
            title: "Toggle Case",
            // `⌘⇧U` translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('u'),
            )),
            action: Action::ToggleCase,
        },
        Command {
            id: "CollapseFold",
            title: "Collapse",
            // `⌘−` translated.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('-'))),
            action: Action::CollapseFold,
        },
        Command {
            id: "ExpandFold",
            title: "Expand",
            // `⌘+` translated.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('+'))),
            action: Action::ExpandFold,
        },
        Command {
            id: "CollapseAllFolds",
            title: "Collapse All",
            // `⌘⇧−` translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('-'),
            )),
            action: Action::CollapseAllFolds,
        },
        Command {
            id: "ExpandAllFolds",
            title: "Expand All",
            // `⌘⇧+` translated.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('+'),
            )),
            action: Action::ExpandAllFolds,
        },
        Command {
            id: "AddNextOccurrence",
            title: "Add Selection for Next Occurrence",
            // `⌃G` -- already `Ctrl`-based in JetBrains' own macOS keymap
            // (same precedent as `JoinLines`'s `⌃⇧J`), used literally.
            // `docs/features/tui-multiple-cursors.md` §1.1 -- only a
            // global command while the find bar is closed; `handle_key`'s
            // `self.find.is_some()` check runs before `binding_for`, so
            // this chord still means "jump to next match" while the bar
            // is open, exactly as `tui-find.md` §4.2 already established.
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('g'))),
            action: Action::AddNextOccurrence,
        },
        Command {
            id: "UnselectOccurrence",
            title: "Unselect Occurrence",
            // `⌃⇧G`, literal -- lowercase `'g'` with an explicit `SHIFT`
            // bit, not `'G'` (this file's own established Kitty/CSI-u
            // convention). Same find-bar-closed caveat as above.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('g'),
            )),
            action: Action::UnselectOccurrence,
        },
        Command {
            id: "SelectAllOccurrences",
            title: "Select All Occurrences",
            // `ide-ui`'s mac binding is `⌃⌘G` -- the `⌘` half can't reach
            // a terminal, and substituting `Ctrl` for it would collide
            // with `AddNextOccurrence`'s own `Ctrl+G`. This is the real,
            // non-invented Windows/Linux JetBrains binding for this exact
            // action instead (`docs/features/tui-multiple-cursors.md`
            // §1.1), used verbatim rather than translated.
            binding: Some((
                KeyModifiers::CONTROL
                    .union(KeyModifiers::ALT)
                    .union(KeyModifiers::SHIFT),
                KeyCode::Char('j'),
            )),
            action: Action::SelectAllOccurrences,
        },
        Command {
            id: "CollapseSelections",
            title: "Collapse Selections to the Primary Caret",
            binding: Some((KeyModifiers::NONE, KeyCode::Esc)),
            action: Action::CollapseSelections,
        },
        Command {
            id: "GoToFile",
            title: "Go to File",
            // `⌘⇧O` translated -- mac `⌘` unreachable in a terminal, so
            // this uses `ide-ui`'s own `other`-keymap binding instead
            // (`docs/features/tui-go-to-file-and-symbol.md` §1.2).
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('n'),
            )),
            action: Action::GoToFile,
        },
        Command {
            id: "GoToSymbol",
            title: "Go to Symbol",
            // `⌘⌥O` translated, same reasoning as `GoToFile` above.
            binding: Some((
                KeyModifiers::CONTROL
                    .union(KeyModifiers::ALT)
                    .union(KeyModifiers::SHIFT),
                KeyCode::Char('n'),
            )),
            action: Action::GoToSymbol,
        },
        Command {
            id: "RecentFiles",
            title: "Recent Files",
            // `⌘E` translated -- mac `⌘` unreachable in a terminal, real
            // JetBrains Windows/Linux default is the same letter with
            // `Ctrl` instead of `Cmd`, no genuine mac/other split here
            // (`docs/features/tui-recent-files-and-bookmarks.md` §1.2).
            binding: Some((KeyModifiers::CONTROL, KeyCode::Char('e'))),
            action: Action::RecentFiles,
        },
        Command {
            id: "ToggleBookmark",
            title: "Toggle Bookmark",
            // Literal `F3`, not a `Ctrl`-translation -- `ide-ui`'s own mac
            // binding is a genuine `{ mac: F3, other: F11 }` split, the
            // same "no single mac binding to translate" case
            // `QuickDocumentation`'s `{F1, Ctrl+Q}` already established. A
            // function key needs no `Ctrl`-masking or Kitty-protocol
            // disambiguation on any terminal, so it's used exactly as the
            // mac binding reads (`docs/features/
            // tui-recent-files-and-bookmarks.md` §1.2).
            binding: Some((KeyModifiers::NONE, KeyCode::F(3))),
            action: Action::ToggleBookmark,
        },
        Command {
            id: "ShowBookmarks",
            title: "Show Bookmarks",
            // `⌘F3` translated -- same reasoning as `RecentFiles` above,
            // `Cmd` becomes `Ctrl`, `F3` stays literal.
            binding: Some((KeyModifiers::CONTROL, KeyCode::F(3))),
            action: Action::ShowBookmarks,
        },
        Command {
            id: "ToggleTodoPanel",
            title: "TODO",
            // Palette-only, no default binding -- no JetBrains macOS
            // keymap entry exists for a TODO tool-window shortcut
            // (`docs/roadmap.md` §5.2's tool-window row doesn't include
            // it), so per `CLAUDE.md`'s "never invent a binding" rule this
            // joins `ToggleGitPanel`/`ToggleCargoPanel`/
            // `JumpToMatchingBracket` in the no-binding category
            // (`docs/features/tui-todo-panel.md` §1.2).
            binding: None,
            action: Action::ToggleTodoPanel,
        },
        Command {
            id: "ReloadFromDisk",
            title: "Reload from Disk",
            // Palette-only -- `ide-ui` itself registers no command-palette
            // entry for this (a pure banner-button click, confirmed by
            // grepping `crates/ui/src/command.rs`), so there is no
            // existing binding to translate (`docs/features/
            // tui-file-watcher.md` §1.1).
            binding: None,
            action: Action::ReloadFromDisk,
        },
        Command {
            id: "DismissExternalChange",
            title: "Dismiss External Change Notice (Keep Mine)",
            // Same reasoning as `ReloadFromDisk` above.
            binding: None,
            action: Action::DismissExternalChange,
        },
        Command {
            id: "FindAction",
            title: "Find Action",
            // Was a hardcoded special case in `handle_key`, checked before
            // `binding_for` -- folded into a real registry entry so it's
            // rebindable through `keymap::KeymapOverlay` like everything
            // else (`docs/features/tui-keymap.md` §2.4). `ide-ui` already
            // uses this exact id (`crates/ui/src/command.rs`) for the
            // identical action.
            binding: Some((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('a'),
            )),
            action: Action::OpenPalette,
        },
        Command {
            id: "ToggleKeymapSettings",
            title: "Keymap",
            // Palette-only -- `ide-ui`'s Keymap window is opened by a
            // toolbar button, never a keybinding, so there is no existing
            // binding to translate (`docs/features/tui-keymap.md` §2.4).
            binding: None,
            action: Action::ToggleKeymapSettings,
        },
        Command {
            id: "ResetAllKeybindings",
            title: "Reset All Keybindings to Default",
            // Palette-only -- `ide-ui`'s own "Reset All" is a
            // settings-window button, same reasoning as
            // `ToggleKeymapSettings` above.
            binding: None,
            action: Action::ResetAllKeybindings,
        },
        Command {
            id: "NewScratchFile",
            title: "New Scratch File",
            // Palette-only -- no default keybinding for this action exists
            // in the JetBrains macOS keymap table this project tracks
            // (`docs/roadmap.md` §5.2), so per `CLAUDE.md`'s "never invent
            // a binding" rule this joins `ToggleTodoPanel`/
            // `ToggleKeymapSettings` in the no-binding category
            // (`docs/features/tui-scratch-files.md` §2.2).
            binding: None,
            action: Action::NewScratchFile,
        },
        Command {
            id: "ScratchFiles",
            title: "Scratch Files",
            // Palette-only, same reasoning as `NewScratchFile` above.
            binding: None,
            action: Action::ToggleScratchFiles,
        },
        Command {
            id: "ClaudePanel",
            title: "Claude",
            // Palette-only, same reasoning as `ToggleCargoPanel`/
            // `ToggleGitPanel`/`ToggleTodoPanel` above -- none of this
            // project's tracked JetBrains keymap table entries cover an
            // assistant panel (`docs/features/tui-claude-panel.md` §1.1).
            binding: None,
            action: Action::ToggleClaudePanel,
        },
        Command {
            id: "ToggleDockerPanel",
            title: "Docker",
            // Palette-only, same reasoning as `ToggleCargoPanel`/
            // `ToggleGitPanel`/`ClaudePanel` above -- no JetBrains macOS
            // keymap this project tracks binds a Docker tool window
            // (`docs/features/tui-docker-and-kubernetes.md` §2.5).
            binding: None,
            action: Action::ToggleDockerPanel,
        },
        Command {
            id: "ToggleK8sPanel",
            title: "Kubernetes",
            // Same reasoning as `ToggleDockerPanel` immediately above.
            binding: None,
            action: Action::ToggleK8sPanel,
        },
        Command {
            id: "Debug",
            title: "Debug",
            // `⌃⌥D` / `Alt+Shift+F9` -- the real `debugger.md` §3 keymap
            // table entry, already used verbatim by `ide-ui`'s own
            // `Debug` command (`docs/features/tui-debugger.md` §2.7).
            binding: Some((KeyModifiers::ALT.union(KeyModifiers::SHIFT), KeyCode::F(9))),
            action: Action::Debug,
        },
        Command {
            id: "ResumeProgram",
            title: "Resume Program",
            binding: Some((KeyModifiers::NONE, KeyCode::F(9))),
            action: Action::ResumeProgram,
        },
        Command {
            id: "StepOver",
            title: "Step Over",
            binding: Some((KeyModifiers::NONE, KeyCode::F(8))),
            action: Action::StepOver,
        },
        Command {
            id: "StepInto",
            title: "Step Into",
            binding: Some((KeyModifiers::NONE, KeyCode::F(7))),
            action: Action::StepInto,
        },
        Command {
            id: "StepOut",
            title: "Step Out",
            binding: Some((KeyModifiers::SHIFT, KeyCode::F(8))),
            action: Action::StepOut,
        },
        Command {
            id: "ToggleLineBreakpoint",
            title: "Toggle Line Breakpoint",
            // `⌘F8` translated -- `ide-tui` has no gutter click to also
            // wire this to, so this is the *only* way to toggle a
            // breakpoint here (`docs/features/tui-debugger.md` §2.7).
            binding: Some((KeyModifiers::CONTROL, KeyCode::F(8))),
            action: Action::ToggleLineBreakpoint,
        },
        Command {
            id: "StopDebugging",
            title: "Stop Debugging",
            binding: Some((KeyModifiers::CONTROL, KeyCode::F(2))),
            action: Action::StopDebugging,
        },
        Command {
            id: "PauseProgram",
            title: "Pause Program",
            // Palette-only -- not in the reference keymap either
            // (`docs/features/debugger.md` §3 note), same as `ide-ui`.
            binding: None,
            action: Action::PauseProgram,
        },
        Command {
            id: "ToggleDebugPanel",
            title: "Debug",
            // Palette-only, same reasoning as `ToggleCargoPanel`/
            // `ToggleGitPanel` above.
            binding: None,
            action: Action::ToggleDebugPanel,
        },
        Command {
            id: "ConfigureDebugAdapter",
            title: "Configure Debug Adapter",
            // Palette-only -- `ide-tui`-only command, no `ide-ui`
            // analogue to translate a binding from.
            binding: None,
            action: Action::ConfigureDebugAdapter,
        },
        Command {
            id: "GitBranches",
            title: "Git Branches...",
            // Palette-only (`docs/features/
            // tui-git-staging-branches-and-log-filters.md` §2.3) -- the
            // `b` binding inside the Git Panel overlay is a panel-internal
            // micro-shortcut, not a global keymap entry, the same
            // category as `handle_git_panel_key`'s own 'o'/'t'.
            binding: None,
            action: Action::GitBranches,
        },
        Command {
            id: "ShowFileHistory",
            title: "Show History for File",
            // Palette-only, same reasoning as `GitBranches` above.
            binding: None,
            action: Action::ShowFileHistory,
        },
        Command {
            id: "Exit",
            title: "Exit",
            binding: None,
            action: Action::Exit,
        },
    ];
    COMMANDS
}

/// The static, compile-time default lookup -- test-only since `T22`
/// (`docs/features/tui-keymap.md`): every real dispatch site now goes
/// through `keymap::KeymapOverlay::action_for`, which falls through to
/// this same table but isn't literally implemented by calling this
/// function (it looks a binding up by id, not by key). Kept `#[cfg(test)]`
/// rather than deleted, since it remains the most direct way this crate's
/// own tests verify the static table in isolation, independent of the
/// overlay layered on top of it.
#[cfg(test)]
pub fn binding_for(key: KeyEvent) -> Option<Action> {
    commands()
        .iter()
        .find(|c| c.binding == Some((key.modifiers, key.code)))
        .map(|c| c.action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(modifiers: KeyModifiers, code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn ctrl_s_maps_to_save_active() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('s')));
        assert_eq!(action, Some(Action::SaveActive));
    }

    #[test]
    fn ctrl_shift_z_maps_to_redo_not_ctrl_z() {
        let redo = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('z'),
        ));
        assert_eq!(redo, Some(Action::Redo));
        let undo = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('z')));
        assert_eq!(undo, Some(Action::Undo));
    }

    #[test]
    fn ctrl_t_maps_to_toggle_tree_focus() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('t')));
        assert_eq!(action, Some(Action::ToggleTreeFocus));
    }

    #[test]
    fn ctrl_shift_bracket_keys_map_to_next_and_previous_tab() {
        let next = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char(']'),
        ));
        assert_eq!(next, Some(Action::NextTab));
        let previous = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('['),
        ));
        assert_eq!(previous, Some(Action::PreviousTab));
    }

    #[test]
    fn ctrl_w_maps_to_close_tab() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('w')));
        assert_eq!(action, Some(Action::CloseTab));
    }

    #[test]
    fn ctrl_f_maps_to_find() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('f')));
        assert_eq!(action, Some(Action::Find));
    }

    #[test]
    fn ctrl_r_maps_to_replace() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('r')));
        assert_eq!(action, Some(Action::Replace));
    }

    #[test]
    fn ctrl_b_and_ctrl_u_map_to_goto_and_find_usages() {
        let goto = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('b')));
        assert_eq!(goto, Some(Action::GoToDeclaration));
        let usages = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('u')));
        assert_eq!(usages, Some(Action::FindUsages));
    }

    #[test]
    fn ctrl_p_maps_to_toggle_problems() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('p')));
        assert_eq!(action, Some(Action::ToggleProblems));
    }

    #[test]
    fn ctrl_g_and_ctrl_shift_g_now_resolve_to_the_multi_cursor_actions() {
        // `docs/features/tui-multiple-cursors.md` §1.1/Revision notes:
        // this used to assert these chords were *not* globally registered
        // (`docs/features/tui-find.md` §4.2) -- still true while the find
        // bar is open (`handle_key`'s `self.find.is_some()` check runs
        // before `binding_for` is ever consulted), but no longer true of
        // `binding_for` itself, which only ever sees a key while the bar
        // is closed.
        let next = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('g')));
        assert_eq!(next, Some(Action::AddNextOccurrence));
        let previous = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('g'),
        ));
        assert_eq!(previous, Some(Action::UnselectOccurrence));
    }

    #[test]
    fn exit_has_no_default_binding() {
        assert!(commands()
            .iter()
            .find(|c| c.id == "Exit")
            .unwrap()
            .binding
            .is_none());
    }

    #[test]
    fn toggle_cargo_panel_has_no_default_binding() {
        assert!(commands()
            .iter()
            .find(|c| c.id == "ToggleCargoPanel")
            .unwrap()
            .binding
            .is_none());
    }

    #[test]
    fn toggle_git_panel_has_no_default_binding() {
        assert!(commands()
            .iter()
            .find(|c| c.id == "ToggleGitPanel")
            .unwrap()
            .binding
            .is_none());
    }

    #[test]
    fn jump_to_matching_bracket_has_no_default_binding() {
        assert!(commands()
            .iter()
            .find(|c| c.id == "JumpToMatchingBracket")
            .unwrap()
            .binding
            .is_none());
    }

    #[test]
    fn f1_maps_to_quick_documentation() {
        let action = binding_for(key(KeyModifiers::NONE, KeyCode::F(1)));
        assert_eq!(action, Some(Action::QuickDocumentation));
    }

    #[test]
    fn ctrl_shift_f_maps_to_find_in_path_not_ctrl_f() {
        let find_in_path = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('f'),
        ));
        assert_eq!(find_in_path, Some(Action::FindInPath));
        let find = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('f')));
        assert_eq!(find, Some(Action::Find));
    }

    #[test]
    fn alt_enter_maps_to_show_intention_actions() {
        let action = binding_for(key(KeyModifiers::ALT, KeyCode::Enter));
        assert_eq!(action, Some(Action::ShowIntentionActions));
        // Plain Enter (no Alt) is unbound in this table.
        let plain = binding_for(key(KeyModifiers::NONE, KeyCode::Enter));
        assert_eq!(plain, None);
    }

    #[test]
    fn shift_f6_maps_to_rename() {
        let action = binding_for(key(KeyModifiers::SHIFT, KeyCode::F(6)));
        assert_eq!(action, Some(Action::Rename));
        // Plain F6 (no Shift) is unbound in this table.
        let plain = binding_for(key(KeyModifiers::NONE, KeyCode::F(6)));
        assert_eq!(plain, None);
    }

    #[test]
    fn an_unbound_key_returns_none() {
        let action = binding_for(key(KeyModifiers::NONE, KeyCode::Char('x')));
        assert_eq!(action, None);
    }

    #[test]
    fn every_command_id_is_unique() {
        let ids: Vec<&str> = commands().iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len());
    }

    #[test]
    fn no_two_bound_commands_share_the_same_chord() {
        let bindings: Vec<(KeyModifiers, KeyCode)> =
            commands().iter().filter_map(|c| c.binding).collect();
        let mut sorted = bindings.clone();
        sorted.sort_by_key(|(m, c)| (m.bits(), format!("{c:?}")));
        sorted.dedup();
        assert_eq!(bindings.len(), sorted.len());
    }

    #[test]
    fn ctrl_alt_shift_j_maps_to_select_all_occurrences() {
        let action = binding_for(key(
            KeyModifiers::CONTROL
                .union(KeyModifiers::ALT)
                .union(KeyModifiers::SHIFT),
            KeyCode::Char('j'),
        ));
        assert_eq!(action, Some(Action::SelectAllOccurrences));
    }

    #[test]
    fn plain_esc_maps_to_collapse_selections() {
        let action = binding_for(key(KeyModifiers::NONE, KeyCode::Esc));
        assert_eq!(action, Some(Action::CollapseSelections));
    }

    #[test]
    fn ctrl_shift_n_maps_to_go_to_file() {
        let action = binding_for(key(
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyCode::Char('n'),
        ));
        assert_eq!(action, Some(Action::GoToFile));
    }

    #[test]
    fn ctrl_alt_shift_n_maps_to_go_to_symbol() {
        let action = binding_for(key(
            KeyModifiers::CONTROL
                .union(KeyModifiers::ALT)
                .union(KeyModifiers::SHIFT),
            KeyCode::Char('n'),
        ));
        assert_eq!(action, Some(Action::GoToSymbol));
    }

    #[test]
    fn ctrl_e_maps_to_recent_files() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::Char('e')));
        assert_eq!(action, Some(Action::RecentFiles));
    }

    #[test]
    fn bare_f3_maps_to_toggle_bookmark() {
        let action = binding_for(key(KeyModifiers::NONE, KeyCode::F(3)));
        assert_eq!(action, Some(Action::ToggleBookmark));
    }

    #[test]
    fn ctrl_f3_maps_to_show_bookmarks() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::F(3)));
        assert_eq!(action, Some(Action::ShowBookmarks));
    }

    #[test]
    fn alt_shift_f9_maps_to_debug_not_plain_f9() {
        let debug = binding_for(key(
            KeyModifiers::ALT.union(KeyModifiers::SHIFT),
            KeyCode::F(9),
        ));
        assert_eq!(debug, Some(Action::Debug));
        let resume = binding_for(key(KeyModifiers::NONE, KeyCode::F(9)));
        assert_eq!(resume, Some(Action::ResumeProgram));
    }

    #[test]
    fn f8_and_shift_f8_map_to_step_over_and_step_out() {
        let step_over = binding_for(key(KeyModifiers::NONE, KeyCode::F(8)));
        assert_eq!(step_over, Some(Action::StepOver));
        let step_out = binding_for(key(KeyModifiers::SHIFT, KeyCode::F(8)));
        assert_eq!(step_out, Some(Action::StepOut));
    }

    #[test]
    fn f7_maps_to_step_into() {
        let action = binding_for(key(KeyModifiers::NONE, KeyCode::F(7)));
        assert_eq!(action, Some(Action::StepInto));
    }

    #[test]
    fn ctrl_f8_maps_to_toggle_line_breakpoint_not_plain_f8() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::F(8)));
        assert_eq!(action, Some(Action::ToggleLineBreakpoint));
    }

    #[test]
    fn ctrl_f2_maps_to_stop_debugging() {
        let action = binding_for(key(KeyModifiers::CONTROL, KeyCode::F(2)));
        assert_eq!(action, Some(Action::StopDebugging));
    }

    #[test]
    fn pause_toggle_debug_panel_and_configure_debug_adapter_have_no_default_binding() {
        for id in ["PauseProgram", "ToggleDebugPanel", "ConfigureDebugAdapter"] {
            assert!(
                commands()
                    .iter()
                    .find(|c| c.id == id)
                    .unwrap()
                    .binding
                    .is_none(),
                "{id} should have no default binding"
            );
        }
    }
}
