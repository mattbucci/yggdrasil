//! Runs tab — recent task_runs (yggdrasil-101). Read-only for the MVP;
//! interactive requeue / cancel land per a follow-up. Switches in via Shift+R.

use crate::db::DbPool;
use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use uuid::Uuid;

#[derive(Debug, Clone)]
struct RunRow {
    task_ref: String,
    title: String,
    attempt: i32,
    state: String,
    reason: String,
    agent_name: Option<String>,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    commit_sha: Option<String>,
}

pub struct RunsView {
    rows: Vec<RunRow>,
    selected: usize,
    /// Filter: "all" | "live" (scheduled/ready/running) | "terminal".
    filter: Filter,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Filter {
    All,
    Live,
    Terminal,
}

impl Default for RunsView {
    fn default() -> Self {
        Self::new()
    }
}

impl RunsView {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            selected: 0,
            filter: Filter::All,
        }
    }

    pub fn cycle_filter(&mut self) {
        self.filter = match self.filter {
            Filter::All => Filter::Live,
            Filter::Live => Filter::Terminal,
            Filter::Terminal => Filter::All,
        };
    }

    pub fn select_next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1).min(self.rows.len() - 1);
        }
    }
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        let where_clause = match self.filter {
            Filter::All => "TRUE",
            Filter::Live => "tr.state IN ('scheduled','ready','running','retrying')",
            Filter::Terminal => "tr.state IN ('succeeded','failed','crashed','cancelled','poison')",
        };

        let sql = format!(
            r#"SELECT r.task_prefix, t.seq, t.title, tr.attempt,
                      tr.state, tr.reason,
                      ag.agent_name, tr.started_at, tr.ended_at, tr.output_commit_sha
                 FROM task_runs tr
                 JOIN tasks t ON t.task_id = tr.task_id
                 JOIN repos r ON r.repo_id = t.repo_id
            LEFT JOIN agents ag ON ag.agent_id = tr.agent_id
                WHERE {where_clause}
                ORDER BY COALESCE(tr.started_at, tr.scheduled_at) DESC
                LIMIT 100"#,
        );

        let rows: Vec<(
            String,
            i32,
            String,
            i32,
            String,
            String,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
            Option<String>,
        )> = sqlx::query_as(&sql).fetch_all(pool).await?;

        self.rows = rows
            .into_iter()
            .map(
                |(
                    prefix,
                    seq,
                    title,
                    attempt,
                    state,
                    reason,
                    agent_name,
                    started_at,
                    ended_at,
                    commit,
                )| {
                    RunRow {
                        task_ref: format!("{prefix}-{seq}"),
                        title,
                        attempt,
                        state,
                        reason,
                        agent_name,
                        started_at,
                        ended_at,
                        commit_sha: commit,
                    }
                },
            )
            .collect();
        if self.selected >= self.rows.len() && !self.rows.is_empty() {
            self.selected = self.rows.len() - 1;
        }
        Ok(())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let title = format!(
            "Task runs · filter: {} · {} rows",
            match self.filter {
                Filter::All => "all",
                Filter::Live => "live",
                Filter::Terminal => "terminal",
            },
            self.rows.len(),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::DarkGray));

        if self.rows.is_empty() {
            let p = Paragraph::new("no task runs yet — run `ygg scheduler tick` or claim a task")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            frame.render_widget(p, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("task").style(Style::default().fg(Color::DarkGray)),
            Cell::from("#").style(Style::default().fg(Color::DarkGray)),
            Cell::from("state").style(Style::default().fg(Color::DarkGray)),
            Cell::from("reason").style(Style::default().fg(Color::DarkGray)),
            Cell::from("agent").style(Style::default().fg(Color::DarkGray)),
            Cell::from("dur").style(Style::default().fg(Color::DarkGray)),
            Cell::from("commit").style(Style::default().fg(Color::DarkGray)),
            Cell::from("title").style(Style::default().fg(Color::DarkGray)),
        ]);

        let now = Utc::now();
        let rows: Vec<Row> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (color, _) = state_style(&r.state);
                let dur = match (r.started_at, r.ended_at) {
                    (Some(s), Some(e)) => format!("{}s", (e - s).num_seconds().max(0)),
                    (Some(s), None) => format!("{}s+", (now - s).num_seconds().max(0)),
                    _ => "—".into(),
                };
                let commit = r
                    .commit_sha
                    .as_deref()
                    .map(|s| s.chars().take(10).collect::<String>())
                    .unwrap_or_default();
                let agent = r.agent_name.as_deref().unwrap_or("—").to_string();
                let row = Row::new(vec![
                    Cell::from(r.task_ref.clone()),
                    Cell::from(format!("#{}", r.attempt)),
                    Cell::from(r.state.clone()).style(Style::default().fg(color)),
                    Cell::from(r.reason.clone()).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(agent),
                    Cell::from(dur),
                    Cell::from(commit).style(Style::default().fg(Color::DarkGray)),
                    Cell::from(r.title.clone()),
                ]);
                if i == self.selected {
                    row.style(Style::default().bg(Color::DarkGray))
                } else {
                    row
                }
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(20),
                Constraint::Length(4),
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Length(20),
                Constraint::Length(8),
                Constraint::Length(11),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(block);
        frame.render_widget(table, area);
    }
}

fn state_style(state: &str) -> (Color, &'static str) {
    match state {
        "succeeded" => (Color::Green, "✓"),
        "failed" | "crashed" | "poison" => (Color::Red, "✗"),
        "cancelled" => (Color::DarkGray, "○"),
        "running" => (Color::Blue, "▶"),
        "ready" => (Color::Yellow, "□"),
        "scheduled" => (Color::DarkGray, "□"),
        "retrying" => (Color::Yellow, "↻"),
        _ => (Color::Gray, "·"),
    }
}

#[allow(dead_code)]
fn _suppress_unused(p: &DbPool) -> &DbPool {
    let _: String = Uuid::nil().to_string();
    p
}
