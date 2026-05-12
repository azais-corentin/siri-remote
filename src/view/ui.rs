//! Pure render functions for the `view` dashboard. No IO.
//!
//! Two top-level panes — the remote on the left, status / touch / events on
//! the right. The remote is rendered on a single `Canvas` widget in
//! remote-local coordinates (x in `0..=100`, y in `0..=300`, ≈1:3 aspect)
//! so all shape coordinates stay stable regardless of terminal cell size.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Circle, Context, Line as CanvasLine, Points, Rectangle};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use tokio::time::Instant;

use crate::decoder::BUTTON_NAMES;
use crate::session::PowerState;
use crate::view::state::{
    AppState, BUTTON_AFTERGLOW, ConnState, EventLine, EventSource, TrailPoint, power_label,
};

// Canvas bounds. The remote's physical aspect (~136×35mm) is closer to 4:1
// than 3:1, but a 3:1 plot reads better at the terminal cell aspect.
const W: f64 = 100.0;
const H: f64 = 300.0;
/// Ellipse axis scale: semi-axis (canvas units) = `major_or_minor * SCALE`.
/// `0.8 * radius / 256.0` ≈ 0.0938 — a mid-swipe contact (major ≈ 0x70,
/// minor ≈ 0x60) spans roughly a quarter of the touchpad ring.
const TOUCH_ELLIPSE_AXIS_SCALE: f64 = 0.8 * 30.0 / 256.0;

const MIN_COLS: u16 = 70;
const MIN_ROWS: u16 = 24;

/// Minimum width (in cells) for the remote column. Below this the silhouette
/// degrades into an unreadable mess.
const REMOTE_MIN_COLS: u16 = 18;
/// Minimum width (in cells) reserved for the side panel.
const PANEL_MIN_COLS: u16 = 40;

// --- Public entry -----------------------------------------------------------

pub fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    if area.width < MIN_COLS || area.height < MIN_ROWS {
        let p = Paragraph::new(format!(
            "Terminal too small. Need at least {MIN_COLS} cols × {MIN_ROWS} rows; \
             have {}×{}.",
            area.width, area.height
        ))
        .wrap(Wrap { trim: true });
        frame.render_widget(p, area);
        return;
    }

    // Scale the remote column to keep the canvas's 1:3 (W:H) aspect.
    // Braille marker dots are roughly square (2 dots/cell horizontal,
    // 4 dots/cell vertical at ~1:2 cell aspect), so for an inner area of
    // `cols × rows` cells the canvas reads as 1:3 when `rows == cols * 3/2`.
    // Solving for cols given height H gives `cols = (H − 2) * 2 / 3 + 2`.
    let desired_remote = area.height.saturating_sub(2) * 2 / 3 + 2;
    let max_remote = area.width.saturating_sub(PANEL_MIN_COLS);
    let remote_cols = desired_remote.clamp(REMOTE_MIN_COLS, max_remote.max(REMOTE_MIN_COLS));
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(remote_cols), Constraint::Min(PANEL_MIN_COLS)])
        .split(area);

    draw_remote(frame, chunks[0], state);
    draw_panel(frame, chunks[1], state);
}

// --- Remote canvas ----------------------------------------------------------

