//! The Battery screen: the pack, and the thermal sensors that share its concerns.
//!
//! ```text
//! + BATTERY --------------------------------------------- 82% discharging -+
//! | CHARGE     82% [#########################-------------]  -discharging  |
//! | TO EMPTY 4h 00m        CYCLES   214       TEMP  31.4C   POWER  12.4 W  |
//! | CAPACITY 48.2 Wh full of 52.6 Wh design  HEALTH  92%                   |
//! + THERMAL SENSORS ------------------------------------------ 2 sensors --+
//! | performance         62.5C  [###########=--------]  peak 95.0C  crit 105.0C
//! | efficiency          44.0C  [########=-----------]  peak 95.0C  crit 105.0C
//! ```
//!
//! And on a desktop, which is the reading this screen was designed around:
//!
//! ```text
//! + BATTERY ------------------------------------ no battery on this machine +
//! | CHARGE   - n/a [.....................]  no battery on this machine      |
//! |                                                                        |
//! | A desktop, a server, a virtual machine and a container all read this    |
//! | way. It is a fact about the hardware, not a failed read and not a       |
//! | charge of zero.                                                        |
//! ```
//!
//! # A machine with no battery is the case this screen is built around
//!
//! Every server, every container, every CI runner and every desktop reaches this
//! screen with [`SensorSnapshot::battery`] set to [`MetricState::Unsupported`], and
//! that is the reading the screen is designed to render well. It gets the charge
//! meter's own `n/a` track and symbol — never an empty bar, which would read as a
//! measured zero — plus a sentence naming *why* there is nothing here, and then the
//! screen stops rather than printing five more `n/a` fields to fill the panel. The
//! secondary rows appear only when there is a pack to describe (§4, §26).
//!
//! # What is not derived
//!
//! **No time remaining is computed.** [`BatterySnapshot::time_remaining`] is
//! rendered exactly as the platform reported it, which on macOS is
//! `IOPSGetTimeRemainingEstimate` and on Linux is `time_to_empty_now` where the
//! driver publishes one. Charge divided by instantaneous current would give a figure
//! that swings by hours between consecutive samples, and §4 exists to stop precisely
//! that: on a pack with no published estimate this screen says so.
//!
//! **No health figure is stored.** The wear percentage comes out of
//! [`BatterySnapshot::health`], which derives it from the capacity pair beside it, so
//! the number on screen cannot disagree with the two it sits next to.
//!
//! # No temperature bar without a threshold
//!
//! A bar needs a full scale, and a temperature has none: 62 °C is most of the way to a
//! laptop's limit and barely warm for a GPU. The bar is therefore drawn only where the
//! *sensor itself* declares a critical threshold, which is a real denominator, and a
//! sensor that declares none gets the unknown track and a note saying why. This is the
//! same rule §7.4 applies to network utilization without a known link speed, and the
//! refusal lives in [`TemperatureReading::share_of_critical`] rather than here so no
//! other screen can quietly disagree with it.
//!
//! [`TemperatureReading::peak_celsius`] is deliberately *not* used as a substitute
//! scale. It is the highest value seen rather than a declared limit, so a bar drawn
//! against it would sit at 100% for the whole run — which is why it is shown as
//! `peak`, in words, beside the figure instead.
//!
//! Nothing here concludes thermal throttling from a temperature: §11.3 forbids it,
//! and the `!` marker on a hot row is the sensor's own claim about its own threshold.
//!
//! # Where a retained reading's age goes
//!
//! Both metrics on this screen are read on the sensor cadence rather than every tick,
//! so both arrive as [`MetricState::Stale`] most of the time — and §4 allows a retained
//! value on screen only alongside its age. The awkward part is that the `Stale`
//! envelope wraps the *whole* [`BatterySnapshot`] and the *whole* reading list, while
//! what is on screen is a dozen fields unwrapped out of them. Every one of those fields
//! is the same age.
//!
//! So the age is stated **once per panel, in the panel's own trailing label** —
//! `82% discharging ~00:28`, `2 sensors ~00:28` — and that label is the only place any
//! of it appears as *text*. Two reasons it goes there rather than on each field:
//!
//! * `~00:28` printed six times down a three-row panel is one fact rendered as six,
//!   and the trailing label is already where each panel describes itself.
//! * `VALUE_WIDTH` is eleven cells, which holds `31.4C` and not `31.4C ~00:28`.
//!   Widening it to thirteen would cost eight cells of a row that already truncates at
//!   80 columns, permanently, to make room for a suffix that is usually absent.
//!
//! The fields are still routed through [`states::describe`] with the envelope's
//! staleness pushed into them (`retained_field`), because a 28-second-old instantaneous
//! wattage must not be *drawn* as a fresh measurement either. **What that buys them is a
//! style and not a character:** `push_field` renders
//! [`MetricDisplay::fitted`], which emits the text alone — [`MetricDisplay::flagged`] is
//! the form that prefixes the symbol, and no field on this screen uses it. So each
//! retained field is [`Token::Stale`], which is `DIM | ITALIC` and therefore survives
//! [`ColorDepth::Off`] (`theme::Token::emphasis`), and the `~` itself appears only in the
//! panel label above. Colour is not the only carrier (§5.2, §5.3), but the redundant cue
//! here is an attribute rather than a glyph, and the text that names the age is one row
//! away rather than in the cell.
//!
//! A field that is unavailable in its own right keeps its own reason and its own symbol,
//! since [`MetricState::into_stale`] only touches `Available`.
//!
//! One consequence worth knowing before editing: because the cells differ only by style,
//! a snapshot taken through `text_of` cannot see the difference, which is why
//! `a_retained_pack_is_styled_stale_at_the_call_site_and_not_only_in_the_helper`
//! inspects spans instead.
//!
//! The one place the age is repeated is the charge meter, which annotates its own value
//! ([`Meter`] does this for every metric it draws) — that is the widget's rule, not this
//! screen's, and it is right there beside the figure it dates.
//!
//! [`ColorDepth::Off`]: crate::theme::ColorDepth::Off
//! [`SensorSnapshot::battery`]: monitrs_core::model::SensorSnapshot::battery
//! [`TemperatureReading::share_of_critical`]: monitrs_core::model::TemperatureReading::share_of_critical
//! [`TemperatureReading::peak_celsius`]: monitrs_core::model::TemperatureReading::peak_celsius
//! [`MetricState`]: monitrs_core::model::MetricState

