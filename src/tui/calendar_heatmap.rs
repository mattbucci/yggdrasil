//! Calendar heatmap of run terminals (yggdrasil-171). Day × hour grid
//! coloured by failure rate over the last 7 days. Surfaces temporal
//! anomalies — "Tuesday morning flake" — that linear logs hide.
//!
//! Rendered as a 24×7 cell grid where row = day-of-week (Mon→Sun) and
//! column = hour-of-day (0→23). Cells are colored by the fraction of
//! `task_runs` that finished in a *failure-shaped* state during that
//! hour:
//!
//!   no runs       → space (the column is dim — quiet hours sit empty)
//!   0% failure    → green block
//!   < 25%         → yellow-green
//!   < 50%         → yellow
//!   < 75%         → orange
//!   ≥ 75%         → red

use crate::db::DbPool;
use ratatui::layout::Rect;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

/// One bucketed sample. `total` is the run count in this (day, hour);
/// `failed` is how many of those landed in a failure terminal state.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeatCell {
    pub total: u32,
    pub failed: u32,
}

impl HeatCell {
    pub fn fail_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.failed as f64 / self.total as f64
        }
    }
}

/// 7 (Mon..Sun) × 24 (00..23) grid.
pub type HeatGrid = [[HeatCell; 24]; 7];

pub struct CalendarHeatmap {
    pub grid: HeatGrid,
    pub loaded: bool,
    pub last_status: String,
}

impl Default for CalendarHeatmap {
    fn default() -> Self {
        Self::new()
    }
}

impl CalendarHeatmap {
    pub fn new() -> Self {
        Self {
            grid: [[HeatCell::default(); 24]; 7],
            loaded: false,
            last_status: String::new(),
        }
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        self.loaded = true;
        // dow (strftime('%w')) returns 0=Sunday, 1=Monday, …, 6=Saturday.
        // Calendar convention here is Mon=0..Sun=6, so remap below.
        let rows: Vec<(i32, i32, i64, i64)> = sqlx::query_as(
            r#"SELECT
                 CAST(strftime('%w', ended_at) AS INTEGER) AS dow,
                 CAST(strftime('%H', ended_at) AS INTEGER) AS hour,
                 COUNT(*)                  AS total,
                 COUNT(*) FILTER (WHERE state IN ('failed','crashed','poison')) AS failed
               FROM task_runs
               WHERE ended_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-7 days')
                 AND ended_at IS NOT NULL
               GROUP BY dow, hour"#,
        )
        .fetch_all(pool)
        .await?;

        let mut grid: HeatGrid = [[HeatCell::default(); 24]; 7];
        for (dow_pg, hour, total, failed) in rows {
            // dow: 0=Sun … 6=Sat → calendar row Mon=0..Sun=6.
            let calendar_row = match dow_pg {
                0 => 6,                // Sun
                d => (d - 1) as usize, // Mon..Sat → 0..5
            };
            if calendar_row >= 7 || !(0..24).contains(&hour) {
                continue;
            }
            grid[calendar_row][hour as usize] = HeatCell {
                total: total.max(0) as u32,
                failed: failed.max(0) as u32,
            };
        }
        self.grid = grid;
        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::with_capacity(8);
        // Header row: " hour  00 01 02 ... 23"
        let mut header = vec![Span::styled("      ", Style::default().fg(Color::DarkGray))];
        for h in 0..24 {
            header.push(Span::styled(
                format!("{h:>2} "),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(header));

        for (row_idx, row) in self.grid.iter().enumerate() {
            let label = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][row_idx];
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(25);
            spans.push(Span::styled(
                format!(" {label}  "),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            ));
            for cell in row.iter() {
                let (glyph, color) = cell_glyph(cell);
                spans.push(Span::styled(
                    format!("{glyph}  "),
                    Style::default().fg(color),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ░ no data   █ green:0%   █ yel-grn:<25%   █ yellow:<50%   █ orange:<75%   █ red:≥75%",
            Style::default().fg(Color::DarkGray),
        )));

        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Run terminals · last 7 days · failure-rate heatmap "),
        );
        frame.render_widget(para, area);
    }
}

/// Map a heat cell to its display glyph + color. Empty cells render as
/// space-tinted dim so the eye drifts past quiet hours.
pub fn cell_glyph(cell: &HeatCell) -> (char, Color) {
    if cell.total == 0 {
        return ('░', Color::DarkGray);
    }
    let f = cell.fail_rate();
    let color = if f <= 0.0 {
        Color::Green
    } else if f < 0.25 {
        Color::LightGreen
    } else if f < 0.5 {
        Color::Yellow
    } else if f < 0.75 {
        Color::LightRed
    } else {
        Color::Red
    };
    ('█', color)
}
