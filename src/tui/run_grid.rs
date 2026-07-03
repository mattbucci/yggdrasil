//! Run-grid pane (yggdrasil-146). Airflow's grid view applied to
//! `task_runs`: rows are tasks with recent activity, columns are recent
//! attempts colored by state. The densest possible failure-surfacing for
//! a multi-attempt orchestrator.
//!
//! Toggle via `[G]` from any other view. Read-only — interactive
//! requeue/cancel land on the existing Runs pane.

use std::collections::BTreeMap;

use crate::db::DbPool;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

/// One cell in the grid: either a recorded attempt or an empty slot
/// (the task has fewer than `MAX_ATTEMPT_COLS` recorded runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptCell {
    Empty,
    Run { attempt: i32, state: GridState },
}

/// Compact mirror of `RunState` keyed for rendering. Kept narrow on
/// purpose — the grid is a glance view; the Runs pane carries detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GridState {
    Scheduled,
    Ready,
    Running,
    Succeeded,
    Failed,
    Crashed,
    Cancelled,
    Retrying,
    Poison,
}

impl GridState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "scheduled" => Self::Scheduled,
            "ready" => Self::Ready,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "crashed" => Self::Crashed,
            "cancelled" => Self::Cancelled,
            "retrying" => Self::Retrying,
            "poison" => Self::Poison,
            _ => return None,
        })
    }

    /// Glyph + color for the cell. Keep glyphs single-width so the grid
    /// stays a strict column lattice.
    pub fn style(self) -> (char, Color) {
        match self {
            Self::Scheduled => ('░', Color::DarkGray),
            Self::Ready => ('▒', Color::Blue),
            Self::Running => ('▶', Color::Cyan),
            Self::Succeeded => ('✓', Color::Green),
            Self::Failed => ('✗', Color::Red),
            Self::Crashed => ('!', Color::Red),
            Self::Cancelled => ('-', Color::DarkGray),
            Self::Retrying => ('↻', Color::Yellow),
            Self::Poison => ('☠', Color::Magenta),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GridRow {
    pub task_ref: String,
    pub title: String,
    /// Most-recent first. Length always `MAX_ATTEMPT_COLS`; missing
    /// attempts are `AttemptCell::Empty`.
    pub attempts: [AttemptCell; MAX_ATTEMPT_COLS],
}

/// How many recent attempts to render per task. Eight is wide enough to
/// see retry storms without overflowing typical widths.
pub const MAX_ATTEMPT_COLS: usize = 8;

/// How many tasks to surface. Tasks are ordered by most-recent attempt
/// timestamp DESC so live work floats to the top.
pub const MAX_TASK_ROWS: usize = 30;

pub struct RunGridView {
    pub rows: Vec<GridRow>,
    pub state: TableState,
    pub last_status: String,
    pub loaded: bool,
}

impl Default for RunGridView {
    fn default() -> Self {
        Self::new()
    }
}

impl RunGridView {
    pub fn new() -> Self {
        let mut st = TableState::default();
        st.select(Some(0));
        Self {
            rows: Vec::new(),
            state: st,
            last_status: String::new(),
            loaded: false,
        }
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1).min(self.rows.len() - 1)));
    }

    pub fn select_prev(&mut self) {
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(i.saturating_sub(1)));
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        self.loaded = true;
        // Pull every recent run for tasks that have at least one row in
        // the last 7 days. ORDER BY task_id, attempt DESC so we can take
        // the first MAX_ATTEMPT_COLS per task in a single pass.
        let rows: Vec<(String, String, i32, String, i32, String)> = sqlx::query_as(
            r#"
            SELECT t.task_id, r.task_prefix, t.seq, t.title, tr.attempt, tr.state
              FROM task_runs tr
              JOIN tasks t ON t.task_id = tr.task_id
              JOIN repos r ON r.repo_id = t.repo_id
             WHERE tr.scheduled_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-7 days')
             ORDER BY t.task_id,
                      tr.attempt DESC
            "#,
        )
        .fetch_all(pool)
        .await?;

        // Group by task_id, pick the most-recent attempt timestamp as
        // the row's sort key (we already ordered by attempt DESC inside
        // each group, so the first row's attempt is the freshest).
        let mut by_task: BTreeMap<String, GridRow> = BTreeMap::new();
        for (task_id, prefix, seq, title, attempt, state_str) in rows {
            let state = GridState::parse(&state_str);
            let entry = by_task.entry(task_id).or_insert_with(|| GridRow {
                task_ref: format!("{prefix}-{seq}"),
                title: title.clone(),
                attempts: [AttemptCell::Empty; MAX_ATTEMPT_COLS],
            });
            // Find the next empty slot left-to-right (= most recent
            // attempt populates index 0 thanks to attempt DESC).
            for slot in entry.attempts.iter_mut() {
                if matches!(slot, AttemptCell::Empty) {
                    if let Some(s) = state {
                        *slot = AttemptCell::Run { attempt, state: s };
                    }
                    break;
                }
            }
        }

        let mut grouped: Vec<GridRow> = by_task.into_values().collect();
        // Tasks with the most-recent activity (highest first-attempt
        // number) sort first. Falls back to task_ref alphabetical for
        // ties, which keeps the row order stable across refreshes for
        // a quiet system.
        grouped.sort_by(|a, b| {
            let a_top = match a.attempts[0] {
                AttemptCell::Run { attempt, .. } => attempt,
                AttemptCell::Empty => 0,
            };
            let b_top = match b.attempts[0] {
                AttemptCell::Run { attempt, .. } => attempt,
                AttemptCell::Empty => 0,
            };
            b_top.cmp(&a_top).then_with(|| a.task_ref.cmp(&b.task_ref))
        });
        grouped.truncate(MAX_TASK_ROWS);

        if grouped.is_empty() {
            self.last_status = "no task_runs in last 7d".into();
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= grouped.len() {
            self.state.select(Some(grouped.len() - 1));
        }
        self.rows = grouped;
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.loaded {
            let p = Paragraph::new(" loading task_runs… ")
                .block(Block::default().borders(Borders::ALL).title(" Run grid "));
            frame.render_widget(p, area);
            return;
        }
        if self.rows.is_empty() {
            let p = Paragraph::new(format!(" {} ", self.last_status))
                .block(Block::default().borders(Borders::ALL).title(" Run grid "));
            frame.render_widget(p, area);
            return;
        }

        // Split area: table on top, single-line legend on the bottom.
        // The table's Block carries its own borders, so the legend
        // line sits flush below the bottom border.
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(3),
                ratatui::layout::Constraint::Length(1),
            ])
            .split(area);
        let table_area = chunks[0];
        let legend_area = chunks[1];

        let header = {
            let mut cells: Vec<Cell> = vec![
                Cell::from("task").style(Style::default().fg(Color::DarkGray)),
                Cell::from("title").style(Style::default().fg(Color::DarkGray)),
            ];
            for i in 0..MAX_ATTEMPT_COLS {
                cells.push(
                    Cell::from(format!("#{}", i + 1)).style(Style::default().fg(Color::DarkGray)),
                );
            }
            Row::new(cells)
        };

        let body: Vec<Row> = self
            .rows
            .iter()
            .map(|r| {
                let mut cells: Vec<Cell> = vec![
                    Cell::from(r.task_ref.clone())
                        .style(Style::default().add_modifier(Modifier::BOLD)),
                    Cell::from(truncate(&r.title, 36)),
                ];
                for cell in &r.attempts {
                    let (glyph, color) = match cell {
                        AttemptCell::Empty => (' ', Color::DarkGray),
                        AttemptCell::Run { state, .. } => state.style(),
                    };
                    cells.push(Cell::from(glyph.to_string()).style(Style::default().fg(color)));
                }
                Row::new(cells)
            })
            .collect();

        let mut widths: Vec<Constraint> = vec![Constraint::Length(20), Constraint::Length(38)];
        for _ in 0..MAX_ATTEMPT_COLS {
            widths.push(Constraint::Length(2));
        }

        let table = Table::new(body, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Run grid · {} task(s) · 7d ", self.rows.len())),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, table_area, &mut self.state);
        frame.render_widget(legend_paragraph(), legend_area);
    }
}

/// Glyph + label pairs — single source of truth for the legend so the
/// chrome and the GridState::style() picker can't drift.
pub const LEGEND_ENTRIES: &[(GridState, &str)] = &[
    (GridState::Succeeded, "ok"),
    (GridState::Retrying, "retry"),
    (GridState::Failed, "fail"),
    (GridState::Crashed, "crash"),
    (GridState::Running, "run"),
    (GridState::Ready, "ready"),
    (GridState::Scheduled, "queued"),
    (GridState::Cancelled, "cancel"),
    (GridState::Poison, "poison"),
];

fn legend_paragraph() -> Paragraph<'static> {
    let mut spans: Vec<ratatui::text::Span<'static>> = vec![ratatui::text::Span::styled(
        " legend ",
        Style::default().fg(Color::DarkGray),
    )];
    for (state, label) in LEGEND_ENTRIES {
        let (glyph, color) = state.style();
        spans.push(ratatui::text::Span::styled(
            format!("{glyph} "),
            Style::default().fg(color),
        ));
        spans.push(ratatui::text::Span::styled(
            format!("{label}  "),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(ratatui::text::Span::styled(
        "  #1 = latest attempt",
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(ratatui::text::Line::from(spans))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
