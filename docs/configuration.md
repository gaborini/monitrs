# Configuration

monitrs is useful with no configuration at all, and **does not create a
configuration file on first launch**. If you never run `config init`, no file
exists and the built-in defaults apply.

```sh
monitrs config path            # where monitrs looks, and whether a file is there
monitrs config init            # write a documented starter file (never overwrites)
monitrs config init ./my.toml  # ...somewhere else
monitrs config check           # validate without launching
monitrs config check ./my.toml
```

## Where the file lives

Search order:

1. `--config <PATH>`, if given. A path you name that does not exist is an
   **error** — falling back to defaults would hide a typo.
2. The platform's user configuration directory:
   * Linux: `$XDG_CONFIG_HOME/monitrs/monitrs.toml`, else
     `~/.config/monitrs/monitrs.toml`
   * macOS: `~/Library/Application Support/monitrs/monitrs.toml`
3. Built-in defaults.

`--no-config` skips steps 1 and 2 entirely.

Files written by `config init` are `0600` — owner-only — because configuration
sits next to things that can contain paths and filters you may not want readable.

## Rules

* **CLI flags override file values.** Not "not given" — actually given. A flag you
  omit leaves the file's value alone.
* **An invalid value names its exact key**, and *every* problem is reported at
  once, so fixing three mistakes takes one run:

  ```
  monitrs: 3 problem(s) in ./my.toml:
    sampling.interval: must be between 250ms and 1m, got 50ms
    sampling.history: must be between 30s and 1h, got 5s
    processes.sort: "entropy" is not a sortable column; expected one of cpu, memory, ...
  ```

* **An unknown key is rejected, not ignored** — a silently ignored key is a
  setting you believe is in effect. Near misses get a suggestion:

  ```
  unknown field `intervall`, expected one of `interval`, `history`, ...
  did you mean `interval`?
  ```

* **Reload is atomic.** The whole candidate file is parsed and validated before
  anything replaces the running configuration, so a typo cannot leave monitrs
  half-reconfigured. A refused reload says so and leaves the running configuration
  in force; it is not a reason to stop.

  What a successful reload reaches is deliberately all three places the settings
  live: the running configuration, the interface, and the sampler thread. The
  interface picks up the theme, the glyph mode, the colour depth, the units, the
  ordering, the filter, the kernel-thread toggle and the keymap; the sampler picks
  up `sampling.interval` and the whole of `[diagnostics]`. A reload that changes
  `sampling.interval` or `sampling.history` reshapes the history ring, which
  **discards the samples it holds** — the Time Lens starts again from now, and
  monitrs says so rather than letting a scrubbed timeline quietly empty.

  Two settings cannot take effect until restart, and are named in the notice
  rather than silently dropped:

  | Key | Why |
  |---|---|
  | `display.mouse` | Mouse capture is a terminal mode set once at startup. Reported for as long as the file disagrees with the mode the terminal is actually in, not merely on the first reload that changed it. |
  | `config_version` | The version the running session was validated against. |

  An unusable keymap — a binding that conflicts with a built-in key — invalidates
  the whole candidate. Falling back to the built-in keymap would take away the
  bindings you still have, on the strength of a file you have just broken.
* **Configuration is data.** Nothing in it is ever executed, and there are no
  pre- or post-action hooks.
* **Environment variable interpolation is not supported** in v1. `${HOME}` is used
  literally, and monitrs warns when it sees `${` rather than letting you assume
  otherwise.

## Units

Durations: `250ms`, `1s`, `30s`, `5m`, `1h`. A bare number is rejected — `1` is
ambiguous between a second and a millisecond.

Sizes: `512kB`, `32MiB`, `1.5GiB`, or a bare number meaning bytes. Both families
are accepted regardless of `display.units`; the `i` is what distinguishes them
(`MiB` is 1024², `MB` is 1000²). A single decimal fraction is applied with integer
arithmetic, so `1.5GiB` is exactly 1610612736 bytes.

## Every key

### Top level

| Key | Default | Meaning |
|---|---|---|
| `config_version` | `1` | Schema version. A file from a newer monitrs is refused with an explanation rather than misread. |

