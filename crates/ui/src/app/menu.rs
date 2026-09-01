//! Native macOS menu bar (`docs/features/native-menu-bar.md`). A submodule
//! of `app` (not a top-level sibling module) specifically so it can call
//! `IdeApp::run_command`, a private `fn` visible to descendant modules of
//! `app` but not to an unrelated module -- the same reason `app::render`
//! is structured this way.

use crate::command::{commands, CommandAction};

/// One native menu. `items` is in display order; `None` renders as a
/// separator. Pure data -- `menu_groups_reference_only_real_commands`
/// (below) checks every id against `crate::command::commands()` without
/// ever constructing a real `muda::Menu`.
pub(super) struct MenuGroup {
    pub title: &'static str,
    pub items: &'static [Option<&'static str>],
}

/// `docs/features/native-menu-bar.md` §3.2's exact table. Deliberately a
/// different grouping than `Command::category` (that grouping is for the
/// command palette's own sections; this one follows real macOS/JetBrains
/// menu-bar convention -- e.g. tool-window toggles live under View here,
/// not Window).
const MENU_GROUPS: &[MenuGroup] = &[
    MenuGroup {
        title: "File",
        items: &[Some("SaveAll"), Some("RefreshTree")],
    },
    MenuGroup {
        title: "Edit",
        items: &[
            Some("Undo"),
            Some("Redo"),
            None,
            Some("Find"),
            Some("Replace"),
            Some("ReplaceAll"),
            Some("ReplaceInPath"),
            Some("FindNext"),
            Some("FindPrevious"),
            None,
            Some("CollapseFold"),
            Some("ExpandFold"),
            Some("CollapseAllFolds"),
            Some("ExpandAllFolds"),
            None,
            Some("ReformatCode"),
            Some("ToggleFormatOnSave"),
            None,
            Some("Rename"),
            Some("RefactorThis"),
            Some("ExtractVariable"),
            Some("ExtractMethod"),
            Some("ExtractConstant"),
            Some("ExtractField"),
            Some("Inline"),
            None,
            Some("GenerateMenu"),
            Some("ImplementMethods"),
            Some("OverrideMethods"),
            Some("CreateTest"),
            Some("OptimizeImports"),
        ],
    },
    MenuGroup {
        title: "View",
        items: &[
            Some("ToggleTheme"),
            Some("ToggleZenMode"),
            None,
            Some("ToggleProjectToolWindow"),
            Some("ToggleFindToolWindow"),
            Some("ToggleRunToolWindow"),
            Some("ToggleProblemsToolWindow"),
            Some("ToggleVcsToolWindow"),
            Some("ToggleClaudeToolWindow"),
        ],
    },
    MenuGroup {
        title: "Go",
        items: &[
            Some("GoToFile"),
            Some("GoToClass"),
            Some("GoToSymbol"),
            Some("GoToLine"),
            Some("FileStructure"),
            Some("RecentFiles"),
            Some("RecentLocations"),
            None,
            Some("GoToDeclaration"),
            Some("GoToImplementation"),
            Some("GoToTypeDeclaration"),
            None,
            Some("NavigateBack"),
            Some("NavigateForward"),
            None,
            Some("FindUsages"),
            Some("ShowUsages"),
            Some("FindInPath"),
            Some("FindAction"),
            None,
            Some("QuickDocumentation"),
            Some("ShowIntentionActions"),
            None,
            Some("ToggleSmartMode"),
        ],
    },
    MenuGroup {
        title: "Git",
        items: &[Some("GitBranches"), Some("ToggleBlameAnnotations")],
    },
    MenuGroup {
        title: "Window",
        items: &[Some("NextTab"), Some("PreviousTab"), Some("CloseTab")],
    },
];

/// The app-name menu's non-predefined items (Languages…/Keymap…),
/// inserted before the predefined Services/Hide/Quit block (§3.2).
const APP_MENU_ITEMS: &[&str] = &["ShowLanguageSettings", "ShowKeymapSettings"];

pub(super) fn menu_groups() -> &'static [MenuGroup] {
    MENU_GROUPS
}

