//! User-editable override layer over `commands.rs`'s compile-time
//! defaults (`docs/features/tui-keymap.md`, `T22`). Depends on
//! `commands.rs` (reads `commands()`), never the reverse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::commands::{commands, Action};

pub type Chord = (KeyModifiers, KeyCode);

/// id -> `Some(encoded chord)` (bound), `Some(None)`'s serialized form is
/// never actually stored -- an *explicit* unbind is `overrides.insert(id,
/// None)`, i.e. the map's value itself is `Option<String>` and `None`
/// there already means "explicitly unbound," while a missing key means
/// "no override, fall through to the static default" (`docs/features/
/// tui-keymap.md` §2.1).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KeymapOverlay {
    overrides: BTreeMap<String, Option<String>>,
}

impl KeymapOverlay {
    pub fn effective_binding(&self, id: &str) -> Option<Chord> {
        match self.overrides.get(id) {
            Some(encoded) => encoded.as_deref().and_then(decode_chord),
            None => commands()
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.binding),
        }
    }

    pub fn set_override(&mut self, id: &str, binding: Option<Chord>) {
        self.overrides
            .insert(id.to_string(), binding.map(encode_chord));
    }

    pub fn reset(&mut self, id: &str) {
        self.overrides.remove(id);
    }

    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    pub fn is_customized(&self, id: &str) -> bool {
        self.overrides.contains_key(id)
    }

    pub fn conflicts(&self, id: &str, proposed: Chord) -> Vec<&'static str> {
        commands()
            .iter()
            .filter(|c| c.id != id && self.effective_binding(c.id) == Some(proposed))
            .map(|c| c.id)
            .collect()
    }

    pub fn action_for(&self, modifiers: KeyModifiers, code: KeyCode) -> Option<Action> {
        commands()
            .iter()
            .find(|c| self.effective_binding(c.id) == Some((modifiers, code)))
            .map(|c| c.action)
    }
}

/// Display label for a chord, e.g. `"Ctrl+Shift+G"`, `"F3"`, `"Esc"`.
pub fn label(chord: Chord) -> String {
    let (modifiers, code) = chord;
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_string());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_string());
    }
    parts.push(code_label(code));
    parts.join("+")
}

fn code_label(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "BackTab".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Insert => "Insert".to_string(),
        KeyCode::Null => "Null".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::CapsLock => "CapsLock".to_string(),
        KeyCode::ScrollLock => "ScrollLock".to_string(),
        KeyCode::NumLock => "NumLock".to_string(),
        KeyCode::PrintScreen => "PrintScreen".to_string(),
        KeyCode::Pause => "Pause".to_string(),
        KeyCode::Menu => "Menu".to_string(),
        KeyCode::KeypadBegin => "KeypadBegin".to_string(),
        KeyCode::Media(_) | KeyCode::Modifier(_) => "?".to_string(),
    }
}

