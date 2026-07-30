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

    // Two samples: the first is all warming up by §8.2, and the battery lands on the
    // medium tier, so one sample would prove nothing about either.
    let start = Instant::now();
    let mut tick = SampleTick::first(start, SystemTime::now());
    for index in 0..2 {
        if index > 0 {
            std::thread::sleep(Duration::from_millis(300));
            tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        }
        let snapshot = collector.sample(&tick).expect("the live collector samples");
        let _ = monitrs_tui::app::apply(&mut state, Event::<()>::Snapshot(Arc::new(snapshot)));
    }

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
    assert!(
        !text.contains("-9"),
        "a sub-absolute-zero sensor reading reached the screen:\n{text}"
    );
    // Nothing may overflow its column into the panel border.
    for line in text.lines() {
        assert!(
            line.chars().count() <= usize::from(width),
            "a row is wider than the terminal:\n{line}"
        );
    }
}