use core::time::Duration;

use monitrs_core::model::{
    BatteryCapacity, BatterySnapshot, ChargeState, MetricState, SensorSnapshot, TemperatureReading,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Borders;

use crate::app::AppState;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Meter, Presentation};

use super::{
    Chrome, SHARED_BOTTOM, draw_bordered_panel, fit_label, inner_of, inset, muted_line,
    row_builder, split_rows, truncation_label, write_lines,
};

/// Cells reserved for a field label, wide enough for `TO EMPTY` and `CAPACITY`.
const LABEL_WIDTH: u16 = 9;

/// Cells reserved for a field's value, wide enough for `permission denied`
/// abbreviated and for `1234.5 Wh`.
const VALUE_WIDTH: u16 = 11;

/// Cells between two labelled fields on the same row.
const FIELD_GAP: u16 = 4;

/// Cells reserved for `48.2 Wh full of 52.6 Wh design`.
///
/// Sized for a 999.9 Wh pack, which is well past anything portable, so the sentence
/// never truncates and `HEALTH` beside it never moves.
const CAPACITY_TEXT_WIDTH: u16 = 34;

/// Cells reserved for a sensor's label before it is tail-truncated.
///
/// Twenty because that is what real labels need: `gas gauge battery` and
/// `NAND CH0 temp` come off an Apple Silicon Mac, and `coretemp Package id 0` off an
/// Intel Linux box. Sixteen truncated the first of those.
const SENSOR_WIDTH: u16 = 20;

/// Cells reserved for a temperature figure, `-11.5C` through `105.0C` plus a flag.
const DEGREES_WIDTH: u16 = 8;

/// Cells reserved for a thermal row's bar, threshold note excluded.
const THERMAL_BAR_WIDTH: u16 = 22;

/// Content rows the battery panel writes when there is a pack: meter, vitals, wear.
const PRESENT_ROWS: u16 = 3;

/// The shortest panel worth drawing: two borders and one row of content.
const MIN_PANEL_ROWS: u16 = 3;

/// Draws the Battery screen.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &AppState, presentation: Presentation<'_>) {
    let Some(body) = Chrome::resolve(area).body else {
        return;
    };
    let buffer = frame.buffer_mut();
    let Some(snapshot) = state.snapshot() else {
        write_lines(
            buffer,
            body,
            &[muted_line(presentation, body.width, "warming up")],
        );
        return;
    };
    let sensors = &snapshot.sensors;

    // The battery panel takes exactly the rows its content needs and the sensors take
    // the rest. Sizing it by share of the screen instead would leave a desktop — whose
    // whole reading is one sentence — staring at an eighteen-row panel holding it,
    // which is the same emptiness that made the Storage screen worth complaining
    // about.
    //
    // §5.5 shares the row between vertically adjacent panels, so the battery panel
    // draws no bottom edge and the sensors' top rule doubles as it. Where the sensor
    // panel will not fit at all the battery panel keeps its own bottom edge, because
    // the status footer is not a panel and cannot close it.
    let lines = battery_line_count(sensors);
    let shared = body.height >= lines.saturating_add(1).saturating_add(MIN_PANEL_ROWS);
    let battery_rows = if shared {
        lines.saturating_add(1)
    } else {
        lines.saturating_add(2).min(body.height)
    };
    let rows = split_rows(
        body,
        &[battery_rows, body.height.saturating_sub(battery_rows)],
    );

    if let Some(area) = rows.first() {
        let borders = if shared { SHARED_BOTTOM } else { Borders::ALL };
        draw_battery(buffer, *area, sensors, presentation, borders);
    }
    if let Some(area) = rows.get(1).filter(|area| area.height >= MIN_PANEL_ROWS) {
        draw_thermal(buffer, *area, sensors, presentation);
    }
}

/// How many content rows the battery panel will write.
///
/// A machine with no pack is complete in three: the meter carrying the placeholder,
/// a blank, and the sentence saying why. A pack adds the two secondary rows. Padding
/// the absent case out to the same height would be padding for its own sake.
fn battery_line_count(sensors: &SensorSnapshot) -> u16 {
    if sensors.battery.displayable().is_some() {
        PRESENT_ROWS
    } else {
        let sentences = u16::try_from(explanation(&sensors.battery).len()).unwrap_or(1);
        // The meter and the blank line under it, plus however many sentences.
        2u16.saturating_add(sentences)
    }
}

/// Draws the battery panel, present or absent.
fn draw_battery(
    buffer: &mut Buffer,
    area: Rect,
    sensors: &SensorSnapshot,
    presentation: Presentation<'_>,
    borders: Borders,
) {
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "BATTERY",
        Some(&headline(&sensors.battery)),
        false,
        borders,
    ));
    if inner.is_empty() {
        return;
    }

    let mut lines = vec![charge_line(&sensors.battery, presentation, inner.width)];
    match sensors.battery.displayable() {
        Some((battery, _)) => {
            // The age of the envelope these fields came out of, so each of them can be
            // drawn as the retained reading it is. The figure itself was stated once, in
            // the trailing label above.
            let retained = retained_age(&sensors.battery);
            lines.push(vitals_line(battery, retained, presentation, inner.width));
            lines.push(capacity_line(battery, retained, presentation, inner.width));
        }
        None => {
            // §4: name the absence in words. The meter above already carries the
            // state's placeholder and symbol; this is the sentence that stops a
            // reader wondering whether monitrs simply failed to look.
            lines.push(muted_line(presentation, inner.width, ""));
            for sentence in explanation(&sensors.battery) {
                lines.push(muted_line(presentation, inner.width, sentence));
            }
        }
    }
    write_lines(buffer, inner, &lines);
}

