//! Per-user keybinding customisation layered over `command::commands()`'s
//! defaults (`docs/features/keymap.md`): preset schemes (JetBrains macOS /
//! Fleet / VS Code) as alternate default tables, a user-override map on top
//! of whichever scheme is active, conflict detection, and a hand-rolled
//! export/import text format. No `IdeApp` dependency, same shape as
//! `command.rs` -- depends on it (reads `command::commands()`), not the
//! reverse.

use crate::command::{self, Binding, KeyChord};
use std::collections::BTreeMap;

/// A whole-registry default-binding table, selected by the user
/// (`docs/features/keymap.md` §3.2) independently of any per-command
/// override.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum KeymapScheme {
    #[default]
    JetBrainsMacOs,
    Fleet,
    VsCode,
}

impl KeymapScheme {
    pub const ALL: [KeymapScheme; 3] = [Self::JetBrainsMacOs, Self::Fleet, Self::VsCode];

    /// Settings-window label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::JetBrainsMacOs => "JetBrains macOS",
            Self::Fleet => "Fleet",
            Self::VsCode => "VS Code",
        }
    }

    /// Export/import token (`docs/features/keymap.md` §3.4's `scheme`
    /// line) -- deliberately not `label()`, whose text is meant to change
    /// freely for display without breaking a previously-exported file.
    fn token(&self) -> &'static str {
        match self {
            Self::JetBrainsMacOs => "JetBrainsMacOs",
            Self::Fleet => "Fleet",
            Self::VsCode => "VsCode",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "JetBrainsMacOs" => Some(Self::JetBrainsMacOs),
            "Fleet" => Some(Self::Fleet),
            "VsCode" => Some(Self::VsCode),
            _ => None,
        }
    }

    /// This scheme's own default for `id`, or `None` if this scheme leaves
    /// the command unbound. `JetBrainsMacOs` always equals `command::
    /// commands()`'s own `binding` field for `id` -- it is not a second,
    /// hand-copied table that could drift from B3's, it *is* B3's table,
    /// looked up by id.
    pub fn default_binding(&self, id: &str) -> Option<Binding> {
        match self {
            Self::JetBrainsMacOs => command::commands()
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.binding),
            Self::Fleet => fleet_binding(id),
            Self::VsCode => vscode_binding(id),
        }
    }
}

/// Sourced from JetBrains' own official Fleet keymap reference PDF
/// (`docs/features/keymap.md` §5). `ShowUsages` is deliberately absent:
/// Fleet has one Find Usages action, not a popup/panel split, so it maps
/// onto `FindUsages` instead (§3.3). `SaveAll`/`Undo`/`Redo` bind to the
/// same chord JetBrains macOS uses even though Fleet's reference card
/// omits them -- that omission means "not Fleet-specific", not "inert"
/// (§3.3's revision note explains why the opposite reading was wrong).
fn fleet_binding(id: &str) -> Option<Binding> {
    use egui::Key;
    match id {
        "SaveAll" => Some(Binding::same(KeyChord::new(Key::S).command())),
        "Undo" => Some(Binding::same(KeyChord::new(Key::Z).command())),
        "Redo" => Some(Binding::same(KeyChord::new(Key::Z).command().shift())),
        "Find" => Some(Binding::same(KeyChord::new(Key::F).command())),
        "Replace" => Some(Binding::same(KeyChord::new(Key::F).command().alt())),
        "FindNext" => Some(Binding::same(KeyChord::new(Key::G).command())),
        "FindPrevious" => Some(Binding::same(KeyChord::new(Key::G).command().shift())),
        "FindInPath" => Some(Binding::same(KeyChord::new(Key::F).command().shift())),
        "FindUsages" => Some(Binding::same(KeyChord::new(Key::U).command())),
        "ShowUsages" => None,
        "FindAction" => Some(Binding::same(KeyChord::new(Key::K).command().shift())),
        _ => None,
    }
}

