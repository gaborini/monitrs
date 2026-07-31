//! A live render of the Battery screen against the real machine (§17.6).
//!
//! `#[ignore]`d, like every other platform smoke test: it reads the live power source
//! and the live thermal sensors, so it cannot be part of a hermetic `cargo test`. It
//! exists because the fake collector cannot produce what a real machine does — this is
//! the test that found `PMU tdev*` sensors reporting −9200 °C, `Component::max` being
//! a high-water mark rather than a threshold, and `gas gauge battery` overflowing a
//! sixteen-cell label column. None of those were visible in any snapshot.
//!
//! Run it with the frame printed:
//!
//! ```text
//! cargo test -p monitrs-tui --all-features --test live_battery_frame -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::unwrap_used)]

use core::time::Duration;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use monitrs_collectors::source::{SampleTick, SnapshotSource};
use monitrs_collectors::tier::DueTiers;
use monitrs_tui::action::ViewId;
use monitrs_tui::app::{AppSettings, AppState};
use monitrs_tui::event::Event;
use monitrs_tui::glyphs::GlyphSet;
use monitrs_tui::theme::{ColorDepth, ThemeId};
use monitrs_tui::views;
use monitrs_tui::widgets::Presentation;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

/// The platform collector for this host, or nothing on an unsupported one.
#[cfg(target_os = "macos")]
fn platform() -> Option<impl SnapshotSource> {
    monitrs_collectors::macos::MacosCollector::new().ok()
}

#[cfg(target_os = "linux")]
fn platform() -> Option<impl SnapshotSource> {
    monitrs_collectors::linux::collector::LinuxCollector::new().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform() -> Option<impl SnapshotSource> {
    None::<monitrs_collectors::fake::FakeCollector>
}

#[test]
#[ignore = "platform smoke test: reads the live power source and thermal sensors"]
fn the_live_battery_screen_reads_as_a_screen_rather_than_as_a_wall_of_placeholders() {
    let Some(mut collector) = platform() else {
        return;
    };
    let mut state = AppState::new(AppSettings {
        started_at: Instant::now(),
        size: (140, 44),
        view: ViewId::Battery,
        ..AppSettings::default()
    });

    // Two samples: the first is all warming up by §8.2, and the battery is read with
    // the sensor group rather than every tick, so one sample would prove nothing about
    // either. `DueTiers::ALL` includes the sensor group, which is what makes the second
    // sample carry a reading at all.
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::now());
    let mut snapshot = None;
    for index in 0..2 {
        if index > 0 {
            std::thread::sleep(Duration::from_millis(300));
            tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        }
        let sample = Arc::new(collector.sample(&tick).expect("the live collector samples"));
        // Kept, so the assertions can ask the collector's own output whether it
        // discarded the unwired sensors — not only whether the screen hid them.
        snapshot = Some(Arc::clone(&sample));
        let _ = monitrs_tui::app::apply(&mut state, Event::<()>::Snapshot(sample));
    }
    let snapshot = snapshot.expect("two samples were taken");

    let presentation = Presentation::new(
        GlyphSet::ascii(),
        ThemeId::DefaultDark.theme(),
        ColorDepth::TrueColor,
    );
    let (width, height) = state.size();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test backend");
    terminal
        .draw(|frame| views::render(frame, Rect::new(0, 0, width, height), &state, presentation))
        .expect("drawing to a test backend never fails");

    let mut text = String::new();
    for y in 0..height {
        for x in 0..width {
            if let Some(cell) = terminal.backend().buffer().cell((x, y)) {
                text.push_str(cell.symbol());
            }
        }
        text.push('\n');
    }
    println!("{text}");

    // What the fake collector cannot check.
    assert!(text.contains("BATTERY"), "{text}");
    assert!(text.contains("THERMAL SENSORS"), "{text}");
    // §4: an unwired sensor reporting −9200 °C must be discarded by the collector
    // rather than rendered as a temperature with a decimal point on it.
    //
    // Asserted twice, at the two places it can go wrong, and asserted about
    // *temperatures* rather than about the frame's characters. The first version of this
    // searched the whole frame for `-9`, which is not a temperature: it is any two
    // characters, and it failed on a CI runner whose host name is a UUID containing
    // `-947c`. A wattage on the battery panel would have broken it just as easily.
    //
    // Note the bound is absolute zero, not zero. A sensor reading −5 °C is a machine in
    // a cold room, which is information; −9200 °C is a sensor that is not wired up.
    if let Some(readings) = snapshot.sensors.temperatures.fresh() {
        for reading in readings {
            assert!(
                reading.celsius > ABSOLUTE_ZERO_CELSIUS,
                "the collector published {} at {} \u{b0}C, below absolute zero",
                reading.label,
                reading.celsius
            );
        }
    }
    let rendered = rendered_temperatures(&text);
    // A parser that matches nothing turns the loop below into a no-op, which is a green
    // test that checks nothing — the failure mode worth guarding against explicitly.
    if snapshot
        .sensors
        .temperatures
        .fresh()
        .is_some_and(|readings| !readings.is_empty())
    {
        assert!(
            !rendered.is_empty(),
            "this machine reports sensors but none was found in the panel:\n{text}"
        );
    }
    for value in rendered {
        assert!(
            value > ABSOLUTE_ZERO_CELSIUS,
            "{value} \u{b0}C reached the screen, below absolute zero:\n{text}"
        );
    }
    // Nothing may overflow its column into the panel border.
    for line in text.lines() {
        assert!(
            line.chars().count() <= usize::from(width),
            "a row is wider than the terminal:\n{line}"
        );
    }
}

