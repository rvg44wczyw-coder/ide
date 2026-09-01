# IntelliJ shell iconography and full-width status bar (B2c)

## 1. Purpose

Roadmap phase **B2c**, following **B2b** (`intellij-shell.md`, merged
2026-08-24). Grounded in a design-canvas mockup the user reviewed and
approved (`/design`, "IDE GUI Review" artifact) that itself was built
from the real running app (a live screenshot) and the real theme tokens
in `crates/ui/src/theme/palette.rs` — not invented from memory. B2b
restyled the shell's chrome (headers, boxed tabs, toolbar/status-bar
padding) but kept B1's abstract painter-drawn stripe shapes
(circle/diamond/triangle/loupe) with tooltip-only names, and kept the
status bar and bottom tool window confined to the width between the
open left/right tool windows rather than spanning the full window.

This doc closes both gaps with changes that have real backing
functionality in the app today — it explicitly does **not** add toolbar
buttons for features that don't exist yet (run/debug configurations,
VCS commit/push, a unified settings dialog, notifications): inventing
non-functional chrome for those would contradict this project's own
established discipline (see `intellij-shell.md` §1's non-scope list,
and `docs/roadmap.md` §9).

## 2. Behaviour

### 2.1 Full-width status bar (call-order fix, no new code)

`crates/ui/src/app/render.rs`'s `ui()` method currently calls, inside
`if !self.zen_mode { ... }`:

```rust
self.render_project_rail(ui);
self.render_claude_rail(&ctx, ui);
self.render_bottom_rail(ui);
self.render_status_bar(ui);
```

Because every `egui::Panel::{top,left,right,bottom}` call carves its
slice from whatever rect is currently available (not just from panels
on the same edge), calling `render_status_bar` *last* means the
project/claude rails (already claimed on the left/right) shrink the
rect status bar's `Panel::bottom` draws from — the status bar visibly
stops at the tree panel's right edge today (confirmed live, both by
reading this call order and by a screenshot of the running app: the
`1:1 UTF-8 LF ...` row starts exactly where the Project tree ends, not
at the window's left edge).

The fix is call-order only: move `render_status_bar` to run *before*
`render_project_rail`/`render_claude_rail`, right after `render_top_bar`:

```rust
if !self.zen_mode {
    self.render_status_bar(ui);
    self.render_project_rail(ui);
    self.render_claude_rail(&ctx, ui);
}
```

`render_bottom_rail` is deleted as part of §2.2 below, so it drops out
of this sequence entirely rather than being reordered. No change to
`render_status_bar`'s own body — same content, same click behaviour,
same `Panel::bottom("status_bar")` id.

### 2.2 `render_bottom_rail` is removed; its three icons move onto the side stripes

`render_bottom_rail` today draws two things: a `Panel::bottom(
"bottom_rail")` icon strip (Problems/Run/Find) and, conditionally, the
`Panel::bottom("bottom_panel")` tabs+content. This doc removes the
`"bottom_rail"` panel and its three `render_stripe_icon` calls; the
`"bottom_panel"` tabs+content panel is **unchanged** (same id, same
`resizable`/`default_size`, same tab row, same `match self.bottom_view`)
and moves into a new, smaller `render_bottom_panel` function containing
just that part.

The three relocated icons, plus one new one with real existing
backing, are redistributed across the bottom of the left and right
stripes (top-anchored: the existing Project/Claude icon; bottom-
anchored: two more each), matching real IntelliJ's convention of
grouping a vertical stripe's icons at both ends rather than adding a
fourth, separate horizontal strip purely for icons:

| Stripe | Top (unchanged) | Bottom-anchored (new) |
|---|---|---|
| Left (`render_project_rail`) | Project | Problems, Run |
| Right (`render_claude_rail`) | Claude | Find, Version Control |