/// Sourced from VS Code's own official default-keybindings reference page
/// (`docs/features/keymap.md` §5). `ShowUsages` is absent for the same
/// asymmetry reason as Fleet's table: VS Code's closest default to "Find
/// Usages" is its peek/"Go to References" view, with no separate
/// default-bound panel variant. `SaveAll` maps to VS Code's plain Save
/// (`⌘S`), not VS Code's own, differently-scoped Save All (`⌥⌘S`) -- this
/// app's `SaveAll` only ever saves the active tab (§3.3).
fn vscode_binding(id: &str) -> Option<Binding> {
    use egui::Key;
    match id {
        "SaveAll" => Some(Binding::same(KeyChord::new(Key::S).command())),
        "Undo" => Some(Binding::same(KeyChord::new(Key::Z).command())),
        "Redo" => Some(Binding::same(KeyChord::new(Key::Z).command().shift())),
        "Find" => Some(Binding::same(KeyChord::new(Key::F).command())),
        "Replace" => Some(Binding::same(KeyChord::new(Key::F).command().alt())),
        "FindNext" => Some(Binding::same(KeyChord::new(Key::G).command())),
        "FindPrevious" => Some(Binding::same(KeyChord::new(Key::G).command().shift())),
        "FindInPath" => Some(Binding::same(KeyChord::new(Key::F).command().shift())),
        "FindUsages" => Some(Binding::same(KeyChord::new(Key::F12).shift())),
        "ShowUsages" => None,
        "FindAction" => Some(Binding::same(KeyChord::new(Key::P).command().shift())),
        _ => None,
    }
}

/// A trigger shape `egui` has no primitive for, whose effect lives inside
/// a specific widget's own per-frame state rather than `IdeApp::
/// run_command` (`docs/features/keymap.md` §2.3/§6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GestureTrigger {
    /// Two presses of `modifier` within `editor::double_tap::
    /// DOUBLE_TAP_WINDOW` arm the gesture (`editor::double_tap::
    /// DoubleTap`, implemented in A3).
    DoubleTap { modifier: egui::Modifiers },
    /// `prefix` pressed, then `key` alone within `ACCORD_ARMED_WINDOW`
    /// (`Accord`, below). No command uses this shape yet -- kept for D2's
    /// Refactor This (`docs/roadmap.md`'s own binding table), same reason
    /// `Accord` itself is kept unconsumed this phase.
    #[allow(dead_code)]
    Accord { prefix: KeyChord, key: egui::Key },
}

impl GestureTrigger {
    /// Settings-window label, mirroring `KeyChord::label`'s mac-style glyph
    /// row for the `DoubleTap` case.
    pub fn label(&self, mac_style: bool) -> String {
        match self {
            Self::DoubleTap { modifier } => {
                let glyph = if mac_style && modifier.alt {
                    "⌥⌥"
                } else if modifier.alt {
                    "Alt Alt"
                } else if mac_style && modifier.shift {
                    "⇧⇧"
                } else {
                    "Shift Shift"
                };
                glyph.to_string()
            }
            Self::Accord { prefix, key } => {
                format!("{} then {:?}", prefix.label(mac_style), key)
            }
        }
    }
}

/// A registry entry for a gesture, for display in the Keymap settings
/// window (`docs/features/keymap.md` §3.6) -- **not** dispatched from
/// here; each gesture's own widget-local code still owns detection and
/// effect, unchanged by this phase.
#[derive(Debug)]
pub struct Gesture {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub default: GestureTrigger,
}

/// Every gesture-triggered action this build knows about: the existing A3
/// clone-caret gesture, and (C2) `⇧⇧` Search Everywhere
/// (`docs/features/search-everywhere.md` §3.5).
pub fn gestures() -> &'static [Gesture] {
    const GESTURES: &[Gesture] = &[
        Gesture {
            id: "CloneCaretUpDown",
            title: "Clone Caret Above/Below",
            category: "Edit",
            default: GestureTrigger::DoubleTap {
                modifier: egui::Modifiers::ALT,
            },
        },
        Gesture {
            id: "SearchEverywhere",
            title: "Search Everywhere",
            category: "Navigate",
            default: GestureTrigger::DoubleTap {
                modifier: egui::Modifiers::SHIFT,
            },
        },
    ];
    GESTURES
}

