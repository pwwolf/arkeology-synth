//! Rendering of the TUI with ratatui.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus, Popup, Target, channel_label};
use crate::params::{Kind, ParamDesc};
use crate::synth::{SynthKind, fm};

const ACCENT: Color = Color::Rgb(255, 170, 60);
const ACCENT_DIM: Color = Color::Rgb(140, 95, 40);
const FG_DIM: Color = Color::Rgb(120, 120, 130);
const BAR_EMPTY: Color = Color::Rgb(60, 60, 70);
const SEL_BG: Color = Color::Rgb(45, 45, 60);
const OK: Color = Color::Rgb(110, 200, 120);
const WARN: Color = Color::Rgb(230, 200, 80);
const HOT: Color = Color::Rgb(235, 80, 70);
const LEARN: Color = Color::Rgb(90, 180, 255);

const SAMPLER_COLOR: Color = Color::Rgb(165, 150, 255);

const COL_WIDTH: u16 = 40;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(2),
    ])
    .areas(area);

    draw_header(f, app, header);

    let [left, right] = Layout::horizontal([Constraint::Length(48), Constraint::Min(30)]).areas(body);
    let [rack, monitor] = Layout::vertical([Constraint::Min(6), Constraint::Length(9)]).areas(left);
    draw_rack(f, app, rack);
    draw_monitor(f, app, monitor);
    draw_params(f, app, right);
    draw_footer(f, app, footer);

    if let Some(popup) = &app.popup {
        draw_popup(f, app, popup, area);
    }
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let color = if focused { ACCENT } else { FG_DIM };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(Span::styled(format!(" {title} "), Style::default().fg(color).bold()))
}

fn meter_spans(level: f32, width: usize) -> Vec<Span<'static>> {
    const PARTS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let db = if level > 1e-6 { 20.0 * level.log10() } else { -120.0 };
    let norm = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let cells = norm * width as f32;
    let full = cells.floor() as usize;
    let mut s = String::new();
    for _ in 0..full.min(width) {
        s.push('█');
    }
    if full < width {
        let frac = cells - full as f32;
        if frac > 0.1 {
            s.push(PARTS[((frac * 8.0) as usize).min(7)]);
        }
    }
    let used = s.chars().count();
    let color = if db > -1.0 {
        HOT
    } else if db > -9.0 {
        WARN
    } else {
        OK
    };
    vec![
        Span::styled(s, Style::default().fg(color)),
        Span::styled("·".repeat(width - used), Style::default().fg(BAR_EMPTY)),
    ]
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let b = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_DIM));
    let inner = b.inner(area);
    f.render_widget(b, area);

    let midi_count = app.midi.connected_names().len();
    let midi_txt = format!(
        "MIDI {} in{}",
        midi_count,
        if app.midi.has_virtual() { " + virtual" } else { "" }
    );
    let cpu_color = if app.cpu > 0.8 { HOT } else if app.cpu > 0.5 { WARN } else { FG_DIM };
    let mut spans = vec![
        Span::styled(" ◢◤ ARKEOLOGY ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::raw("  "),
        Span::styled(format!("{:.1} kHz", app.sample_rate / 1000.0), Style::default().fg(FG_DIM)),
        Span::styled(" · ", Style::default().fg(BAR_EMPTY)),
        Span::styled(app.device_name.clone(), Style::default().fg(FG_DIM)),
        Span::styled(" · ", Style::default().fg(BAR_EMPTY)),
        Span::styled(format!("CPU {:>3.0}%", app.cpu * 100.0), Style::default().fg(cpu_color)),
        Span::styled(" · ", Style::default().fg(BAR_EMPTY)),
        Span::styled(midi_txt, Style::default().fg(FG_DIM)),
    ];
    if let Some(addr) = app.mcp_addr {
        spans.push(Span::styled(" · ", Style::default().fg(BAR_EMPTY)));
        spans.push(Span::styled(format!("MCP :{}", addr.port()), Style::default().fg(FG_DIM)));
    }
    if app.keyboard.enabled {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" ♪ KEYS oct {} vel {} ", app.keyboard.octave, app.keyboard.velocity),
            Style::default().fg(Color::Black).bg(LEARN).bold(),
        ));
    }
    let soloed = app.soloed_slots();
    if !soloed.is_empty() {
        let list: Vec<String> = soloed.iter().map(|i| (i + 1).to_string()).collect();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" SOLO {} ", list.join(",")),
            Style::default().fg(Color::Black).bg(OK).bold(),
        ));
    }
    if let Some(rec) = &app.recording {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" ● REC {} ", crate::app::format_duration(rec.seconds())),
            Style::default().fg(Color::White).bg(HOT).bold(),
        ));
    }
    if app.learning.is_some() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(" MIDI LEARN ", Style::default().fg(Color::Black).bg(LEARN).bold()));
    }
    let [left, right] = Layout::horizontal([Constraint::Min(10), Constraint::Length(28)]).areas(inner);
    f.render_widget(Paragraph::new(Line::from(spans)), left);

    let mut m = vec![Span::styled("L ", Style::default().fg(FG_DIM))];
    m.extend(meter_spans(app.master_meter.0, 10));
    m.push(Span::styled("  R ", Style::default().fg(FG_DIM)));
    m.extend(meter_spans(app.master_meter.1, 10));
    f.render_widget(Paragraph::new(Line::from(m)), right);
}

