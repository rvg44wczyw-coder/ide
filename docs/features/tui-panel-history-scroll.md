# TUI scroll-back for Cargo output, Claude chat, and AI chat

## 1. Purpose

Direct continuation of T51's audit (`tui-panel-focus-and-scroll.md` §1),
which found — and deliberately left as a documented non-goal — that Cargo
panel output and the Claude chat panel cannot scroll back through
history at all: neither `handle_cargo_panel_key` nor
`handle_claude_chat_key` has any `KeyCode::Up`/`Down` arm, so both always
render only the live tail (`render_cargo_panel`, and `render_claude_
chat`'s own doc comment: "No scroll-back in v1, same precedent as
`render_cargo_panel`"). Continuing the same "all tui panels must be
clickable for focus and scrollable" request, this doc adds the missing
scroll-back state itself (not just routing, unlike T51 — there is
genuinely nothing to route to until this state exists).

Re-auditing the rest of T51's "confirmed non-goals" list while touching
this area found the AI chat dock tab (T49, `handle_ai_panel_key`) has the
exact same gap — its `_ => {}` catch-all silently drops `Up`/`Down` too,
and `render_ai_panel` has the identical `let start = lines.len().
saturating_sub(visible_rows);` always-tail pattern. Not previously
flagged (T51's audit only checked `claude_panel_open`'s Claude-CLI chat,
a different feature from T49's local-LLM AI dock tab), but the same fix
applies verbatim, so it's included here rather than deferred again.

**This reverses a previously-documented "v1 scope cut"**, not merely
fills an oversight: `render_claude_chat`'s own comment cites `tui-claude-
panel.md` §1.1 as the doc that originally decided against scroll-back.
Flagged here explicitly (`CLAUDE.md`'s "fix all findings, discuss
controversial ones" convention) rather than silently reversed — the
justification is the same one that motivated T51: a direct, repeated user
request for every panel to be scrollable, which this was the one
remaining unaddressed corner of.

Notifications panel, Hover popup, and Rename preview remain unchanged —
re-confirmed in T51 (not re-checked here) to have no scrollable content
model at all (`_open: bool`/`Option` flags only), a different category of
gap than these three panels' "has a growing list of content but no way to
see any but the tail" problem.

## 2. Interface

### 2.1 `crates/tui/src/ui.rs` — shared windowing helper

```rust
/// Windows `items` to the last `visible_rows` entries, offset back by
/// `scroll` from the tail (`docs/features/tui-panel-history-scroll.md`
/// §2.1, T52) -- shared by every "live-tailing log" panel (Cargo output,
/// Claude chat, AI chat). `scroll == 0` is the tail exactly as every one
/// of these three rendered unconditionally before this feature;
/// increasing it slides the window backward through history, capped at
/// showing `items[0..visible_rows]` (scrolling further is a no-op, not a
/// shrinking window). Never panics regardless of `scroll`'s value.
fn tail_window<T>(items: &[T], visible_rows: usize, scroll: u16) -> &[T] {
    let max_scroll = items.len().saturating_sub(visible_rows);
    let scroll = (scroll as usize).min(max_scroll);
    let end = items.len() - scroll;
    let start = end.saturating_sub(visible_rows);
    &items[start..end]
}
```

`render_cargo_panel`, `render_claude_chat`, and `render_ai_panel` each
replace their own `let start = ...len().saturating_sub(visible_rows);
...[start..]` with a single `tail_window(...)` call.

### 2.2 `crates/tui/src/cargo_panel.rs` — `CargoPanel`

```rust
pub(crate) struct CargoPanel {
    pub(crate) output: Vec<String>,
    pub(crate) running: Option<CargoCommand>,
    /// Lines scrolled back from the live tail (§3.1) -- `u16`, same type
    /// and "unclamped in the handler, clamped at render" shape as
    /// `GitPanelState::diff_scroll`.
    pub(crate) output_scroll: u16,
    rx: Option<Receiver<StreamEvent>>,
}
```

`run()` gains one line, right next to its existing `self.output.clear()`:
`self.output_scroll = 0;` — a fresh command starts following its own
output from the tail, the same reasoning `GitPanelState`'s `Enter` arm
already applies when selecting a different commit resets `diff_scroll`.

### 2.3 `crates/tui/src/claude_panel.rs` — `ClaudePanel`

```rust
pub struct ClaudePanel {
    pub input: String,
    pub history: Vec<ClaudeMessage>,
    /// Lines scrolled back from the live tail (§3.1) -- same shape as
    /// `CargoPanel::output_scroll`. Never reset by `submit`/`poll`: see
    /// §3.1 for why history growth doesn't need an explicit reset here.
    pub history_scroll: u16,
    queue: Vec<String>,
    rx: Option<Receiver<ClaudeOutcomeResult>>,
    runner: Runner,
}
```

### 2.4 `crates/tui/src/ai_panel.rs` — `AiPanel`

```rust
pub struct AiPanel {
    pub input: String,
    pub history: Vec<AiDisplayMessage>,
    pub sanitized: bool,
    /// Lines scrolled back from the live tail (§3.1) -- same shape as
    /// `ClaudePanel::history_scroll`.
    pub history_scroll: u16,
    pub(crate) provider: Option<String>,
    streaming: bool,
    rx: Option<Receiver<AiDisplayMessage>>,
    root: PathBuf,
    runner: AiRunner,
}
```

### 2.5 `crates/tui/src/app.rs` — key handling

Four new arms, identical shape, in each of `handle_cargo_panel_key`,
`handle_claude_chat_key`, `handle_ai_panel_key` (operating on
`self.cargo.output_scroll` / `self.claude.history_scroll` /
`self.ai.history_scroll` respectively):

```rust
KeyCode::Up => self.<panel>.<field> = self.<panel>.<field>.saturating_add(1),
KeyCode::Down => self.<panel>.<field> = self.<panel>.<field>.saturating_sub(1),
KeyCode::PageUp => self.<panel>.<field> = self.<panel>.<field>.saturating_add(10),
KeyCode::PageDown => self.<panel>.<field> = self.<panel>.<field>.saturating_sub(10),
```

### 2.6 `crates/tui/src/app.rs` — `handle_mouse_scroll`

One new branch, alongside the existing `AppScreen::Keys` special case
(same reasoning: `AppScreen::Run` renders no `tree_area`/
`editor_text_area`/dock body, so the position-based branches would drop
the wheel event):

```rust
if self.active_screen == AppScreen::Run {
    self.handle_key(synthetic);
    return;
}
```

No other mouse-scroll routing changes needed:

- `BottomDockTab::Cargo`/`BottomDockTab::Ai` are already wheel-scrollable
  via T51's `bottom_dock_body` → `handle_bottom_dock_key` routing, which
  already delegates to `handle_cargo_panel_key`/`handle_ai_panel_key` —
  those functions simply had nothing to do with `Up`/`Down` before this
  doc. No T51 code changes; the plumbing was already general enough.
- Claude chat (`claude_panel_open`) is already wheel-scrollable via the
  pre-existing `any_popup_open()` → synthetic-key → `handle_key` →
  `handle_claude_panel_key` → `handle_claude_chat_key` path (`tui-mouse-
  support.md` §3.3) — again, nothing new to route, only new logic for the
  handler it already reaches to act on.

## 3. Behaviour

### 3.1 Why "offset from the tail", not `diff_scroll`'s "offset from the top"

`GitPanelState::diff_scroll` (the closest existing precedent for a plain,
unselectable scroll offset) is anchored at the **top**: `0` shows from
the first line, and rendering does `lines[state.diff_scroll.min(len)..]`.
That's correct for a diff view, whose content is fully loaded and static
the moment the panel opens.

Cargo output, Claude chat, and AI chat are different: their content
**grows over time** while the panel may already be open (a build
streaming lines, a reply arriving), and the default, expected behaviour
is to keep watching the live tail — exactly what all three already did,
unconditionally, before this doc. Anchoring at the top like `diff_scroll`
would break that: a freshly-opened Cargo panel would show line 1 of a
build that's already 200 lines in, not the current progress.

Anchoring at the **tail** instead (`scroll` = how many lines back from
the end) preserves the existing default (`scroll == 0` renders bit-for-
bit what today's unconditional tail-render already produces) and gets
auto-follow for free with no extra bookkeeping: since `end = len -
scroll` is recomputed fresh every frame, a user sitting at `scroll == 0`
automatically sees new lines the instant they arrive (end grows with
`len`), while a user who scrolled back to `scroll == 5` keeps seeing
themselves 5 lines behind the *current* tail as it grows underneath them
— the window slides forward automatically, without `submit`/`poll`/`run`
ever needing to reset or touch `scroll` on new content. `run()` is the
one exception: it clears `output` outright (a new command, unrelated
content), so it also resets `output_scroll` to `0`, the same "reset on
an unrelated content swap" reasoning `GitPanelState`'s commit-select
already applies to `diff_scroll`.

### 3.2 Clamping

Exactly `diff_scroll`'s existing convention: the key handler never
clamps upward (`saturating_add`, no upper bound check against content
length — the handler doesn't know the render area's height anyway).
`tail_window`'s `scroll.min(items.len().saturating_sub(visible_rows))`
is the only clamp, applied at render time. The cap is `len -
visible_rows`, not `len`: capping at `len` would let `end` shrink below
`visible_rows`, making the window get visibly *smaller* the further back
a user scrolls instead of simply stopping. Capping at `len -
visible_rows` instead pins the window at exactly
`items[0..visible_rows]` once scrolled all the way back — scrolling `Up`
further is then a harmless no-op, matching `diff_scroll`'s own "nothing
more to reveal past the start" behaviour, just reached from the opposite
end.

### 3.3 Interaction with T51's dock-body wheel-scroll

No change to T51's `handle_mouse_scroll` routing for the Cargo/AI dock
tabs — the existing `bottom_dock_body` branch already synthesizes an
`Up`/`Down` key into `handle_bottom_dock_key`, which already delegates to
`handle_cargo_panel_key`/`handle_ai_panel_key`. Those functions simply
had no `Up`/`Down` arm to receive it before this doc (T51 §1 called this
out explicitly as "routed correctly but has nothing to do once it
arrives — a silent no-op"). This doc is what gives them something to do;
T51's routing needed no changes.

## 4. Constraints & invariants

- `output_scroll`/`history_scroll` are always `u16`, matching `diff_
  scroll`'s type — a scroll depth beyond 65535 lines is not a realistic
  concern for any of these three panels' content.
- No new `HitMap` fields, no new mouse-click behaviour — this is purely
  new scroll *state* plus the key/render wiring to use it. Click-to-focus
  for these three panels is already covered (Cargo/AI dock tabs by T51;
  Claude chat by already being an exclusive-focus popup while open).
- `tail_window` never panics regardless of `scroll`'s value, including
  `u16::MAX` against an empty or single-line `items`.
- Notifications/Hover/Rename Preview remain explicitly out of scope
  (§1) — this doc does not revisit that part of T51's non-goal list.

## 5. Examples

A `cargo build` streams 40 lines into a panel 10 rows tall.
`output_scroll == 0` shows lines 31-40 (the tail, unchanged from before
this feature). Pressing `Up` three times sets `output_scroll = 3`;
`tail_window` now shows lines 28-37. If the build produces 5 more lines
while scrolled back (`len` becomes 45, `scroll` still `3`), the window
recomputes to lines 33-42 — three lines behind the new tail, not frozen
at the old absolute line numbers, and not snapped back to the live tail
either.

Wheel-scrolling over the Cargo dock tab's body: `hits.bottom_dock_body`
matches (T51, unchanged) → synthesizes `KeyCode::Up` → `handle_bottom_
dock_key` → `BottomDockTab::Cargo` arm → `handle_cargo_panel_key` → this
doc's new `KeyCode::Up` arm increments `output_scroll`.

## 6. Dependencies & integration points

Extends `tui-panel-focus-and-scroll.md` (T51, whose `bottom_dock_body`
routing this doc reuses unchanged) and revises the scroll-back cut
documented in `tui-claude-panel.md` §1.1 and `tui-cargo-panel.md`. No
`ide-core`/`ide-lsp`/`ide-dap` changes; entirely `crates/tui/src/{app.rs,
ui.rs,cargo_panel.rs,claude_panel.rs,ai_panel.rs}`.