/// How long an accord's prefix chord stays armed, waiting for the second
/// key. Mirrors `editor::double_tap::ARMED_WINDOW`'s role exactly.
pub const ACCORD_ARMED_WINDOW: f64 = 1.0;

/// `prefix` held+released, then a bare key within `ACCORD_ARMED_WINDOW`,
/// triggers an accord (`docs/roadmap.md` §5.1's `⌃T`→letter). Mirrors
/// `editor::double_tap::DoubleTap`'s shape: fed frame time, not a timer,
/// so every rule is testable without a clock. Lives here rather than
/// `editor/`, since an accord's second key can target any part of the UI,
/// not just the editor widget.
///
/// No call site exists yet -- `docs/roadmap.md`'s own binding table
/// assigns `⌃T`'s first real consumer (Refactor This) to a later,
/// unscheduled phase (D2), so this type is intentionally unconsumed by
/// `ide-ui` right now; `#[allow(dead_code)]` reflects that on purpose
/// (`docs/features/keymap.md` §2.4's revision notes) rather than the
/// implementer discovering the lint failure and deleting a roadmap-
/// mandated deliverable to silence it.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Accord {
    armed_until: Option<f64>,
}

#[allow(dead_code)]
impl Accord {
    /// Call once the prefix chord is detected pressed this frame. Arms for
    /// `ACCORD_ARMED_WINDOW`.
    pub fn arm(&mut self, now: f64) {
        self.armed_until = Some(now + ACCORD_ARMED_WINDOW);
    }

    pub fn is_armed(&self, now: f64) -> bool {
        self.armed_until.is_some_and(|until| now <= until)
    }

    /// Always disarms -- unlike `DoubleTap::disarm`, an accord's prefix
    /// isn't "half of the next accord" the way a double-tap's first press
    /// is, so nothing is preserved across this call.
    pub fn disarm(&mut self) {
        self.armed_until = None;
    }
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub skipped_unknown_ids: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("unsupported keymap file version: {0:?}")]
    UnsupportedVersion(String),
    #[error("malformed line {line}: {text:?}")]
    MalformedLine { line: usize, text: String },
    #[error("unknown key name {0:?}")]
    UnknownKey(String),
}

/// A user's whole keybinding customisation: which preset scheme to fall
/// back to, plus per-command overrides on top of it
/// (`docs/features/keymap.md` §2.5).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct KeymapOverlay {
    pub scheme: KeymapScheme,
    overrides: BTreeMap<String, Option<Binding>>,
}

impl KeymapOverlay {
    /// `overrides.get(id)`, including an explicit `Some(None)` (user
    /// turned the default off), else `scheme.default_binding(id)`.
    pub fn effective_binding(&self, id: &str) -> Option<Binding> {
        match self.overrides.get(id) {
            Some(over) => *over,
            None => self.scheme.default_binding(id),
        }
    }

    /// Explicit assignment -- `Some(binding)` to bind, `None` to unbind.
    pub fn set_override(&mut self, id: &str, binding: Option<Binding>) {
        self.overrides.insert(id.to_string(), binding);
    }

    /// Removes `id`'s override, falling back to `scheme`'s default again.
    pub fn reset(&mut self, id: &str) {
        self.overrides.remove(id);
    }

    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    pub fn is_customized(&self, id: &str) -> bool {
        self.overrides.contains_key(id)
    }

