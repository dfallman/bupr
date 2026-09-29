//! The live inline dashboard (spec §8.2): a 9-line ratatui viewport below the
//! prompt; never full-screen, never raw mode (so Ctrl-C stays a signal).

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::engine::{Event, Mode};
use crate::ui::format::{bytes, clock, count, shorten_middle};

pub const HEIGHT: u16 = 9;
pub const MIN_WIDTH: u16 = 50;
const TAU: f64 = 5.0;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Copying,
    Deleting,
    Finished,
}

#[derive(Clone, Debug)]
pub struct DashState {
    pub preset: String,
    pub mode: Mode,
    pub phase: Phase,
    pub scan_files: u64,
    pub scan_bytes: u64,
    pub total_files: u64,
    pub total_bytes: u64,
    pub unchanged: u64,
    pub done_files: u64,
    pub done_bytes: u64,
    pub delete_total: u64,
    pub deleted: u64,
    pub errors: u64,
    /// Newest first, at most three.
    pub recent: VecDeque<(String, u64)>,
    pub elapsed: f64,
    pub speed: f64,
    last_sample: Option<(f64, u64)>,
}

impl DashState {
    pub fn new(preset: &str, mode: Mode) -> DashState {
        DashState {
            preset: preset.to_string(),
            mode,
            phase: Phase::Scanning,
            scan_files: 0,
            scan_bytes: 0,
            total_files: 0,
            total_bytes: 0,
            unchanged: 0,
            done_files: 0,
            done_bytes: 0,
            delete_total: 0,
            deleted: 0,
            errors: 0,
            recent: VecDeque::new(),
            elapsed: 0.0,
            speed: 0.0,
            last_sample: None,
        }
    }

    pub fn apply(&mut self, ev: &Event) {
        match ev {
            Event::Scanning { files, bytes } => {
                self.scan_files = *files;
                self.scan_bytes = *bytes;
            }
            Event::Planned { summary } => {
                self.phase = Phase::Copying;
                self.total_files = summary.totals.copy_files;
                self.total_bytes = summary.totals.copy_bytes;
                self.unchanged = summary.totals.unchanged_files;
            }
            Event::FileStart { path, size } => {
                self.recent.push_front((path.clone(), *size));
                self.recent.truncate(3);
            }
            Event::FileProgress { bytes } => self.done_bytes += bytes,
            Event::FileDone { .. } => self.done_files += 1,
            Event::FileError { .. } => self.errors += 1,
            Event::Deleting { total } => {
                self.phase = Phase::Deleting;
                self.delete_total = *total;
            }
            Event::Deleted { .. } => self.deleted += 1,
            Event::Done { .. } => self.phase = Phase::Finished,
            _ => {}
        }
    }

    /// Update elapsed time and the exponentially smoothed speed.
    pub fn tick(&mut self, elapsed: f64) {
        self.elapsed = elapsed;
        match self.last_sample {
            None => self.last_sample = Some((elapsed, self.done_bytes)),
            Some((t0, b0)) => {
                let dt = elapsed - t0;
                if dt >= 0.2 {
                    let inst = self.done_bytes.saturating_sub(b0) as f64 / dt;
                    self.speed = if self.speed == 0.0 {
                        inst
                    } else {
                        let a = 1.0 - (-dt / TAU).exp();
                        a * inst + (1.0 - a) * self.speed
                    };
                    self.last_sample = Some((elapsed, self.done_bytes));
                }
            }
        }
    }

    pub fn eta_secs(&self) -> Option<u64> {
        if self.elapsed < 2.0 || self.speed <= 0.0 {
            return None;
        }
        Some((self.total_bytes.saturating_sub(self.done_bytes) as f64 / self.speed).ceil() as u64)
    }

    pub fn ratio(&self) -> f64 {
        let r = match self.phase {
            Phase::Deleting if self.delete_total > 0 => {
                self.deleted as f64 / self.delete_total as f64
            }
            _ if self.total_bytes > 0 => self.done_bytes as f64 / self.total_bytes as f64,
            _ if self.total_files > 0 => self.done_files as f64 / self.total_files as f64,
            _ => 1.0,
        };
        r.clamp(0.0, 1.0)
    }
}