fn draw_remote(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let title: Vec<Span<'_>> = if state.is_calibrating() {
        vec![
            Span::styled("Remote ", Style::default().fg(Color::Gray)),
            Span::styled(
                "— CALIBRATING",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    } else {
        vec![Span::styled("Remote", Style::default().fg(Color::Gray))]
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Inscribe the largest rect satisfying `rows == cols * 3/2` inside `inner`
    // and center it. Without this, the clamps in `draw` (very tall narrow or
    // very short wide terminals) would distort the silhouette.
    let canvas_area = inscribe_remote_canvas(inner);

    let canvas = Canvas::default()
        .x_bounds([0.0, W])
        .y_bounds([0.0, H])
        .marker(Marker::Braille)
        .paint(move |ctx| paint_remote(ctx, state));

    frame.render_widget(canvas, canvas_area);
}

/// Return the largest sub-rect of `inner` whose cell aspect renders the
/// canvas's 1:3 (W:H) bounds without distortion, centered within `inner`.
fn inscribe_remote_canvas(inner: Rect) -> Rect {
    if inner.width == 0 || inner.height == 0 {
        return inner;
    }
    // Want rows == cols * 3 / 2, bounded by inner.
    let height_from_width = inner.width.saturating_mul(3) / 2;
    let (cols, rows) = if height_from_width <= inner.height {
        (inner.width, height_from_width)
    } else {
        let width_from_height = inner.height.saturating_mul(2) / 3;
        (width_from_height, inner.height)
    };
    let x = inner.x + (inner.width.saturating_sub(cols)) / 2;
    let y = inner.y + (inner.height.saturating_sub(rows)) / 2;
    Rect::new(x, y, cols, rows)
}

fn paint_remote(ctx: &mut Context<'_>, state: &AppState) {
    let body = Color::DarkGray;
    let now = Instant::now();

    // 1. Body silhouette (rounded rect approximated with four edges + four
    //    corner arcs drawn as small circles).
    draw_body(ctx, body);
    ctx.layer();

    // 2. Top edge: power glyph (top-right) + mic hole (top-center).
    draw_power(ctx, state, now);
    draw_mic_hole(ctx);
    ctx.layer();

    // 3. Right edge: Siri/mic pill.
    draw_siri_pill(ctx, state, now);
    ctx.layer();

    // 4. Touchpad ring + directional hints.
    draw_touchpad(ctx, state, now);
    ctx.layer();

    // 5. Touch trail.
    draw_trail(ctx, &state.touch_trail, now);
    ctx.layer();

    // 5a. Current-frame contact ellipses (per active slot).
    draw_contact_ellipses(ctx, state);
    ctx.layer();

    // 5b. Running calibration bounds overlay (firmware-space rectangle
    //     projected onto the touchpad disc).
    draw_calibration_overlay(ctx, state);
    ctx.layer();

    // 6. Button cluster below touchpad.
    draw_back(ctx, state, now);
    draw_tv(ctx, state, now);
    draw_play_pause(ctx, state, now);
    draw_volume(ctx, state, now);
    draw_mute(ctx, state, now);
}

fn draw_body(ctx: &mut Context<'_>, color: Color) {
    // Body rectangle 12..88 × 5..295, with corners softened by overlaid
    // small circles.
    ctx.draw(&Rectangle {
        x: 12.0,
        y: 5.0,
        width: 76.0,
        height: 290.0,
        color,
    });
    for (cx, cy) in [(12.0, 5.0), (88.0, 5.0), (12.0, 295.0), (88.0, 295.0)] {
        ctx.draw(&Circle {
            x: cx,
            y: cy,
            radius: 2.0,
            color,
        });
    }
}

fn draw_power(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0010, now);
    let cx = 75.0;
    let cy = 285.0;
    ctx.draw(&Circle {
        x: cx,
        y: cy,
        radius: 6.0,
        color,
    });
    // ⏻ glyph: vertical bar through top.
    ctx.draw(&CanvasLine {
        x1: cx,
        y1: cy + 3.0,
        x2: cx,
        y2: cy + 9.0,
        color,
    });
}

fn draw_mic_hole(ctx: &mut Context<'_>) {
    ctx.draw(&Points {
        coords: &[(50.0, 292.0)],
        color: Color::Gray,
    });
}

fn draw_siri_pill(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0020, now);
    // Vertical pill on the right edge.
    ctx.draw(&Rectangle {
        x: 88.0,
        y: 210.0,
        width: 2.0,
        height: 40.0,
        color,
    });
    // Round the ends.
    ctx.draw(&Circle {
        x: 89.0,
        y: 210.0,
        radius: 1.0,
        color,
    });
    ctx.draw(&Circle {
        x: 89.0,
        y: 250.0,
        radius: 1.0,
        color,
    });
}