fn draw_rack(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Rack;
    let b = block("Rack", focused);
    let inner = b.inner(area);
    f.render_widget(b, area);

    let rows = app.rack_rows();
    let any_solo = !app.soloed_slots().is_empty();
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(row, t)| {
            let selected = row == app.rack_cursor;
            let marker = if selected { "▶" } else { " " };
            let base = if selected { Style::default().bg(SEL_BG) } else { Style::default() };
            let line = match t {
                Target::Master => {
                    let peak = app.master_meter.0.max(app.master_meter.1);
                    let mut spans = vec![
                        Span::styled(format!("{marker}    "), Style::default().fg(ACCENT)),
                        Span::styled(format!("{:<26}", "MASTER  · reverb · drive"), Style::default().bold()),
                    ];
                    spans.extend(meter_spans(peak, 8));
                    Line::from(spans)
                }
                Target::Slot(i) => {
                    let s = app.slots[*i].as_ref().expect("slot");
                    let active = s.activity.is_some_and(|t| t.elapsed().as_millis() < 150);
                    let ch_style = if active {
                        Style::default().fg(Color::Black).bg(ACCENT)
                    } else {
                        Style::default().fg(LEARN)
                    };
                    let kind_color = match s.kind {
                        SynthKind::Fm => Color::Rgb(255, 120, 160),
                        SynthKind::Granular => Color::Rgb(120, 220, 200),
                        SynthKind::Acid => Color::Rgb(190, 230, 90),
                        SynthKind::Drums => Color::Rgb(255, 210, 90),
                        SynthKind::Kit => Color::Rgb(255, 180, 130),
                        SynthKind::Sampler => SAMPLER_COLOR,
                        SynthKind::Analog => Color::Rgb(255, 150, 90),
                        SynthKind::Physical => Color::Rgb(205, 170, 120),
                    };
                    let name: String = s.name.chars().take(13).collect();
                    let mut spans = vec![
                        Span::styled(format!("{marker}{:>2} ", i + 1), Style::default().fg(ACCENT)),
                        Span::styled(format!("{:<4}", s.kind.label()), Style::default().fg(kind_color).bold()),
                        Span::styled(format!("{:<5}", short_channel(s.channel)), ch_style),
                        Span::raw(" "),
                        Span::styled(
                            format!("{name:<14}"),
                            if s.mute {
                                Style::default().fg(FG_DIM).crossed_out()
                            } else if any_solo && !s.solo {
                                // Silenced by another slot's solo.
                                Style::default().fg(FG_DIM).italic()
                            } else {
                                Style::default()
                            },
                        ),
                    ];
                    spans.extend(meter_spans(s.meter, 8));
                    spans.push(Span::styled(format!("{:>3}", s.voices), Style::default().fg(FG_DIM)));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        "M",
                        if s.mute { Style::default().fg(Color::Black).bg(WARN) } else { Style::default().fg(BAR_EMPTY) },
                    ));
                    spans.push(Span::styled(
                        "S",
                        if s.solo { Style::default().fg(Color::Black).bg(OK) } else { Style::default().fg(BAR_EMPTY) },
                    ));
                    Line::from(spans)
                }
            };
            ListItem::new(line).style(base)
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(app.rack_cursor));
    f.render_stateful_widget(List::new(items), inner, &mut state);

    if rows.len() == 1 && inner.height > 3 {
        let hint = Paragraph::new("No synths yet: press 'a' to add one.")
            .style(Style::default().fg(FG_DIM))
            .alignment(Alignment::Center);
        f.render_widget(hint, Rect { y: inner.y + 2, height: 1, ..inner });
    }
}