/// The panel's trailing label: `82% discharging ~00:28`, or the reason there is nothing.
///
/// The age is this panel's single statement of how old everything in it is (see the
/// module documentation), so it goes through [`states::describe`] and
/// [`MetricDisplay::annotated`] rather than being assembled by hand — the charge in
/// `82%` is a measurement like any other, and an undated one here would be the panel
/// vouching for the freshness of every figure below it.
fn headline(battery: &MetricState<BatterySnapshot>) -> String {
    if battery.displayable().is_none() {
        // Not a metric placeholder: it is the panel describing itself, which is what
        // `muted_line`'s own documentation reserves plain sentences for.
        return "no battery on this machine".to_owned();
    }
    states::describe(battery, |battery: &BatterySnapshot| {
        format!("{} {}", battery.charge, battery.state.label())
    })
    .annotated()
}

/// How old the retained value in `state` is, or `None` when it was freshly measured.
///
/// The panels on this screen unwrap a `Stale` envelope and render its inner fields, so
/// they need the envelope's age separately from its value.
fn retained_age<T>(state: &MetricState<T>) -> Option<Duration> {
    state
        .displayable()
        .and_then(|(_, age)| state.is_stale().then_some(age))
}

/// One field of a retained snapshot, marked with the age of the envelope it arrived in.
///
/// The `Stale` envelope sits around the whole snapshot; each field inside it is
/// `Available`, because each one *was* available when the read happened. Describing them
/// as they stand paints a 28-second-old instantaneous wattage in the style of a fresh
/// measurement, which is the distinction §4 exists to protect. Pushing the envelope's
/// staleness down gives every field the stale token and the `~` cue; the age is printed
/// once, in the panel's trailing label.
///
/// [`MetricState::into_stale`] leaves anything that is not `Available` alone, so a field
/// the platform refused keeps `permission denied` and its own symbol rather than being
/// recast as a retained reading.
fn retained_field<T>(state: MetricState<T>, retained: Option<Duration>) -> MetricState<T> {
    match retained {
        Some(age) => state.into_stale(age),
        None => state,
    }
}

/// `CHARGE  82%  [####----]  -discharging`
///
/// The charge is the one battery field with no [`MetricState`] of its own, so its
/// availability is the *whole snapshot's*: an absent pack produces the meter's `n/a`
/// track rather than an empty bar, which would read as a measured zero (§4).
fn charge_line(
    battery: &MetricState<BatterySnapshot>,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let charge = battery.as_ref().map(|battery| battery.charge);
    let note = match battery.displayable() {
        // §5.2: the symbol before the word, so the state survives with colour off.
        Some((battery, _)) => format!("{}{}", battery.state.symbol(), battery.state.label()),
        None => absence_reason(battery).to_owned(),
    };
    Meter::new(presentation, charge)
        .with_label("CHARGE")
        .with_label_width(LABEL_WIDTH)
        .with_note(&note)
        .styled_line(width)
}

/// `TO EMPTY  4h 00m    CYCLES  214    TEMP  31.4C    POWER  12.4 W`
///
/// `retained` is the age of the envelope the pack arrived in, which every field on this
/// row shares; see [`retained_field`].
fn vitals_line(
    battery: &BatterySnapshot,
    retained: Option<Duration>,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    // The label says which direction the estimate runs, because "4h 00m" beside a
    // charging pack and beside a discharging one mean opposite things.
    let remaining_label = match battery.state {
        ChargeState::Charging => "TO FULL",
        _ => "TO EMPTY",
    };
    let fields = [
        (
            remaining_label,
            states::describe(
                &retained_field(battery.time_remaining.as_ref(), retained),
                |value: &&Duration| remaining_text(**value),
            ),
        ),
        (
            "CYCLES",
            states::describe(
                &retained_field(battery.cycle_count.as_ref(), retained),
                |count: &&u32| count.to_string(),
            ),
        ),
        (
            "TEMP",
            states::describe(
                &retained_field(battery.temperature_celsius.as_ref(), retained),
                |celsius: &&f32| format!("{celsius:.1}C"),
            ),
        ),
        (
            "POWER",
            states::describe(
                &retained_field(battery.power_watts.as_ref(), retained),
                |watts: &&f32| format!("{watts:.1} W"),
            ),
        ),
    ];
    for (index, (label, display)) in fields.iter().enumerate() {
        if index > 0 {
            row.pad(FIELD_GAP);
        }
        push_field(&mut row, presentation, label, display);
    }
    row.finish()
}

/// `CAPACITY  48.2 Wh full of 52.6 Wh design    HEALTH  91%`
///
/// The two capacities are printed together and the health beside them, because the
/// wear figure is only interpretable next to the numbers it came out of.
///
/// `retained` is the age of the envelope the pack arrived in; see [`retained_field`].
fn capacity_line(
    battery: &BatterySnapshot,
    retained: Option<Duration>,
    presentation: Presentation<'_>,
    width: u16,
) -> Line<'static> {
    let mut row = row_builder(presentation, width);
    let capacity = states::describe(
        &retained_field(battery.capacity.as_ref(), retained),
        |capacity: &&BatteryCapacity| {
            format!(
                "{} full of {} design",
                watt_hours(capacity.full_microwatt_hours),
                watt_hours(capacity.design_microwatt_hours)
            )
        },
    );
    row.push_field(
        "CAPACITY",
        LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    // A wide field rather than `VALUE_WIDTH`: this one is a sentence, and truncating
    // it to eleven cells would leave `48.2 Wh fu`. Fixed rather than "everything
    // left", so `HEALTH` lands under the row above's third field instead of drifting
    // to the right edge on a wide terminal (§5.4).
    let field = CAPACITY_TEXT_WIDTH.min(width.saturating_sub(LABEL_WIDTH));
    row.push_field(
        &capacity.fitted(usize::from(field), presentation.glyphs()),
        field,
        Align::Left,
        presentation.metric_style(&capacity),
    );
    row.pad(FIELD_GAP);
    push_field(
        &mut row,
        presentation,
        "HEALTH",
        &states::describe_percent(&retained_field(battery.health(), retained)),
    );
    row.finish()
}

