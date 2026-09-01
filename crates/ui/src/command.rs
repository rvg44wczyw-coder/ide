//! The command registry (`docs/features/command-palette.md` §2.1): the
//! single, static list of every invokable action in `ide-ui`, each with an
//! id, a title/category for the palette, an optional default keybinding,
//! and the `CommandAction` `IdeApp::run_command` dispatches on. No `egui`
//! dependency beyond `Key`/`Modifiers`/`InputState` -- no `IdeApp`
//! dependency at all, same shape as `find_bar.rs`.

use std::sync::OnceLock;

use crate::cargo_panel::CargoCommand;

/// A single modifier+key combination. Wraps `egui::Modifiers` rather than
/// hand-rolled flags: `egui::Modifiers::command` is `true` on Cmd (mac) or
/// Ctrl (elsewhere), but on non-mac the backend also sets `ctrl` to that
/// same value (`Modifiers::ctrl`'s own doc comment: "On Windows and Linux,
/// set [`command`] to the same value as `ctrl`") -- so a chord meaning
/// "just the primary modifier" must compare via `Modifiers::matches_exact`
/// (egui's own recommended comparison, which special-cases exactly this),
/// not raw field equality, or it would never match real non-mac input at
/// all: a naive `self.ctrl == false` check would always fail against real
/// input where Ctrl-as-primary-modifier also sets `ctrl = true`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyChord {
    pub key: egui::Key,
    pub modifiers: egui::Modifiers,
}

impl KeyChord {
    pub const fn new(key: egui::Key) -> Self {
        Self {
            key,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Cmd on mac, Ctrl elsewhere -- `egui::Modifiers::command` already
    /// abstracts that substitution, so this single flag covers both
    /// platforms; see `Binding`'s doc comment for why a chord built with
    /// only this flag set is still spelled out on both sides of a
    /// `Binding` rather than assumed identical.
    pub const fn command(mut self) -> Self {
        self.modifiers.command = true;
        self
    }

    pub const fn shift(mut self) -> Self {
        self.modifiers.shift = true;
        self
    }

    /// Literal Control key, distinct from `command()`'s Cmd/Ctrl
    /// abstraction -- for the rare JetBrains binding (`GoToTypeDeclaration`,
    /// `⌃⇧B`) that genuinely wants physical Ctrl even on mac, where Ctrl
    /// and Cmd are different keys. Still correct as a single `Binding::same`
    /// chord on non-mac too: `Modifiers::cmd_ctrl_matches` only requires
    /// `self.ctrl` when `pattern.ctrl` is set, regardless of `self.command`
    /// (which the backend also mirrors to `true` there) -- see egui's own
    /// `matches_exact` doc examples.
    pub const fn ctrl(mut self) -> Self {
        self.modifiers.ctrl = true;
        self
    }

    /// Option on mac, Alt elsewhere -- the same physical key, so this
    /// flag needs no platform substitution at all.
    pub const fn alt(mut self) -> Self {
        self.modifiers.alt = true;
        self
    }

    /// True iff `input.modifiers.matches_exact(self.modifiers)` (egui's
    /// own cross-platform-correct comparison -- see this type's doc
    /// comment) and `input.key_pressed(key)` fired this frame. Exact
    /// match, not "at least these modifiers", is what lets e.g. `⌘F` and
    /// `⌘⇧F` coexist as two distinct chords without either explicitly
    /// excluding the other's extra modifier.
    pub fn pressed(&self, input: &egui::InputState) -> bool {
        input.modifiers.matches_exact(self.modifiers) && input.key_pressed(self.key)
    }

    /// Display text: mac style uses the glyph row (⌃⌥⇧⌘) in JetBrains'
    /// own left-to-right order followed by the key name; non-mac style
    /// spells modifiers as words, with `ctrl`/`command` both rendering as
    /// `Ctrl+` since there is no separate physical-Ctrl glyph need there.
    /// The key name is `egui::Key`'s own `Debug` output -- for every key
    /// this registry uses (`S`, `Z`, `F`, `G`, `B`, `A`, `R`, `F7`, `F9`)
    /// that already renders as the exact single-token label JetBrains
    /// uses.
    pub fn label(&self, mac_style: bool) -> String {
        let m = self.modifiers;
        let mut s = String::new();
        if mac_style {
            if m.ctrl {
                s.push('⌃');
            }
            if m.alt {
                s.push('⌥');
            }
            if m.shift {
                s.push('⇧');
            }
            if m.command {
                s.push('⌘');
            }
        } else {
            if m.ctrl || m.command {
                s.push_str("Ctrl+");
            }
            if m.alt {
                s.push_str("Alt+");
            }
            if m.shift {
                s.push_str("Shift+");
            }
        }
        s.push_str(&format!("{:?}", self.key));
        s
    }
}

/// A command's default binding on each platform family. `other` is a
/// separate field, not derived from `mac`, because -- per CLAUDE.md's
/// keyboard-shortcuts section -- only some JetBrains-macOS bindings
/// substitute modifiers mechanically (Cmd→Ctrl) and some genuinely
/// diverge (Quick Documentation: `F1` vs `Ctrl+Q`). Every command this
/// phase registers happens to be a pure substitution (`Binding::same`
/// covers all of them, since `KeyChord::command` already abstracts
/// Cmd/Ctrl), but the type does not encode that as an invariant, since a
/// later phase's command will need to diverge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Binding {
    pub mac: KeyChord,
    pub other: KeyChord,
}

impl Binding {
    /// Both platforms share the same chord.
    pub const fn same(chord: KeyChord) -> Self {
        Self {
            mac: chord,
            other: chord,
        }
    }