/// Pure lookup: the `CommandAction` a native menu item with this id
/// should dispatch, or `None` for an id this app doesn't recognize (a
/// `PredefinedMenuItem` click -- About/Quit/Hide/Minimize/etc -- never
/// reaches this function at all, since macOS handles those itself and
/// never emits a `MenuEvent` for them). Separated from `poll_menu_events`
/// so the dispatch logic is testable without any `muda`/AppKit call.
fn command_action_for(id: &str) -> Option<CommandAction> {
    commands().iter().find(|c| c.id == id).map(|c| c.action)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{command_action_for, menu_groups, APP_MENU_ITEMS};
    use crate::command::commands;
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    /// Builds the real menu from `MENU_GROUPS`/`APP_MENU_ITEMS` and
    /// attaches it via `init_for_nsapp()` (`docs/features/
    /// native-menu-bar.md` §3.1 -- confirmed working, called once from
    /// `IdeApp::new`). Panicking on a `menu_groups()` id that isn't in
    /// `commands()` is correct: that's a programmer error in this file,
    /// caught by `menu_groups_reference_only_real_commands` long before
    /// it ships, not a runtime condition to handle gracefully.
    pub(in crate::app) fn install_native_menu() {
        let menu = Menu::new();

        let app_menu = Submenu::new("ide", true);
        app_menu
            .append(&PredefinedMenuItem::about(Some("ide"), None))
            .expect("append About");
        app_menu
            .append(&PredefinedMenuItem::separator())
            .expect("append separator");
        for id in APP_MENU_ITEMS {
            app_menu.append(&menu_item_for(id)).expect("append item");
        }
        app_menu
            .append(&PredefinedMenuItem::services(None))
            .expect("append Services");
        app_menu
            .append(&PredefinedMenuItem::separator())
            .expect("append separator");
        app_menu
            .append(&PredefinedMenuItem::hide(None))
            .expect("append Hide");
        app_menu
            .append(&PredefinedMenuItem::hide_others(None))
            .expect("append Hide Others");
        app_menu
            .append(&PredefinedMenuItem::show_all(None))
            .expect("append Show All");
        app_menu
            .append(&PredefinedMenuItem::separator())
            .expect("append separator");
        app_menu
            .append(&PredefinedMenuItem::quit(None))
            .expect("append Quit");
        menu.append(&app_menu).expect("append app menu");

        for group in menu_groups() {
            let submenu = Submenu::new(group.title, true);
            for item in group.items {
                match item {
                    Some(id) => submenu.append(&menu_item_for(id)).expect("append item"),
                    None => submenu
                        .append(&PredefinedMenuItem::separator())
                        .expect("append separator"),
                }
            }
            if group.title == "Window" {
                submenu
                    .append(&PredefinedMenuItem::separator())
                    .expect("append separator");
                submenu
                    .append(&PredefinedMenuItem::minimize(None))
                    .expect("append Minimize");
                submenu
                    .append(&PredefinedMenuItem::fullscreen(None))
                    .expect("append Fullscreen");
            }
            menu.append(&submenu).expect("append menu");
        }

        menu.init_for_nsapp();
    }

    fn menu_item_for(id: &'static str) -> MenuItem {
        let command = commands()
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("menu.rs references unknown command id {id:?}"));
        MenuItem::with_id(command.id, command.title, true, None)
    }

    /// Drains at most one event from `MenuEvent::receiver()` (a global
    /// channel, not tied to the `Menu` built above) and dispatches it via
    /// `command_action_for`. Returns whether an event was handled, the
    /// same shape every other per-frame `.poll()` in this crate uses.
    pub(in crate::app) fn poll_menu_event() -> Option<super::CommandAction> {
        let event = MenuEvent::receiver().try_recv().ok()?;
        command_action_for(event.id().as_ref())
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(in crate::app) fn install_native_menu() {}

    pub(in crate::app) fn poll_menu_event() -> Option<super::CommandAction> {
        None
    }
}

pub(super) fn install_native_menu() {
    platform::install_native_menu();
}

impl super::IdeApp {
    /// `docs/features/native-menu-bar.md` §2.2 -- called once per frame
    /// from `IdeApp::ui`, alongside `self.lsp.poll()`/`self.cargo.poll()`/
    /// etc. Always safe to call on any platform: a no-op returning `false`
    /// off macOS (§3.4).
    pub(super) fn poll_menu_events(&mut self, ctx: &egui::Context) -> bool {
        match platform::poll_menu_event() {
            Some(action) => {
                self.run_command(action, ctx);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_groups_reference_only_real_commands() {
        let ids: Vec<&str> = commands().iter().map(|c| c.id).collect();
        for group in menu_groups() {
            for id in group.items.iter().flatten() {
                assert!(
                    ids.contains(id),
                    "menu group {:?} references unknown command id {id:?}",
                    group.title
                );
            }
        }
        for id in APP_MENU_ITEMS {
            assert!(
                ids.contains(id),
                "app menu references unknown command id {id:?}"
            );
        }
    }

    #[test]
    fn menu_groups_never_references_a_build_category_command() {
        // Deliberate v1 scope cut (`native-menu-bar.md` §1): Build-category
        // commands stay reachable only via the cargo panel/toolbar/palette.
        let build_ids: Vec<&str> = commands()
            .iter()
            .filter(|c| c.category == "Build")
            .map(|c| c.id)
            .collect();
        for group in menu_groups() {
            for item in group.items.iter().flatten() {
                assert!(
                    !build_ids.contains(item),
                    "menu group {:?} unexpectedly references Build command {item:?}",
                    group.title
                );
            }
        }
    }

    #[test]
    fn every_non_build_command_appears_in_the_native_menu_exactly_once() {
        let mut menu_ids: Vec<&str> = menu_groups()
            .iter()
            .flat_map(|g| g.items.iter().flatten().copied())
            .collect();
        menu_ids.extend(APP_MENU_ITEMS.iter().copied());
        menu_ids.sort_unstable();

        let mut expected: Vec<&str> = commands()
            .iter()
            .filter(|c| c.category != "Build")
            .map(|c| c.id)
            .collect();
        expected.sort_unstable();

        assert_eq!(menu_ids, expected);
    }

    #[test]
    fn command_action_for_resolves_a_known_id() {
        assert_eq!(command_action_for("SaveAll"), Some(CommandAction::SaveAll));
    }

    #[test]
    fn command_action_for_is_none_for_an_unknown_id() {
        // Covers both a typo'd command id and a click on a
        // `PredefinedMenuItem` (About/Quit/Hide/...), neither of which
        // maps to a `CommandAction`.
        assert_eq!(command_action_for("save_all"), None);
        assert_eq!(command_action_for("about"), None);
    }
}