/// One `LABEL  value` pair in fixed columns, so a changing value moves nothing.
fn push_field(
    row: &mut crate::widgets::RowBuilder,
    presentation: Presentation<'_>,
    label: &str,
    display: &MetricDisplay,
) {
    row.push_field(
        label,
        LABEL_WIDTH,
        Align::Left,
        presentation.style(Token::Muted),
    );
    row.push_field(
        &display.fitted(usize::from(VALUE_WIDTH), presentation.glyphs()),
        VALUE_WIDTH,
        Align::Left,
        presentation.metric_style(display),
    );
}

/// `4h 00m`, `49m`, `2d 04h` — a battery estimate, in the largest two units.
///
/// Not [`monitrs_core::units::format_age`]: that renders `04:00:00`, and a colon-
/// separated triple next to `TO EMPTY` reads as a clock time rather than a span.
/// Presentation lives here by §10.1.
fn remaining_text(remaining: Duration) -> String {
    let total = remaining.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else {
        // Under an hour the minutes are the whole story, and a leading `0h` would
        // suggest a precision the estimate does not have.
        format!("{minutes}m")
    }
}

/// `48.2 Wh` from a µWh figure.
///
/// One decimal because that is the precision a pack's capacity is meaningful to:
/// `48.243100 Wh` implies a measurement nobody made.
fn watt_hours(microwatt_hours: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let watt_hours = microwatt_hours as f64 / 1e6;
    format!("{watt_hours:.1} Wh")
}

/// Why there is no battery reading, in the vocabulary of the state itself.
fn absence_reason(battery: &MetricState<BatterySnapshot>) -> &'static str {
    match battery {
        MetricState::Available(_) | MetricState::Stale { .. } => "",
        MetricState::Unsupported => "no battery on this machine",
        MetricState::WarmingUp => "not read yet",
        MetricState::PermissionDenied => "the OS refused the read",
        MetricState::TemporarilyUnavailable(reason) => reason.message(),
    }
}

/// The sentences under the meter when there is nothing to show.
///
/// Two lines at most, because a panel that explains itself at length is a panel
/// nobody reads. The `Unsupported` case gets the longer explanation on purpose: it
/// is the one a user is most likely to mistake for a bug in monitrs.
fn explanation(battery: &MetricState<BatterySnapshot>) -> &'static [&'static str] {
    match battery {
        MetricState::Unsupported => &[
            "A desktop, a server, a virtual machine and a container all read this way.",
            "It is a fact about the hardware, not a failed read and not a charge of zero.",
        ],
        // Not "on the medium tier" any more: §8.6 moved the battery into the sensor
        // group, which reads every 30 seconds — except that opening *this* screen
        // clears the sensor deadline (`TierScheduler::set_sensor_interest`), so the
        // reading really is seconds away rather than up to half a minute.
        MetricState::WarmingUp => {
            &["The battery is read with the sensors, and opening this screen asks for one now."]
        }
        MetricState::PermissionDenied => {
            &["The power source is present but unreadable at this privilege level."]
        }
        MetricState::TemporarilyUnavailable(_) => {
            &["The pack is there; this sample did not get a usable reading out of it."]
        }
        MetricState::Available(_) | MetricState::Stale { .. } => &[],
    }
}

/// Draws the thermal-sensor panel.
///
/// These readings exist in every snapshot and until now reached the screen only as
/// the single hottest figure in the Overview header. A machine with twenty-five
/// sensors had twenty-four of them measured, retained, and never shown.
fn draw_thermal(
    buffer: &mut Buffer,
    area: Rect,
    sensors: &SensorSnapshot,
    presentation: Presentation<'_>,
) {
    let readings = sensors
        .temperatures
        .displayable()
        .map(|(readings, _)| readings);
    // Sensors run on their own cadence, so this list is a retained one most of the time.
    // Its age is the panel's to state (see the module documentation) and the rows' to be
    // marked with.
    let retained = retained_age(&sensors.temperatures);
    let probe = inner_of(presentation, area, Borders::ALL);
    let room = usize::from(probe.height);
    // Apple Silicon declares no critical threshold on any sensor, so on that machine
    // every one of seventeen rows would carry the same "no declared limit" note.
    // Saying it once, in the panel's own label, is the difference between a fact and
    // seventeen repetitions of it — and a machine where only *some* sensors declare a
    // limit still needs it per row, which is why the rows are told which case they are
    // in rather than deciding for themselves.
    let unscaled = readings.is_some_and(|readings| {
        !readings.is_empty() && readings.iter().all(|r| r.share_of_critical().is_none())
    });
    // `annotated()` rather than a hand-built count, so a retained list says `2 sensors
    // ~00:28` and the age cannot be separated from the readings it applies to.
    let counted = states::describe(
        &sensors.temperatures,
        |readings: &Vec<TemperatureReading>| {
            let total = readings.len();
            truncation_label(room.min(total), total).unwrap_or_else(|| format!("{total} sensors"))
        },
    )
    .annotated();
    let trailing = match readings {
        // Through `fit_label` because the label now has up to two clauses: at 80 columns
        // the "no limit" note does not fit, and losing the count and the age with it
        // would be the worse trade (§5.4).
        Some(_) if unscaled => fit_label(
            &[counted, "none declares a limit".to_owned()],
            "THERMAL SENSORS",
            area.width,
        ),
        Some(_) => counted,
        None => absence_reason_for_sensors(&sensors.temperatures).to_owned(),
    };
    let inner = inset(draw_bordered_panel(
        buffer,
        area,
        presentation,
        "THERMAL SENSORS",
        Some(trailing.as_str()),
        false,
        Borders::ALL,
    ));
    if inner.is_empty() {
        return;
    }

    let Some(readings) = readings else {
        // Straight through `states`, because this is a metric's absence and not the
        // panel's own remark about itself.
        let display = states::describe(&sensors.temperatures, |_| String::new());
        let mut row = row_builder(presentation, inner.width);
        row.push_field(
            &display.symbol().to_string(),
            1,
            Align::Left,
            presentation.metric_style(&display),
        );
        row.push(display.text(), presentation.metric_style(&display));
        write_lines(buffer, inner, &[row.finish()]);
        return;
    };
    if readings.is_empty() {
        // A platform that exposes the sensor class and lists nothing under it. Not a
        // failure, and not a metric state either: the list really is empty.
        write_lines(
            buffer,
            inner,
            &[muted_line(
                presentation,
                inner.width,
                "the platform exposes no thermal sensors",
            )],
        );
        return;
    }

    // Hottest first: on a machine with more sensors than rows, the ones that get cut
    // must be the ones with least to say, and the panel's trailing label says how
    // many were cut.
    let mut ordered: Vec<&TemperatureReading> = readings.iter().collect();
    ordered.sort_by(|left, right| {
        right
            .celsius
            .total_cmp(&left.celsius)
            .then_with(|| left.label.cmp(&right.label))
    });
    let lines: Vec<Line<'static>> = ordered
        .into_iter()
        .take(room)
        .map(|reading| thermal_line(reading, retained, presentation, inner.width, unscaled))
        .collect();
    write_lines(buffer, inner, &lines);
}