    /// Resolves to `mac` when `cfg!(target_os = "macos")`, else `other`.
    pub fn for_platform(&self) -> KeyChord {
        if cfg!(target_os = "macos") {
            self.mac
        } else {
            self.other
        }
    }
}

/// Every action `IdeApp::run_command` can perform. A closed enum, not a
/// `fn(&mut IdeApp)` pointer: this module has no `IdeApp` dependency, and
/// the match arms that call the existing per-action methods
/// (`try_save_active`, `undo_active`, ...) live in `app.rs` alongside
/// those methods, so none of them need a visibility change just to be
/// reachable from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAction {
    SaveAll,
    Undo,
    Redo,
    FindUsages,
    ShowUsages,
    FindInPath,
    Find,
    Replace,
    ReplaceAll,
    ReplaceInPath,
    FindNext,
    FindPrevious,
    FindAction,
    ToggleTheme,
    RefreshTree,
    ToggleSmartMode,
    RunCargo(CargoCommand),
    ToggleProjectToolWindow,
    ToggleFindToolWindow,
    ToggleRunToolWindow,
    ToggleProblemsToolWindow,
    ToggleVcsToolWindow,
    ToggleClaudeToolWindow,
    ToggleZenMode,
    ShowLanguageSettings,
    ShowKeymapSettings,
    CollapseFold,
    ExpandFold,
    CollapseAllFolds,
    ExpandAllFolds,
    GoToDeclaration,
    GoToImplementation,
    GoToTypeDeclaration,
    NavigateBack,
    NavigateForward,
    QuickDocumentation,
    ShowIntentionActions,
    GoToFile,
    GoToClass,
    GoToSymbol,
    GoToLine,
    FileStructure,
    RecentFiles,
    RecentLocations,
    ReformatCode,
    ToggleFormatOnSave,
    Rename,
    RefactorThis,
    ExtractVariable,
    ExtractMethod,
    ExtractConstant,
    ExtractField,
    Inline,
    GenerateMenu,
    ImplementMethods,
    OverrideMethods,
    CreateTest,
    OptimizeImports,
    NextTab,
    PreviousTab,
    CloseTab,
    GitBranches,
    ToggleBlameAnnotations,
    GitWorktrees,
}

#[derive(Debug)]
pub struct Command {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub binding: Option<Binding>,
    pub action: CommandAction,
}

