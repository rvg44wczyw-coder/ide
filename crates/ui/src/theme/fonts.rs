//! Embedded typography (`docs/features/fleet-look-foundation.md` §3.1):
//! Inter for the UI, JetBrains Mono for code, both compiled into the binary
//! so the app looks the same on a machine with no fonts installed.

use eframe::egui;
use std::sync::Arc;

/// Font family for emphasis in UI text (Inter Medium), used by
/// `TextStyle::Heading`.
pub const UI_MEDIUM: &str = "ui-medium";
/// Font family for emphasis in code (JetBrains Mono Bold). Registered here
/// so the editor widget (roadmap A2) has it available without touching this
/// module.
pub const CODE_BOLD: &str = "code-bold";

const INTER_REGULAR_NAME: &str = "Inter";
const INTER_MEDIUM_NAME: &str = "InterMedium";
const MONO_REGULAR_NAME: &str = "JetBrainsMono";
const MONO_BOLD_NAME: &str = "JetBrainsMonoBold";

const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const INTER_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Inter-Medium.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf");

const fn is_truetype(bytes: &[u8]) -> bool {
    bytes.len() > 4 && bytes[0] == 0x00 && bytes[1] == 0x01 && bytes[2] == 0x00 && bytes[3] == 0x00
}

// The font files are Git LFS objects. In a clone made without LFS the files
// on disk are ~130-byte text pointers, which `include_bytes!` would embed
// happily -- egui would then panic at font-parse time pointing at its own
// font code instead of at the missing fetch. Anonymous `const _` rather than
// a named constant: a named one nothing references trips `dead_code`, which
// under this project's `clippy -D warnings` would make the guard break the
// build it exists to protect.
const _: () = {
    assert!(
        is_truetype(INTER_REGULAR),
        "assets/fonts/Inter-Regular.ttf is not a TrueType file -- it is probably an unfetched Git LFS pointer; run `git lfs pull`"
    );
    assert!(
        is_truetype(INTER_MEDIUM),
        "assets/fonts/Inter-Medium.ttf is not a TrueType file -- it is probably an unfetched Git LFS pointer; run `git lfs pull`"
    );
    assert!(
        is_truetype(MONO_REGULAR),
        "assets/fonts/JetBrainsMono-Regular.ttf is not a TrueType file -- it is probably an unfetched Git LFS pointer; run `git lfs pull`"
    );
    assert!(
        is_truetype(MONO_BOLD),
        "assets/fonts/JetBrainsMono-Bold.ttf is not a TrueType file -- it is probably an unfetched Git LFS pointer; run `git lfs pull`"
    );
};

/// `FontDefinitions` with the embedded faces *prepended* to egui's own
/// families rather than replacing them. The fallback is load-bearing (doc
/// §3.1): this app renders text it doesn't author -- filenames, paths,
/// assistant replies, raw `cargo` output -- and neither embedded face covers
/// CJK or emoji.
pub fn font_definitions() -> egui::FontDefinitions {
    let mut defs = egui::FontDefinitions::default();

    for (name, bytes) in [
        (INTER_REGULAR_NAME, INTER_REGULAR),
        (INTER_MEDIUM_NAME, INTER_MEDIUM),
        (MONO_REGULAR_NAME, MONO_REGULAR),
        (MONO_BOLD_NAME, MONO_BOLD),
    ] {
        defs.font_data.insert(
            name.to_owned(),
            Arc::new(egui::FontData::from_static(bytes)),
        );
    }

    let proportional = family(&defs, &egui::FontFamily::Proportional);
    let monospace = family(&defs, &egui::FontFamily::Monospace);

    defs.families.insert(
        egui::FontFamily::Proportional,
        prepend(&[INTER_REGULAR_NAME], &proportional),
    );
    defs.families.insert(
        egui::FontFamily::Monospace,
        prepend(&[MONO_REGULAR_NAME], &monospace),
    );
    defs.families.insert(
        egui::FontFamily::Name(UI_MEDIUM.into()),
        prepend(&[INTER_MEDIUM_NAME, INTER_REGULAR_NAME], &proportional),
    );
    defs.families.insert(
        egui::FontFamily::Name(CODE_BOLD.into()),
        prepend(&[MONO_BOLD_NAME, MONO_REGULAR_NAME], &monospace),
    );

    defs
}