/// Absolute zero, the only bound a temperature reading cannot physically cross.
const ABSOLUTE_ZERO_CELSIUS: f32 = -273.15;

/// Every temperature figure in the frame's thermal panel, in degrees Celsius.
///
/// Confined to that panel on purpose: the header carries the host name, and a battery
/// panel can carry a signed wattage, so a search over the whole frame would be answering
/// a different question. Values are read as "the number immediately before a `C`", which
/// is how this screen renders one.
fn rendered_temperatures(frame: &str) -> Vec<f32> {
    frame
        .lines()
        .skip_while(|line| !line.contains("THERMAL SENSORS"))
        .skip(1)
        .take_while(|line| !line.trim_start_matches('|').trim_start().starts_with('+'))
        .flat_map(temperatures_in_row)
        .collect()
}

/// The temperature figures in one rendered row.
fn temperatures_in_row(row: &str) -> Vec<f32> {
    let cells: Vec<char> = row.chars().collect();
    let mut values = Vec::new();
    for (index, character) in cells.iter().enumerate() {
        if *character != 'C' {
            continue;
        }
        let mut start = index;
        while start > 0 && matches!(cells[start - 1], '0'..='9' | '.' | '-') {
            start -= 1;
        }
        if start == index {
            continue;
        }
        if let Ok(value) = cells[start..index]
            .iter()
            .collect::<String>()
            .parse::<f32>()
        {
            values.push(value);
        }
    }
    values
}

/// The row parser, checked against the shapes it has to get right.
///
/// Not `#[ignore]`d: it needs no machine, and the live assertion above is only worth
/// anything if this reads a real row the way the panel writes one — and rejects the two
/// things that broke the first version of that assertion.
#[test]
fn the_row_parser_reads_temperatures_and_nothing_else() {
    assert_eq!(
        temperatures_in_row("| PMU tdie1               60.8C  [....]  peak 62.0C   |"),
        vec![60.8, 62.0]
    );
    // The regression this whole assertion exists for: an unwired sensor.
    assert_eq!(
        temperatures_in_row("| PMU tdev1            -9200.0C  [....]              |"),
        vec![-9200.0]
    );
    // A host name is not a temperature, which is what the first version got wrong.
    assert!(
        temperatures_in_row("| host:iad20-fj917-7276b13b-5ae8-417f-947c-8cf7.local |").is_empty()
    );
    // Nor is a wattage, whatever its sign.
    assert!(temperatures_in_row("| power        -9.5 W     |").is_empty());
    // And a genuinely cold room is a reading, not a fault: the bound is absolute zero.
    assert_eq!(temperatures_in_row("| ambient  -5.0C |"), vec![-5.0]);
    // A compile-time statement about the bound itself: it is absolute zero, so that
    // tightening it to plain zero one day fails here rather than in a cold room.
    const { assert!(-5.0_f32 > ABSOLUTE_ZERO_CELSIUS) }
}