- **Problems** and **Run** keep their exact current click targets
  (`self.toggle_bottom_tool_window(BottomView::Problems)` /
  `(BottomView::CargoOutput)`) and open state
  (`open && self.bottom_view == BottomView::Problems` /
  `CargoOutput`) — only their stripe position changes.
- **Find** keeps its current target (`BottomView::Search`) and open
  state, unchanged.
- **Version Control** is new chrome for existing behaviour: `ViewMode`
  and `IdeApp::toggle_view_mode` (`crates/ui/src/app.rs`) already
  toggle the central panel between `ViewMode::Editor` and
  `ViewMode::SourceControl` (`render_source_control`) — today reachable
  only via its registered command/shortcut, with no stripe entry
  point. Click target: `self.toggle_view_mode()`. Open state (for the
  icon's filled-vs-stroked treatment): `self.view_mode ==
  ViewMode::SourceControl`.

`render_project_rail`'s and `render_claude_rail`'s existing
`Panel::left("project_rail")` / `Panel::right("claude_rail")` calls
gain a second, bottom-anchored `ui.horizontal` (or a `with_layout`
bottom-aligned group) inside the same panel closure, laid out with
`egui::Layout::bottom_up` so the two bottom icons sit flush with the
panel's bottom edge regardless of window height — the panel itself is
already full-height (§2.1 established this), so this is the same panel,
not a new one.

### 2.3 Recognizable stripe icons, replacing the abstract shapes

`StripeIconShape` (B2b) is renamed `StripeIcon` and its five abstract
variants (`Circle`/`Diamond`/`TriangleUp`/`TriangleRight`/`Loupe`)
are replaced with six recognizable ones, each a small set of
`painter.add(Shape::...)` calls in the existing 16×16 icon box (same
`SIZE`/`R` constants, same open-vs-closed color logic: `tokens.color
.accent` filled when open, `tokens.color.fg_secondary` stroked when
closed — unchanged from B2b):

- `Folder` (Project) — a flat-bottomed folder outline: a small tab
  rectangle plus a body rectangle, matching the mockup's folder glyph.
- `Chat` (Claude) — a rounded speech-bubble outline with a small tail.
- `Warning` (Problems) — a triangle outline (reuses the existing
  `TriangleUp` point math) with a vertical tick and a dot inside, the
  standard warning-triangle glyph.
- `Output` (Run) — a rectangle outline with a small filled corner tab
  (a terminal/output-pane glyph), replacing the plain right-pointing
  triangle.
- `Loupe` (Find) — unchanged from B2b (already a real, recognizable
  magnifying-glass glyph, not abstract).
- `Branch` (Version Control) — three small circles connected by two
  short strokes (the standard git-branch glyph), reusing the exact
  path the mockup's `Refactored.dc.html` used.

Every call site (`render_project_rail`, `render_claude_rail`) updates
to the new enum/name; no old shape is left unused. `render_stripe_icon`
itself keeps its current signature and hover-fill behaviour from B2b —
only the `shape` match arms' drawing code changes, plus §2.4 below.

### 2.4 Always-visible rotated labels, not tooltip-only

B2b's `render_stripe_icon` added a `name: &str` parameter surfaced only
via `.on_hover_text(name)`, explicitly deferring true rotated on-canvas
labels as future work (`intellij-shell.md` §2.2, §3). This doc
implements that deferred work: every stripe icon keeps its hover
tooltip (harmless, and a real accessibility aid `.on_hover_text`
already gave it) **and** gains an always-visible vertical label painted
below the icon, using `egui::epaint::TextShape` with a non-zero
`angle` — confirmed available in the pinned `egui 0.36.1`
(`epaint::shapes::text_shape::TextShape`, a public `angle: f32` field,
"radians clockwise, pivot is `pos`, the galley's pre-rotation top-left
corner").

Add a new function:

```rust
fn render_vertical_stripe_label(
    ui: &mut egui::Ui,
    tokens: &Tokens,
    anchor: egui::Pos2,
    text: &str,
    reads_upward: bool,
) {
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::new(tokens.text.small, egui::FontFamily::Proportional),
        tokens.color.fg_secondary,
    );
    let (w, h) = (galley.size().x, galley.size().y);
    let pos = if reads_upward {
        egui::pos2(anchor.x - h / 2.0, anchor.y)
    } else {
        egui::pos2(anchor.x + h / 2.0, anchor.y)
    };
    let mut shape = egui::epaint::TextShape::new(pos, galley, tokens.color.fg_secondary);
    shape.angle = if reads_upward {
        -std::f32::consts::FRAC_PI_2
    } else {
        std::f32::consts::FRAC_PI_2
    };
    ui.painter().add(shape);
}
```

Geometry (worked out and checked against `TextShape`'s documented
"clockwise around `pos`" convention before writing this doc, not left
for the implementer to re-derive):

- **Left stripe** (`reads_upward: true`, angle `-π/2`): the rotated
  block's bottom edge lands at `anchor.y`, extending *upward* by `w`;
  its horizontal center lands at `anchor.x`. The first character sits
  at the bottom, the last just below the icon — reading bottom-to-top,
  the real IntelliJ left-stripe convention. Call with `anchor` = the
  bottom-anchored point where the label should end (e.g. the stripe's
  bottom edge minus a small margin for the lowest icon, or the point
  just below a higher icon for one stacked above another).
- **Right stripe** (`reads_upward: false`, angle `+π/2`): the rotated
  block's top edge lands at `anchor.y`, extending *downward* by `w`;
  same horizontal centering. First character at top (nearest the
  icon), reading top-to-bottom.

Call site shape, e.g. inside `render_project_rail`'s top icon group:

```rust
let icon_response = Self::render_stripe_icon(ui, tokens, StripeIcon::Folder, "Project", open);
Self::render_vertical_stripe_label(
    ui,
    tokens,
    egui::pos2(icon_response.rect.center().x, icon_response.rect.bottom() + tokens.space.xs),
    "Project",
    true,
);
```

Label text stays literal per-call-site strings ("Project", "Claude",
"Problems", "Run", "Find", "Version Control") — not derived from the
existing `name: &str` hover-tooltip parameter's own storage, though the
two are always passed the same literal at each call site; no new
runtime coupling is implied, this is just two parameters carrying the
same string.

Vertical space per stripe icon group grows from 16px (icon only) to
roughly 16 + `space.xs` + label-width-as-height (a short word in
`text.small`, ~11px font, comfortably under 60px for every label used
here) — both stripes are already full-height panels (§2.1), so this
fits without layout changes elsewhere.

### 2.5 Real folder/file icons in the Project tree

`render_tree_entry`'s directory/file glyphs — currently a filled/
stroked 8×8 circle for a directory and an 8×8 stroked square for a
file (`ICON_SIZE = 8.0`) — are replaced with the same two recognizable
shapes introduced in §2.3: the `Folder` glyph (directory rows) and a
small filled-circle "file dot" kept as-is for files in the *general*
case, EXCEPT `.rs` files get a distinct accent-colored dot (reusing
`tokens.color.accent`) so Rust source is visually distinguishable in
the tree, matching the mockup's orange-red Rust-file marker recolored
to this app's accent token (the mockup's literal orange `#F26D50` was
a generic "distinct file type" placeholder color, not a real token —
using the app's own `accent` keeps every color in the tree sourced from
`Tokens`, consistent with `theme/palette.rs`'s "no color literals
outside this module" test). File-extension detection: a small
`entry.name.ends_with(".rs")` check, matching the existing plain
`DirEntryKind` match already in this function — no new dependency, no
change to `DirEntry`/`DirEntryKind` in `ide-core`.