fn family(defs: &egui::FontDefinitions, family: &egui::FontFamily) -> Vec<String> {
    defs.families.get(family).cloned().unwrap_or_default()
}

fn prepend(ours: &[&str], fallback: &[String]) -> Vec<String> {
    let mut names: Vec<String> = ours.iter().map(|n| (*n).to_owned()).collect();
    for name in fallback {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// Installs the embedded fonts into `ctx`. Call **once**, at startup: egui
/// rebuilds its font atlas on every call, so running this per frame or per
/// theme toggle is a visible stall. Must run before
/// [`super::apply`], which references [`UI_MEDIUM`] -- egui panics on a
/// family bound to no fonts.
pub fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faces() -> [(&'static str, &'static [u8]); 4] {
        [
            ("Inter-Regular.ttf", INTER_REGULAR),
            ("Inter-Medium.ttf", INTER_MEDIUM),
            ("JetBrainsMono-Regular.ttf", MONO_REGULAR),
            ("JetBrainsMono-Bold.ttf", MONO_BOLD),
        ]
    }

    /// Runtime mirror of the `const _` guard above, so a failure names the
    /// offending file instead of only failing to compile.
    #[test]
    fn every_embedded_face_is_a_truetype_file() {
        for (name, bytes) in faces() {
            assert!(!bytes.is_empty(), "{name} is empty");
            assert!(
                is_truetype(bytes),
                "{name} is not TrueType (first bytes {:?}) -- unfetched Git LFS pointer?",
                &bytes[..4.min(bytes.len())]
            );
        }
    }

    #[test]
    fn is_truetype_rejects_a_git_lfs_pointer_and_short_input() {
        assert!(!is_truetype(
            b"version https://git-lfs.github.com/spec/v1\n"
        ));
        assert!(!is_truetype(b""));
        assert!(!is_truetype(&[0x00, 0x01, 0x00]));
        assert!(is_truetype(&[0x00, 0x01, 0x00, 0x00, 0x42]));
    }

    #[test]
    fn definitions_register_all_four_faces() {
        let defs = font_definitions();
        for name in [
            INTER_REGULAR_NAME,
            INTER_MEDIUM_NAME,
            MONO_REGULAR_NAME,
            MONO_BOLD_NAME,
        ] {
            assert!(defs.font_data.contains_key(name), "{name} not registered");
        }
    }

    #[test]
    fn our_faces_come_first_and_egui_defaults_stay_as_fallbacks() {
        let stock = egui::FontDefinitions::default();
        let defs = font_definitions();

        for (family, ours) in [
            (egui::FontFamily::Proportional, INTER_REGULAR_NAME),
            (egui::FontFamily::Monospace, MONO_REGULAR_NAME),
        ] {
            let stock_list = stock.families.get(&family).cloned().unwrap_or_default();
            let list = defs.families.get(&family).expect("family missing");
            assert_eq!(list.first().map(String::as_str), Some(ours));
            for fallback in &stock_list {
                assert!(
                    list.contains(fallback),
                    "{family:?} lost egui fallback {fallback}"
                );
            }
            assert!(
                list.len() > 1,
                "{family:?} has no fallback -- CJK/emoji would render as tofu"
            );
        }
    }

    #[test]
    fn emphasis_families_exist_and_fall_back_to_their_regular_face() {
        let defs = font_definitions();
        for (name, first, second) in [
            (UI_MEDIUM, INTER_MEDIUM_NAME, INTER_REGULAR_NAME),
            (CODE_BOLD, MONO_BOLD_NAME, MONO_REGULAR_NAME),
        ] {
            let list = defs
                .families
                .get(&egui::FontFamily::Name(name.into()))
                .unwrap_or_else(|| panic!("family {name} missing"));
            assert_eq!(list.first().map(String::as_str), Some(first));
            assert_eq!(list.get(1).map(String::as_str), Some(second));
        }
    }

    #[test]
    fn prepend_does_not_duplicate_a_name_already_in_the_fallback() {
        let fallback = vec!["Inter".to_owned(), "Emoji".to_owned()];
        let names = prepend(&["Inter"], &fallback);
        assert_eq!(names, vec!["Inter".to_owned(), "Emoji".to_owned()]);
    }
}