pub fn render(s: &DashState, area: Rect, buf: &mut Buffer, color: bool) {
    let st = |style: Style| if color { style } else { Style::default() };
    let dim = st(Style::default().fg(Color::DarkGray));
    let mode = if s.mode == Mode::Simulate {
        " simulate "
    } else {
        " mirror "
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim)
        .padding(Padding::horizontal(1))
        .title(Line::from(format!(" bupr · {} ", s.preset)))
        .title_top(Line::from(mode).right_aligned());
    let inner = block.inner(area);
    block.render(area, buf);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if s.phase == Phase::Scanning {
        let spin = SPINNER[(s.elapsed * 10.0) as usize % SPINNER.len()];
        lines.push(Line::from(format!(
            "{spin} scanning… {} files · {}",
            count(s.scan_files),
            bytes(s.scan_bytes)
        )));
    } else {
        let ratio = s.ratio();
        let right = if s.phase == Phase::Deleting {
            format!("deleting {}/{}", count(s.deleted), count(s.delete_total))
        } else {
            format!("{} / {}", bytes(s.done_bytes), bytes(s.total_bytes))
        };
        let suffix = format!(" {:>3}%   {right}", (ratio * 100.0).floor() as u64);
        let bar_w = w.saturating_sub(suffix.chars().count());
        let filled = ((bar_w as f64) * ratio).round() as usize;
        lines.push(Line::from(vec![
            Span::styled("█".repeat(filled), st(Style::default().fg(Color::Cyan))),
            Span::styled("░".repeat(bar_w - filled), dim),
            Span::raw(suffix),
        ]));
        let speed = if s.speed > 0.0 {
            format!("{}/s", bytes(s.speed as u64))
        } else {
            "–".into()
        };
        let eta = s.eta_secs().map_or_else(|| "–".to_string(), clock);
        let (done, total, elapsed) = (
            count(s.done_files),
            count(s.total_files),
            clock(s.elapsed as u64),
        );
        let full = format!("files {done}/{total}   {speed}   {elapsed} → eta {eta}");
        let stats = if full.chars().count() <= w {
            full
        } else {
            format!("{done}/{total} · {speed} · {elapsed} → {eta}")
        };
        lines.push(Line::from(stats));
        let err_style = if s.errors > 0 {
            st(Style::default().fg(Color::Red))
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(format!(
                "unchanged {}   deleted {}   ",
                count(s.unchanged),
                count(s.deleted)
            )),
            Span::styled(format!("errors {}", count(s.errors)), err_style),
        ]));
        lines.push(Line::from(""));
        for (i, (path, size)) in s.recent.iter().enumerate() {
            let lead = if i == 0 { "▸ " } else { "  " };
            let size_s = bytes(*size);
            let name = shorten_middle(
                path,
                w.saturating_sub(lead.chars().count() + size_s.len() + 2),
            );
            let pad = w.saturating_sub(lead.chars().count() + name.chars().count() + size_s.len());
            lines.push(Line::from(vec![
                Span::raw(lead),
                Span::raw(name),
                Span::raw(" ".repeat(pad)),
                Span::styled(size_s, dim),
            ]));
        }
    }
    Paragraph::new(lines).render(inner, buf);
}

pub struct Dashboard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    state: DashState,
    start: Instant,
    last_draw: Option<Instant>,
    color: bool,
}