`INDENT_PER_DEPTH`/`ICON_SIZE` constants, the expand/collapse triangle,
and every click/temp-state behaviour in `render_tree_entry` are
unchanged.

### 2.6 Toolbar: search pill and a branch glyph

`render_top_bar`'s center column (the clickable context-line label
that opens the command palette) gains a pill background and a
magnifier-icon prefix — purely a style change, **same click handler**
(`self.open_command_palette()`), same text content and fallback
("No file open"):

- Wrap the existing `egui::Label` in a `Frame` with
  `corner_radius: tokens.radius.lg`, `fill: tokens.color.bg_hover`, and
  `inner_margin` using `tokens.space.sm`/`tokens.space.md`.
- Prefix the label text with the same `Loupe`-style magnifier glyph
  §2.3 keeps for Find (drawn via `render_stripe_icon`'s existing
  `Loupe` painter code, extracted into a small `paint_loupe(painter,
  center, radius, color)` helper both call sites share, so the icon
  stays pixel-identical between the toolbar pill and the Find stripe
  icon rather than drifting into two near-duplicate implementations).

The toolbar's right group (branch label) gains a small `Branch` glyph
(§2.3's icon, same helper extraction pattern via a shared
`paint_branch` function used by both the toolbar and the Version
Control stripe icon) drawn immediately before `ui.label(branch)`, no
change to the `if let Some(branch) = ... ui.label(branch)` logic
itself.

