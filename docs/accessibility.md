# Accessibility review

This is the review §21 M6 requires. It is a measurement, not a statement of
intent: every claim below is asserted by
[`crates/monitrs-tui/tests/accessibility.rs`](../crates/monitrs-tui/tests/accessibility.rs),
which renders real frames through the real reducer and computes the contrast
ratios with the same code that prints the tables on this page. Where monitrs falls
short, the number is here with the reason it was not fixed and the change that
would fix it.

Regenerate the contrast tables with:

```sh
cargo test -p monitrs-tui --test accessibility -- --nocapture contrast_report
```

## Verdict

| Requirement | Result |
|---|---|
| Colour is never the only signal (§5.2, §2.3) | Pass. Every state enum carries a distinct symbol and every widget that colours by state emits it. Three cues are weaker than the rest and are named below. |
| `--color off` stays legible (§5.2) | Pass. Characters are identical at all four depths; selection, notable process states, and pressure severity all survive. |
| Contrast, text ≥ 4.5:1 (WCAG 2.1 AA) | Pass after two fixes, with six documented shortfalls in secondary tokens. |
| `high-contrast` is actually high contrast (§3.1) | Pass after one fix. It failed on the selected row's band. |
| Strict ASCII is 7-bit (§5.1) | Pass. Coverage extended from the glyph table to whole frames and every model string. |
| No flashing, no animation (§3.2, §5.2) | Pass. No blink attribute anywhere; a frame is a pure function of state. |
| Keyboard-only operation | Pass. No pointer is required, mouse reporting is off by default, and the one timed input has an untimed alternative. |
| Screen-reader support | **Not available.** A full-screen TUI cannot provide it. See [§6](#6-screen-readers). |
| Narrow terminals | Usable to 60×16; pressure severity is lost at 80×24 and the key hints at 60×16. See [§7](#7-breakpoints). |

Three defects were found and fixed. All three were in `crates/monitrs-tui/src/theme.rs`:

1. `default-light` drew `good` in ANSI `Green` — **2.16:1** against its own
   background — and `watch` in ANSI `Yellow` — **1.70:1**. The word `watch` in
   yellow on white is not readable at any size.
2. `default-light` at 256 colours drew `good` at **4.05:1** and `watch` at
   **4.06:1** against `surface`, which is the background the overlays paint.
3. `high-contrast` drew the selected row's band in `#0000c0`, **1.76:1** against
   its own black background. Blue carries 7% of relative luminance, so no
   saturated dark blue can separate from black; the band was invisible in the one
   theme whose stated purpose is separation.

## 1. Colour is never the only signal

§5.2 forbids relying on red/green alone and §2.3 names `.`, `!`, `X` as required
redundant cues. The implementation is structural rather than conventional in two
places, which is why the property holds: `Cue` in `theme.rs` pairs a colour token
with its character so the main state-colouring path cannot hand out one without the
other, and `MetricDisplay` in `widgets/states.rs` is the *only* thing in the crate
that matches on `MetricState` for display — nine widgets each writing their own
`match` would be nine chances to render `PermissionDenied` as `0%`. The characters
themselves come from frozen `symbol()` methods in `monitrs-core`, so a state reads
identically on every screen.

A widget can still ask for a bare `Token`, and one does: `table.rs` draws a whole
notable row in `Token::Critical`. That is why the checks below are made against
rendered frames rather than against the token API.

### The symbol inventory

Every variant of every state enum, with the symbol and word it renders as. Symbols
are asserted pairwise distinct **within** each type — that is the property that
matters, because a symbol is only ever read inside one column.

| `MetricState` | Symbol | Text |
|---|:--:|---|
| `Available` | *(space)* | the value |
| `Stale` | `~` | the value, plus its age |
| `WarmingUp` | `.` | `warming up` |
| `PermissionDenied` | `!` | `permission denied` |
| `Unsupported` | `-` | `n/a` |
| `TemporarilyUnavailable` | `?` | the named reason, e.g. `link speed unknown` |

| `PressureState` | Symbol | Text |
|---|:--:|---|
| `Normal` | `.` | `normal` |
| `Watch` | `!` | `watch` |
| `Critical` | `X` | `critical` |

| `ProcessState` | Code | Text |
|---|:--:|---|
| `Running` | `R` | `running` |
| `Sleeping` | `S` | `sleeping` |
| `UninterruptibleSleep` | `D` | `uninterruptible sleep` |
| `Zombie` | `Z` | `zombie` |
| `Stopped` | `T` | `stopped` |
| `Traced` | `t` | `traced` |
| `Idle` | `I` | `idle` |
| `Dead` | `X` | `dead` |
| `Unknown` | `?` | `unknown` |

| `LinkState` | Symbol | Text |
|---|:--:|---|
| `Up` | `+` | `up` |
| `Down` | `-` | `down` |
| `Dormant` | `.` | `dormant` |
| `Unknown` | `?` | `unknown` |

| `ChargeState` | Symbol | Text |
|---|:--:|---|
| `Charging` | `+` | `charging` |
| `Discharging` | `-` | `discharging` |
| `Full` | `=` | `full` |
| `NotCharging` | `.` | `not charging` |
| `Unknown` | `?` | `unknown` |

| `CapabilityState` | Symbol | Text |
|---|:--:|---|
| `Available` | `+` | `available` |
| `Unsupported` | `-` | `unsupported` |
| `PermissionDenied` | `!` | `permission denied` |
| `Unknown` | `?` | `not probed` |

| `Severity` | Symbol | Text |
|---|:--:|---|
| `Info` | `.` | `info` |
| `Watch` | `!` | `watch` |
| `Critical` | `X` | `critical` |

### Where the widgets emit them

Distinct symbols in the model prove nothing on their own; the review checked the
rendered buffer. Each row below is asserted against a real frame:

| State | Rendered as | Screen |
|---|---|---|
| `PressureState` | `. CPU     normal        79%` — leading symbol column | Overview radar, Inspect pressure section |
| pressure with no derived state | `? CPU     warming     16%` — the kind of unknown, never `normal` | Overview radar |
| `MetricState`, wide field | `!permission denied`, `.warming up` — symbol then text, via `MetricDisplay::flagged` | radar rows, header meter notes, compact summary, Inspect |
| `MetricState`, narrow column | the text alone, degrading `permission denied` → `denied` → `n/a` → `!` as the column narrows | process table numeric cells |
| `MetricState::Stale` | `~2.6G` — the marker is prefixed even in a six-cell column | every numeric cell |
| `ProcessState` (notable) | `Z` / `D` in the one-cell marker column | process table, all four breakpoints |
| `LinkState` | `+up`, `-down` | Network interfaces, Overview footer |
| `ChargeState` | `bat 82%-` | header meter notes, compact summary |
| `CapabilityState` | `  ! process I/O    permission denied`, `  - disk busy    unsupported` | Inspect, `UNAVAILABLE METRICS` |
| `Severity` | `! collector /proc/diskstats read failed` | status footer and notice overlay |
| selection | `>` in the marker column, plus a reversed row with colour off | every table |
| active tab | `[1 Overview] 2 Processes` — brackets replace padding, so no width shifts | status footer |
| timeline state | `[>LIVE]`, `[=PAUSED]`, `[<HISTORY -00:37]` | header badge |

Two details are deliberate and worth naming, because both look like bugs until you
know why:

* **`MetricState::WarmingUp` and `PressureState::Normal` both answer `.`.** A radar
  row for a signal that has no derived state would therefore be indistinguishable
  from a healthy one. `states::describe_pressure` overrides the symbol to `?` for
  any pressure state without a value, so "we don't know" never reads as "fine".
  This is the single most consequential accessibility decision in the codebase. It
  is pinned by three tests, one of which asserts `? NET` in a rendered frame rather
  than in the model.
* **`Token::Good` carries no modifier**, exactly like `Token::Text`. That is the
  point: §5.2's "avoid more than one accent colour in a numeric row" and §4's "a
  normal reading is not an anomaly" both say a healthy value must not be decorated.
  Only `watch` (bold) and `critical` (bold + underline) get emphasis.

### Fixed: narrow columns no longer collapse `permission denied` into `n/a`

This review found that `MetricDisplay::fitted` degraded every placeholder straight
to `n/a`, which is `MetricState::Unsupported`'s own placeholder — so in any column
narrower than 17 cells `permission denied`, `warming up` and `unsupported` rendered
as the same three characters, separated only by weight (`Token::Watch` bold versus
`Token::Muted` dim). Bold-versus-dim survives `--color off`, so it was not a
colour-only failure, but it was the weakest cue anywhere in the interface: it asked
a low-vision reader to distinguish "the OS refused", "wait a second" and "this
machine cannot report this" by font weight.

It was also not hypothetical. The Overview's Pressure Radar reserves eight cells
for its state column, so on a machine still warming up **every** radar row read
`n/a` — indistinguishable from a machine that supports none of the signals.

**Fixed** by `states::abbreviated_placeholder`, which adds one rung to the ladder:
`permission denied` → `denied` (six cells) → `n/a` → `!`, and `warming up` →
`warming` (seven cells) → `n/a` → `!`. `MetricState::TemporarilyUnavailable`
deliberately gets no abbreviation, because its message is a specific claim
(`counter reset`, `device disappeared`) and a shortened specific claim is a
different claim. The abbreviations are matched against
`MetricState::placeholder()` rather than written out as literals, so renaming a
phrase in `monitrs-core` makes the abbreviation stop applying instead of applying
to the wrong state.

### Shortfall: the three- to five-cell numeric columns still collapse

`CPU%` and `MEM%` are five cells wide, which fits neither `denied` nor `warming`, so
those two columns still fall back to `n/a` for every unavailable state. What
separates them there is the token (bold versus dim, surviving `--color off`) and,
at one cell, the symbol. **Not changed**, because the alternative — spending one of
the five cells on the symbol to print `! n/a` — means widening both columns by a
cell and taking it from the `NAME`/`COMMAND` columns that §7.2 gives higher
priority. The information is available elsewhere for anyone who needs it: Inspect's
`UNAVAILABLE METRICS` panel names every capability with its own symbol and its own
words, and the process-detail overlay has room for the full placeholder.

Pinned by `tests/accessibility.rs::a_narrow_column_keeps_two_unavailable_states_apart_by_text`,
which asserts the distinction from six cells up *and* the remaining collapse below
it, so neither half can change silently.

### Shortfall: `Stopped` and `Traced` differ only by letter case

`ProcessState::Stopped` is `T` and `Traced` is `t`. These are the `ps` letters and
existing knowledge transfers, which is why they were chosen — but a user who
cannot resolve case at their font size cannot tell a job-control stop from a
debugger stop. Neither state is "notable" under §7.2, so neither reaches the marker
column; they appear only in the one-cell `STATE` column and in the detail overlay,
which spells the state out in full (`stopped` / `traced`). **Not changed**, because
diverging from `ps` would cost every user who already knows the letters. If it
needs fixing, the honest fix is to widen the `STATE` column to three cells at
Standard and Wide and print `stp` / `trc`, which costs two cells of the `NAME`
column.

### Shortfall: `Token::Accent` and `Token::Watch` are the same with colour off

Both resolve to plain `Modifier::BOLD`, so at `--color off` an accent (the brand,
the active filter, the `LIVE` badge) and a `watch` state are typographically
identical. They never compete for the same cell — an accent is chrome, a `watch` is
a value — and in every place `watch` is used the *text* is the signal: `watch` in a
radar row, `!permission denied` in a wide field, and a `lag 1.4s` segment that is
only present at all when the collector is behind. **Not changed**, because the
alternatives are worse: adding `ITALIC` to `Accent` would collide with `Stale`, and
adding `UNDERLINED` would collide with `Critical`. There are four widely supported
attributes and five meanings that need them.

## 2. Colour modes

| Depth | Selected by | What monitrs uses |
|---|---|---|
| `truecolor` | `--color truecolor`, or `COLORTERM=truecolor`/`24bit` | 24-bit RGB |
| `256` | `--color 256`, or `TERM` containing `256color` | the xterm indexed palette |
| `16` | `--color 16`, or any other non-`dumb` `TERM` | the sixteen ANSI names |
| `off` | `--color off`, `--no-color`, `NO_COLOR=1`, `TERM=dumb`, or no `TERM` at all | `Color::Reset` plus modifiers only |

`NO_COLOR` is honoured unless an explicit `--color` flag overrides it. `--color
auto` is *not* such an override: "detect" cannot be a statement of intent to have
colour, so `NO_COLOR=1 monitrs --color auto` is monochrome.

At `ColorDepth::Off` every foreground is `Color::Reset` and the only surviving
signals are:

* the **symbols** in the table above;
* the **modifiers** — `DIM` for `muted` and `border`, `BOLD` for `accent`, `watch`
  and `focus_border`, `BOLD | UNDERLINED` for `critical`, `DIM | ITALIC` for
  `stale`;
* `REVERSED` on the selected row, which is the only way to separate a row when
  both of its colours are the terminal's default.

The characters drawn are **identical** at all four depths. That is asserted for all
five screens in all three themes: if a depth changed a character, one rendering
would be carrying meaning the other did not.

A theme cannot add its own modifiers. `Token::emphasis` belongs to the token's
meaning, not to the palette, because a theme that boldened everything would
collapse `good` into `watch` at zero colour and silently destroy the property this
section is about.

## 3. Contrast

### Method

Ratios are WCAG 2.1 relative luminance: each channel linearised as
`c/12.92` below 0.03928 and `((c+0.055)/1.055)^2.4` above, weighted
`0.2126 R + 0.7152 G + 0.0722 B`, then `(L_lighter + 0.05) / (L_darker + 0.05)`.

Floors applied:

* **4.5:1** for anything that renders readable text — `text`, `good`, `watch`,
  `critical`, `accent`, `muted`, `stale`, and the selected row.
* **3:1** for symbols, bars, bands, and borders — `border`, `focus_border`,
  `graph_1`..`graph_6`, and the selected row's band against the surrounding
  background.

Four caveats, all of which limit what these numbers can mean:

1. **monitrs does not paint `base`.** No screen fills the terminal background;
   `Token::Base` describes the background the theme was *designed for*, and
   `Token::Surface` is painted only behind overlays. A ratio against `base` is
   therefore a statement about the theme's internal consistency — if you run
   `default-light` on a black terminal you get different numbers, better for
   `good` and `watch` and much worse for `text`.
2. **The sixteen ANSI colours are not monitrs's to choose.** The `16` column below
   resolves them against xterm's defaults. Every terminal ships its own and many
   users replace them, so those figures are indicative. What *is* under monitrs's
   control is which of the sixteen names a token uses, and that is where fix (1)
   applies.
3. **`DIM` is not modelled.** `muted`, `border`, and `stale` carry
   `Modifier::DIM`, which terminals implement by reducing intensity — typically to
   half. The figures for those three tokens are therefore upper bounds, and the
   real rendered contrast is lower by an amount no palette value can compensate
   for.
4. **`off` has no ratio.** `Color::Reset` means "whatever the terminal already
   uses", which has no measurable luminance. That mode is covered by section 2
   instead.

### `default-dark`

| pair | min | truecolor | 256 | 16 |
|---|---:|---:|---:|---:|
| `text` on `base` | 4.5 | 13.15 | 11.05 | 16.67 |
| `text` on `surface` | 4.5 | 12.16 | 9.81 | 16.67 |
| selected row (`text` on `selection`) | 4.5 | 9.06 | 4.56 | 7.46 |
| `good` on `base` | 4.5 | 9.26 | 9.82 | 15.30 |
| `watch` on `base` | 4.5 | 9.21 | 8.27 | 19.56 |
| `critical` on `base` | 4.5 | 6.96 | 5.87 | 5.25 |
| `good` on `surface` | 4.5 | 8.57 | 8.72 | 15.30 |
| `watch` on `surface` | 4.5 | 8.52 | 7.34 | 19.56 |
| `critical` on `surface` | 4.5 | 6.44 | 5.22 | 5.25 |
| `accent` on `base` | 4.5 | 7.31 | 7.80 | **4.43** |
| `muted` on `base` | 4.5 | 4.90 | 4.94 | 5.24 |
| `stale` on `base` | 4.5 | **3.81** | **3.75** | 5.24 |
| `focus_border` on `base` | 3.0 | 7.77 | 10.27 | 16.75 |
| `border` on `base` | 3.0 | **1.83** | **2.40** | 5.24 |
| `selection` band vs `base` | 3.0 | **1.45** | **2.42** | **2.23** |
| `graph_1` on `base` | 3.0 | 7.31 | 7.80 | 4.43 |
| `graph_2` on `base` | 3.0 | 9.26 | 9.82 | 15.30 |
| `graph_3` on `base` | 3.0 | 9.21 | 8.27 | 19.56 |
| `graph_4` on `base` | 3.0 | 7.65 | 6.77 | 6.70 |
| `graph_5` on `base` | 3.0 | 7.77 | 9.89 | 16.75 |
| `graph_6` on `base` | 3.0 | 6.96 | 5.87 | 5.25 |

### `default-light`

Figures include fix (1) and fix (2).

| pair | min | truecolor | 256 | 16 |
|---|---:|---:|---:|---:|
| `text` on `base` | 4.5 | 15.01 | 13.20 | 21.00 |
| `text` on `surface` | 4.5 | 13.87 | 11.38 | 21.00 |
| selected row (`text` on `selection`) | 4.5 | 10.78 | 8.50 | 10.61 |
| `good` on `base` | 4.5 | 5.19 | 7.96 | 9.40 |
| `watch` on `base` | 4.5 | 5.73 | 5.73 | 4.69 |
| `critical` on `base` | 4.5 | 7.51 | 7.44 | 5.84 |
| `good` on `surface` | 4.5 | 4.80 | 6.86 | 9.40 |
| `watch` on `surface` | 4.5 | 5.30 | 4.94 | 4.69 |
| `critical` on `surface` | 4.5 | 6.93 | 6.41 | 5.84 |
| `accent` on `base` | 4.5 | 5.89 | 6.45 | 9.40 |
| `muted` on `base` | 4.5 | 5.85 | **3.95** | **4.00** |
| `stale` on `base` | 4.5 | 4.68 | **3.45** | **4.00** |
| `focus_border` on `base` | 3.0 | 5.68 | 4.36 | 4.74 |
| `border` on `base` | 3.0 | **1.64** | **1.90** | **1.26** |
| `selection` band vs `base` | 3.0 | **1.39** | **1.55** | **1.98** |
| `graph_1` on `base` | 3.0 | 5.89 | 6.45 | 9.40 |
| `graph_2` on `base` | 3.0 | 5.19 | 4.70 | **2.16** |
| `graph_3` on `base` | 3.0 | 5.73 | 4.71 | **1.70** |
| `graph_4` on `base` | 3.0 | 7.37 | 8.82 | 4.69 |
| `graph_5` on `base` | 3.0 | 5.68 | 4.36 | 4.74 |
| `graph_6` on `base` | 3.0 | 7.51 | 7.44 | 5.84 |

### `high-contrast`

Figures include fix (3).

| pair | min | truecolor | 256 | 16 |
|---|---:|---:|---:|---:|
| `text` on `base` | 4.5 | 21.00 | 21.00 | 21.00 |
| `text` on `surface` | 4.5 | 21.00 | 21.00 | 21.00 |
| selected row (`text` on `selection`) | 4.5 | 5.09 | 5.12 | 4.74 |
| `good` on `base` | 4.5 | 15.30 | 15.30 | 15.30 |
| `watch` on `base` | 4.5 | 19.56 | 19.56 | 19.56 |
| `critical` on `base` | 4.5 | 5.25 | 5.25 | 5.25 |
| `good` on `surface` | 4.5 | 15.30 | 15.30 | 15.30 |
| `watch` on `surface` | 4.5 | 19.56 | 19.56 | 19.56 |
| `critical` on `surface` | 4.5 | 5.25 | 5.25 | 5.25 |
| `accent` on `base` | 4.5 | 16.75 | 16.75 | 16.75 |
| `muted` on `base` | 4.5 | 11.54 | 11.06 | 16.67 |
| `stale` on `base` | 4.5 | 11.54 | 11.06 | 16.67 |
| `focus_border` on `base` | 3.0 | 19.56 | 19.56 | 19.56 |
| `border` on `base` | 3.0 | 21.00 | 21.00 | 21.00 |
| `selection` band vs `base` | 3.0 | 4.13 | 4.10 | 4.43 |
| `graph_1` on `base` | 3.0 | 21.00 | 21.00 | 21.00 |
| `graph_2` on `base` | 3.0 | 16.75 | 16.75 | 16.75 |
| `graph_3` on `base` | 3.0 | 15.30 | 15.30 | 15.30 |
| `graph_4` on `base` | 3.0 | 19.56 | 19.56 | 19.56 |
| `graph_5` on `base` | 3.0 | 6.70 | 6.70 | 6.70 |
| `graph_6` on `base` | 3.0 | 5.25 | 5.25 | 5.25 |

`critical` at 5.25:1 is the lowest of this theme's three state colours, and it
cannot be raised: pure `#ff0000` has a relative luminance of 0.2126, so red on
black cannot exceed 5.25:1. Reaching AAA (7:1) would need about `#ff6666` (7.34:1),
which stops reading as red. The `X` symbol and the word `critical` carry the state
regardless, so this is reported rather than changed.

### Fixes applied

**Fix 1 — `default-light`, ANSI-16 `good` and `watch`.** On a light background only
four of the sixteen ANSI names clear 4.5:1: `Black` (21.00), `Blue` (9.40), `Red`
(5.84), and `Magenta` (4.69). `Green` is 2.16:1 and `Yellow` is 1.70:1 under any
plausible palette, because green and yellow are the highest-luminance hues in every
16-colour scheme. `good` therefore takes `Blue` and `watch` takes `Magenta`, with
`critical` keeping `Red`. Magenta is not an obvious "elevated" colour; it is the
only remaining legible option, and the `!` symbol and the word `watch` are what
actually carry the state.

**Fix 2 — `default-light`, 256-colour `good` and `watch`.** Palette index 28
(`#008700`) measured 4.05:1 and index 130 (`#af5f00`) measured 4.06:1 against
`surface` — the background the overlays paint, where the signal-confirmation
dialog and the notice list live. Both moved darker, to index 22 (`#005f00`) and
index 94 (`#875f00`).

**Fix 3 — `high-contrast`, the selection band.** `#0000c0` measured 1.76:1 against
black at truecolor, 1.62:1 at 256, and 2.23:1 at 16. The band is now `#5555ff` at
truecolor, index 62 (`#5f5fd7`) at 256, and `LightBlue` at 16. At truecolor that
gives white text 5.09:1 *and* lifts the band itself to 4.13:1 against `base`. Those
two ratios trade against each other: for white text on a solid colour on black,
`min(text, band)` cannot exceed 4.58:1, so 5.09 and 4.13 is close to the best
available split.

### Shortfalls reported, not fixed

| Shortfall | Measured | Why not changed | What would fix it |
|---|---|---|---|
| `default-dark` `stale` on `base` | 3.81 / 3.75 | The only way to raise it inside this palette is to move it into `muted`'s luminance band, erasing the muted/stale distinction. `stale` also carries `DIM`, so no RGB value makes it reliably pass. A retained value is never the only copy: Inspect's `STALE DATA` panel lists the metric and its age in `text`. | Give `stale` its own hue rather than its own grey — a blue-grey around `#7a8fa8` reaches 5.5:1 while staying visibly cooler than `text` — and drop `DIM`, keeping `ITALIC`. |
| `border` on `base`, both default themes | 1.83 / 1.64 | A panel's identity is carried by its **title text** in the border row (`+ PRESSURE ----`), and the focused panel's border is `focus_border` at 7.77:1 / 5.68:1. Raising `border` fights §5's "restrained colours, strong alignment". | Lift `border` to ≥3:1 — for the dark theme `#3b4252` → `#606a7c` measures 3.37:1. The cost is a visibly busier grid. |
| `selection` band vs `base`, both default themes | 1.45 / 1.39 | Selection is carried by the `>` marker, which is column priority 0 and present at every width, and by the row's own foreground/background pair at 9.06:1 / 10.78:1. | Same trade as `high-contrast` fix 3: a brighter band at the cost of the restrained look. Would also reduce the selected row's own text contrast. |
| `default-light` `muted` and `stale` | `muted` 3.95 (256), 4.00 (16); `stale` 3.45 (256), 4.00 (16) | The ANSI-16 set has exactly one grey (`DarkGray`, 4.00:1 on white) and it has to be *less* prominent than `text` while staying legible. There is no second candidate. | At 256, grey indices 242 and 241 reach 4.53:1 and 5.26:1 against `surface`. At 16 there is nothing to move to. |
| `default-dark` `accent` at 16 | 4.43 | Under 2% below the floor, on a value the terminal owns (`LightBlue`). The alternatives — `LightCyan`, `LightMagenta` — are already spent on `focus_border` and `graph_4`. | Nothing inside 16 colours. Users who need it should use `--color 256` or the `high-contrast` theme. |
| `default-light` `graph_2` / `graph_3` at 16 | 2.16 / 1.70 | Six distinct series colours at ≥3:1 on a light background **do not exist** in ANSI-16: only `Black`, `Blue`, `Red`, `Magenta`, and `DarkGray` clear it, and that is five. | Reduce the light theme to five series at 16 colours, or refuse to draw multi-series plots below 256 colours. Both are behaviour changes beyond the scope of a review. |

**There is no way for a user to override a single token.** `display.theme` and
`--theme` accept one of the three built-in names; the palette itself is not
configurable. A user whose terminal renders one of the shortfalls above worse than
the table says has no recourse except switching theme or colour depth. Adding a
`[display.palette]` table of token-to-colour overrides is the obvious remedy and is
not implemented.

## 4. Strict ASCII

`--glyphs ascii` (or `--ascii`) restricts output to printable 7-bit ASCII,
`0x20..=0x7e`. Auto-detection falls back to ASCII unless the effective locale
declares a UTF-8 codeset **and** `TERM` is set and is not `dumb`.

The existing tests covered the *inventory* — every `Glyph` variant, the nine-level
ramp, the ellipsis — and every string `bar`, `meter`, `sparkline`,
`dense_sparkline`, `unknown_bar`, and `unknown_meter` can produce, including
adversarial `NaN` and infinite inputs at every width from 0 to 40. That is a real
guarantee for the design system, but it did not cover the strings that reach the
screen *around* the glyphs. Three gaps were closed:

1. **Whole frames.** All five screens × ten fixtures (including the warming-up
   first frame, the frame before any snapshot, permission-denied, stale, 256 cores,
   an empty process list, and a saturated machine) × five sizes (140×38, 110×30,
   80×24, 60×16, 52×12) are now asserted byte-by-byte. The previous frame-level
   test covered one screen at one size.
2. **Every model string.** `MetricState::placeholder` and `symbol` for all six
   variants, all ten `UnavailableReason` messages — which are also asserted
   pairwise distinct, because they all share the `?` symbol and the words are the
   only thing separating a counter reset from an interface rename — and the symbol
   and label of `PressureState`, `ProcessState`, `LinkState`, `ChargeState`,
   `CapabilityState`, and `Severity`.
3. **Every chrome string monitrs writes by hand** — all fourteen column headers,
   the four breakpoint labels, the three theme names, the eighteen token names, and
   every chord label, binding description, help section title, and help entry in
   the built-in keymap across all seven input modes. These are exactly where a
   stray `–` or `…` would appear, because they are hand-written literals rather
   than glyph lookups.

Process names and command lines are **not** constrained, and cannot be: a process
may legitimately be called `日本語のプロセス`. Strict ASCII governs the characters the
design system emits. Width accounting is grapheme- and East-Asian-width aware, so a
double-width name occupies the two cells it needs and the columns to its right do
not shift.

The complement is also asserted: enhanced mode must produce at least one non-ASCII
character in a real frame, so `--glyphs ascii` cannot be trivially satisfied by an
enhanced set that quietly regressed to ASCII.

## 5. No flashing, no animation

§3.2 forbids animated effects that reduce legibility and §5.2 forbids continuously
alternating or flashing colours. Both are structural properties here rather than
review items:

* **`Theme` holds no state.** No frame counter, no phase, no clock. `Theme::style`
  is a pure function of `(token, depth)`.
* **No blink is representable in a token.** `Token::emphasis` returns only `DIM`,
  `BOLD`, `UNDERLINED`, and `ITALIC`. Asserted over every cell of every frame of
  every screen at every colour depth: no `SLOW_BLINK`, no `RAPID_BLINK`. The three
  occurrences of `SLOW_BLINK` in the crate are test sentinels — a style no widget
  produces, used to prove a widget did not write outside its rectangle.
* **A frame is a pure function of state.** Rendering the same `AppState` four times
  produces four identical buffers, and two independently constructed states with
  different `Instant::now()` values produce identical frames. There is no time
  input for an animation to be a function of. Effects — including anything that
  reads a clock — are returned by the reducer and executed by the binary (§10.5),
  never performed during render.
* **Nothing disappears on a timer.** Notices are evicted by count (16 retained),
  not by age, so a message cannot vanish before it has been read. The only
  animated-looking element is the sparkline, which advances one cell per sample at
  the configured interval (1 s by default, `--interval` accepts 250 ms to 60 s);
  `Space` freezes the timeline entirely (§2.1) and the header then says
  `[=PAUSED]`, so a reader who needs unlimited time has it.

The one timing requirement in the interface is the 500 ms window for a two-key
sequence, and it is **not configurable** from the CLI or the configuration file
(`KeyResolver::with_timeout` exists but the binary always passes the default).
That is acceptable only because every sequence has a single-key alternative — the
sole two-key binding is `gg`, and `Home` does the same thing — which is asserted
rather than assumed. If a second chord is ever added without an alternative, that
test fails.

## 6. Screen readers

**monitrs is not usable with a screen reader, and it cannot be made so as a TUI.**
Saying otherwise would be dishonest, so here is precisely why and what exists
instead.

A screen reader reads a linear document with a semantic structure. monitrs writes a
two-dimensional grid of cells to the alternate screen, in raw mode, with the
hardware cursor hidden and the whole frame redrawn every second. There is no
document, no reading order, no roles or labels, and no way to announce "the value
in this cell changed" — the terminal protocol has no channel for any of that.
Specifically:

* **Alternate screen** means the content is outside scrollback, so a screen
  reader's review cursor has nothing to walk.
* **Raw mode** means keystrokes never reach the reader's own command layer.
* **Hidden cursor** removes the one position marker a reader could track. monitrs
  draws its own `>` marker instead, which is visual only.
* **A full redraw every interval** would produce continuous speech even if the
  content were readable.

What monitrs does instead:

1. **`monitrs snapshot --format json` is a complete, machine-readable alternative.**
   It takes real samples through the same collectors and prints the whole
   `SystemSnapshot`, so anything the interface can show can be read, scripted, or
   piped into a tool that *is* accessible. Availability is a named string, never a
   number:

   ```console
   $ monitrs snapshot --format json | jq '.capabilities.disk_busy, .host.environment'
   "unsupported"
   "unsupported"

   $ monitrs snapshot --format json | jq '.pressure.signals[0]'
   {
     "id": "cpu",
     "state": "warming_up",
     "severity": "warming_up",
     "raw": null,
     "rule": "awaiting samples",
     "held_for": null
   }
   ```

   (Both transcripts are from a real macOS arm64 host; which capabilities are
   `unsupported` differs by platform.) Note `"state": "warming_up"` rather than
   `0`, and `rule` carrying the sentence that produced the state — the same text
   §2.3 requires on screen. Process arguments are redacted unless
   `--include-arguments` is passed, and environment variable values are never read
   at all (§15.2), so the output is safe to paste into a bug report. The default
   `--samples 2` exists because delta-based metrics are `warming up` in the first
   sample.
2. **Every visual distinction has a textual equivalent**, which is what makes the
   JSON export a faithful substitute rather than a different product. The words are
   the same ones on screen: `permission_denied`, `warming_up`, `unsupported`,
   `critical`, and the rule sentence.
3. **Placeholders are prose.** `warming up`, `permission denied`, `n/a`, `link
   speed unknown` — never an empty cell, never a dash standing in for a number, and
   never a `0`.
4. **The resize notice is plain text**, not a diagram, and it names the current
   size so a user knows how far to drag:

   ```text
   monitrs needs at least 60x16
   current terminal: 52x12
   resize or press q to quit
   ```

What it would take to do better: a `monitrs --plain` mode that writes successive
line-oriented reports to stdout without the alternate screen, raw mode, or cursor
hiding — effectively `vmstat` with monitrs's semantics. That is a new output
front-end, not an adjustment to this one, and it is not implemented.

## 7. Breakpoints

§5.7 fixes four bands. What follows is what is actually on screen at each, taken
from rendered frames rather than from the specification.

| | Wide 140×38 | Standard 110×30 | Compact 80×24 | Minimal 60×16 | Below 60×16 |
|---|:--:|:--:|:--:|:--:|:--:|
| header line (host, timeline badge, interval, clock) | yes | yes | yes | yes | — |
| uptime in the header | yes | yes | yes | dropped | — |
| two header meters with bars | yes | yes | one-line summary | dropped | — |
| `PRESSURE` panel | yes | yes (focus-selected) | **dropped** | dropped | — |
| `HISTORY` sparklines | yes | yes | **dropped** | dropped | — |
| `PINS` / `NETWORK` footer panels | yes | dropped | dropped | dropped | — |
| process table | yes | yes | yes | bare list, no border | — |
| tab strip and `? help` hint | yes | yes | yes | **dropped** | — |
| resize notice | — | — | — | — | yes |

Process table columns, in §7.2 admission order:

| Column | 140×38 | 110×30 | 80×24 | 60×16 |
|---|:--:|:--:|:--:|:--:|
| selection marker | yes | yes | yes | yes |
| `NAME` | yes | yes | yes | yes |
| `PID` | yes | yes | yes | yes |
| `CPU%` | yes | yes | yes | yes |
| `RSS` | yes | yes | yes | yes |
| `MEM%` | yes | yes | yes | yes |
| `USER` | yes | yes | yes | yes |
| `S` (state code) | yes | yes | yes | yes |
| `READ/s` `WRITE/s` | yes | yes | yes | **dropped** |
| `AGE` | yes | yes | **dropped** | dropped |
| `THR` | yes | yes | **dropped** | dropped |
| `VIRT` | yes | yes | **dropped** | dropped |
| `COMMAND` | yes | **dropped** | dropped | dropped |

Two things survive every step, by design: the **selection marker** and the
**notable state code**. The marker column is priority 0 and the state code is
written into it for zombie and `D`-state rows, precisely because the `S` column is
the sort of thing a narrow layout drops. Both are asserted present at 140×38,
110×30, 80×24, and 60×16 with colour switched off.

Overlays — help, the command palette, process detail, the signal confirmation, the
sort selector, spike attribution, the notice list — all render at 80×24; the
confirmation dialog and the notice list have dedicated 80×24 snapshots. Overlays
erase what is behind them rather than compositing, which matters for the
confirmation dialog: a §15.1 safety prompt must not have another screen's text
showing through it.

### Shortfall: pressure severity is invisible in the Compact band

At 80×24 — and at any size below 28 rows, however wide — the `PRESSURE` panel is
not drawn, and the one-line summary shows `CPU 99%  MEM 99%` in ordinary `text`
without a severity cue. A machine in `critical` memory pressure looks like a
machine with high memory use. This matches §5.7, which lists the Compact band's
contents and does not include pressure, but for an accessibility review it is the
most consequential thing lost on a small terminal.

Reachable instead: `5` opens Inspect, whose `PRESSURE` section lists every
non-normal signal with its symbol, its state, and the rule that derived it, and
which does fit at 80×24. **Not changed** — spending a header cell on a severity
badge is a layout decision beyond a review's remit — but the concrete fix is small:
prefix the compact summary's `CPU`/`MEM` segments with
`snapshot.pressure` worst-state symbol and colour them with the matching token,
which costs one cell each and is the same `Cue` the radar already uses.

### Shortfall: the key hints disappear at 60×16

At 60×16 the tab strip and the `? help` hint are dropped for the row they occupy.
Every key still works — `?` still opens help, `1`–`5` still switch screens — but
the only affordance telling a first-time user that is gone at exactly the size
where the interface is least self-explanatory. **Not changed** — §5.7 asks only for
"a stable minimal process list" at this size — and the fix costs one of the
fifteen list rows: reserve the last row for a two-item hint (`? help  q quit`).