impl Dashboard {
    pub fn new(state: DashState, start: Instant, color: bool) -> io::Result<Dashboard> {
        let terminal = Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )?;
        let mut d = Dashboard {
            terminal,
            state,
            start,
            last_draw: None,
            color,
        };
        d.draw(true)?;
        Ok(d)
    }

    fn draw(&mut self, force: bool) -> io::Result<()> {
        if !force
            && self
                .last_draw
                .is_some_and(|t| t.elapsed() < Duration::from_millis(100))
        {
            return Ok(());
        }
        self.state.tick(self.start.elapsed().as_secs_f64());
        let (state, color) = (&self.state, self.color);
        self.terminal
            .draw(|f| render(state, f.area(), f.buffer_mut(), color))?;
        self.last_draw = Some(Instant::now());
        Ok(())
    }

    pub fn event(&mut self, ev: &Event) -> io::Result<()> {
        self.state.apply(ev);
        let force = matches!(
            ev,
            Event::Planned { .. } | Event::Deleting { .. } | Event::Done { .. }
        );
        self.draw(force)
    }

    /// Print a line into the scrollback above the dashboard.
    pub fn note(&mut self, text: &str) -> io::Result<()> {
        let text = text.to_string();
        self.terminal
            .insert_before(1, |buf| Paragraph::new(text).render(buf.area, buf))?;
        self.draw(true)
    }

    /// Clear the viewport and leave the cursor at its top, so ordinary output
    /// (prompts, the summary line) continues where the dashboard was.
    pub fn close(mut self) -> DashState {
        let _ = self.terminal.clear();
        let top = self.terminal.get_frame().area().y;
        let _ = self.terminal.set_cursor_position(Position::new(0, top));
        let _ = self.terminal.show_cursor();
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Event, Mode};
    use ratatui::backend::TestBackend;

    fn copying() -> DashState {
        let mut s = DashState::new("dev", Mode::Run);
        s.phase = Phase::Copying;
        s.total_files = 14_002;
        s.total_bytes = 5_400_000_000;
        s.done_files = 8_214;
        s.done_bytes = 3_100_000_000;
        s.unchanged = 96_110;
        s.deleted = 12;
        s.elapsed = 22.0;
        s.speed = 142_000_000.0;
        for (p, n) in [
            ("player/api/openapi.yaml", 3_000),
            ("player/core/src/decoder.rs", 12_000),
            ("webshop/src/lib/components/Timeline.svelte", 48_000),
        ] {
            s.recent.push_front((p.to_string(), n));
        }
        s
    }

    fn draw(s: &DashState, width: u16) -> String {
        let mut t = ratatui::Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        t.draw(|f| render(s, f.area(), f.buffer_mut(), false))
            .unwrap();
        t.backend().to_string()
    }

    #[test]
    fn renders_the_copying_phase() {
        insta::assert_snapshot!(draw(&copying(), 60));
    }

    #[test]
    fn renders_at_minimum_width() {
        insta::assert_snapshot!(draw(&copying(), MIN_WIDTH));
    }

    #[test]
    fn renders_the_scanning_phase() {
        let mut s = DashState::new("dev", Mode::Simulate);
        s.scan_files = 110_316;
        s.scan_bytes = 12_300_000_000;
        insta::assert_snapshot!(draw(&s, 60));
    }

    #[test]
    fn applies_events() {
        let mut s = DashState::new("dev", Mode::Run);
        s.apply(&Event::Scanning {
            files: 5,
            bytes: 50,
        });
        assert_eq!((s.phase, s.scan_files), (Phase::Scanning, 5));
        let summary: crate::engine::PlanSummary = serde_json::from_str(SUMMARY_JSON).unwrap();
        s.apply(&Event::Planned { summary });
        assert_eq!(
            (s.phase, s.total_files, s.total_bytes, s.unchanged),
            (Phase::Copying, 2, 300, 7)
        );
        s.apply(&Event::FileStart {
            path: "a".into(),
            size: 100,
        });
        s.apply(&Event::FileProgress { bytes: 100 });
        s.apply(&Event::FileDone { path: "a".into() });
        s.apply(&Event::FileError {
            path: "b".into(),
            message: "x".into(),
        });
        assert_eq!((s.done_files, s.done_bytes, s.errors), (1, 100, 1));
        assert_eq!(s.recent.front().unwrap().0, "a");
        s.apply(&Event::Deleting { total: 4 });
        s.apply(&Event::Deleted { path: "z".into() });
        assert_eq!((s.phase, s.deleted, s.ratio()), (Phase::Deleting, 1, 0.25));
    }

    #[test]
    fn speed_smooths_and_eta_waits_two_seconds() {
        let mut s = copying();
        s.speed = 0.0;
        s.elapsed = 0.0;
        s.done_bytes = 0;
        s.tick(0.0);
        s.done_bytes = 10_000_000;
        s.tick(1.0);
        assert!(
            (s.speed - 10_000_000.0).abs() < 1.0,
            "first sample seeds the average"
        );
        assert_eq!(s.eta_secs(), None);
        s.done_bytes = 30_000_000;
        s.tick(2.0);
        assert!(
            s.speed > 10_000_000.0 && s.speed < 20_000_000.0,
            "{}",
            s.speed
        );
        assert!(s.eta_secs().is_some());
    }

    const SUMMARY_JSON: &str = r#"{"source":"/s","dest":"/d","totals":{"copy_files":2,"copy_bytes":300,
        "replaced_bytes":0,"unchanged_files":7,"unchanged_bytes":0,"delete_entries":0,"delete_bytes":0},
        "marker":{"status":"fresh"},"free_bytes":null,"needed_bytes":300,"over_delete_limit":false,
        "insufficient_space":false,"secret_files":0,"largest_deletes":[],"skipped_special":0,
        "skipped_mounts":0,"collisions":0,"scan_errors":0,"case_insensitive":true}"#;
}