/// `performance   62.5C  [####----]  high 95.0C  crit 105.0C`
///
/// `retained` is the age of the reading list this row came out of. It is not printed
/// here — the panel's trailing label carries it once for all the rows — but it decides
/// how the figure is *styled*, because a retained temperature drawn in the colour of a
/// fresh one is the same claim as an undated one (§4, §5.3).
fn thermal_line(
    reading: &TemperatureReading,
    retained: Option<Duration>,
    presentation: Presentation<'_>,
    width: u16,
    panel_says_unscaled: bool,
) -> Line<'static> {
    let glyphs = presentation.glyphs();
    let mut row = row_builder(presentation, width);
    row.push_field(
        &states::fit_within(&reading.label, usize::from(SENSOR_WIDTH), glyphs),
        SENSOR_WIDTH,
        Align::Left,
        presentation.style(Token::Text),
    );
    row.pad(1);

    // §5.2 and §11.3: the marker is the *sensor's* claim that it is past its own
    // critical threshold, and nothing here turns that into a throttling diagnosis.
    let critical = reading.is_critical() == Some(true);
    // Through `states` so the retained figure picks up `Token::Stale` from the one place
    // that decides what a state looks like. `Critical` still wins where the sensor says
    // it is past its limit: the `!` beside the figure is an alert, and an alert the user
    // has to act on outranks the mark saying when it was taken.
    let degrees = states::describe(
        &retained_field(MetricState::Available(reading.celsius), retained),
        |celsius: &f32| format!("{celsius:.1}C{}", if critical { "!" } else { "" }),
    );
    let token = if critical {
        Token::Critical
    } else {
        degrees.token()
    };
    row.push_field(
        &degrees.fitted(usize::from(DEGREES_WIDTH), glyphs),
        DEGREES_WIDTH,
        Align::Right,
        presentation.style(token),
    );
    row.pad(2);

    // §7.4's rule applied to temperature, and the refusal is the *model's*: only a
    // sensor-declared critical threshold is a full scale. Where there is none the
    // column holds the unknown track and says why, rather than going blank — a blank
    // column reads as "nothing to report" and a bar would read as a measured share.
    match reading.share_of_critical() {
        Some(share) => {
            row.push_field(
                &glyphs.meter(
                    share.clamped_to_100().fraction(),
                    usize::from(THERMAL_BAR_WIDTH),
                ),
                THERMAL_BAR_WIDTH,
                Align::Left,
                presentation.style(token),
            );
        }
        None => {
            row.push_field(
                &glyphs.unknown_meter(usize::from(THERMAL_BAR_WIDTH)),
                THERMAL_BAR_WIDTH,
                Align::Left,
                presentation.style(Token::Muted),
            );
        }
    }
    row.pad(2);
    row.push(
        &context_text(reading, panel_says_unscaled),
        presentation.style(Token::Muted),
    );
    row.finish()
}

/// `peak 72.1C  crit 105.0C`, or the reason there is no bar.
///
/// `peak` rather than `high`, because that is what the figure is: the highest value
/// seen, which on some drivers happens to be a declared limit and on macOS never is.
/// Calling it a threshold on screen would invite exactly the reading
/// [`TemperatureReading::share_of_critical`] refuses to compute.
fn context_text(reading: &TemperatureReading, panel_says_unscaled: bool) -> String {
    let mut parts = Vec::new();
    if let Some(peak) = reading.peak_celsius {
        parts.push(format!("peak {peak:.1}C"));
    }
    match reading.critical_celsius {
        Some(critical) => parts.push(format!("crit {critical:.1}C")),
        // The absence has to be stated somewhere, or an empty column reads as
        // "nothing wrong here" — but only once. When no sensor on the machine
        // declares a limit the panel's label says so and the rows stay quiet.
        None if !panel_says_unscaled => parts.push("no declared limit, so no scale".to_owned()),
        None => {}
    }
    parts.join("  ")
}