    /// Every other registered command whose effective binding, resolved
    /// through this same overlay, resolves to the same platform chord as
    /// `proposed`. Non-empty does not block an assignment, it only warns
    /// (`docs/features/keymap.md` §4.4).
    pub fn conflicts(&self, id: &str, proposed: KeyChord) -> Vec<&'static str> {
        command::commands()
            .iter()
            .filter(|c| c.id != id)
            .filter(|c| {
                self.effective_binding(c.id)
                    .is_some_and(|b| b.for_platform() == proposed)
            })
            .map(|c| c.id)
            .collect()
    }

    /// Hand-rolled plain-text serialisation (`docs/features/keymap.md`
    /// §3.4) -- not a JSON crate: this format only ever needs to round-
    /// trip this one struct, and CLAUDE.md's approved-dependency table
    /// doesn't list one.
    pub fn export(&self) -> String {
        let mut out = String::new();
        out.push_str("keymap/1\n");
        out.push_str(&format!("scheme {}\n", self.scheme.token()));
        for (id, binding) in &self.overrides {
            match binding {
                Some(b) => {
                    let chord = b.mac;
                    let mods = mods_token(chord.modifiers);
                    if mods.is_empty() {
                        out.push_str(&format!("bind {} {}\n", id, chord.key.name()));
                    } else {
                        out.push_str(&format!("bind {} {} {}\n", id, chord.key.name(), mods));
                    }
                }
                None => out.push_str(&format!("unbind {id}\n")),
            }
        }
        out
    }

    /// Parses `export`'s format. Fully validates before mutating: on any
    /// malformed line, returns `Err` and leaves `self` untouched. An id
    /// `export` never produced (e.g. a file from a newer build) is not an
    /// error -- collected into `ImportReport::skipped_unknown_ids` instead
    /// of silently dropped.
    pub fn import(&mut self, text: &str) -> Result<ImportReport, ImportError> {
        let mut lines = text.lines().enumerate();
        let (_, first) = lines
            .next()
            .ok_or_else(|| ImportError::UnsupportedVersion(String::new()))?;
        if first.trim() != "keymap/1" {
            return Err(ImportError::UnsupportedVersion(first.trim().to_string()));
        }

        let known_ids: Vec<&str> = command::commands().iter().map(|c| c.id).collect();
        let mut scheme = KeymapScheme::default();
        let mut overrides = BTreeMap::new();
        let mut skipped_unknown_ids = Vec::new();

        for (idx, raw) in lines {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let line_no = idx + 1;
            let malformed = || ImportError::MalformedLine {
                line: line_no,
                text: line.to_string(),
            };
            let tokens: Vec<&str> = line.split_whitespace().collect();
            match tokens.as_slice() {
                ["scheme", name] => {
                    scheme = KeymapScheme::from_token(name).ok_or_else(malformed)?;
                }
                ["unbind", id] => {
                    if known_ids.contains(id) {
                        overrides.insert((*id).to_string(), None);
                    } else {
                        skipped_unknown_ids.push((*id).to_string());
                    }
                }
                ["bind", id, key_name] => {
                    let chord = parse_chord(key_name, egui::Modifiers::NONE)?;
                    if known_ids.contains(id) {
                        overrides.insert((*id).to_string(), Some(Binding::same(chord)));
                    } else {
                        skipped_unknown_ids.push((*id).to_string());
                    }
                }
                ["bind", id, key_name, mods] => {
                    let modifiers = parse_modifiers(mods).ok_or_else(malformed)?;
                    let chord = parse_chord(key_name, modifiers)?;
                    if known_ids.contains(id) {
                        overrides.insert((*id).to_string(), Some(Binding::same(chord)));
                    } else {
                        skipped_unknown_ids.push((*id).to_string());
                    }
                }
                _ => return Err(malformed()),
            }
        }

        self.scheme = scheme;
        self.overrides = overrides;
        Ok(ImportReport {
            skipped_unknown_ids,
        })
    }
}

fn parse_chord(key_name: &str, modifiers: egui::Modifiers) -> Result<KeyChord, ImportError> {
    let key = egui::Key::from_name(key_name)
        .ok_or_else(|| ImportError::UnknownKey(key_name.to_string()))?;
    Ok(KeyChord { key, modifiers })
}