/// The registry: every command this phase knows about, in a fixed
/// declaration order (the palette's default/tie-break order,
/// `command-palette.md` §3.2). Bindings match CLAUDE.md's keyboard-
/// shortcuts section and `docs/roadmap.md` §5.2 exactly -- every one is a
/// straight relocation of a binding that already existed in
/// `handle_shortcuts` before this phase, not a new default.
/// `ShowUsages`'s binding is `⌘⌥F7`, not the `⌘B` it originally shipped
/// with (`docs/roadmap.md` §5.3): JetBrains macOS reserves `⌘B` for Go to
/// Declaration, now `GoToDeclaration` below (`docs/features/
/// goto-definition.md` §3.5, C1).
pub fn commands() -> &'static [Command] {
    static COMMANDS: OnceLock<Vec<Command>> = OnceLock::new();
    COMMANDS.get_or_init(|| {
        use egui::Key;
        vec![
            Command {
                id: "SaveAll",
                title: "Save All",
                category: "File",
                binding: Some(Binding::same(KeyChord::new(Key::S).command())),
                action: CommandAction::SaveAll,
            },
            Command {
                id: "Undo",
                title: "Undo",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Z).command())),
                action: CommandAction::Undo,
            },
            Command {
                id: "Redo",
                title: "Redo",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Z).command().shift())),
                action: CommandAction::Redo,
            },
            Command {
                id: "FindUsages",
                title: "Find Usages",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::F7).alt())),
                action: CommandAction::FindUsages,
            },
            Command {
                id: "ShowUsages",
                title: "Show Usages",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::F7).command().alt())),
                action: CommandAction::ShowUsages,
            },
            Command {
                id: "FindInPath",
                title: "Find in Path",
                category: "Search",
                binding: Some(Binding::same(KeyChord::new(Key::F).command().shift())),
                action: CommandAction::FindInPath,
            },
            Command {
                id: "Find",
                title: "Find",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::F).command())),
                action: CommandAction::Find,
            },
            Command {
                id: "Replace",
                title: "Replace",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::R).command())),
                action: CommandAction::Replace,
            },
            Command {
                id: "ReplaceAll",
                title: "Replace All",
                category: "Edit",
                // Bug fix, found during search-in-path-v2.md's research:
                // verified against the real JetBrains macOS keymap
                // (fetched directly, doc §7/§8) that no standalone
                // "Replace All" action exists there at all -- CLAUDE.md's
                // "never invent a binding" rule being violated, same class
                // of bug D4/C1 already fixed. `⌘⇧R` belongs to "Replace in
                // Files...", now `ReplaceInPath` below.
                binding: None,
                action: CommandAction::ReplaceAll,
            },
            Command {
                id: "ReplaceInPath",
                title: "Replace in Path",
                category: "Search",
                binding: Some(Binding::same(KeyChord::new(Key::R).command().shift())),
                action: CommandAction::ReplaceInPath,
            },
            Command {
                id: "FindNext",
                title: "Find Next",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::G).command())),
                action: CommandAction::FindNext,
            },
            Command {
                id: "FindPrevious",
                title: "Find Previous",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::G).command().shift())),
                action: CommandAction::FindPrevious,
            },
            Command {
                id: "FindAction",
                title: "Find Action",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::A).command().shift())),
                action: CommandAction::FindAction,
            },
            Command {
                id: "ToggleTheme",
                title: "Toggle Theme",
                category: "View",
                binding: None,
                action: CommandAction::ToggleTheme,
            },
            Command {
                id: "RefreshTree",
                title: "Refresh",
                category: "File",
                binding: None,
                action: CommandAction::RefreshTree,
            },
            Command {
                id: "ToggleSmartMode",
                title: "Toggle Smart Mode",
                category: "Navigate",
                binding: None,
                action: CommandAction::ToggleSmartMode,
            },
            // Build/Run get real JetBrains macOS defaults (roadmap.md §5.2:
            // "Make Project" = `⌘F9`, "Run" = `⌃R`, literal Control since
            // that's a distinct action from Cmd-anything already bound).
            // Test/Check/Clippy/Fmt have no JetBrains cargo-tool-window
            // precedent to source from -- CLAUDE.md's "never invent a
            // binding" rule would normally leave these `None` (as they
            // shipped originally), but the user explicitly asked for
            // defaults for all of them; these four are a deliberate,
            // documented departure, built as siblings of Build/Run (same
            // key, added modifier) to keep the scheme mnemonic and to
            // avoid colliding with any roadmap-reserved chord (`⌘⌥L`
            // Reformat Code/A9 in particular, which `⌃⇧F` here does not
            // touch).
            Command {
                id: "CargoBuild",
                title: "Build",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::F9).command())),
                action: CommandAction::RunCargo(CargoCommand::Build),
            },
            Command {
                id: "CargoRun",
                title: "Run",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::R).ctrl())),
                action: CommandAction::RunCargo(CargoCommand::Run),
            },
            Command {
                id: "CargoTest",
                title: "Test",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::R).ctrl().shift())),
                action: CommandAction::RunCargo(CargoCommand::Test),
            },
            Command {
                id: "CargoCheck",
                title: "Check",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::F9).command().shift())),
                action: CommandAction::RunCargo(CargoCommand::Check),
            },
            Command {
                id: "CargoClippy",
                title: "Clippy",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::F9).command().alt())),
                action: CommandAction::RunCargo(CargoCommand::Clippy),
            },
            Command {
                id: "CargoFmt",
                title: "Fmt",
                category: "Build",
                binding: Some(Binding::same(KeyChord::new(Key::F).ctrl().shift())),
                action: CommandAction::RunCargo(CargoCommand::Fmt),
            },
            Command {
                id: "ToggleProjectToolWindow",
                title: "Project",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::Num1).command())),
                action: CommandAction::ToggleProjectToolWindow,
            },
            Command {
                id: "ToggleFindToolWindow",
                title: "Find",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::Num3).command())),
                action: CommandAction::ToggleFindToolWindow,
            },
            Command {
                id: "ToggleRunToolWindow",
                title: "Run",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::Num4).command())),
                action: CommandAction::ToggleRunToolWindow,
            },
            Command {
                id: "ToggleProblemsToolWindow",
                title: "Problems",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::Num6).command())),
                action: CommandAction::ToggleProblemsToolWindow,
            },
            Command {
                id: "ToggleVcsToolWindow",
                title: "VCS",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::Num9).command())),
                action: CommandAction::ToggleVcsToolWindow,
            },
            Command {
                id: "ToggleClaudeToolWindow",
                title: "Claude",
                category: "Window",
                binding: None,
                action: CommandAction::ToggleClaudeToolWindow,
            },
            Command {
                id: "NextTab",
                title: "Next Tab",
                category: "Window",
                binding: Some(Binding::same(
                    KeyChord::new(Key::CloseBracket).command().shift(),
                )),
                action: CommandAction::NextTab,
            },
            Command {
                id: "PreviousTab",
                title: "Previous Tab",
                category: "Window",
                binding: Some(Binding::same(
                    KeyChord::new(Key::OpenBracket).command().shift(),
                )),
                action: CommandAction::PreviousTab,
            },
            Command {
                id: "CloseTab",
                title: "Close Tab",
                category: "Window",
                binding: Some(Binding::same(KeyChord::new(Key::W).command())),
                action: CommandAction::CloseTab,
            },
            Command {
                id: "GitBranches",
                title: "Git Branches...",
                category: "Git",
                binding: None,
                action: CommandAction::GitBranches,
            },
            Command {
                id: "ToggleBlameAnnotations",
                title: "Annotate with Blame",
                category: "Git",
                binding: None,
                action: CommandAction::ToggleBlameAnnotations,
            },
            Command {
                id: "GitWorktrees",
                title: "Git Worktrees...",
                category: "Git",
                // No JetBrains-IDE precedent to copy a binding from --
                // per root CLAUDE.md's "never invent a binding" rule,
                // palette/menu-only (`git-worktrees.md` §2.2.2).
                binding: None,
                action: CommandAction::GitWorktrees,
            },
            Command {
                id: "ToggleZenMode",
                title: "Toggle Zen Mode",
                category: "View",
                binding: None,
                action: CommandAction::ToggleZenMode,
            },
            Command {
                id: "ShowLanguageSettings",
                title: "Languages…",
                category: "Settings",
                binding: None,
                action: CommandAction::ShowLanguageSettings,
            },
            Command {
                id: "ShowKeymapSettings",
                title: "Keymap…",
                category: "Settings",
                binding: None,
                action: CommandAction::ShowKeymapSettings,
            },
            Command {
                id: "CollapseFold",
                title: "Collapse",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Minus).command())),
                action: CommandAction::CollapseFold,
            },
            Command {
                id: "ExpandFold",
                title: "Expand",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Plus).command())),
                action: CommandAction::ExpandFold,
            },
            Command {
                id: "CollapseAllFolds",
                title: "Collapse All",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Minus).command().shift())),
                action: CommandAction::CollapseAllFolds,
            },
            Command {
                id: "ExpandAllFolds",
                title: "Expand All",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::Plus).command().shift())),
                action: CommandAction::ExpandAllFolds,
            },
            Command {
                id: "GoToDeclaration",
                title: "Go to Declaration",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::B).command())),
                action: CommandAction::GoToDeclaration,
            },
            Command {
                id: "GoToImplementation",
                title: "Go to Implementation",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::B).command().alt())),
                action: CommandAction::GoToImplementation,
            },
            Command {
                id: "GoToTypeDeclaration",
                title: "Go to Type Declaration",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::B).ctrl().shift())),
                action: CommandAction::GoToTypeDeclaration,
            },
            Command {
                id: "NavigateBack",
                title: "Back",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::ArrowLeft).command().alt())),
                action: CommandAction::NavigateBack,
            },
            Command {
                id: "NavigateForward",
                title: "Forward",
                category: "Navigate",
                binding: Some(Binding::same(
                    KeyChord::new(Key::ArrowRight).command().alt(),
                )),
                action: CommandAction::NavigateForward,
            },
            Command {
                id: "QuickDocumentation",
                title: "Quick Documentation",
                category: "Navigate",
                // The one binding in this registry where `mac`/`other`
                // genuinely diverge rather than a mechanical Cmd->Ctrl
                // substitution -- `F1` on macOS, `Ctrl+Q` elsewhere, per
                // CLAUDE.md's own keyboard-shortcuts section and
                // `docs/features/inlay-hints-and-hover.md` §2.2.
                binding: Some(Binding {
                    mac: KeyChord::new(Key::F1),
                    other: KeyChord::new(Key::Q).ctrl(),
                }),
                action: CommandAction::QuickDocumentation,
            },
            Command {
                id: "ShowIntentionActions",
                title: "Show Intention Actions",
                category: "Navigate",
                // `⌥↩` is identical across every JetBrains macOS keymap
                // variant, so unlike `QuickDocumentation` this is a genuine
                // `Binding::same`, not a `{mac, other}` split
                // (`docs/features/code-actions.md` §2.3).
                binding: Some(Binding::same(KeyChord::new(Key::Enter).alt())),
                action: CommandAction::ShowIntentionActions,
            },
            Command {
                id: "GoToFile",
                title: "Go to File",
                category: "Navigate",
                // `docs/features/search-everywhere.md` §3.5: mac/other
                // genuinely diverge here (Shift+N, not the mechanical
                // Cmd->Ctrl substitution of the letter alone), per
                // JetBrains' own published keymaps.
                binding: Some(Binding {
                    mac: KeyChord::new(Key::O).command().shift(),
                    other: KeyChord::new(Key::N).ctrl().shift(),
                }),
                action: CommandAction::GoToFile,
            },
            Command {
                id: "GoToClass",
                title: "Go to Class",
                category: "Navigate",
                binding: Some(Binding {
                    mac: KeyChord::new(Key::O).command(),
                    other: KeyChord::new(Key::N).ctrl(),
                }),
                action: CommandAction::GoToClass,
            },
            Command {
                id: "GoToSymbol",
                title: "Go to Symbol",
                category: "Navigate",
                binding: Some(Binding {
                    mac: KeyChord::new(Key::O).command().alt(),
                    other: KeyChord::new(Key::N).ctrl().alt().shift(),
                }),
                action: CommandAction::GoToSymbol,
            },
            Command {
                id: "GoToLine",
                title: "Go to Line",
                category: "Navigate",
                binding: Some(Binding {
                    mac: KeyChord::new(Key::L).command(),
                    other: KeyChord::new(Key::G).ctrl(),
                }),
                action: CommandAction::GoToLine,
            },
            Command {
                id: "FileStructure",
                title: "File Structure",
                category: "Navigate",
                // `docs/roadmap.md` §5.2: identical across JetBrains macOS
                // keymap variants, `.command()`'s existing mac->ctrl
                // substitution matches JetBrains' own cross-platform
                // default verbatim -- no `{mac, other}` split needed
                // (`docs/features/file-structure-and-breadcrumbs.md` §2.3).
                binding: Some(Binding::same(KeyChord::new(Key::F12).command())),
                action: CommandAction::FileStructure,
            },
            Command {
                id: "RecentFiles",
                title: "Recent Files",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::E).command())),
                action: CommandAction::RecentFiles,
            },
            Command {
                id: "RecentLocations",
                title: "Recent Locations",
                category: "Navigate",
                binding: Some(Binding::same(KeyChord::new(Key::E).command().shift())),
                action: CommandAction::RecentLocations,
            },
            Command {
                id: "ReformatCode",
                title: "Reformat Code",
                category: "Edit",
                binding: Some(Binding::same(KeyChord::new(Key::L).command().alt())),
                action: CommandAction::ReformatCode,
            },
            Command {
                id: "ToggleFormatOnSave",
                title: "Toggle Format on Save",
                category: "Edit",
                // No JetBrains macOS default keymap binding exists for this
                // exact toggle (its closest analogue, "Reformat on Save",
                // lives in Settings rather than the default keymap) --
                // per CLAUDE.md's "never invent a binding" rule, registers
                // with none: reachable from the palette, user-bindable.
                binding: None,
                action: CommandAction::ToggleFormatOnSave,
            },
            Command {
                id: "Rename",
                title: "Rename",
                // First phase in track D (refactoring) -- "Edit"/"Navigate"
                // both already exist for different kinds of action (mutating
                // the buffer directly vs. moving the caret/opening a query),
                // neither of which accurately fits "invoke a multi-file,
                // server-driven code transformation"
                // (`docs/features/rename-refactoring.md` §2.3).
                category: "Refactor",
                // `⇧F6` is identical on every JetBrains keymap variant
                // (`docs/roadmap.md` §5.2), a genuine `Binding::same` the
                // same way `⌥↩` already is.
                binding: Some(Binding::same(KeyChord::new(Key::F6).shift())),
                action: CommandAction::Rename,
            },
            Command {
                id: "RefactorThis",
                title: "Refactor This",
                category: "Refactor",
                // Literal Control, not Cmd/Ctrl-abstracted `command()` --
                // JetBrains macOS genuinely uses physical Ctrl here, the
                // same `ctrl()` helper `GoToTypeDeclaration` already uses
                // for its own literal-Ctrl-on-mac binding
                // (`docs/features/refactor-this.md` §2.2).
                binding: Some(Binding::same(KeyChord::new(Key::T).ctrl())),
                action: CommandAction::RefactorThis,
            },
            Command {
                id: "ExtractVariable",
                title: "Extract Variable",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::V).command().alt())),
                action: CommandAction::ExtractVariable,
            },
            Command {
                id: "ExtractMethod",
                title: "Extract Method",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::M).command().alt())),
                action: CommandAction::ExtractMethod,
            },
            Command {
                id: "ExtractConstant",
                title: "Extract Constant",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::C).command().alt())),
                action: CommandAction::ExtractConstant,
            },
            Command {
                id: "ExtractField",
                title: "Extract Field",
                category: "Refactor",
                // rust-analyzer has no direct "extract field" equivalent
                // today (Rust doesn't have Java/C#-style field extraction)
                // -- the binding still exists per CLAUDE.md's "never
                // invent a binding, use the JetBrains one verbatim" rule;
                // it will commonly report "not available here" for Rust
                // code, which is correct, not a bug
                // (`docs/features/refactor-this.md` §1).
                binding: Some(Binding::same(KeyChord::new(Key::F).command().alt())),
                action: CommandAction::ExtractField,
            },
            Command {
                id: "Inline",
                title: "Inline",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::N).command().alt())),
                action: CommandAction::Inline,
            },
            Command {
                id: "GenerateMenu",
                title: "Generate",
                category: "Refactor",
                // A genuine two-chord split, not a modifier substitution --
                // JetBrains' real Generate binding is `⌘N` on mac,
                // `Alt+Insert` (not `Ctrl+N`) elsewhere
                // (`docs/features/code-generation.md` §2.2).
                binding: Some(Binding {
                    mac: KeyChord::new(Key::N).command(),
                    other: KeyChord::new(Key::Insert).alt(),
                }),
                action: CommandAction::GenerateMenu,
            },
            Command {
                id: "ImplementMethods",
                title: "Implement Methods",
                category: "Refactor",
                // Literal Control, not Cmd/Ctrl-abstracted `command()` --
                // same `ctrl()` shape `RefactorThis`'s own `⌃T` uses
                // (`docs/features/code-generation.md` §2.2).
                binding: Some(Binding::same(KeyChord::new(Key::I).ctrl())),
                action: CommandAction::ImplementMethods,
            },
            Command {
                id: "OverrideMethods",
                title: "Override Methods",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::O).ctrl())),
                action: CommandAction::OverrideMethods,
            },
            Command {
                id: "CreateTest",
                title: "Create Test",
                category: "Refactor",
                binding: Some(Binding::same(KeyChord::new(Key::T).command().shift())),
                action: CommandAction::CreateTest,
            },
            Command {
                id: "OptimizeImports",
                title: "Optimize Imports",
                category: "Refactor",
                // No default binding on either platform -- verified
                // against JetBrains' own docs, not assumed via the usual
                // Cmd->Ctrl-substitution pattern (which would collide with
                // `GoToSymbol`'s own genuine `⌘⌥O`/`Ctrl+Alt+Shift+N`
                // binding above). Reachable via Find Action only
                // (`docs/features/code-generation.md` §2.2).
                binding: None,
                action: CommandAction::OptimizeImports,
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_unique_id() {
        let ids: Vec<&str> = commands().iter().map(|c| c.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len());
    }

    #[test]
    fn file_structure_is_command_f12_same_on_both_platforms() {
        let cmd = commands().iter().find(|c| c.id == "FileStructure").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(
            binding,
            Binding::same(KeyChord::new(egui::Key::F12).command())
        );
    }

    #[test]
    fn find_action_is_cmd_shift_a_not_cmd_shift_k() {
        let cmd = commands().iter().find(|c| c.id == "FindAction").unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::A);
        let m = chord.modifiers;
        assert!(m.command && m.shift && !m.alt && !m.ctrl);
    }

    /// Drives a real `egui::Context` through one pass with `key` pressed
    /// under `modifiers`, then reports whether `chord` matches the
    /// resulting `InputState` -- `InputState` and `Modifiers` have private
    /// fields outside `egui` itself, so a real pass through `Context` is
    /// the only way to construct one; `Event::ModifiersChanged` is what
    /// actually populates `InputState::modifiers` (`egui`'s own
    /// `begin_pass`), not the `modifiers` field on the `Key` event itself.
    fn pressed_after(chord: &KeyChord, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(modifiers),
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
            ],
            ..Default::default()
        });
        ctx.input(|i| chord.pressed(i))
    }

    #[test]
    fn pressed_matches_the_primary_modifier_even_when_the_backend_also_sets_raw_ctrl() {
        // On Windows/Linux, egui sets `ctrl` to the same value as `command`
        // (`Modifiers::command`'s own doc comment) -- a chord built with
        // only `.command()` (ctrl left false) must still match that, or it
        // would never fire on non-mac at all.
        let chord = KeyChord::new(egui::Key::S).command();
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        assert!(pressed_after(&chord, egui::Key::S, modifiers));
    }

    #[test]
    fn pressed_rejects_an_extra_modifier_not_in_the_chord() {
        // Cmd+F must not also fire when Cmd+Shift+F is what's actually
        // held (`in-buffer-find-replace.md` §7's disambiguation).
        let chord = KeyChord::new(egui::Key::F).command();
        let modifiers = egui::Modifiers {
            command: true,
            shift: true,
            ..egui::Modifiers::NONE
        };
        assert!(!pressed_after(&chord, egui::Key::F, modifiers));
    }

    #[test]
    fn replace_all_has_no_default_binding() {
        let replace_all = commands().iter().find(|c| c.id == "ReplaceAll").unwrap();
        assert!(replace_all.binding.is_none());
    }

    #[test]
    fn replace_in_path_is_cmd_shift_r_distinct_from_replace() {
        let replace = commands().iter().find(|c| c.id == "Replace").unwrap();
        let replace_in_path = commands().iter().find(|c| c.id == "ReplaceInPath").unwrap();
        let chord = replace_in_path.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::R);
        let m = chord.modifiers;
        assert!(m.command && m.shift && !m.alt && !m.ctrl);
        assert_ne!(chord, replace.binding.unwrap().mac);
    }

    #[test]
    fn find_and_find_in_path_are_distinct_chords() {
        let find = commands().iter().find(|c| c.id == "Find").unwrap();
        let find_in_path = commands().iter().find(|c| c.id == "FindInPath").unwrap();
        assert_ne!(find.binding.unwrap().mac, find_in_path.binding.unwrap().mac);
    }

    #[test]
    fn mac_label_uses_glyphs_in_jetbrains_order() {
        let chord = KeyChord::new(egui::Key::G).command().shift();
        assert_eq!(chord.label(true), "⇧⌘G");
    }

    #[test]
    fn non_mac_label_spells_modifiers_as_words() {
        let chord = KeyChord::new(egui::Key::G).command().shift();
        assert_eq!(chord.label(false), "Ctrl+Shift+G");
    }

    #[test]
    fn alt_only_chord_has_no_command_glyph() {
        let chord = KeyChord::new(egui::Key::F7).alt();
        assert_eq!(chord.label(true), "⌥F7");
        assert_eq!(chord.label(false), "Alt+F7");
    }

    #[test]
    fn binding_same_resolves_identically_on_both_platform_branches() {
        let binding = Binding::same(KeyChord::new(egui::Key::S).command());
        assert_eq!(binding.mac, binding.other);
        assert_eq!(binding.for_platform(), binding.mac);
    }

    #[test]
    fn quick_documentation_binding_genuinely_diverges_between_mac_and_other() {
        // The one command in this registry where `mac`/`other` are not the
        // same chord (CLAUDE.md's own named example: `F1` vs `Ctrl+Q`).
        let cmd = commands()
            .iter()
            .find(|c| c.id == "QuickDocumentation")
            .unwrap();
        let binding = cmd.binding.unwrap();
        assert_ne!(binding.mac, binding.other);
        assert_eq!(binding.mac.key, egui::Key::F1);
        assert!(binding.mac.modifiers == egui::Modifiers::NONE);
        assert_eq!(binding.other.key, egui::Key::Q);
        assert!(binding.other.modifiers.ctrl);
    }

    #[test]
    fn tool_window_bindings_match_the_roadmap_5_2_numbering() {
        let expect = [
            ("ToggleProjectToolWindow", egui::Key::Num1),
            ("ToggleFindToolWindow", egui::Key::Num3),
            ("ToggleRunToolWindow", egui::Key::Num4),
            ("ToggleProblemsToolWindow", egui::Key::Num6),
            ("ToggleVcsToolWindow", egui::Key::Num9),
        ];
        for (id, key) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            let chord = cmd.binding.unwrap().mac;
            assert_eq!(chord.key, key);
            assert!(chord.modifiers.command && !chord.modifiers.shift && !chord.modifiers.alt);
        }
    }

    #[test]
    fn debug_bookmarks_and_run_configs_windows_are_entirely_absent_from_the_registry() {
        // ⌘2/⌘5/⌘7/⌘8 have no tool window to bind to yet -- absent, not
        // present-with-no-binding (doc §2.3/§4.6).
        for id in ["ToggleBookmarksToolWindow", "ToggleDebugToolWindow"] {
            assert!(commands().iter().all(|c| c.id != id));
        }
        let bound_keys: Vec<egui::Key> = commands()
            .iter()
            .filter_map(|c| c.binding)
            .map(|b| b.mac.key)
            .collect();
        assert!(!bound_keys.contains(&egui::Key::Num2));
        assert!(!bound_keys.contains(&egui::Key::Num5));
        assert!(!bound_keys.contains(&egui::Key::Num7));
        assert!(!bound_keys.contains(&egui::Key::Num8));
    }

    #[test]
    fn palette_only_toggle_commands_have_no_default_binding() {
        for id in [
            "ToggleTheme",
            "RefreshTree",
            "ToggleSmartMode",
            "ToggleClaudeToolWindow",
            "ToggleZenMode",
            "ShowLanguageSettings",
            "ShowKeymapSettings",
            "ToggleFormatOnSave",
        ] {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            assert!(cmd.binding.is_none(), "{id} should have no default binding");
        }
    }

    #[test]
    fn cargo_command_bindings_are_distinct_and_match_expected_chords() {
        // (id, key, command, ctrl, alt, shift)
        let expect = [
            ("CargoBuild", egui::Key::F9, true, false, false, false),
            ("CargoRun", egui::Key::R, false, true, false, false),
            ("CargoTest", egui::Key::R, false, true, false, true),
            ("CargoCheck", egui::Key::F9, true, false, false, true),
            ("CargoClippy", egui::Key::F9, true, false, true, false),
            ("CargoFmt", egui::Key::F, false, true, false, true),
        ];
        for (id, key, command, ctrl, alt, shift) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            let binding = cmd
                .binding
                .unwrap_or_else(|| panic!("{id} should have a binding"));
            assert_eq!(binding.mac.key, key, "{id} key");
            assert_eq!(binding.mac.modifiers.command, command, "{id} command");
            assert_eq!(binding.mac.modifiers.ctrl, ctrl, "{id} ctrl");
            assert_eq!(binding.mac.modifiers.alt, alt, "{id} alt");
            assert_eq!(binding.mac.modifiers.shift, shift, "{id} shift");
            assert_eq!(
                binding,
                Binding::same(binding.mac),
                "{id} should be Binding::same"
            );
        }

        let bound: Vec<KeyChord> = expect
            .iter()
            .map(|(id, ..)| {
                commands()
                    .iter()
                    .find(|c| c.id == *id)
                    .unwrap()
                    .binding
                    .unwrap()
                    .mac
            })
            .collect();
        for i in 0..bound.len() {
            for j in (i + 1)..bound.len() {
                assert_ne!(
                    bound[i], bound[j],
                    "cargo bindings must be pairwise distinct"
                );
            }
        }
    }

    #[test]
    fn fold_command_bindings_match_code_folding_2_4() {
        let expect = [
            ("CollapseFold", egui::Key::Minus, false),
            ("ExpandFold", egui::Key::Plus, false),
            ("CollapseAllFolds", egui::Key::Minus, true),
            ("ExpandAllFolds", egui::Key::Plus, true),
        ];
        for (id, key, shift) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            let chord = cmd.binding.unwrap().mac;
            assert_eq!(chord.key, key);
            assert!(chord.modifiers.command && !chord.modifiers.alt);
            assert_eq!(chord.modifiers.shift, shift);
        }
    }

    #[test]
    fn show_usages_is_rebound_to_cmd_option_f7_not_cmd_b() {
        let cmd = commands().iter().find(|c| c.id == "ShowUsages").unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::F7);
        assert!(chord.modifiers.command && chord.modifiers.alt && !chord.modifiers.shift);
    }

    #[test]
    fn go_to_declaration_reuses_show_usages_old_cmd_b_binding() {
        let cmd = commands()
            .iter()
            .find(|c| c.id == "GoToDeclaration")
            .unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::B);
        assert!(chord.modifiers.command && !chord.modifiers.alt && !chord.modifiers.shift);
    }

    #[test]
    fn go_to_implementation_is_cmd_option_b() {
        let cmd = commands()
            .iter()
            .find(|c| c.id == "GoToImplementation")
            .unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::B);
        assert!(chord.modifiers.command && chord.modifiers.alt && !chord.modifiers.shift);
    }

    #[test]
    fn reformat_code_is_cmd_option_l() {
        let cmd = commands().iter().find(|c| c.id == "ReformatCode").unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::L);
        assert!(chord.modifiers.command && chord.modifiers.alt && !chord.modifiers.shift);
        assert_eq!(
            cmd.binding.unwrap(),
            Binding::same(chord),
            "ReformatCode should be Binding::same (mechanical Cmd->Ctrl substitution)"
        );
    }

    #[test]
    fn go_to_type_declaration_is_literal_ctrl_shift_b_not_cmd_shift_b() {
        let cmd = commands()
            .iter()
            .find(|c| c.id == "GoToTypeDeclaration")
            .unwrap();
        let chord = cmd.binding.unwrap().mac;
        assert_eq!(chord.key, egui::Key::B);
        assert!(chord.modifiers.ctrl && chord.modifiers.shift && !chord.modifiers.alt);
        assert!(!chord.modifiers.command);
    }

    #[test]
    fn navigate_back_and_forward_are_cmd_option_arrow_keys() {
        let expect = [
            ("NavigateBack", egui::Key::ArrowLeft),
            ("NavigateForward", egui::Key::ArrowRight),
        ];
        for (id, key) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            let chord = cmd.binding.unwrap().mac;
            assert_eq!(chord.key, key);
            assert!(chord.modifiers.command && chord.modifiers.alt && !chord.modifiers.shift);
        }
    }

    /// A real, driven-through-`egui::Context` proof that `GoToTypeDeclaration`'s
    /// literal-Ctrl chord actually matches physical Ctrl+Shift+B input on a
    /// simulated non-mac backend, which also mirrors `command` to `true`
    /// (`Modifiers::command`'s own doc comment) -- exercising the exact
    /// `cmd_ctrl_matches` interaction `ctrl()`'s doc comment reasons about,
    /// not just asserting the stored chord's raw fields.
    #[test]
    fn go_to_type_declaration_matches_ctrl_shift_b_even_with_command_mirrored() {
        let chord = commands()
            .iter()
            .find(|c| c.id == "GoToTypeDeclaration")
            .unwrap()
            .binding
            .unwrap()
            .mac;
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            shift: true,
            ..egui::Modifiers::NONE
        };
        assert!(pressed_after(&chord, egui::Key::B, modifiers));
    }

    #[test]
    fn run_cargo_payload_carries_the_right_subcommand() {
        let expect = [
            ("CargoBuild", CargoCommand::Build),
            ("CargoRun", CargoCommand::Run),
            ("CargoTest", CargoCommand::Test),
            ("CargoCheck", CargoCommand::Check),
            ("CargoClippy", CargoCommand::Clippy),
            ("CargoFmt", CargoCommand::Fmt),
        ];
        for (id, expected) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            assert_eq!(cmd.action, CommandAction::RunCargo(expected));
        }
    }

    #[test]
    fn go_to_file_mac_and_other_genuinely_diverge() {
        let cmd = commands().iter().find(|c| c.id == "GoToFile").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(binding.mac.key, egui::Key::O);
        assert!(binding.mac.modifiers.command && binding.mac.modifiers.shift);
        assert_eq!(binding.other.key, egui::Key::N);
        assert!(binding.other.modifiers.ctrl && binding.other.modifiers.shift);
        assert_ne!(binding.mac, binding.other);
    }

    #[test]
    fn go_to_class_mac_and_other_genuinely_diverge() {
        let cmd = commands().iter().find(|c| c.id == "GoToClass").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(binding.mac.key, egui::Key::O);
        assert!(
            binding.mac.modifiers.command
                && !binding.mac.modifiers.shift
                && !binding.mac.modifiers.alt
        );
        assert_eq!(binding.other.key, egui::Key::N);
        assert!(binding.other.modifiers.ctrl && !binding.other.modifiers.shift);
        assert_ne!(binding.mac, binding.other);
    }

    #[test]
    fn go_to_symbol_mac_and_other_genuinely_diverge() {
        let cmd = commands().iter().find(|c| c.id == "GoToSymbol").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(binding.mac.key, egui::Key::O);
        assert!(binding.mac.modifiers.command && binding.mac.modifiers.alt);
        assert_eq!(binding.other.key, egui::Key::N);
        assert!(
            binding.other.modifiers.ctrl
                && binding.other.modifiers.alt
                && binding.other.modifiers.shift
        );
        assert_ne!(binding.mac, binding.other);
    }

    #[test]
    fn go_to_line_mac_and_other_genuinely_diverge() {
        let cmd = commands().iter().find(|c| c.id == "GoToLine").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(binding.mac.key, egui::Key::L);
        assert!(binding.mac.modifiers.command);
        assert_eq!(binding.other.key, egui::Key::G);
        assert!(binding.other.modifiers.ctrl);
        assert_ne!(binding.mac, binding.other);
    }

    #[test]
    fn go_to_file_class_symbol_line_are_pairwise_distinct_on_mac() {
        let ids = ["GoToFile", "GoToClass", "GoToSymbol", "GoToLine"];
        let chords: Vec<KeyChord> = ids
            .iter()
            .map(|id| commands().iter().find(|c| c.id == *id).unwrap())
            .map(|c| c.binding.unwrap().mac)
            .collect();
        for i in 0..chords.len() {
            for j in (i + 1)..chords.len() {
                assert_ne!(chords[i], chords[j], "{} and {} collide", ids[i], ids[j]);
            }
        }
    }

    #[test]
    fn generate_menu_mac_and_other_genuinely_diverge() {
        let cmd = commands().iter().find(|c| c.id == "GenerateMenu").unwrap();
        let binding = cmd.binding.unwrap();
        assert_eq!(binding.mac.key, egui::Key::N);
        assert!(binding.mac.modifiers.command && !binding.mac.modifiers.alt);
        assert_eq!(binding.other.key, egui::Key::Insert);
        assert!(binding.other.modifiers.alt && !binding.other.modifiers.command);
        assert_ne!(binding.mac, binding.other);
    }

    #[test]
    fn generate_family_direct_commands_bindings_match_expected_chords() {
        // (id, key, command, ctrl, alt, shift)
        let expect = [
            ("ImplementMethods", egui::Key::I, false, true, false, false),
            ("OverrideMethods", egui::Key::O, false, true, false, false),
            ("CreateTest", egui::Key::T, true, false, false, true),
        ];
        for (id, key, command, ctrl, alt, shift) in expect {
            let cmd = commands().iter().find(|c| c.id == id).unwrap();
            let binding = cmd
                .binding
                .unwrap_or_else(|| panic!("{id} should have a binding"));
            assert_eq!(binding.mac.key, key, "{id} key");
            assert_eq!(binding.mac.modifiers.command, command, "{id} command");
            assert_eq!(binding.mac.modifiers.ctrl, ctrl, "{id} ctrl");
            assert_eq!(binding.mac.modifiers.alt, alt, "{id} alt");
            assert_eq!(binding.mac.modifiers.shift, shift, "{id} shift");
            assert_eq!(
                binding,
                Binding::same(binding.mac),
                "{id} should be Binding::same"
            );
        }
    }

    #[test]
    fn optimize_imports_has_no_default_binding() {
        let cmd = commands()
            .iter()
            .find(|c| c.id == "OptimizeImports")
            .unwrap();
        assert!(cmd.binding.is_none());
    }

    #[test]
    fn no_two_commands_in_the_registry_share_the_same_default_mac_chord() {
        let bound: Vec<(&str, KeyChord)> = commands()
            .iter()
            .filter_map(|c| c.binding.map(|b| (c.id, b.mac)))
            .collect();
        for i in 0..bound.len() {
            for j in (i + 1)..bound.len() {
                assert_ne!(
                    bound[i].1, bound[j].1,
                    "{} and {} share the same default mac chord",
                    bound[i].0, bound[j].0
                );
            }
        }
    }
}