fn draw_touchpad(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let center_x = 50.0;
    let center_y = 230.0;
    let radius = 30.0;

    // Outer ring color emphasizes when the clickpad center (0x0008 Select)
    // or any of the directional clicks light up.
    let select = style_for_bit(state, 0x0008, now);
    let ring_color = if select != Color::White {
        select
    } else {
        Color::Gray
    };
    ctx.draw(&Circle {
        x: center_x,
        y: center_y,
        radius,
        color: ring_color,
    });

    // Directional click hints (Up/Down/Left/Right) drawn just inside the
    // bezel, lit when their mask bits are set or in afterglow.
    let cardinals: [(u16, f64, f64); 4] = [
        (0x0200, center_x, center_y + radius - 4.0), // Up
        (0x0400, center_x, center_y - radius + 4.0), // Down
        (0x0800, center_x - radius + 4.0, center_y), // Left
        (0x1000, center_x + radius - 4.0, center_y), // Right
    ];
    for (bit, x, y) in cardinals {
        let c = style_for_bit(state, bit, now);
        ctx.draw(&Points {
            coords: &[(x, y)],
            color: c,
        });
    }
}

/// Render the touch trail as multiple `Points` layers, one per fade bucket.
/// Ratatui's `Points` shape carries a single color, so trail samples are
/// grouped by age (4 buckets, ~25% intensity step).
fn draw_trail(ctx: &mut Context<'_>, trail: &std::collections::VecDeque<TrailPoint>, now: Instant) {
    let center_x = 50.0;
    let center_y = 230.0;
    let radius = 30.0;
    if trail.is_empty() {
        return;
    }
    let max_age = Duration::from_millis(600);
    // Per-slot per-bucket coordinate lists. Slot 1 = cyan-leaning, slot 2 =
    // magenta-leaning. 4 buckets (newest → oldest).
    let mut buckets: [[Vec<(f64, f64)>; 4]; 2] = Default::default();
    for p in trail {
        let age = now.saturating_duration_since(p.stamp);
        if age > max_age {
            continue;
        }
        let frac = age.as_secs_f64() / max_age.as_secs_f64();
        let bucket = (frac * 4.0).clamp(0.0, 3.999) as usize;
        let slot_idx = (p.slot as usize).saturating_sub(1).min(1);
        // Map normalized 0..=1 → canvas coords across the touchpad disc.
        let cx = center_x + (p.x - 0.5) * 2.0 * radius;
        let cy = center_y + (p.y - 0.5) * 2.0 * radius;
        buckets[slot_idx][bucket].push((cx, cy));
    }

    // Render older buckets first so the freshest layer ends up on top.
    let slot_colors: [(Color, Color, Color, Color); 2] = [
        (
            Color::Cyan,
            Color::LightCyan,
            Color::White,
            Color::White,
        ),
        (
            Color::Magenta,
            Color::LightMagenta,
            Color::White,
            Color::White,
        ),
    ];
    for slot_idx in 0..2 {
        for bucket in (0..4).rev() {
            if buckets[slot_idx][bucket].is_empty() {
                continue;
            }
            // bucket 3 = oldest, 0 = newest. Pick a dimmer color for old.
            let color = match bucket {
                3 => Color::DarkGray,
                2 => slot_colors[slot_idx].0,
                1 => slot_colors[slot_idx].1,
                _ => slot_colors[slot_idx].2,
            };
            ctx.draw(&Points {
                coords: &buckets[slot_idx][bucket],
                color,
            });
        }
    }

    // Current finger dots — drawn fatter (3x3 stamp) so they stand out.
    if let Some(last) = trail.back() {
        let cx = center_x + (last.x - 0.5) * 2.0 * radius;
        let cy = center_y + (last.y - 0.5) * 2.0 * radius;
        let dot = [
            (cx, cy),
            (cx - 1.0, cy),
            (cx + 1.0, cy),
            (cx, cy - 1.0),
            (cx, cy + 1.0),
        ];
        let color = if last.slot == 2 {
            Color::LightMagenta
        } else {
            Color::LightCyan
        };
        ctx.draw(&Points {
            coords: &dot,
            color,
        });
    }
}