fn short_channel(ch: Option<u8>) -> String {
    match ch {
        None => "omni".into(),
        Some(c) => format!("ch{}", c + 1),
    }
}

fn draw_monitor(f: &mut Frame, app: &App, area: Rect) {
    let b = block("MIDI in", false);
    let inner = b.inner(area);
    f.render_widget(b, area);
    let lines: Vec<Line> = if app.midi_log.is_empty() {
        vec![
            Line::styled("waiting for MIDI…", Style::default().fg(FG_DIM)),
            Line::styled("p: choose ports · k: play from keyboard", Style::default().fg(FG_DIM)),
        ]
    } else {
        app.midi_log
            .iter()
            .take(inner.height as usize)
            .enumerate()
            .map(|(i, m)| {
                let c = if i == 0 { Color::White } else { FG_DIM };
                Line::styled(m.clone(), Style::default().fg(c))
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// One line of the parameter grid.
enum GridLine {
    Header(String),
    Param(usize),
    Blank,
}

fn group_title(app: &App, target: Target, group: &str) -> String {
    if let Some(n) = group.strip_prefix("FX ").and_then(|n| n.parse::<usize>().ok()) {
        let i = app.fx_base(target) + (n - 1) * crate::fx::STRIDE + crate::fx::TYPE;
        let kind = crate::fx::FxKind::from_value(app.param_value(target, i));
        let title = if target == Target::Master { format!("Master FX {n}") } else { group.to_string() };
        return format!("{title} · {}", kind.name());
    }
    if let Target::Slot(i) = target
        && let Some(s) = &app.slots[i]
        && s.kind == SynthKind::Fm
        && let Some(n) = group.strip_prefix("Op ").and_then(|n| n.parse::<usize>().ok())
    {
        let algo_idx = SynthKind::Fm.index_of("algorithm").expect("algorithm param");
        let algo = s.params[algo_idx].round() as usize;
        let role = if fm::is_carrier(algo, n - 1) { "carrier" } else { "modulator" };
        return format!("{group} · {role}");
    }
    if group == "Source"
        && let Target::Slot(i) = target
        && let Some(s) = &app.slots[i]
        && let Some(loaded) = s.sample(0)
    {
        return format!("Source · file: {}", loaded.sample.name);
    }
    // Kit pads: "Pad 3 · 42 F#2 · hat.wav" (or the pad's default role if empty).
    if let Target::Slot(i) = target
        && let Some(s) = &app.slots[i]
        && s.kind == SynthKind::Kit
        && let Some(n) = group.strip_prefix("Pad ").and_then(|n| n.parse::<usize>().ok())
    {
        let note = s.params[s.kind.index_of(&format!("pad{n}_note")).expect("pad note")].round() as u8;
        let what = match s.sample(n - 1) {
            Some(l) => l.sample.name.clone(),
            None => format!("empty ({})", crate::synth::kit::PAD_ROLES[n - 1]),
        };
        return format!("Pad {n} · {note} {} · {what}", crate::midi::note_name(note));
    }
    group.to_string()
}

fn draw_params(f: &mut Frame, app: &mut App, area: Rect) {
    let target = app.selected();
    let focused = app.focus == Focus::Params;
    let title = match target {
        Target::Master => "Master".to_string(),
        Target::Slot(i) => {
            let s = app.slots[i].as_ref().expect("slot");
            format!("{} · {} · {} · slot {}", s.name, s.kind.long_name(), channel_label(s.channel), i + 1)
        }
    };
    let b = block(&title, focused);
    let mut inner = b.inner(area);
    f.render_widget(b, area);

    // Samplers get a waveform view above their parameters.
    if let Target::Slot(i) = target
        && app.slots[i].as_ref().is_some_and(|s| s.kind == SynthKind::Sampler)
        && inner.height >= 18
    {
        let [wave, rest] = Layout::vertical([Constraint::Length(9), Constraint::Min(4)]).areas(inner);
        draw_waveform(f, app, i, wave);
        inner = rest;
    }

    let visible = app.visible_params(target);
    if visible.is_empty() || inner.height < 2 {
        return;
    }
    // Keep the cursor on a visible parameter (e.g. after an FX type change).
    if !visible.contains(&app.param_cursor) {
        let pos = visible.iter().rposition(|&i| i <= app.param_cursor).unwrap_or(0);
        app.param_cursor = visible[pos];
    }

    // Split visible params into contiguous groups.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for &i in &visible {
        let g = app.param_desc(target, i).group;
        match groups.last_mut() {
            Some((name, idx)) if name == g => idx.push(i),
            _ => groups.push((g.to_string(), vec![i])),
        }
    }

    // Fill columns in reading order, moving on once a column reaches its
    // share of the total height, so groups (e.g. Op 1..4) stay in sequence.
    let ncols = ((inner.width + 1) / (COL_WIDTH + 1)).max(1) as usize;
    let col_w = ((inner.width + 1) / ncols as u16).saturating_sub(1).max(20);
    let sizes: Vec<usize> = groups.iter().map(|(_, idx)| idx.len() + 1).collect();
    let breaks = partition(&sizes, ncols);
    let mut cols: Vec<Vec<GridLine>> = (0..ncols).map(|_| Vec::new()).collect();
    for (g, (name, idx)) in groups.iter().enumerate() {
        let c = breaks.iter().filter(|&&b| b <= g).count();
        if !cols[c].is_empty() {
            cols[c].push(GridLine::Blank);
        }
        cols[c].push(GridLine::Header(group_title(app, target, name)));
        cols[c].extend(idx.iter().map(|&i| GridLine::Param(i)));
    }

    // Keep the selected param visible.
    let height = inner.height as usize;
    let sel_row = cols
        .iter()
        .find_map(|col| col.iter().position(|l| matches!(l, GridLine::Param(i) if *i == app.param_cursor)))
        .unwrap_or(0);
    if sel_row < app.param_scroll + 1 {
        app.param_scroll = sel_row.saturating_sub(1);
    } else if sel_row >= app.param_scroll + height {
        app.param_scroll = sel_row + 1 - height;
    }
    let max_len = cols.iter().map(Vec::len).max().unwrap_or(0);
    app.param_scroll = app.param_scroll.min(max_len.saturating_sub(height));

    for (c, col) in cols.iter().enumerate() {
        let x = inner.x + c as u16 * (col_w + 1);
        let rect = Rect { x, y: inner.y, width: col_w.min(inner.right().saturating_sub(x)), height: inner.height };
        let lines: Vec<Line> = col
            .iter()
            .skip(app.param_scroll)
            .take(height)
            .map(|l| match l {
                GridLine::Blank => Line::raw(""),
                GridLine::Header(h) => {
                    let w = rect.width as usize;
                    let text = format!("── {h} ");
                    let fill = w.saturating_sub(text.chars().count());
                    Line::from(vec![
                        Span::styled(text, Style::default().fg(ACCENT).bold()),
                        Span::styled("─".repeat(fill), Style::default().fg(ACCENT_DIM)),
                    ])
                }
                GridLine::Param(i) => param_line(app, target, *i, rect.width as usize, focused),
            })
            .collect();
        f.render_widget(Paragraph::new(lines), rect);
    }
}

/// Split `sizes` (group heights) into at most `k` contiguous columns,
/// minimising the tallest column. Returns the group indices that start a new
/// column. Each group after the first in a column costs one separator line.
fn partition(sizes: &[usize], k: usize) -> Vec<usize> {
    let n = sizes.len();
    let k = k.min(n).max(1);
    let height = |a: usize, b: usize| sizes[a..b].iter().sum::<usize>() + (b - a).saturating_sub(1);
    // best[j][i] = minimal max height placing the first i groups in j columns.
    let mut best = vec![vec![usize::MAX; n + 1]; k + 1];
    let mut cut = vec![vec![0usize; n + 1]; k + 1];
    best[0][0] = 0;
    for j in 1..=k {
        for i in 1..=n {
            for s in (j - 1)..i {
                if best[j - 1][s] == usize::MAX {
                    continue;
                }
                let cost = best[j - 1][s].max(height(s, i));
                if cost < best[j][i] {
                    best[j][i] = cost;
                    cut[j][i] = s;
                }
            }
        }
    }
    let j_best = (1..=k).min_by_key(|&j| (best[j][n], j)).unwrap_or(1);
    let mut breaks = Vec::new();
    let (mut j, mut i) = (j_best, n);
    while j > 1 {
        let s = cut[j][i];
        breaks.push(s);
        i = s;
        j -= 1;
    }
    breaks.reverse();
    breaks
}

fn draw_waveform(f: &mut Frame, app: &App, slot: usize, area: Rect) {
    use crate::synth::sampler::{self, Mode};
    use ratatui::style::Stylize;
    use ratatui::symbols::Marker;
    use ratatui::widgets::canvas::{Canvas, Line as CLine};

    let Some(s) = app.slots[slot].as_ref() else { return };
    let [info, canvas_area, _gap] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let Some(sample) = app.slot_sample(slot).filter(|x| !x.is_empty()) else {
        f.render_widget(
            Paragraph::new("No sample loaded: press f to load a WAV or FLAC, or pick a built-in Source.")
                .style(Style::default().fg(FG_DIM)),
            info,
        );
        return;
    };
    let layout = sampler::Layout::new(&s.params[crate::synth::COMMON.len()..], &sample);
    let len = sample.len() as f64;
    let buckets = sample.overview.len() as f64;
    let to_x = |frame: usize| frame as f64 / len * buckets;

    let mode = match layout.mode {
        Mode::Classic if layout.looping.is_some() => "classic · looping".to_string(),
        Mode::Classic => "classic".to_string(),
        Mode::OneShot => "one-shot".to_string(),
        Mode::Slice => format!(
            "{} slices · notes {}–{}",
            layout.slices.count,
            layout.base_note,
            layout.base_note as usize + layout.slices.count - 1
        ),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(sample.name.clone(), Style::default().fg(SAMPLER_COLOR).bold()),
            Span::styled(
                format!(
                    "  {:.2}s · {} · {:.1} kHz{} · {}{}",
                    sample.duration(),
                    if sample.is_stereo() { "stereo" } else { "mono" },
                    sample.sample_rate / 1000.0,
                    match sample.pitch {
                        Some(p) => {
                            let n = p.round().clamp(0.0, 127.0);
                            format!(" · pitch {} {:+.0} ct", crate::midi::note_name(n as u8), (p - n) * 100.0)
                        }
                        None => String::new(),
                    },
                    mode,
                    if layout.reverse { " · reversed" } else { "" }
                ),
                Style::default().fg(FG_DIM),
            ),
        ])),
        info,
    );

    let chars_per_bucket = canvas_area.width as f64 / buckets;
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, buckets])
        .y_bounds([-1.0, 1.0])
        .paint(|ctx| {
            let (rs, re) = (to_x(layout.start), to_x(layout.end));
            let lp = layout.looping.map(|(a, b)| (to_x(a), to_x(b)));
            for (b, &(lo, hi)) in sample.overview.iter().enumerate() {
                let x = b as f64 + 0.5;
                let color = if x < rs || x > re {
                    BAR_EMPTY
                } else if lp.is_some_and(|(a, z)| x >= a && x <= z) {
                    LEARN
                } else {
                    SAMPLER_COLOR
                };
                ctx.draw(&CLine { x1: x, y1: lo as f64, x2: x, y2: hi.max(lo + 0.01) as f64, color });
            }
            ctx.layer();
            let vline = |ctx: &mut ratatui::widgets::canvas::Context, x: f64, color: Color| {
                ctx.draw(&CLine { x1: x, y1: -1.0, x2: x, y2: 1.0, color });
            };
            if layout.mode == Mode::Slice {
                for (i, &p) in layout.slices.points[..=layout.slices.count].iter().enumerate() {
                    vline(ctx, to_x(p), WARN);
                    // Label each slice with its note when there's room.
                    if let Some((a, z)) = layout.slices.range(i)
                        && (to_x(z) - to_x(a)) * chars_per_bucket >= 4.0
                    {
                        ctx.print(to_x(a) + 1.0, 1.0, format!("{}", layout.base_note as usize + i).fg(WARN));
                    }
                }
            } else {
                vline(ctx, rs, ACCENT);
                vline(ctx, re, ACCENT);
                if let Some((a, z)) = lp {
                    vline(ctx, a, LEARN);
                    vline(ctx, z, LEARN);
                }
            }
        });
    f.render_widget(canvas, canvas_area);
}