### `[sampling]`

| Key | Default | Range | Meaning |
|---|---|---|---|
| `interval` | `"1s"` | 250ms–1m | Fast tier: CPU, memory, processes, network and disk counters. |
| `history` | `"5m"` | 30s–1h | How far back Time Lens can scrub. Must be at least one `interval`. |
| `medium_interval` | `"5s"` | ≥ `interval` | Filesystem capacity, static device state. |
| `slow_interval` | `"30s"` | ≥ `interval` | Users, device lists, static metadata. |
| `max_history_memory` | `"32MiB"` | ≥ 1MiB | Ceiling for the history ring. If `interval` and `history` would need more, history is shortened and monitrs tells you it was clamped. |

Shortening `interval` makes the machine work harder for finer resolution; 250 ms
is the floor because below it the OS's own CPU accounting is too coarse to be
meaningful.

Sensors and the battery have no key of their own. They are read as one group whose
cadence follows what is on screen: `slow_interval` normally, `medium_interval` while
the Battery screen is visible, and immediately when that screen is opened. So
`medium_interval` and `slow_interval` bound the sensor group as well, from either end
(§8.6, and [`metrics.md`](metrics.md#sensors-and-battery) for why the group is
scheduled this way).

### `[display]`

| Key | Default | Values |
|---|---|---|
| `glyphs` | `"auto"` | `auto` \| `unicode` \| `ascii` |
| `color` | `"auto"` | `auto` \| `truecolor` \| `256` \| `16` \| `off` |
| `theme` | `"default-dark"` | `default-dark` \| `default-light` \| `high-contrast` |
| `units` | `"iec"` | `iec` (KiB, MiB) \| `si` (kB, MB) |
| `process_cpu_normalization` | `"core"` | `core` \| `machine` |
| `mouse` | `false` | |
| `show_per_core` | `false` | |
| `show_kernel_threads` | `false` | Linux only |
| `command_column` | `"auto"` | `auto` \| `name` \| `full` |

`glyphs = "auto"` uses Unicode on a UTF-8 locale and falls back to strict 7-bit
ASCII otherwise. `color = "auto"` honours the `NO_COLOR` convention; passing
`--color` explicitly on the command line overrides `NO_COLOR`, since an explicit
flag is a clearer statement of intent than an environment variable.

`process_cpu_normalization` is the one to know about: `core` means one core is
100%, so a process using four cores reads `400%`. `machine` means the whole
machine is 100% and nothing exceeds it. See [`metrics.md`](metrics.md).

### `[processes]`

| Key | Default | Meaning |
|---|---|---|
| `sort` | `"cpu"` | `cpu`, `memory`, `read`, `write`, `pid`, `name`, `age`, `user`, `state`, `threads`, `virtual` |
| `descending` | `true` | |
| `tree` | `false` | Start in tree mode. |
| `filter` | `""` | Initial plain-text filter. |
| `top_contributors_per_metric` | `10` | 1–100. How many contributors each history sample keeps per metric, for spike attribution. Higher costs memory per sample and raises evidence coverage. |

### `[diagnostics]`

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | |
| `cpu_watch_percent` | `80` | Must be below `cpu_critical_percent`. |
| `cpu_critical_percent` | `95` | |
| `memory_watch_available_percent` | `15` | |
| `memory_critical_available_percent` | `5` | Must be **below** `memory_watch_available_percent`. |
| `sustained_samples` | `10` | 1–600. How many recent samples must agree before a signal escalates. |
| `bell_on_critical` | `false` | Also ring the terminal bell when a signal reaches `critical`. |

The memory thresholds run the opposite way to the CPU ones, because *less*
available memory is worse. Getting that backwards is the easiest mistake in the
file, so it is validated: `memory_critical_available_percent` must be the smaller
number.

`sustained_samples` is the hysteresis that stops the radar flapping between amber
and green once per second. Lowering it makes monitrs twitchier, not more accurate.

### Being told, instead of watching

A signal that crosses a threshold is recorded as a notice — `X pressure CPU is now
critical (held 1s): cpu busy at or above diagnostics.cpu_watch_percent …` — quoting
the same rule text and duration the radar panel shows, so the notice and the panel
can never disagree. The line appears in the status area and in the notice panel; it
opens no dialog and takes no key, so it cannot interrupt what you were doing.

What it does *not* do is repeat. The reducer sees a snapshot a second and the radar
reports `critical` in every one of them; only the **transition** is announced, which
is what `sustained_samples` makes meaningful in the first place. Recovery is
announced too, once, at a lower severity. A signal that becomes unavailable — the
read was refused, or the machine woke from sleep and the hysteresis window was
discarded — says nothing at all: that is not a recovery, and reporting it as one
would claim the machine was fine on the strength of a metric nobody could read.

Alerts are derived from the newest sample even while the timeline is paused or
scrubbed. §2.1 freezes what is *displayed*, not collection, and pausing to read one
spike is no reason to be kept in the dark about the next.

`bell_on_critical` adds a single `\x07` to that, and only for an escalation into
`critical`:

* never for `watch`, and never on recovery;
* once per episode, not once per second — a signal that is still critical has not
  transitioned;
* once per sample even if two signals cross together, because two beeps say nothing
  the first did not;
* off by default. A monitor left running on a second screen should not make noise
  unless you asked it to, and the notice is the primary cue either way.

Setting it with `enabled = false` is rejected rather than ignored: with diagnostics
off no signal is derived at all, so the bell could never ring.

### `[keys]`

Rebinds the built-in keys for a documented subset of actions. Each entry
*replaces* the default binding.

```toml
[keys]
quit = ["q", "ctrl-c"]
help = ["?"]
filter = ["/"]
pause = ["space"]
live = ["L"]
```

Key names: a single character, or `enter`, `esc`, `tab`, `backtab`, `backspace`,
`delete`, `insert`, `space`, `left`, `right`, `up`, `down`, `home`, `end`,
`pageup`, `pagedown`, `f1`–`f24`. Prefix with `ctrl-`, `alt-`, or `shift-`.

Two things worth knowing:

* **A bare character is case-sensitive.** `g` and `G` are different keys and are
  bound to different actions, so monitrs will not quietly lower-case them.
  `shift-a` and `A` are the same key press, because that is what the terminal
  reports.
* **Binding one key to two actions is rejected**, naming the key and both
  actions. It is not resolved by precedence, because which one wins would be
  invisible.

## A complete example

This is what `monitrs config init` writes, minus the comments. Every value is the
built-in default, so a file identical to this one changes nothing.

```toml
config_version = 1

[sampling]
interval = "1s"
history = "5m"
medium_interval = "5s"
slow_interval = "30s"
max_history_memory = "32MiB"

[display]
glyphs = "auto"
color = "auto"
theme = "default-dark"
units = "iec"
process_cpu_normalization = "core"
mouse = false
show_per_core = false
show_kernel_threads = false
command_column = "auto"

[processes]
sort = "cpu"
descending = true
tree = false
filter = ""
top_contributors_per_metric = 10

[diagnostics]
enabled = true
cpu_watch_percent = 80
cpu_critical_percent = 95
memory_watch_available_percent = 15
memory_critical_available_percent = 5
sustained_samples = 10
bell_on_critical = false
```

## Command-line equivalents

| Flag | Key |
|---|---|
| `--interval <DURATION>` | `sampling.interval` |
| `--history <DURATION>` | `sampling.history` |
| `--glyphs <MODE>`, `--ascii` | `display.glyphs` |
| `--color <MODE>`, `--no-color` | `display.color` |
| `--theme <NAME>` | `display.theme` |
| `--units <FAMILY>` | `display.units` |
| `--mouse` | `display.mouse` |
| `--per-core` | `display.show_per_core` |
| `--sort <FIELD>` | `processes.sort` |
| `--tree` | `processes.tree` |
| `--filter <TEXT>` | `processes.filter` |

`--config <PATH>`, `--no-config`, and `--debug-log <PATH>` have no file
equivalents by design: where to read configuration from cannot itself be
configured, and a log destination is a per-invocation decision.