/// Render the current-frame contact ellipse for each active slot.
///
/// Sourced from `state.last_touch` so the ellipse reflects this frame's
/// major/minor/angle, independent of the historical trail. Slot 1 →
/// `LightCyan`, slot 2 → `LightMagenta`; hovering slots recolor to
/// `DarkGray` so the "near-but-not-touching" state is visually distinct.
fn draw_contact_ellipses(ctx: &mut Context<'_>, state: &AppState) {
    let Some(touch) = state.last_touch.as_ref() else {
        return;
    };
    let center_x = 50.0_f64;
    let center_y = 230.0_f64;
    let radius = 30.0_f64;

    const SAMPLES: usize = 32;
    let slot_colors = [Color::LightCyan, Color::LightMagenta];

    for (idx, slot) in touch.points.iter().enumerate() {
        let Some(f) = slot else { continue };
        let Some((nx, ny)) = crate::view::state::normalize_finger(f, &state.calibration)
        else {
            continue;
        };
        let cx = center_x + (nx - 0.5) * 2.0 * radius;
        let cy = center_y + (ny - 0.5) * 2.0 * radius;
        let a = f64::from(f.major) * TOUCH_ELLIPSE_AXIS_SCALE;
        let b = f64::from(f.minor) * TOUCH_ELLIPSE_AXIS_SCALE;
        let theta = f64::from(f.angle_deg()).to_radians();
        let (sin_t, cos_t) = theta.sin_cos();

        let mut coords: [(f64, f64); SAMPLES] = [(0.0, 0.0); SAMPLES];
        for (k, slot_pt) in coords.iter_mut().enumerate() {
            let u = std::f64::consts::TAU * (k as f64) / (SAMPLES as f64);
            let (sin_u, cos_u) = u.sin_cos();
            let dx = a * cos_u * cos_t - b * sin_u * sin_t;
            let dy = a * cos_u * sin_t + b * sin_u * cos_t;
            *slot_pt = (cx + dx, cy + dy);
        }
        let color = if f.hover {
            Color::DarkGray
        } else {
            slot_colors[idx.min(1)]
        };
        ctx.draw(&Points {
            coords: &coords,
            color,
        });
    }
}

/// While calibrating, draw a yellow rectangle over the touchpad disc
/// showing how much of the pad the running session has covered. Both
/// axes are linear, so the rectangle spans the running `(x_min, x_max)`
/// and `(y_min, y_max)`. Projection uses the default calibration so the
/// overlay aligns with the trail (which is also rendered via the
/// default mapping during calibration).
fn draw_calibration_overlay(ctx: &mut Context<'_>, state: &AppState) {
    use crate::decoder::FingerData;
    use crate::view::state::{Calibration, normalize_finger};

    let Some(session) = state.calibration_session() else {
        return;
    };
    if session.samples == 0
        || session.x_max <= session.x_min
        || session.y_max <= session.y_min
    {
        return;
    }

    let center_x = 50.0_f64;
    let center_y = 230.0_f64;
    let radius = 30.0_f64;
    let default_cal = Calibration::default();

    let project = |x: i16, y: i16| -> Option<(f64, f64)> {
        let f = FingerData {
            x,
            y,
            major: 0x20,
            minor: 0x20,
            pressure: 0x20,
            flags: 0,
            hover: false,
            angle_idx: 0,
        };
        normalize_finger(&f, &default_cal).map(|(nx, ny)| {
            (
                center_x + (nx - 0.5) * 2.0 * radius,
                center_y + (ny - 0.5) * 2.0 * radius,
            )
        })
    };

    let (Some((x_left, y_bot)), Some((x_right, y_top))) = (
        project(session.x_min as i16, session.y_min as i16),
        project(session.x_max as i16, session.y_max as i16),
    ) else {
        return;
    };

    let x = x_left.min(x_right);
    let y = y_top.min(y_bot);
    let width = (x_right - x_left).abs();
    let height = (y_bot - y_top).abs();
    if width < 0.5 || height < 0.5 {
        return;
    }
    ctx.draw(&Rectangle {
        x,
        y,
        width,
        height,
        color: Color::Yellow,
    });
}

fn draw_back(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0040, now);
    let cx = 30.0;
    let cy = 180.0;
    ctx.draw(&Circle {
        x: cx,
        y: cy,
        radius: 13.5,
        color,
    });
    // ‹ glyph: two diagonal lines.
    ctx.draw(&CanvasLine {
        x1: cx + 3.75,
        y1: cy + 6.0,
        x2: cx - 3.75,
        y2: cy,
        color,
    });
    ctx.draw(&CanvasLine {
        x1: cx - 3.75,
        y1: cy,
        x2: cx + 3.75,
        y2: cy - 6.0,
        color,
    });
}