/// Encodes a chord as `<mod>+<mod>+...+<code>`, e.g. `"ctrl+shift+char:g"`
/// (`docs/features/tui-keymap.md` §2.2). Total: every `KeyCode` variant
/// this crate's own keyboard-enhancement setup (`lib.rs`'s
/// `DISAMBIGUATE_ESCAPE_CODES`-only opt-in) can actually deliver has a
/// real encoding; `Media`/`Modifier` (which need
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES`, never requested here) get an inert
/// marker `decode_chord` never produces, rather than making this
/// function fallible for two variants that cannot occur in practice.
fn encode_chord(chord: Chord) -> String {
    let (modifiers, code) = chord;
    let mut parts: Vec<String> = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    parts.push(encode_code(code));
    parts.join("+")
}

fn encode_code(code: KeyCode) -> String {
    match code {
        KeyCode::Char(c) => format!("char:{c}"),
        KeyCode::F(n) => format!("f:{n}"),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "backtab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Null => "null".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::CapsLock => "capslock".to_string(),
        KeyCode::ScrollLock => "scrolllock".to_string(),
        KeyCode::NumLock => "numlock".to_string(),
        KeyCode::PrintScreen => "printscreen".to_string(),
        KeyCode::Pause => "pause".to_string(),
        KeyCode::Menu => "menu".to_string(),
        KeyCode::KeypadBegin => "keypadbegin".to_string(),
        KeyCode::Media(_) | KeyCode::Modifier(_) => "unsupported".to_string(),
    }
}

fn decode_chord(s: &str) -> Option<Chord> {
    let parts: Vec<&str> = s.split('+').collect();
    let (mods, code_part) = parts.split_last()?;
    let mut modifiers = KeyModifiers::NONE;
    for m in code_part {
        match *m {
            "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "alt" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let code = decode_code(mods)?;
    Some((modifiers, code))
}

fn decode_code(s: &str) -> Option<KeyCode> {
    if let Some(rest) = s.strip_prefix("char:") {
        let mut chars = rest.chars();
        let c = chars.next()?;
        return chars.next().is_none().then_some(KeyCode::Char(c));
    }
    if let Some(rest) = s.strip_prefix("f:") {
        return rest.parse::<u8>().ok().map(KeyCode::F);
    }
    Some(match s {
        "backspace" => KeyCode::Backspace,
        "enter" => KeyCode::Enter,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "null" => KeyCode::Null,
        "esc" => KeyCode::Esc,
        "capslock" => KeyCode::CapsLock,
        "scrolllock" => KeyCode::ScrollLock,
        "numlock" => KeyCode::NumLock,
        "printscreen" => KeyCode::PrintScreen,
        "pause" => KeyCode::Pause,
        "menu" => KeyCode::Menu,
        "keypadbegin" => KeyCode::KeypadBegin,
        _ => return None,
    })
}

/// Best-effort load: a missing file, malformed JSON, or an unresolvable
/// home directory all yield [`KeymapOverlay::default`] -- same contract
/// as `state.rs::load`, and for the same reason (no user-facing error
/// channel this early, and a broken file must never block startup).
pub fn load() -> KeymapOverlay {
    match keymap_file_path() {
        Some(path) => load_from(&path),
        None => KeymapOverlay::default(),
    }
}

/// Best-effort save: creates the parent directory if needed; any failure
/// is silently swallowed, same contract as `state.rs::save`.
pub fn save(overlay: &KeymapOverlay) {
    if let Some(path) = keymap_file_path() {
        save_to(&path, overlay);
    }
}

fn load_from(path: &Path) -> KeymapOverlay {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, overlay: &KeymapOverlay) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(overlay) {
        let _ = std::fs::write(path, json);
    }
}

/// Deliberately its own file, not folded into `state.rs`'s
/// `PersistedState` -- see `docs/features/tui-keymap.md` §2.3 for why
/// sharing that struct would risk silently discarding customizations on
/// every launch.
fn keymap_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join(".config/ide-tui/keymap.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Action;

    fn ctrl(c: char) -> Chord {
        (KeyModifiers::CONTROL, KeyCode::Char(c))
    }

    #[test]
    fn effective_binding_falls_through_to_the_static_default() {
        let overlay = KeymapOverlay::default();
        assert_eq!(overlay.effective_binding("SaveAll"), Some(ctrl('s')));
    }

    #[test]
    fn effective_binding_prefers_an_override() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", Some(ctrl('x')));
        assert_eq!(overlay.effective_binding("SaveAll"), Some(ctrl('x')));
    }

    #[test]
    fn set_override_none_explicitly_unbinds() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", None);
        assert_eq!(overlay.effective_binding("SaveAll"), None);
    }

    #[test]
    fn effective_binding_for_an_unknown_id_is_none() {
        let overlay = KeymapOverlay::default();
        assert_eq!(overlay.effective_binding("NoSuchCommand"), None);
    }

    #[test]
    fn reset_falls_back_to_the_default_again() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", Some(ctrl('x')));
        overlay.reset("SaveAll");
        assert_eq!(overlay.effective_binding("SaveAll"), Some(ctrl('s')));
        assert!(!overlay.is_customized("SaveAll"));
    }

    #[test]
    fn reset_all_clears_every_override() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", Some(ctrl('x')));
        overlay.set_override("Undo", None);
        overlay.reset_all();
        assert_eq!(overlay.effective_binding("SaveAll"), Some(ctrl('s')));
        assert!(overlay.effective_binding("Undo").is_some());
        assert!(!overlay.is_customized("SaveAll"));
        assert!(!overlay.is_customized("Undo"));
    }

    #[test]
    fn is_customized_is_true_for_both_a_rebind_and_an_explicit_unbind() {
        let mut overlay = KeymapOverlay::default();
        assert!(!overlay.is_customized("SaveAll"));
        overlay.set_override("SaveAll", Some(ctrl('x')));
        assert!(overlay.is_customized("SaveAll"));
        overlay.set_override("Undo", None);
        assert!(overlay.is_customized("Undo"));
    }

    #[test]
    fn conflicts_finds_another_command_sharing_the_proposed_chord() {
        let overlay = KeymapOverlay::default();
        // `Undo`'s own default is `Ctrl+Z` -- proposing that chord for a
        // different id must report `Undo` as a conflict.
        let conflicts = overlay.conflicts("SaveAll", (KeyModifiers::CONTROL, KeyCode::Char('z')));
        assert_eq!(conflicts, vec!["Undo"]);
    }

    #[test]
    fn conflicts_excludes_the_id_being_assigned() {
        let overlay = KeymapOverlay::default();
        assert_eq!(overlay.conflicts("SaveAll", ctrl('s')), Vec::<&str>::new());
    }

    #[test]
    fn conflicts_is_empty_for_a_genuinely_free_chord() {
        let overlay = KeymapOverlay::default();
        assert_eq!(
            overlay.conflicts("SaveAll", (KeyModifiers::NONE, KeyCode::Char('~'))),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn action_for_resolves_a_default_binding() {
        let overlay = KeymapOverlay::default();
        assert_eq!(
            overlay.action_for(KeyModifiers::CONTROL, KeyCode::Char('s')),
            Some(Action::SaveActive)
        );
    }

    #[test]
    fn action_for_resolves_a_rebound_chord_and_stops_answering_the_old_one() {
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", Some(ctrl('x')));
        assert_eq!(
            overlay.action_for(KeyModifiers::CONTROL, KeyCode::Char('x')),
            Some(Action::SaveActive)
        );
        assert_eq!(
            overlay.action_for(KeyModifiers::CONTROL, KeyCode::Char('s')),
            None
        );
    }

    #[test]
    fn action_for_picks_the_first_registry_match_when_two_ids_share_a_chord() {
        let mut overlay = KeymapOverlay::default();
        // Force a real collision: rebind `Undo` onto `SaveAll`'s own
        // default chord. `SaveAll` is registered before `Undo`
        // (`commands.rs`'s own table order), so it must win.
        overlay.set_override("Undo", Some(ctrl('s')));
        assert_eq!(
            overlay.action_for(KeyModifiers::CONTROL, KeyCode::Char('s')),
            Some(Action::SaveActive)
        );
    }

    #[test]
    fn action_for_an_unbound_chord_is_none() {
        let overlay = KeymapOverlay::default();
        assert_eq!(
            overlay.action_for(KeyModifiers::NONE, KeyCode::Char('~')),
            None
        );
    }

    #[test]
    fn label_renders_a_multi_modifier_chord() {
        assert_eq!(
            label((
                KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                KeyCode::Char('g')
            )),
            "Ctrl+Shift+g"
        );
    }

    #[test]
    fn label_renders_a_bare_function_key() {
        assert_eq!(label((KeyModifiers::NONE, KeyCode::F(3))), "F3");
    }

    #[test]
    fn label_renders_esc_with_no_modifiers() {
        assert_eq!(label((KeyModifiers::NONE, KeyCode::Esc)), "Esc");
    }

    #[test]
    fn encode_decode_round_trips_every_reachable_key_code() {
        let codes = [
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Null,
            KeyCode::Esc,
            KeyCode::CapsLock,
            KeyCode::ScrollLock,
            KeyCode::NumLock,
            KeyCode::PrintScreen,
            KeyCode::Pause,
            KeyCode::Menu,
            KeyCode::KeypadBegin,
            KeyCode::F(1),
            KeyCode::F(12),
            KeyCode::Char('a'),
            KeyCode::Char('Z'),
            KeyCode::Char('/'),
        ];
        for code in codes {
            let chord = (KeyModifiers::CONTROL.union(KeyModifiers::SHIFT), code);
            let encoded = encode_chord(chord);
            assert_eq!(
                decode_chord(&encoded),
                Some(chord),
                "round trip of {code:?}"
            );
        }
    }

    #[test]
    fn encode_decode_round_trips_every_modifier_combination() {
        let combos = [
            KeyModifiers::NONE,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL.union(KeyModifiers::ALT),
            KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
            KeyModifiers::ALT.union(KeyModifiers::SHIFT),
            KeyModifiers::CONTROL
                .union(KeyModifiers::ALT)
                .union(KeyModifiers::SHIFT),
        ];
        for modifiers in combos {
            let chord = (modifiers, KeyCode::Char('g'));
            let encoded = encode_chord(chord);
            assert_eq!(
                decode_chord(&encoded),
                Some(chord),
                "round trip of {modifiers:?}"
            );
        }
    }

    #[test]
    fn unsupported_key_codes_encode_to_an_inert_marker_that_never_decodes() {
        use crossterm::event::ModifierKeyCode;
        let chord = (
            KeyModifiers::NONE,
            KeyCode::Modifier(ModifierKeyCode::LeftControl),
        );
        let encoded = encode_chord(chord);
        assert_eq!(encoded, "unsupported");
        assert_eq!(decode_chord(&encoded), None);
    }

    #[test]
    fn decode_chord_rejects_an_unknown_modifier_name() {
        assert_eq!(decode_chord("bogus+char:a"), None);
    }

    #[test]
    fn decode_chord_rejects_an_unknown_code_name() {
        assert_eq!(decode_chord("ctrl+bogus"), None);
    }

    #[test]
    fn decode_chord_rejects_a_multi_char_char_encoding() {
        assert_eq!(decode_chord("char:ab"), None);
    }

    #[test]
    fn decode_chord_rejects_an_empty_string() {
        assert_eq!(decode_chord(""), None);
    }

    #[test]
    fn load_from_a_fresh_directory_returns_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = load_from(&dir.path().join("keymap.json"));
        assert_eq!(overlay, KeymapOverlay::default());
    }

    #[test]
    fn save_then_load_round_trips_an_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/keymap.json");
        let mut overlay = KeymapOverlay::default();
        overlay.set_override("SaveAll", Some(ctrl('x')));
        overlay.set_override("Undo", None);
        save_to(&path, &overlay);
        assert_eq!(load_from(&path), overlay);
    }

    #[test]
    fn load_on_malformed_json_returns_the_default_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_from(&path), KeymapOverlay::default());
    }

    #[test]
    fn save_creates_the_parent_directory_if_it_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/keymap.json");
        assert!(!path.parent().unwrap().exists());
        save_to(&path, &KeymapOverlay::default());
        assert!(path.exists());
    }

    #[test]
    fn save_on_an_unresolvable_parent_is_a_silent_no_op() {
        save_to(Path::new("/"), &KeymapOverlay::default());
    }

    #[test]
    fn keymap_file_path_resolves_to_some_path_in_this_test_environment() {
        assert!(keymap_file_path().is_some());
    }

    #[test]
    fn load_against_the_real_environment_never_panics() {
        let _ = load();
    }
}
