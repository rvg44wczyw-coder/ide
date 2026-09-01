# Shell polish: narrow stripes and remembered last project

## 1. Purpose

User feedback after running the real app post-B2c (`intellij-shell-
iconography.md`):

1. The left (`project_rail`) and right (`claude_rail`) stripes are wider
   than they need to be — they should be just wide enough for the 16px
   icon and its rotated label, "just to get only texts on the side."
2. The IDE doesn't remember the last project it had open — every launch
   starts at the welcome screen (`render_welcome`) even if a project was
   open moments ago.

A third item from the same feedback batch — missing directory icons in
the project tree — needs **no code change**: `render_tree_entry` already
paints real folder/file icons as of the B2c merge
(`paint_small_folder`/the `.rs`-accent dot). The user was looking at a
build from before that merge landed; restarting the app is the fix. This
doc does not touch `render_tree_entry`.

This is Batch A of a larger feedback batch (see the user's approved plan);
Batches B–E (async tree scan, a `claude`-CLI PTY terminal, a native macOS
menu bar, and clone-from-repo) are separate, later docs/chains.

## 2. Behaviour

### 2.1 Narrow `project_rail`/`claude_rail`

Both panels currently use `egui::Panel::left("project_rail")`/
`::right("claude_rail")` with no size override — they get the *default*
`egui::Panel` sizing, whose default frame
(`Frame::side_top_panel`) applies an 8px horizontal `inner_margin`, so the
panel wraps its 16px-wide icon content out to roughly 32px, and stays
resizable (a user could drag it wider still).

Fix: give both panels an explicit, tight, non-resizable size via
`egui::Panel`'s own builder methods (confirmed in the pinned `egui
0.36.1` source, `containers/panel.rs`):

- `.exact_size(size)` — "Enforce this exact size, including margins,"
  pins both `default_outer_size` and the min/max range to one point, so
  it can't be dragged wider even though `resizable` stays at its default.
- `.frame(...)` — override the panel's `Frame` to use a tighter
  horizontal `inner_margin` than the 8px default, so the icon isn't
  wrapped in more whitespace than the design needs.

```rust
let tokens = self.theme.tokens();
let rail_frame = egui::Frame::side_top_panel(ui.style())
    .inner_margin(egui::Margin::symmetric(tokens.space.xs as i8, tokens.space.sm as i8));
egui::Panel::left("project_rail")
    .frame(rail_frame)
    .exact_size(16.0 + tokens.space.xs * 2.0)
    .show(ui, |ui| { /* unchanged body */ });
```

Same pattern for `egui::Panel::right("claude_rail")`. `16.0` is
`render_stripe_icon`'s existing `SIZE` constant (the icon's width, and
the widest thing in the column — the rotated label's on-screen width
after rotation is the *text's height*, `tokens.text.small` = 11px plus
line-height, comfortably under 16px). `tokens.space.xs` (2px) on each
side keeps a hairline gap between the icon and the panel's resize/border
edge without reintroducing the old 8px default.

No change to the panels' contents (icon groups, click targets, labels
from `intellij-shell-iconography.md` §2.2/§2.4) — sizing only.

### 2.2 Remember the last-opened project

New storage key, following the exact pattern `THEME_STORAGE_KEY`/
`CUSTOM_LANGUAGES_STORAGE_KEY`/`KEYMAP_STORAGE_KEY`/
`FORMAT_ON_SAVE_STORAGE_KEY` already establish in `crates/ui/src/app.rs`:

```rust
const LAST_PROJECT_STORAGE_KEY: &str = "ide_last_project";
```

**Save** (`IdeApp::save`, `crates/ui/src/app/render.rs:2553`): persist the
current project's root path (as a `String`, via `Path::display` /
`PathBuf`, `serde`-serializable) whenever a project is open; write nothing
(don't clear a previously-saved value) when `self.project` is `None`, so
closing back to the welcome screen without opening a different project
doesn't erase the last real project on the next launch:

```rust
if let Some(project) = &self.project {
    eframe::set_value(
        storage,
        LAST_PROJECT_STORAGE_KEY,
        &project.root().to_path_buf(),
    );
}
```

**Load** (`IdeApp::new`, `crates/ui/src/app.rs:451`): after building `Self`
the same way `new` already does, before returning, try to reopen the
remembered path:

```rust
let mut app = Self { /* ...existing field list, unchanged... */ };
if let Some(path) = cc
    .storage
    .and_then(|s| eframe::get_value::<std::path::PathBuf>(s, LAST_PROJECT_STORAGE_KEY))
{
    if let Ok(project) = ide_core::Project::open(&path) {
        app.load_project(project);
    }
}
app
```

Reuses `load_project` (`crates/ui/src/app.rs:693`) exactly as-is — same
tree scan, git refresh, file watcher setup, and language (re)detection
every other project-open path already goes through. No new method.

## 3. Constraints & invariants

- `ide_core::Project::open` returns `Result<Project, ProjectError>`
  (`crates/core/src/project.rs:55`); a missing/moved/deleted directory,
  or one that's no longer a valid project, fails and is silently
  ignored — falls through to the normal welcome screen (`self.project`
  stays `None`), exactly like any other "nothing to restore" case. No
  error message shown for this specific failure (distinct from
  `open_project`/`create_project`'s existing `self.error = Some(...)` on
  a *user-initiated* open failing — this one is silent because the user
  didn't ask for it this session).
- Storage key is new; existing keys/values (`ide_theme`,
  `ide_custom_languages`, `ide_keymap`, `ide_format_on_save`) are
  untouched — an app upgrading from before this doc simply has no
  `ide_last_project` value yet (`eframe::get_value` returns `None`,
  handled the same as any first-run case already is for the other keys).
- `.exact_size(...)` on both stripes means they're no longer
  user-resizable (previously they had no explicit size, so were already
  effectively fixed at their auto-computed content width — this doesn't
  remove a resizing capability users had, since default `egui::Panel`
  sizing without an explicit range still doesn't let a user drag past
  its content-fitted size in practice). `tree_panel`/`claude_panel` (the
  *second*, content panels behind each stripe) are untouched and stay
  resizable exactly as today.
- No new dependency, no `ide-core`/`ide-lsp` change beyond calling the
  existing public `Project::open`. Not security-sensitive:
  `crates/ui/src/app.rs`/`crates/ui/src/app/render.rs` aren't on
  CLAUDE.md's declared list — confirm against the real diff once it
  exists rather than assuming.
- Coverage: `render.rs` changes are pure-rendering (module doc-comment
  exemption, same as B2c). The new load-on-startup logic in `app.rs`
  (`IdeApp::new`'s added branch) is **not** pure rendering — it's a real
  decision (open vs. don't) with a real failure path, so it needs a unit
  test: construct a fake `eframe::Storage` (the existing tests in
  `app.rs` already have a pattern for this via `eframe::CreationContext`
  in `IdeApp::new`'s own existing test coverage, if any — otherwise test
  `load_project` being reachable/called is enough; the goal is coverage
  of "valid path restores a project" and "missing/invalid path leaves
  `project: None`, not a panic or a shown error").

## 4. Examples

**Narrowed left stripe** (right stripe mirrors this):

```rust
fn render_project_rail(&mut self, ui: &mut egui::Ui) {
    let open = self.is_tool_window_open(ToolWindow::Project);
    // ...showing_run / showing_problems unchanged...
    let tokens = self.theme.tokens();
    let rail_frame = egui::Frame::side_top_panel(ui.style())
        .inner_margin(egui::Margin::symmetric(tokens.space.xs as i8, tokens.space.sm as i8));
    egui::Panel::left("project_rail")
        .frame(rail_frame)
        .exact_size(16.0 + tokens.space.xs * 2.0)
        .show(ui, |ui| {
            // unchanged: top Folder/"Project" group, bottom_up
            // Warning/"Problems" + Output/"Run" group
        });
    // tree_panel unchanged
}
```

**Restoring the last project at startup**, end of `IdeApp::new`:

```rust
let mut app = Self { theme, view_mode: ViewMode::Editor, /* ... */ };
if let Some(path) = cc
    .storage
    .and_then(|s| eframe::get_value::<std::path::PathBuf>(s, LAST_PROJECT_STORAGE_KEY))
{
    if let Ok(project) = ide_core::Project::open(&path) {
        app.load_project(project);
    }
}
app
```

## 5. Dependencies & integration points

- Depends on B2c (merged) for `render_stripe_icon`'s `SIZE` constant and
  the current `project_rail`/`claude_rail` structure this doc resizes in
  place.
- Single role: `rust-ui-dev` only, `crates/ui/**`. No `ide-core` change —
  `Project::open`/`Project::root` are already public and used elsewhere
  (`open_project`, `IdeApp::save`'s sibling code paths).
- No `/design` mockup for this one — it's a direct fix from live
  user feedback on the running app, not a new visual direction.