fn draw_tv(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0001, now);
    let cx = 70.0;
    let cy = 180.0;
    ctx.draw(&Circle {
        x: cx,
        y: cy,
        radius: 13.5,
        color,
    });
    // ▢ glyph: small inner square.
    ctx.draw(&Rectangle {
        x: cx - 6.0,
        y: cy - 4.5,
        width: 12.0,
        height: 9.0,
        color,
    });
}

fn draw_play_pause(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0100, now);
    let cx = 30.0;
    let cy = 140.0;
    ctx.draw(&Circle {
        x: cx,
        y: cy,
        radius: 13.5,
        color,
    });
    // ⏸ bars.
    ctx.draw(&CanvasLine {
        x1: cx - 3.75,
        y1: cy - 4.5,
        x2: cx - 3.75,
        y2: cy + 4.5,
        color,
    });
    ctx.draw(&CanvasLine {
        x1: cx + 3.75,
        y1: cy - 4.5,
        x2: cx + 3.75,
        y2: cy + 4.5,
        color,
    });
}

fn draw_volume(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let up = style_for_bit(state, 0x0002, now);
    let dn = style_for_bit(state, 0x0004, now);

    // Volume pill — caps at row 2 (+) and row 3 (−). Outline is drawn
    // directly (semicircle arcs + side lines + divider) so the cap and body
    // share an unbroken silhouette; using full `Circle` + `Rectangle`
    // outlines would leave visible seams where the cap rim crosses the
    // rectangle edge.
    let cx = 70.0_f64;
    let r = 13.5_f64;
    let top_cy = 140.0_f64; // + cap center (row 2)
    let bot_cy = 100.0_f64; // − cap center (row 3)
    let mid_y = (top_cy + bot_cy) / 2.0;

    // Straight sides, split at the divider so each half tracks its own bit.
    ctx.draw(&CanvasLine {
        x1: cx - r,
        y1: mid_y,
        x2: cx - r,
        y2: top_cy,
        color: up,
    });
    ctx.draw(&CanvasLine {
        x1: cx + r,
        y1: mid_y,
        x2: cx + r,
        y2: top_cy,
        color: up,
    });
    ctx.draw(&CanvasLine {
        x1: cx - r,
        y1: bot_cy,
        x2: cx - r,
        y2: mid_y,
        color: dn,
    });
    ctx.draw(&CanvasLine {
        x1: cx + r,
        y1: bot_cy,
        x2: cx + r,
        y2: mid_y,
        color: dn,
    });

    // Divider between + and − halves.
    ctx.draw(&CanvasLine {
        x1: cx - r,
        y1: mid_y,
        x2: cx + r,
        y2: mid_y,
        color: up,
    });

    // Cap arcs: upper semicircle for the + cap, lower semicircle for −.
    // Sampling the outline manually means the inner half of each cap
    // (which would otherwise cross the body) simply isn't drawn.
    const ARC_SAMPLES: usize = 48;
    let mut top_arc = [(0.0_f64, 0.0_f64); ARC_SAMPLES];
    for (k, p) in top_arc.iter_mut().enumerate() {
        let theta = std::f64::consts::PI * (k as f64) / ((ARC_SAMPLES - 1) as f64);
        *p = (cx + r * theta.cos(), top_cy + r * theta.sin());
    }
    ctx.draw(&Points {
        coords: &top_arc,
        color: up,
    });
    let mut bot_arc = [(0.0_f64, 0.0_f64); ARC_SAMPLES];
    for (k, p) in bot_arc.iter_mut().enumerate() {
        let theta = std::f64::consts::PI * (1.0 + (k as f64) / ((ARC_SAMPLES - 1) as f64));
        *p = (cx + r * theta.cos(), bot_cy + r * theta.sin());
    }
    ctx.draw(&Points {
        coords: &bot_arc,
        color: dn,
    });

    // + glyph centered on the upper cap.
    ctx.draw(&CanvasLine {
        x1: cx - 6.0,
        y1: top_cy,
        x2: cx + 6.0,
        y2: top_cy,
        color: up,
    });
    ctx.draw(&CanvasLine {
        x1: cx,
        y1: top_cy - 6.0,
        x2: cx,
        y2: top_cy + 6.0,
        color: up,
    });
    // − glyph centered on the lower cap.
    ctx.draw(&CanvasLine {
        x1: cx - 6.0,
        y1: bot_cy,
        x2: cx + 6.0,
        y2: bot_cy,
        color: dn,
    });
}

