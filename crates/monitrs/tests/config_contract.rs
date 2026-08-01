//! The configuration keys are a contract (§12, and `CONTRIBUTING.md`'s "What
//! 1.0.0 froze").
//!
//! `1.0.0` freezes four surfaces, and until this file existed only three had a
//! machine behind them: the library API has `cargo-semver-checks`, the JSON export
//! has `docs/schema/v2.json` plus `crates/monitrs/tests/schema_contract.rs`, and
//! the default keymap has `every_global_binding_from_the_spec_resolves_in_normal_mode`
//! and its list-binding sibling in `crates/monitrs-tui/src/keymap.rs`, which assert
//! exact key→action pairs. The configuration keys had only prose.
//!
//! `#[serde(deny_unknown_fields)]` is not that guard. It rejects a key the *user*
//! invented; it cannot notice monitrs itself deleting or renaming one, which is the
//! break that matters — a user's file stops meaning what it meant, and the only
//! symptom is an error message about a key they did not invent. `known_keys()` is
//! not that guard either: it is a hand-maintained list used for typo suggestions,
//! and nothing tied it to the actual serde structs until
//! [`the_typo_suggestion_list_matches_the_structs`] below.
//!
//! This is deliberately the same shape as `schema_contract.rs`: a committed
//! inventory of every key path, and a test that fails when a recorded one is
//! **missing**. Adding a key is not a break — nobody's file mentions a key that did
//! not exist yesterday — so an addition is printed rather than asserted. A reader
//! who knows one of these two files now knows the other.
//!
//! # Why the module source is included rather than imported
//!
//! `monitrs` is a binary-only crate — `crates/monitrs/Cargo.toml` declares a
//! `[[bin]]` and no `[lib]` — so there is no library target to link against, and
//! `config.rs`'s `Config` is `pub(crate)`: 1.0.0 freezes the public API, and a
//! contract test is not a reason to widen it. `#[path]` puts the real module into
//! this test binary, exactly as `crates/monitrs/tests/schema_contract.rs:85` and
//! `crates/monitrs/tests/integration.rs:90-101` already do. `cli` comes along
//! because `config` needs it for `--flag` precedence and for the interval bounds
//! `Config::validate` checks against. One side effect, shared with those files:
//! `cargo test` sets `cfg(test)` for an integration target too, so `config.rs`'s
//! own `mod tests` is compiled into this binary and runs again here under
//! `config::tests::*`.
//!
//! # How the key set is derived, and the three things that method cannot see
//!
//! Serialising [`Config`] to TOML and walking the result is the closest analogue to
//! what `schema_contract.rs` does with an export, and it has the same virtue: the
//! keys come out of the serde structs themselves, through the very code path that
//! writes a configuration file, rather than out of a second list somebody has to
//! remember to update. It carries three blind spots, and none of them is silent
//! here:
//!
//! * **A key whose value is `None` does not serialise at all.** TOML has no
//!   representation for it, so the whole `[keys]` table would be absent from a
//!   serialised `Config::default()` and its five paths would never reach the
//!   inventory. [`every_key_populated`] is the answer, and it constructs
//!   [`KeysConfig`] field by field rather than with `..Default::default()` on
//!   purpose: adding a sixth rebindable action stops **this file compiling**, which
//!   is the only way a derivation-by-serialisation can be made to notice a new
//!   `Option` key. Do not "fix" that compile error with `..KeysConfig::default()`.
//! * **A field that never serialises is invisible.** `#[serde(skip_serializing)]`
//!   would hide a key from this guard completely, and a deserialise-only
//!   `#[serde(alias)]` is invisible too. The alias case is benign — an alias is how
//!   a key gets renamed *without* breaking anyone — but a `skip_serializing`
//!   configuration field would be a real hole, and there is none today.
//! * **Key paths are not meanings.** A key that keeps its spelling while what it
//!   governs changes is a break no path inventory can see; `sampling.slow_interval`
//!   taking over the sensor read from `sampling.medium_interval` on this very branch
//!   is the example. Nothing mechanical will catch the next one:
//!   `docs/configuration.md` is the record, and keeping it true is part of changing
//!   a key's behaviour.
//!
//! Accepted *values* are a frozen surface too, and also outside this file: the
//! spellings live on the value enums (`display.color = "256"` is a
//! `#[serde(rename)]`), and `config.rs`'s own
//! `the_numeric_colour_modes_use_their_natural_spelling_in_toml` is what pins the
//! two that could not be spelled naturally.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "§18.2 narrow allowance: in a test these assert a precondition, and a \
              failure must name the line that broke"
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

#[path = "../src/cli.rs"]
mod cli;
#[path = "../src/config.rs"]
mod config;

use config::{Config, KeysConfig, SUPPORTED_VERSION, known_keys};

/// Where the key-path inventory for configuration version `version` lives.
///
/// Named `config-v<N>.json` beside the export's `v<N>.json` in the same directory:
/// two inventories of two different contracts, each versioned by the number its own
/// contract carries — `config_version` here, `schema_version` there.
fn inventory_path(version: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/schema")
        .join(format!("config-v{version}.json"))
}