/// Why there are no sensor readings, for the panel's trailing label.
fn absence_reason_for_sensors<T>(state: &MetricState<T>) -> &'static str {
    match state {
        MetricState::Unsupported => "no sensors on this platform",
        MetricState::WarmingUp => "not read yet",
        MetricState::PermissionDenied => "reads refused",
        MetricState::TemporarilyUnavailable(reason) => reason.message(),
        MetricState::Available(_) | MetricState::Stale { .. } => "",
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::UnavailableReason;
    use monitrs_core::units::Percent;

    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The style of the first span whose text contains `needle`.
    ///
    /// The styles are exactly what [`text_of`] throws away, and on this screen they are
    /// the *only* thing separating a retained field from a freshly measured one: the
    /// cells are byte-identical either way, because `push_field` renders
    /// `MetricDisplay::fitted` and that never emits the `~`. So a test — or a snapshot —
    /// that reads only text cannot fail when `retained_field` is dropped from a call
    /// site, which is the hole this exists to close.
    fn style_of(line: &Line<'static>, needle: &str) -> ratatui::style::Style {
        line.spans
            .iter()
            .find(|span| span.content.contains(needle))
            .map_or_else(
                || panic!("no span containing {needle:?} in {:?}", text_of(line)),
                |span| span.style,
            )
    }

    fn battery() -> BatterySnapshot {
        BatterySnapshot {
            charge: Percent::new(82.0).expect("finite"),
            state: ChargeState::Discharging,
            time_remaining: MetricState::Available(Duration::from_secs(4 * 3_600)),
            cycle_count: MetricState::Available(214),
            capacity: MetricState::Available(BatteryCapacity {
                design_microwatt_hours: 52_600_000,
                full_microwatt_hours: 48_200_000,
            }),
            temperature_celsius: MetricState::Available(31.4),
            power_watts: MetricState::Available(12.4),
        }
    }

    fn reading(label: &str, celsius: f32, critical: Option<f32>) -> TemperatureReading {
        TemperatureReading {
            label: label.into(),
            celsius,
            peak_celsius: None,
            critical_celsius: critical,
        }
    }

    #[test]
    fn a_machine_with_no_battery_says_so_and_shows_no_zero_anywhere() {
        // The case every CI runner and every server hits. §4 and §26: `n/a` with a
        // reason, never `0%`, and never a silently blank panel.
        let absent: MetricState<BatterySnapshot> = MetricState::Unsupported;
        let charge = text_of(&charge_line(&absent, presentation(), 120));
        assert!(charge.contains("n/a"), "{charge}");
        assert!(charge.contains("no battery on this machine"), "{charge}");
        assert!(
            !charge.contains("0%"),
            "an absent battery must never render as a charge level: {charge}"
        );
        // §5.2: the state also carries its own character, so the reading survives
        // with colour off.
        assert!(charge.contains('-'), "{charge}");
        assert_eq!(headline(&absent), "no battery on this machine");
        assert_eq!(explanation(&absent).len(), 2);
    }

    #[test]
    fn the_absent_panel_is_sized_to_its_prose_rather_than_to_a_grid_of_placeholders() {
        // Five rows of `n/a` would be an empty panel with extra steps, and a fixed
        // height would grow blank rows to reach it. Each absence gets exactly the
        // meter, a blank, and its own sentences.
        let sensors = |battery| SensorSnapshot {
            temperatures: MetricState::Unsupported,
            battery,
        };
        for state in [
            MetricState::Unsupported,
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
        ] {
            let rows = battery_line_count(&sensors(state));
            let sentences = u16::try_from(explanation(&state).len()).expect("two at most");
            assert_eq!(rows, sentences + 2, "{state:?} asked for {rows} rows");
        }
        assert_eq!(
            battery_line_count(&sensors(MetricState::Available(battery()))),
            PRESENT_ROWS
        );
    }

    #[test]
    fn every_absence_keeps_its_own_reason_rather_than_collapsing_to_one_message() {
        // §4 draws distinctions the UI must not throw away: "this machine has none",
        // "not looked yet", and "the OS refused" call for three different responses
        // from the person reading the screen.
        let states: [MetricState<BatterySnapshot>; 4] = [
            MetricState::Unsupported,
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
        ];
        let mut reasons: Vec<&str> = states.iter().map(absence_reason).collect();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), states.len(), "{reasons:?}");
        for state in &states {
            assert!(!explanation(state).is_empty(), "{state:?}");
        }
    }

    #[test]
    fn the_estimate_label_names_the_direction_it_runs_in() {
        // `4h 00m` beside a charging pack and beside a discharging one are opposite
        // claims, and an unlabelled figure would be read as whichever the user
        // expected.
        let discharging = text_of(&vitals_line(&battery(), None, presentation(), 140));
        assert!(discharging.contains("TO EMPTY"), "{discharging}");
        let mut charging = battery();
        charging.state = ChargeState::Charging;
        let text = text_of(&vitals_line(&charging, None, presentation(), 140));
        assert!(text.contains("TO FULL"), "{text}");
    }

    #[test]
    fn a_pack_with_no_published_estimate_shows_no_time_at_all() {
        // §4's central case on this screen: charge divided by current is not an
        // answer, so a pack whose platform publishes nothing gets a placeholder.
        let mut unreported = battery();
        unreported.time_remaining = MetricState::Unsupported;
        let text = text_of(&vitals_line(&unreported, None, presentation(), 140));
        assert!(text.contains("n/a"), "{text}");
        // Nothing that could be read as a duration.
        assert!(!text.contains('h'), "{text}");
        assert!(!text.contains("00m"), "{text}");
    }

    #[test]
    fn the_worn_capacity_is_printed_beside_the_design_capacity_and_the_health() {
        // The three numbers only mean anything together: 48.2 Wh is alarming or fine
        // depending entirely on what the cell shipped holding.
        let text = text_of(&capacity_line(&battery(), None, presentation(), 140));
        assert!(text.contains("48.2 Wh"), "{text}");
        assert!(text.contains("52.6 Wh design"), "{text}");
        assert!(text.contains("HEALTH"), "{text}");
        // 48.2 of 52.6 rounds to 92%: the wear figure a user acts on, and the one
        // that must come out of the two capacities beside it rather than a field of
        // its own that could drift away from them.
        assert!(text.contains("92%"), "{text}");
    }

    #[test]
    fn a_battery_that_reports_no_capacity_reports_no_health_either() {
        // Not 0% healthy, and not a blank column: both figures come from the same
        // absent pair, so both say the same thing about why they are missing.
        let mut unknown = battery();
        unknown.capacity = MetricState::Unsupported;
        let text = text_of(&capacity_line(&unknown, None, presentation(), 140));
        assert!(text.contains("n/a"), "{text}");
        assert!(!text.contains("0%"), "{text}");
    }

    #[test]
    fn a_measured_zero_watt_draw_is_not_confused_with_an_unavailable_one() {
        // A full pack on mains really draws nothing, and that is a reading. The
        // distinction §4 protects only works if the true zero survives too.
        let mut full = battery();
        full.state = ChargeState::Full;
        full.power_watts = MetricState::Available(0.0);
        let measured = text_of(&vitals_line(&full, None, presentation(), 140));
        assert!(measured.contains("0.0 W"), "{measured}");

        full.power_watts = MetricState::Unsupported;
        let absent = text_of(&vitals_line(&full, None, presentation(), 140));
        assert!(!absent.contains("0.0 W"), "{absent}");
    }

    #[test]
    fn a_battery_estimate_reads_as_a_span_and_not_as_a_clock_time() {
        assert_eq!(remaining_text(Duration::from_secs(4 * 3_600)), "4h 00m");
        assert_eq!(remaining_text(Duration::from_secs(2_940)), "49m");
        assert_eq!(remaining_text(Duration::from_secs(14_700)), "4h 05m");
        assert_eq!(
            remaining_text(Duration::from_secs(2 * 86_400 + 4 * 3_600)),
            "2d 04h"
        );
    }

    #[test]
    fn no_temperature_bar_is_drawn_without_a_threshold_to_scale_it_against() {
        // The §7.4 rule, applied to a sensor: 62 °C is most of the way to a laptop's
        // limit and barely warm for a GPU, so a bar without a declared ceiling would
        // be a made-up scale. Every real Apple Silicon sensor is this case.
        let text = text_of(&thermal_line(
            &reading("ambient", 62.5, None),
            None,
            presentation(),
            140,
            false,
        ));
        assert!(text.contains("62.5C"), "{text}");
        assert!(text.contains("no declared limit"), "{text}");
        // The unknown track, not an empty column and not a bar.
        assert!(text.contains('.'), "{text}");
        assert!(!text.contains('#'), "{text}");

        let scaled = text_of(&thermal_line(
            &reading("package", 52.5, Some(105.0)),
            None,
            presentation(),
            140,
            false,
        ));
        assert!(scaled.contains('#'), "{scaled}");
        assert!(scaled.contains("crit 105.0C"), "{scaled}");
    }

    #[test]
    fn a_peak_is_labelled_as_a_peak_and_never_becomes_the_scale() {
        // `peak_celsius` is the highest value seen, not a limit. Labelling it `high`
        // would invite the reading `share_of_critical` refuses to compute, and using
        // it as a denominator would peg every bar at 100% for the whole run.
        let mut peaked = reading("PMU tdie1", 70.8, None);
        peaked.peak_celsius = Some(73.7);
        let text = text_of(&thermal_line(&peaked, None, presentation(), 140, false));
        assert!(text.contains("peak 73.7C"), "{text}");
        assert!(!text.contains("high"), "{text}");
        assert!(!text.contains('#'), "a peak is not a scale:\n{text}");
    }

    #[test]
    fn a_sensor_past_its_own_threshold_is_flagged_without_diagnosing_throttling() {
        // §11.3: the marker is the sensor's claim about its own limit, and §5.2 wants
        // a character rather than only a colour.
        let hot = reading("package", 106.0, Some(105.0));
        let text = text_of(&thermal_line(&hot, None, presentation(), 140, false));
        assert!(text.contains("106.0C!"), "{text}");
        assert!(!text.contains("throttl"), "{text}");
        // Past the end of the scale the bar is full rather than overflowing its field.
        let share = hot.share_of_critical().expect("a declared ceiling");
        assert!(share.value() > 100.0, "{share}");
        assert!(
            (share.clamped_to_100().value() - 100.0).abs() < f32::EPSILON,
            "{share}"
        );
    }

    #[test]
    fn only_the_figures_the_sensor_actually_reports_are_named() {
        // §4: printing `peak 0.0C` for a sensor that reported none would be a
        // fabricated figure.
        assert_eq!(
            context_text(&reading("a", 40.0, None), false),
            "no declared limit, so no scale"
        );
        assert_eq!(
            context_text(&reading("a", 40.0, Some(100.0)), false),
            "crit 100.0C"
        );
        let mut both = reading("a", 40.0, Some(100.0));
        both.peak_celsius = Some(90.0);
        assert_eq!(context_text(&both, false), "peak 90.0C  crit 100.0C");
        // Where the panel already said no sensor declares a limit, the row does not
        // repeat it once per sensor.
        let mut peaked = reading("a", 40.0, None);
        peaked.peak_celsius = Some(41.0);
        assert_eq!(context_text(&peaked, true), "peak 41.0C");
    }

    #[test]
    fn a_retained_pack_is_dated_once_in_the_panels_own_label() {
        // The finding this test exists for: both sensor metrics are read on their own
        // cadence, so at idle the whole `BatterySnapshot` arrives as `Stale`. The screen
        // used to unwrap it and print `POWER 12.4 W` with nothing saying the wattage was
        // measured 28 seconds ago (§4, and the design document's A2).
        let age = Duration::from_secs(28);
        let retained = MetricState::Available(battery()).into_stale(age);
        assert_eq!(headline(&retained), "82% discharging ~00:28");
        assert_eq!(retained_age(&retained), Some(age));
        // And exactly once: the rows below it are marked, not re-dated.
        let vitals = text_of(&vitals_line(&battery(), Some(age), presentation(), 140));
        assert!(!vitals.contains("~00:28"), "{vitals}");
        let capacity = text_of(&capacity_line(&battery(), Some(age), presentation(), 140));
        assert!(!capacity.contains("~00:28"), "{capacity}");
    }

    #[test]
    fn a_freshly_measured_pack_carries_no_mark_at_all() {
        // The other half of §4: a fresh reading must not be decorated, or the mark stops
        // meaning anything.
        let fresh = MetricState::Available(battery());
        assert_eq!(headline(&fresh), "82% discharging");
        assert_eq!(retained_age(&fresh), None);
        for state in [
            MetricState::Unsupported,
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
        ] {
            let state: MetricState<BatterySnapshot> = state;
            assert_eq!(retained_age(&state), None, "{state:?}");
        }
    }

    #[test]
    fn every_field_of_a_retained_pack_is_styled_as_retained_rather_than_as_measured() {
        // §5.2 and §5.3: the panel label states the age once, and what each field owes is
        // the mark. A field drawn in `Token::Text` is a field claiming to be this tick's
        // reading, which is the same lie as an undated figure.
        let age = Duration::from_secs(28);
        let battery = battery();
        for state in [
            retained_field(battery.cycle_count.as_ref(), Some(age)),
            retained_field(battery.time_remaining.as_ref(), Some(age)).map(|_| &214u32),
        ] {
            let display = states::describe(&state, |value: &&u32| value.to_string());
            assert_eq!(display.token(), Token::Stale, "{state:?}");
            assert_eq!(display.symbol(), '~', "{state:?}");
        }
    }

    #[test]
    fn a_field_the_platform_refused_keeps_its_own_reason_inside_a_retained_pack() {
        // `into_stale` only touches `Available`, and that is load-bearing here: a pack
        // whose driver publishes no time estimate must still say `n/a` rather than be
        // recast as a retained reading of something.
        let mut unreported = battery();
        unreported.time_remaining = MetricState::PermissionDenied;
        let display = states::describe(
            &retained_field(
                unreported.time_remaining.as_ref(),
                Some(Duration::from_secs(28)),
            ),
            |value: &&Duration| remaining_text(**value),
        );
        assert_eq!(display.text(), "permission denied");
        assert_eq!(display.symbol(), '!');
        assert_eq!(display.age(), None);
    }

    /// The regression test for C1's own fix, at the call sites rather than the helper.
    ///
    /// `every_field_of_a_retained_pack_is_styled_as_retained_rather_than_as_measured`
    /// exercises `retained_field` directly, and the Battery snapshot fixtures compare
    /// text — so between them, all three `retained_field` call sites could be deleted
    /// while the `retained` parameters stayed in place, the four vitals fields would fall
    /// back to `Token::Text`, and every other test in the workspace would still pass.
    /// That is the same shape of hole C1 existed to close: a correct rendering with
    /// nothing able to observe it. This asserts the styles the cells actually carry.
    #[test]
    fn a_retained_pack_is_styled_stale_at_the_call_site_and_not_only_in_the_helper() {
        let presentation = presentation();
        let stale = presentation.style(Token::Stale);
        let measured = presentation.style(Token::Text);
        let age = Some(Duration::from_secs(28));
        // Every value on the vitals row, by the text a reader sees in it.
        const VITALS: [&str; 4] = ["4h 00m", "214", "31.4C", "12.4 W"];

        let retained = vitals_line(&battery(), age, presentation, 140);
        for value in VITALS {
            assert_eq!(
                style_of(&retained, value),
                stale,
                "{value} is drawn as a fresh measurement"
            );
        }
        // And the other half of §4: a fresh reading is not decorated, so this test fails
        // if `retained_field` ever marks unconditionally.
        let fresh = vitals_line(&battery(), None, presentation, 140);
        for value in VITALS {
            assert_eq!(
                style_of(&fresh, value),
                measured,
                "{value} is marked retained on a freshly measured pack"
            );
        }

        let capacity = capacity_line(&battery(), age, presentation, 140);
        assert_eq!(style_of(&capacity, "48.2 Wh"), stale);
        assert_eq!(
            style_of(&capacity, "92%"),
            stale,
            "HEALTH is derived from the \
             capacities beside it, so it is exactly as old as they are"
        );
        let capacity_fresh = capacity_line(&battery(), None, presentation, 140);
        assert_eq!(style_of(&capacity_fresh, "48.2 Wh"), measured);
        assert_eq!(style_of(&capacity_fresh, "92%"), measured);

        let thermal = thermal_line(
            &reading("performance", 62.5, Some(105.0)),
            age,
            presentation,
            140,
            false,
        );
        assert_eq!(style_of(&thermal, "62.5C"), stale);
        let thermal_fresh = thermal_line(
            &reading("performance", 62.5, Some(105.0)),
            None,
            presentation,
            140,
            false,
        );
        assert_eq!(style_of(&thermal_fresh, "62.5C"), measured);

        // The cells are byte-identical across both, which is why this test reads styles
        // and the snapshot fixtures cannot.
        assert_eq!(text_of(&retained), text_of(&fresh));
        assert_eq!(text_of(&thermal), text_of(&thermal_fresh));
    }

    #[test]
    fn a_retained_temperature_is_styled_stale_unless_the_sensor_calls_it_critical() {
        // The thermal rows are dated by the panel's label, so what a row owes is the
        // mark — except where the sensor says it is past its own limit, in which case the
        // alert is what the reader has to act on and outranks the staleness cue.
        let age = Some(Duration::from_secs(28));
        let warm = states::describe(
            &retained_field(MetricState::Available(62.5f32), age),
            |celsius: &f32| format!("{celsius:.1}C"),
        );
        assert_eq!(warm.token(), Token::Stale);
        // The row itself never repeats the figure the panel label already carries.
        let text = text_of(&thermal_line(
            &reading("performance", 62.5, Some(105.0)),
            age,
            presentation(),
            140,
            false,
        ));
        assert!(text.contains("62.5C"), "{text}");
        assert!(!text.contains("~00:28"), "{text}");
        // And a hot sensor still says so.
        let hot = text_of(&thermal_line(
            &reading("package", 106.0, Some(105.0)),
            age,
            presentation(),
            140,
            false,
        ));
        assert!(hot.contains("106.0C!"), "{hot}");
    }

    #[test]
    fn a_retained_sensor_list_is_dated_in_the_panels_trailing_label() {
        // The label is built through `describe`/`annotated` precisely so the count and
        // the age cannot be separated. Rendered through the whole panel, because the
        // label is assembled in `draw_thermal` rather than in a function of its own.
        let sensors = SensorSnapshot {
            temperatures: MetricState::Available(vec![
                reading("performance", 62.5, Some(105.0)),
                reading("efficiency", 44.0, Some(105.0)),
            ])
            .into_stale(Duration::from_secs(28)),
            battery: MetricState::Unsupported,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 140, 6));
        draw_thermal(
            &mut buffer,
            Rect::new(0, 0, 140, 6),
            &sensors,
            presentation(),
        );
        let rendered: String = (0..6)
            .map(|row| {
                (0..140)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("2 sensors ~00:28"), "{rendered}");
    }

    #[test]
    fn a_wh_figure_is_rounded_to_a_precision_someone_measured() {
        assert_eq!(watt_hours(52_600_000), "52.6 Wh");
        assert_eq!(watt_hours(48_243_100), "48.2 Wh");
        assert_eq!(watt_hours(0), "0.0 Wh");
    }
}