fn draw_mute(ctx: &mut Context<'_>, state: &AppState, now: Instant) {
    let color = style_for_bit(state, 0x0080, now);
    let cx = 30.0;
    let cy = 100.0;
    ctx.draw(&Circle {
        x: cx,
        y: cy,
        radius: 13.5,
        color,
    });
    // Speaker triangle (approximated by a single line) + strike-through.
    ctx.draw(&CanvasLine {
        x1: cx - 4.5,
        y1: cy,
        x2: cx + 4.5,
        y2: cy - 3.0,
        color,
    });
    ctx.draw(&CanvasLine {
        x1: cx - 6.75,
        y1: cy + 6.75,
        x2: cx + 6.75,
        y2: cy - 6.75,
        color,
    });
}

/// Resolve the on-screen color for one button-mask bit, accounting for
/// the held / afterglow state.
///
/// - Held (bit in `buttons_mask`) → bright yellow.
/// - Recently released (within [`BUTTON_AFTERGLOW`]) → dimmer yellow.
/// - Otherwise → plain white (idle bezel).
fn style_for_bit(state: &AppState, bit: u16, now: Instant) -> Color {
    if state.buttons_mask & bit != 0 {
        return Color::Yellow;
    }
    if let Some(deadline) = state.button_afterglow.get(&bit) {
        if let Some(remaining) = deadline.checked_duration_since(now) {
            let frac = remaining.as_secs_f64() / BUTTON_AFTERGLOW.as_secs_f64();
            if frac > 0.66 {
                return Color::LightYellow;
            }
            if frac > 0.33 {
                return Color::Yellow;
            }
            return Color::DarkGray;
        }
    }
    Color::White
}

// --- Side panel -------------------------------------------------------------

fn draw_panel(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(5),
        ])
        .split(area);

    draw_status(frame, chunks[0], state);
    draw_touch_readout(frame, chunks[1], state);
    draw_events(frame, chunks[2], state);
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().borders(Borders::ALL).title("Status");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (conn_label, conn_style) = match &state.connection {
        ConnState::Connecting { since } => (
            format!("connecting… ({})", short_elapsed(since.elapsed())),
            Style::default().fg(Color::Yellow),
        ),
        ConnState::Connected { since } => (
            format!("connected ({})", short_elapsed(since.elapsed())),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ConnState::Reconnecting { reason, since } => (
            format!(
                "reconnecting… {} ({})",
                reason,
                short_elapsed(since.elapsed())
            ),
            Style::default().fg(Color::Red),
        ),
        ConnState::Pairing { since } => (
            format!("pairing-mode scan ({})", short_elapsed(since.elapsed())),
            Style::default().fg(Color::Magenta),
        ),
    };

    let rssi = state
        .selection
        .rssi
        .map(|r| format!("rssi={r} "))
        .unwrap_or_default();
    let battery = match state.battery {
        Some(v) => format!("battery={v}% {}", battery_bar(v)),
        None => "battery=?".to_string(),
    };
    let power = match state.power {
        Some(p) => format!("power={}", power_label(p)),
        None => "power=?".to_string(),
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("status: ", Style::default().fg(Color::Gray)),
            Span::styled(conn_label, conn_style),
        ]),
        Line::from(format!(
            "name={:?} addr={}",
            state.selection.name, state.selection.address
        )),
        Line::from(format!("{rssi}{battery}")),
        Line::from(power),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn battery_bar(pct: u8) -> String {
    let pct = pct.min(100);
    let cells = 10usize;
    let filled = (pct as usize * cells + 50) / 100;
    let mut s = String::with_capacity(cells + 2);
    s.push('[');
    for i in 0..cells {
        s.push(if i < filled { '█' } else { '·' });
    }
    s.push(']');
    s
}