/// A configuration in which every key is present, including the optional ones.
///
/// Starts from the real defaults — the value `config init` writes and an empty file
/// yields — and fills in `[keys]`, whose five fields are `Option` and therefore
/// absent from a serialised default. The bindings are the ones
/// `docs/configuration.md` shows, so this stays a configuration a user could
/// actually write rather than a shape invented for a test; the assertion in
/// [`every_recorded_key_is_still_a_real_key`] that it validates cleanly is what
/// keeps it that way.
///
/// `KeysConfig` is built field by field deliberately. See the module doc comment:
/// with `..Default::default()` a newly added rebindable action would default to
/// `None`, vanish from the serialised TOML, and never be recorded — so the guard
/// would quietly stop covering it. Spelled out, it is a compile error instead.
fn every_key_populated() -> Config {
    Config {
        keys: KeysConfig {
            quit: Some(vec!["q".to_owned(), "ctrl-c".to_owned()]),
            help: Some(vec!["?".to_owned()]),
            filter: Some(vec!["/".to_owned()]),
            pause: Some(vec!["space".to_owned()]),
            live: Some(vec!["L".to_owned()]),
        },
        ..Config::default()
    }
}

/// Every dotted key path in a TOML document, as `sampling.interval`.
///
/// A table is walked; anything else is a leaf. An array is deliberately a leaf and
/// not a level: `keys.quit = ["q", "ctrl-c"]` is *one* setting whose value happens
/// to be a list, and a user writes `keys.quit`, never `keys.quit[]`. (This is the
/// one place the shape departs from `schema_contract.rs`'s `field_paths`, which
/// collapses arrays to `[]` because there the element's own fields are the
/// contract.)
fn key_paths(value: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
    if let toml::Value::Table(table) = value {
        for (key, child) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            key_paths(child, &path, out);
        }
    } else {
        out.insert(prefix.to_owned());
    }
}

/// Every key path a configuration file may contain, taken from the serde structs.
fn current_key_paths() -> BTreeSet<String> {
    let text = toml::to_string(&every_key_populated()).expect("the configuration serializes");
    let document: toml::Value = toml::from_str(&text).expect("what we just wrote is valid TOML");
    let mut paths = BTreeSet::new();
    key_paths(&document, "", &mut paths);
    paths
}

#[test]
fn every_recorded_key_is_still_a_real_key() {
    let version = SUPPORTED_VERSION;
    let recorded = std::fs::read_to_string(inventory_path(version))
        .unwrap_or_else(|error| panic!("{}: {error}", inventory_path(version).display()));
    let recorded: BTreeSet<String> =
        serde_json::from_str(&recorded).expect("the inventory is a JSON array of strings");

    let fixture = every_key_populated();
    assert!(
        fixture.validate().is_empty(),
        "the fixture must be a configuration a user could really write: {:?}",
        fixture.validate()
    );
    let current = current_key_paths();

    // Printed, not asserted: a file written yesterday cannot mention a key added
    // today, so an addition breaks nobody. The list is here because it is what a
    // reviewer needs in order to extend the inventory deliberately rather than by
    // accident.
    let added: Vec<&String> = current.difference(&recorded).collect();
    if !added.is_empty() {
        println!("new configuration keys since the inventory was written: {added:?}");
    }

    let missing: Vec<&String> = recorded.difference(&current).collect();
    assert!(
        missing.is_empty(),
        "these keys are in docs/schema/config-v{version}.json but a configuration file can \
         no longer contain them: {missing:?}\n\
         Restore them — a key that disappears makes every file that sets it fail to load, \
         naming a key the user did not invent. If the removal or rename really is \
         intended, it is a deliberate, documented break: bump SUPPORTED_VERSION to {}, \
         write docs/schema/config-v{}.json beside the old one (the old file stays, so a \
         user can see exactly what changed), update docs/configuration.md, and record it \
         in CHANGELOG.md as a breaking change. Adding a key is not a break; removing or \
         renaming one is.",
        version + 1,
        version + 1
    );
}

#[test]
fn the_typo_suggestion_list_matches_the_structs() {
    // `known_keys()` exists so that `intervall` suggests `interval`. It is
    // hand-maintained, it is the one list in `config.rs` with no compiler behind it,
    // and a suggestion list that has drifted from the structs is worse than none: it
    // proposes a key the parser will reject next. Compared as *names* rather than
    // paths because that is what the suggester matches against — the parser reports
    // an unknown field by its own name, without the section it appeared in.
    let names: BTreeSet<String> = current_key_paths()
        .iter()
        .flat_map(|path| path.split('.').map(ToOwned::to_owned).collect::<Vec<_>>())
        .collect();
    let known: BTreeSet<String> = known_keys().iter().map(|key| (*key).to_owned()).collect();

    let unlisted: Vec<&String> = names.difference(&known).collect();
    assert!(
        unlisted.is_empty(),
        "config.rs's known_keys() does not mention {unlisted:?}, so a user who misspells one \
         of those gets no suggestion — add them to the list"
    );
    let stale: Vec<&String> = known.difference(&names).collect();
    assert!(
        stale.is_empty(),
        "config.rs's known_keys() still lists {stale:?}, which no longer exists in the \
         configuration structs — a suggestion pointing at it would send the user to a key \
         the parser rejects"
    );
    // The two assertions above cover both directions; this pins the equality itself,
    // so a future edit that removes one of them still fails rather than half-checks.
    assert_eq!(names, known);
}