## 3. Constraints & invariants

- **No invented functionality.** Explicitly out of scope, because no
  backing feature exists yet: a run-configuration selector, run/debug/
  stop buttons, a VCS commit/push widget, a settings-dialog toolbar
  button, a notifications icon. The design-canvas mockup's toolbar
  included illustrative versions of these to show the aspirational
  full IntelliJ chrome; this doc deliberately implements only the
  subset with real, already-existing behaviour behind it, consistent
  with `intellij-shell.md` §1's non-scope list and this project's
  established "don't invent features" discipline (`docs/roadmap.md`
  §9's own framing of B1b/B2b).
- `render_bottom_rail` is deleted, not deprecated or left dead —
  every one of its three icons has a real new home (§2.2); no
  `#[allow(dead_code)]` is introduced anywhere by this doc.
- Panel ids: `"bottom_rail"` is removed (no panel with that id exists
  after this doc); `"status_bar"`, `"project_rail"`, `"tree_panel"`,
  `"claude_rail"`, `"claude_panel"`, `"bottom_panel"`, `"top_bar"` are
  unchanged. `render_bottom_panel` (new, replacing the tabs+content
  half of the old `render_bottom_rail`) still opens `Panel::bottom(
  "bottom_panel")` with the exact same id, `resizable(true)`, and
  `default_size(160.0)` as today.
- `ToolWindow`, `BottomView`, `SmartModeState`, `ViewMode`,
  `is_tool_window_open`, `toggle_tool_window`, `toggle_bottom_tool_window`,
  `toggle_view_mode` in `crates/ui/src/app.rs` are **unchanged** — every
  behaviour this doc wires to (§2.2, §2.6) is a new *caller* of
  existing, already-tested methods, never a new state field or a
  changed transition.
- `TextShape`'s rotation geometry (§2.4) is the one genuinely new
  layout technique in this doc; verify it visually (`cargo run`, a
  screenshot of both stripes) before considering the implementation
  done — this is pure-rendering code with no unit-testable assertion
  for "does this pixel position look right," so a live visual check is
  the only real verification available, same as B1b's/B2b's own
  precedent of a live check before calling a visual change complete.
- Coverage: every function this doc touches or adds
  (`render_stripe_icon`, `render_vertical_stripe_label`,
  `render_project_rail`, `render_claude_rail`, `render_bottom_panel`,
  `render_tree_entry`, `render_top_bar`, `paint_loupe`, `paint_branch`)
  is pure-rendering code, exempt from the 80% floor per
  `render.rs`'s own module doc comment — same exemption B1b/B2b's
  touched functions already had. `.ends_with(".rs")` in
  `render_tree_entry` (§2.5) is the one new piece of branching logic
  in this doc; it's a one-line, self-evidently-correct string check
  inside an otherwise-exempt rendering function, not the kind of
  "non-rendering logic" the exemption's carve-out is meant to force a
  test for.
- Not security-sensitive: `crates/ui/src/app/render.rs` is not on
  CLAUDE.md's declared security-sensitive path list (same as B2b);
  confirm against the real diff once it exists rather than assuming.

## 4. Examples

**Left stripe, after (`render_project_rail`)** — top icon plus two
bottom-anchored icons, each with its own vertical label:

```rust
fn render_project_rail(&mut self, ui: &mut egui::Ui) {
    let open = self.is_tool_window_open(ToolWindow::Project);
    let tokens = self.theme.tokens();
    egui::Panel::left("project_rail").show(ui, |ui| {
        ui.vertical(|ui| {
            let r = Self::render_stripe_icon(ui, tokens, StripeIcon::Folder, "Project", open);
            Self::render_vertical_stripe_label(
                ui, tokens,
                egui::pos2(r.rect.center().x, r.rect.bottom() + tokens.space.xs),
                "Project", true,
            );
            if r.clicked() {
                self.toggle_tool_window(ToolWindow::Project);
            }
        });
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            let showing_run = open && self.bottom_view == BottomView::CargoOutput;
            let r = Self::render_stripe_icon(ui, tokens, StripeIcon::Output, "Run", showing_run);
            Self::render_vertical_stripe_label(/* ... */);
            if r.clicked() {
                self.toggle_bottom_tool_window(BottomView::CargoOutput);
            }
            let showing_problems = open && self.bottom_view == BottomView::Problems;
            let r = Self::render_stripe_icon(ui, tokens, StripeIcon::Warning, "Problems", showing_problems);
            Self::render_vertical_stripe_label(/* ... */);
            if r.clicked() {
                self.toggle_bottom_tool_window(BottomView::Problems);
            }
        });
    });
    // tree_panel unchanged from B2b
}
```

(`render_claude_rail` mirrors this with `Chat`/"Claude" on top and
`Loupe`/"Find" + `Branch`/"Version Control" bottom-anchored, calling
`self.toggle_view_mode()` for the latter.)

**Full-width status bar, `ui()` reorder:**

```rust
// before
if !self.zen_mode {
    self.render_project_rail(ui);
    self.render_claude_rail(&ctx, ui);
    self.render_bottom_rail(ui);
    self.render_status_bar(ui);
}

// after
if !self.zen_mode {
    self.render_status_bar(ui);
    self.render_project_rail(ui);
    self.render_claude_rail(&ctx, ui);
}
```

(`render_bottom_panel`, replacing the tabs+content half of the old
`render_bottom_rail`, is called from inside `render_project_rail` or
kept as its own top-level call in this same block — implementer's
choice, since it has no icon of its own left to draw after §2.2;
either placement is behaviourally identical since it's a `Panel::bottom`
independent of call-site nesting.)

## 5. Dependencies & integration points

- Depends on B2b (merged) for the tokens, `render_stripe_icon`'s
  existing shape/signature, and the panel structure this doc edits in
  place.
- Single role: `rust-ui-dev` only, `crates/ui/**`. No `ide-core`/
  `ide-lsp` change.
- Source of truth for the visual target: the `/design` canvas
  "IDE GUI Review" (`Refactored.dc.html`), reviewed and approved by the
  user before this doc was written — this doc implements the subset of
  that mockup with real backing functionality (§3) and corrects two
  mockup inaccuracies found while writing this doc: the mockup's
  right-stripe bottom icon was labeled "Find" + "Git" with a generic
  "Git" concept; the real equivalent is "Find" (already `BottomView
  ::Search`, an existing icon) + "Version Control" (`toggle_view_mode`,
  existing behaviour, no dedicated stripe icon before now) — the
  mockup's free-form labels are not treated as literal spec text.