fn short_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn draw_touch_readout(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    if let Some(session) = state.calibration_session() {
        draw_calibration_readout(frame, area, state, session);
        return;
    }
    let block = Block::default().borders(Borders::ALL).title("Touch / Buttons");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let held = pretty_button_list(state.buttons_mask);
    lines.push(Line::from(vec![
        Span::styled("buttons: ", Style::default().fg(Color::Gray)),
        Span::styled(
            held,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    match &state.last_touch {
        Some(t) => {
            let fingers = t.finger_count();
            lines.push(Line::from(format!(
                "touch: fingers={fingers} seq=0x{:04X} header=0x{:02X}",
                t.seq, t.header
            )));
            for (idx, slot) in t.points.iter().enumerate() {
                if let Some(f) = slot {
                    lines.push(Line::from(format!(
                        "  slot {}: x={} y={} pressure=0x{:02X} flags=0x{:02X} \
                         hover={} angle={:.1}° major={} minor={}",
                        idx + 1,
                        f.x,
                        f.y,
                        f.pressure,
                        f.flags,
                        f.hover,
                        f.angle_deg(),
                        f.major,
                        f.minor,
                    )));
                }
            }
        }
        None => lines.push(Line::from("touch: idle")),
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_calibration_readout(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    session: &crate::view::state::CalibrationSession,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            "Calibration",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let saved = state.calibration;
    let (x_range, y_range, samples) = if session.samples == 0 {
        (
            "[--..--]".to_string(),
            "[--..--]".to_string(),
            0usize,
        )
    } else {
        (
            format!("[{}..{}]", session.x_min, session.x_max),
            format!("[{}..{}]", session.y_min, session.y_max),
            session.samples,
        )
    };
    let lines = vec![
        Line::from(Span::styled(
            "trace a circle on the touchpad",
            Style::default().fg(Color::Yellow),
        )),
        Line::from("press c to finish · Esc to cancel"),
        Line::from(format!("x: {x_range}  y: {y_range}  samples={samples}")),
        Line::from(format!(
            "saved: x=[{}..{}] y=[{}..{}]",
            saved.x_min, saved.x_max, saved.y_min, saved.y_max,
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn pretty_button_list(mask: u16) -> String {
    if mask == 0 {
        return "—".to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    for (bit, name) in BUTTON_NAMES {
        if mask & bit != 0 {
            parts.push(name);
        }
    }
    parts.join(" + ")
}

fn draw_events(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default().borders(Borders::ALL).title("Events");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Show the most recent N events where N matches the available height.
    let capacity = inner.height as usize;
    if capacity == 0 {
        return;
    }
    let start = state.events.len().saturating_sub(capacity);
    let items: Vec<ListItem<'_>> = state
        .events
        .iter()
        .skip(start)
        .map(render_event_line)
        .collect();
    frame.render_widget(List::new(items), inner);
}

fn render_event_line(ev: &EventLine) -> ListItem<'_> {
    let color = match ev.source {
        EventSource::Buttons => Color::Yellow,
        EventSource::Battery => Color::Green,
        EventSource::Power => Color::Green,
        EventSource::Raw => Color::Blue,
        EventSource::System => Color::Magenta,
        EventSource::Warning => Color::LightRed,
    };
    let tag = match ev.source {
        EventSource::Buttons => "buttons",
        EventSource::Battery => "battery",
        EventSource::Power => "power",
        EventSource::Raw => "raw",
        EventSource::System => "system",
        EventSource::Warning => "warn",
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{tag:>7} "), Style::default().fg(color)),
        Span::raw(ev.text.clone()),
    ]))
}

// Power state symbol kept here so the renderer can decorate the status line.
#[allow(dead_code)]
fn power_glyph(state: PowerState) -> &'static str {
    match state {
        PowerState::Charging | PowerState::PluggedIn => "⚡",
        PowerState::Discharging => "🔋",
        PowerState::Unknown(_) => "?",
    }
}