fn parse_modifiers(spec: &str) -> Option<egui::Modifiers> {
    let mut m = egui::Modifiers::NONE;
    for tok in spec.split(',') {
        match tok {
            "command" => m.command = true,
            "shift" => m.shift = true,
            "alt" => m.alt = true,
            _ => return None,
        }
    }
    Some(m)
}

fn mods_token(m: egui::Modifiers) -> String {
    let mut parts = Vec::new();
    if m.command {
        parts.push("command");
    }
    if m.shift {
        parts.push("shift");
    }
    if m.alt {
        parts.push("alt");
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Key;

    #[test]
    fn jetbrains_scheme_matches_the_registry_exactly() {
        for cmd in command::commands() {
            assert_eq!(
                KeymapScheme::JetBrainsMacOs
                    .default_binding(cmd.id)
                    .map(|b| b.mac),
                cmd.binding.map(|b| b.mac),
                "mismatch for {}",
                cmd.id
            );
        }
    }

    #[test]
    fn fleet_and_vscode_leave_show_usages_unbound() {
        assert_eq!(KeymapScheme::Fleet.default_binding("ShowUsages"), None);
        assert_eq!(KeymapScheme::VsCode.default_binding("ShowUsages"), None);
    }

    #[test]
    fn fleet_binds_save_undo_redo_to_the_platform_standard_chord() {
        let save = KeymapScheme::Fleet.default_binding("SaveAll").unwrap().mac;
        assert_eq!(save.key, Key::S);
        assert!(save.modifiers.command);

        let undo = KeymapScheme::Fleet.default_binding("Undo").unwrap().mac;
        assert_eq!(undo.key, Key::Z);
        assert!(undo.modifiers.command && !undo.modifiers.shift);

        let redo = KeymapScheme::Fleet.default_binding("Redo").unwrap().mac;
        assert_eq!(redo.key, Key::Z);
        assert!(redo.modifiers.command && redo.modifiers.shift);
    }

    #[test]
    fn vscode_save_all_maps_to_plain_save_not_save_all() {
        let chord = KeymapScheme::VsCode.default_binding("SaveAll").unwrap().mac;
        assert_eq!(chord.key, Key::S);
        assert!(chord.modifiers.command && !chord.modifiers.alt);
    }

    #[test]
    fn vscode_find_usages_is_shift_f12() {
        let chord = KeymapScheme::VsCode
            .default_binding("FindUsages")
            .unwrap()
            .mac;
        assert_eq!(chord.key, Key::F12);
        assert!(chord.modifiers.shift && !chord.modifiers.command);
    }

    #[test]
    fn effective_binding_falls_back_to_scheme_default_with_no_override() {
        let overlay = KeymapOverlay::default();
        assert_eq!(
            overlay.effective_binding("SaveAll").map(|b| b.mac),
            command::commands()
                .iter()
                .find(|c| c.id == "SaveAll")
                .unwrap()
                .binding
                .map(|b| b.mac)
        );
    }

    #[test]
    fn explicit_unbind_overrides_a_present_scheme_default() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        assert_eq!(overlay.effective_binding("SaveAll"), None);
    }

    #[test]
    fn reset_falls_back_to_the_scheme_default_again() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        overlay.reset("SaveAll");
        assert!(overlay.effective_binding("SaveAll").is_some());
        assert!(!overlay.is_customized("SaveAll"));
    }

    #[test]
    fn reset_all_clears_every_override() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        overlay.set_override("Undo", None);
        overlay.reset_all();
        assert!(!overlay.is_customized("SaveAll"));
        assert!(!overlay.is_customized("Undo"));
    }

    #[test]
    fn switching_scheme_does_not_clear_overrides() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        overlay.scheme = KeymapScheme::Fleet;
        assert_eq!(overlay.effective_binding("SaveAll"), None);
    }

    #[test]
    fn conflicts_finds_the_other_command_sharing_a_chord() {
        let overlay = KeymapOverlay::default();
        let undo_chord = command::commands()
            .iter()
            .find(|c| c.id == "Undo")
            .unwrap()
            .binding
            .unwrap()
            .mac;
        let conflicts = overlay.conflicts("Redo", undo_chord);
        assert_eq!(conflicts, vec!["Undo"]);
    }

    #[test]
    fn conflicts_is_empty_for_an_unused_chord() {
        let overlay = KeymapOverlay::default();
        let chord = KeyChord::new(Key::F9).command().shift().alt();
        assert!(overlay.conflicts("SaveAll", chord).is_empty());
    }

    #[test]
    fn export_import_round_trips_overrides_and_scheme() {
        let mut overlay = KeymapOverlay {
            scheme: KeymapScheme::Fleet,
            ..Default::default()
        };
        overlay.set_override(
            "SaveAll",
            Some(Binding::same(KeyChord::new(Key::S).command().shift())),
        );
        overlay.set_override("ShowUsages", None);

        let text = overlay.export();
        let mut reimported = KeymapOverlay::default();
        let report = reimported.import(&text).unwrap();

        assert!(report.skipped_unknown_ids.is_empty());
        assert_eq!(reimported.scheme, KeymapScheme::Fleet);
        assert_eq!(
            reimported.effective_binding("SaveAll").map(|b| b.mac),
            overlay.effective_binding("SaveAll").map(|b| b.mac)
        );
        assert_eq!(reimported.effective_binding("ShowUsages"), None);
    }

    #[test]
    fn import_rejects_an_unsupported_version() {
        let mut overlay = KeymapOverlay::default();
        let err = overlay.import("keymap/2\n").unwrap_err();
        assert!(matches!(err, ImportError::UnsupportedVersion(_)));
    }

    #[test]
    fn import_rejects_an_unknown_key_name() {
        let mut overlay = KeymapOverlay::default();
        let err = overlay
            .import("keymap/1\nbind SaveAll NotAKey\n")
            .unwrap_err();
        assert!(matches!(err, ImportError::UnknownKey(_)));
    }

    #[test]
    fn import_rejects_a_malformed_line() {
        let mut overlay = KeymapOverlay::default();
        let err = overlay.import("keymap/1\nbind\n").unwrap_err();
        assert!(matches!(err, ImportError::MalformedLine { .. }));
    }

    #[test]
    fn import_skips_and_reports_an_unknown_id_without_dropping_known_overrides() {
        let mut overlay = KeymapOverlay::default();
        let report = overlay
            .import("keymap/1\nunbind NotACommand\nunbind SaveAll\n")
            .unwrap();
        assert_eq!(report.skipped_unknown_ids, vec!["NotACommand"]);
        assert_eq!(overlay.effective_binding("SaveAll"), None);
    }

    #[test]
    fn a_failed_import_leaves_the_overlay_untouched() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        let before = overlay.clone();
        let result = overlay.import("keymap/1\nbind SaveAll NotAKey\n");
        assert!(result.is_err());
        assert_eq!(overlay.scheme, before.scheme);
        assert_eq!(
            overlay.effective_binding("SaveAll"),
            before.effective_binding("SaveAll")
        );
    }

    #[test]
    fn gestures_lists_clone_caret_as_alt_double_tap() {
        let gesture = gestures()
            .iter()
            .find(|g| g.id == "CloneCaretUpDown")
            .unwrap();
        assert!(matches!(
            gesture.default,
            GestureTrigger::DoubleTap { modifier } if modifier.alt
        ));
        assert_eq!(gesture.default.label(true), "⌥⌥");
    }

    #[test]
    fn accord_arms_on_prefix_and_expires() {
        let mut accord = Accord::default();
        assert!(!accord.is_armed(0.0));
        accord.arm(0.0);
        assert!(accord.is_armed(0.0));
        assert!(accord.is_armed(ACCORD_ARMED_WINDOW));
        assert!(!accord.is_armed(ACCORD_ARMED_WINDOW + 0.01));
    }

    #[test]
    fn accord_disarm_clears_the_armed_state() {
        let mut accord = Accord::default();
        accord.arm(0.0);
        accord.disarm();
        assert!(!accord.is_armed(0.0));
    }
}