fn param_line(app: &App, target: Target, i: usize, width: usize, focused: bool) -> Line<'static> {
    let desc: &ParamDesc = app.param_desc(target, i);
    let value = app.param_value(target, i);
    let selected = i == app.param_cursor;
    let learning = app.learning == Some((target, i));
    let mapping = app.mapping_for(target, i);

    let name_w = 14;
    let value_w = 10;
    let bar_w = width.saturating_sub(name_w + value_w + 3).max(3);

    let name_style = if selected && focused {
        Style::default().fg(Color::Black).bg(ACCENT).bold()
    } else if selected {
        Style::default().fg(ACCENT).bold()
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::styled(format!("{:<w$}", truncate(desc.name, name_w), w = name_w), name_style),
        Span::styled(
            if learning { "?" } else if mapping.is_some() { "◆" } else { " " },
            Style::default().fg(LEARN),
        ),
    ];

    let bar_color = if selected { ACCENT } else { Color::Rgb(200, 140, 70) };
    match desc.kind {
        Kind::Enum(_) | Kind::Toggle => {
            let text = desc.format(value);
            let w = bar_w + 1 + value_w;
            spans.push(Span::styled(
                format!(" {:>w$}", truncate(&text, w), w = w),
                Style::default().fg(if selected { Color::White } else { Color::Rgb(200, 200, 210) }),
            ));
        }
        _ => {
            spans.push(Span::raw(" "));
            let norm = desc.normalize(value);
            let bipolar = desc.min < 0.0 && desc.max > 0.0 && desc.scale == crate::params::Scale::Linear;
            let mut bar = String::with_capacity(bar_w * 3);
            let mut on = Vec::with_capacity(bar_w);
            if bipolar {
                let center = desc.normalize(0.0) * bar_w as f32;
                let pos = norm * bar_w as f32;
                let (a, b) = if pos < center { (pos, center) } else { (center, pos) };
                for k in 0..bar_w {
                    let mid = k as f32 + 0.5;
                    on.push(mid >= a && mid <= b.max(a + 0.01) || (k as f32 <= center && center < k as f32 + 1.0));
                }
            } else {
                let filled = (norm * bar_w as f32).round() as usize;
                for k in 0..bar_w {
                    on.push(k < filled);
                }
            }
            let mut k = 0;
            while k < bar_w {
                let state = on[k];
                let start = k;
                while k < bar_w && on[k] == state {
                    k += 1;
                }
                bar.clear();
                for _ in start..k {
                    bar.push(if state { '━' } else { '─' });
                }
                spans.push(Span::styled(
                    bar.clone(),
                    Style::default().fg(if state { bar_color } else { BAR_EMPTY }),
                ));
            }
            spans.push(Span::styled(
                format!(" {:>w$}", truncate(&desc.format(value), value_w), w = value_w),
                Style::default().fg(if selected { Color::White } else { Color::Rgb(200, 200, 210) }),
            ));
        }
    }
    let line = Line::from(spans);
    if selected && focused { line.style(Style::default().bg(SEL_BG)) } else { line }
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn hint(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (k, v) in keys {
        spans.push(Span::styled(format!(" {k} "), Style::default().fg(Color::Black).bg(ACCENT_DIM)));
        spans.push(Span::styled(format!(" {v}  "), Style::default().fg(FG_DIM)));
    }
    Line::from(spans)
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let status = match &app.status {
        Some(s) => Line::styled(
            format!(" {}", s.text),
            Style::default().fg(if s.error { HOT } else { OK }),
        ),
        None => Line::styled(
            format!(" data: {}", app.storage.root.display()),
            Style::default().fg(FG_DIM),
        ),
    };
    let keys = if app.keyboard.enabled {
        hint(&[("a-'", "notes"), ("z/x", "octave"), ("c/v", "velocity"), ("space", "panic"), ("esc", "exit keys")])
    } else if app.focus == Focus::Rack {
        hint(&[
            ("↑↓", "select"),
            ("←→", "midi ch"),
            ("tab", "params"),
            ("a", "add"),
            ("l/w", "patch"),
            ("L/W", "session"),
            ("N", "new"),
            ("k", "play"),
            ("p", "ports"),
            ("?", "help"),
        ])
    } else {
        hint(&[
            ("↑↓", "select"),
            ("←→", "adjust"),
            ("⇧←→", "coarse"),
            (",.", "fine"),
            ("enter", "type"),
            ("⌫", "default"),
            ("c", "learn CC"),
            ("tab", "rack"),
            ("?", "help"),
        ])
    };
    f.render_widget(Paragraph::new(vec![status, keys]), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn list_popup(f: &mut Frame, area: Rect, title: &str, items: Vec<ListItem>, cursor: usize, footer: &str) {
    let h = (items.len() as u16 + 4).clamp(6, 24);
    let rect = centered(area, 64, h);
    f.render_widget(Clear, rect);
    let b = block(title, true);
    let inner = b.inner(rect);
    f.render_widget(b, rect);
    let [list_area, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    if items.is_empty() {
        f.render_widget(Paragraph::new("(nothing here)").style(Style::default().fg(FG_DIM)), list_area);
    } else {
        let mut state = ListState::default().with_selected(Some(cursor));
        let list = List::new(items)
            .highlight_style(Style::default().bg(ACCENT).fg(Color::Black).bold())
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, list_area, &mut state);
    }
    f.render_widget(Paragraph::new(footer.to_string()).style(Style::default().fg(FG_DIM)), foot);
}

fn draw_popup(f: &mut Frame, app: &App, popup: &Popup, area: Rect) {
    match popup {
        Popup::Help => draw_help(f, app, area),
        Popup::NewRack { cursor, save } => {
            let items = vec![
                ListItem::new("Empty rack"),
                ListItem::new("Starter rack: E.Piano, Choir Cloud, Acid Classic, 808 Kit"),
            ];
            let foot = if *save {
                format!("[x] save current rack as \"{}\" first · s: toggle · enter · esc", App::timestamped_rack_name())
            } else {
                "[ ] don't save the current rack · s: toggle · enter · esc".to_string()
            };
            list_popup(f, area, "New rack (replaces synths, master settings and MIDI mappings)", items, *cursor, &foot);
        }
        Popup::AddSynth { cursor } => {
            let items = SynthKind::ALL
                .iter()
                .enumerate()
                .map(|(i, k)| ListItem::new(format!("{}  {}", i + 1, k.long_name())))
                .collect();
            list_popup(f, area, "Add synth", items, *cursor, "enter: add · esc: cancel");
        }
        Popup::ConfirmRemove { slot } => {
            let name = app.slots[*slot].as_ref().map_or("?".into(), |s| s.name.clone());
            let rect = centered(area, 50, 5);
            f.render_widget(Clear, rect);
            let b = block("Remove synth", true);
            let inner = b.inner(rect);
            f.render_widget(b, rect);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("Remove slot {} '{}'?", slot + 1, name)),
                    Line::styled("y / enter: remove · any other key: cancel", Style::default().fg(FG_DIM)),
                ]),
                inner,
            );
        }
        Popup::Text { title, hint, input, .. } => {
            let rect = centered(area, 64, 6);
            f.render_widget(Clear, rect);
            let b = block(title, true);
            let inner = b.inner(rect);
            f.render_widget(b, rect);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled("> ", Style::default().fg(ACCENT)),
                        Span::raw(input.clone()),
                        Span::styled("▏", Style::default().fg(ACCENT).add_modifier(Modifier::SLOW_BLINK)),
                    ]),
                    Line::styled(hint.clone(), Style::default().fg(FG_DIM)),
                    Line::styled("enter: confirm · esc: cancel", Style::default().fg(FG_DIM)),
                ])
                .wrap(Wrap { trim: false }),
                inner,
            );
        }
        Popup::Patches { all, filter, cursor } => {
            let list = crate::app::patch_view(all, *filter)
                .into_iter()
                .map(|e| {
                    let needs_download = e.is_factory()
                        && e.patch.sample_paths().iter().flatten().any(|p| {
                            crate::vcsl::is_vcsl_path(p) && !app.storage.samples_dir().join(p).exists()
                        });
                    let (tag, tag_color) = if needs_download {
                        ("download", WARN)
                    } else if e.is_factory() {
                        ("factory", FG_DIM)
                    } else {
                        ("yours", OK)
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{:<4}", e.patch.kind.label()), Style::default().fg(FG_DIM)),
                        Span::raw(format!("{:<38}", truncate(&e.patch.name, 38))),
                        Span::styled(tag, Style::default().fg(tag_color)),
                    ]))
                })
                .collect();
            let title = match filter {
                Some(k) => format!("Load patch · {}", k.long_name()),
                None => "Load patch · all synths".to_string(),
            };
            list_popup(f, area, &title, list, *cursor, "enter: load · space: audition · tab: synth type · esc");
        }
        Popup::Sessions { items, cursor } => {
            let list = items
                .iter()
                .map(|p| ListItem::new(p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()))
                .collect();
            list_popup(f, area, "Load session", list, *cursor, "enter: load (replaces the rack) · esc: close");
        }
        Popup::Ports { items, cursor } => {
            let list = items
                .iter()
                .map(|name| {
                    let on = app.midi.is_connected(name);
                    ListItem::new(Line::from(vec![
                        Span::styled(if on { "● " } else { "○ " }, Style::default().fg(if on { OK } else { FG_DIM })),
                        Span::raw(name.clone()),
                    ]))
                })
                .collect();
            let foot = if app.midi.has_virtual() {
                "enter: toggle · r: rescan · esc · virtual port 'Arkeology Synth' is open"
            } else {
                "enter: toggle · r: rescan · esc: close"
            };
            list_popup(f, area, "MIDI inputs", list, *cursor, foot);
        }
        Popup::Files { dir, entries, cursor } => {
            let list = entries
                .iter()
                .map(|e| {
                    if e.is_dir {
                        ListItem::new(Span::styled(format!("{}/", e.name), Style::default().fg(LEARN)))
                    } else {
                        ListItem::new(e.name.clone())
                    }
                })
                .collect();
            let kit_slot = app.selected_slot().filter(|&i| app.slots[i].as_ref().is_some_and(|s| s.kind == SynthKind::Kit));
            let (title, foot) = match kit_slot {
                Some(i) => {
                    let pad = app.current_pad(i);
                    (
                        format!("Load into pad {} ({}) · {}", pad + 1, crate::synth::kit::PAD_ROLES[pad], dir.display()),
                        "enter: open/load into pad · K: load this whole folder as the kit · ⌫: up · esc".to_string(),
                    )
                }
                None => (format!("Load sample · {}", dir.display()), "enter: open/load · ⌫: up · esc: close".to_string()),
            };
            list_popup(f, area, &title, list, *cursor, &foot);
        }
    }
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let rect = centered(area, 86, 34);
    f.render_widget(Clear, rect);
    let b = block("Help", true);
    let inner = b.inner(rect);
    f.render_widget(b, rect);
    let k = |key: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {key:<14}"), Style::default().fg(ACCENT)),
            Span::raw(desc.to_string()),
        ])
    };
    let h = |t: &str| Line::styled(t.to_string(), Style::default().fg(LEARN).bold());
    let lines = vec![
        h("Rack"),
        k("↑ ↓", "select master / synth slot"),
        k("← → / - +", "change the slot's MIDI channel (omni, 1-16)"),
        k("a", "add a synth: FM, Analog, Physical, Granular, 303, Drums, Kit, Sampler"),
        k("d / ⌫", "remove the selected synth"),
        k("r", "rename      m  mute      s  solo"),
        k("tab / enter", "edit parameters"),
        h("Parameters"),
        k("↑ ↓  pgup/dn", "select parameter / jump between groups"),
        k("← →", "adjust  (shift: coarse, , and . : fine)"),
        k("enter", "type an exact value (e.g. 250ms, 2.5k, 40%)"),
        k("⌫", "reset to default"),
        k("c / C", "MIDI-learn a CC to this parameter / clear mapping"),
        k("FX 1-3", "set an FX unit's Type (delay, reverb, chorus, drive, EQ…) to show its controls"),
        h("Patches & sessions"),
        k("l / w", "load / write the selected synth's patch"),
        k("L / W", "load / write the whole rack as a session"),
        k("N", "new rack: empty or starter (saves the current one first by default)"),
        k("f", "load a WAV or FLAC into a granular synth or sampler, or a kit pad (K in the browser: whole folder)"),
        h("Playing"),
        k("k", "play the selected synth from the computer keyboard"),
        k("p", "choose MIDI input ports"),
        k("space", "panic: all notes off"),
        k("R", "start / stop recording the output to a WAV (in recordings/)"),
        k("q / ctrl-c", "quit (the rack is autosaved)"),
        Line::raw(""),
        Line::styled(
            "MIDI: each slot listens on its channel; CC1 mod wheel, CC64 sustain and pitch bend",
            Style::default().fg(FG_DIM),
        ),
        Line::styled(
            "are handled per slot. Several slots on one channel layer together.",
            Style::default().fg(FG_DIM),
        ),
        Line::styled(format!("Files live in {}", app.storage.root.display()), Style::default().fg(FG_DIM)),
        Line::styled("press any key to close", Style::default().fg(FG_DIM)),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}
