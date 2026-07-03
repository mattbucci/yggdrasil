//! Tasks pane — ready tasks across every registered repo (flat list).
//! Cwd-scoping silently shows nothing when the TUI runs from the wrong
//! directory; dashboard semantics are cross-system everywhere.

use std::collections::{HashMap, HashSet};

use crate::db::DbPool;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::models::repo::RepoRepo;
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};

pub enum TaskRow {
    Header { kind: TaskKind, count: usize },
    Task { prefix: String, task: Task },
}

pub struct TasksView {
    pub flash: String,
    pub detail_open: bool,
    pub rows: Vec<TaskRow>,
    pub state: TableState,
    pub last_status: String,
    /// Flipped true once `refresh` has executed at least once. Until then the
    /// render path shows a themed loading view — otherwise the first paint
    /// (before the initial query completes) reads as "nothing here" rather
    /// than "still fetching".
    pub loaded: bool,
    /// Armed by Backspace; next key must be `y` to actually delete.
    /// Carries the display label so the confirm prompt stays accurate
    /// even if the selection moves.
    pub pending_delete: Option<(String, String)>,
    /// yggdrasil-155: inline rename buffer. `Some((task_id, edited_title))`
    /// while the user is editing the selected row's title in place; commit
    /// writes back via TaskRepo::update, cancel discards. None means the
    /// pane is in normal nav mode.
    pub rename: Option<(String, String)>,
}

impl Default for TasksView {
    fn default() -> Self {
        Self::new()
    }
}

impl TasksView {
    pub fn new() -> Self {
        let mut st = TableState::default();
        st.select(Some(0));
        Self {
            rows: vec![],
            state: st,
            last_status: String::new(),
            loaded: false,
            flash: String::new(),
            detail_open: false,
            pending_delete: None,
            rename: None,
        }
    }

    pub fn rename_mode(&self) -> bool {
        self.rename.is_some()
    }

    /// Begin renaming the selected task. Pre-populates the buffer with
    /// the current title so the user can edit rather than retype. No-op
    /// when the selection isn't on a Task row.
    pub fn rename_begin(&mut self) {
        let Some(i) = self.state.selected() else {
            return;
        };
        if let Some(TaskRow::Task { task, .. }) = self.rows.get(i) {
            self.rename = Some((task.task_id.clone(), task.title.clone()));
        }
    }

    pub fn rename_cancel(&mut self) {
        self.rename = None;
    }

    pub fn rename_push(&mut self, c: char) {
        if let Some((_, buf)) = &mut self.rename {
            buf.push(c);
        }
    }

    pub fn rename_pop(&mut self) {
        if let Some((_, buf)) = &mut self.rename {
            buf.pop();
        }
    }

    pub fn rename_buffer(&self) -> Option<&str> {
        self.rename.as_ref().map(|(_, b)| b.as_str())
    }

    /// Commit the rename to the database and clear the buffer. Reload
    /// happens on the caller's next refresh tick.
    pub async fn rename_commit(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        let Some((task_id, title)) = self.rename.take() else {
            return Ok(());
        };
        let trimmed = title.trim();
        if trimmed.is_empty() {
            // Empty title would clear the row entirely; treat empty as
            // cancel rather than silently nuking the title.
            self.flash = "rename cancelled (empty)".into();
            return Ok(());
        }
        TaskRepo::new(pool)
            .update(
                &task_id,
                None,
                crate::models::task::TaskUpdate {
                    title: Some(trimmed),
                    ..Default::default()
                },
            )
            .await?;
        self.flash = format!("renamed to {trimmed}");
        Ok(())
    }

    /// task_id + display label ("yggdrasil-42") for the selected row, if any.
    pub fn selected_task_id(&self) -> Option<(String, String)> {
        let i = self.state.selected()?;
        match self.rows.get(i)? {
            TaskRow::Task { prefix, task } => {
                Some((task.task_id.clone(), format!("{prefix}-{}", task.seq)))
            }
            _ => None,
        }
    }

    pub fn delete_begin(&mut self) {
        self.pending_delete = self.selected_task_id();
    }
    pub fn delete_cancel(&mut self) {
        self.pending_delete = None;
    }
    pub fn take_pending_delete(&mut self) -> Option<String> {
        self.pending_delete.take().map(|(id, _)| id)
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        self.last_status.clear();
        self.loaded = true;
        let repos = RepoRepo::new(pool).list().await.unwrap_or_default();
        let prefix_by_repo: HashMap<String, String> = repos
            .iter()
            .map(|r| (r.repo_id.clone(), r.task_prefix.clone()))
            .collect();

        let tasks: Vec<Task> = TaskRepo::new(pool)
            .list(None, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .collect();

        let task_ids: Vec<String> = tasks.iter().map(|t| t.task_id.clone()).collect();
        let edges: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
            "SELECT task_id, blocker_id FROM task_deps
              WHERE task_id IN (SELECT value FROM json_each($1))",
        )
        .bind(serde_json::to_string(&task_ids).unwrap_or_default())
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        // A task is blocked iff one of its blockers is still open/in-progress.
        let open_ids: HashSet<String> = tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked
                )
            })
            .map(|t| t.task_id.clone())
            .collect();
        let mut blocked: HashSet<String> = HashSet::new();
        for (tid, bid) in &edges {
            if open_ids.contains(bid) {
                blocked.insert(tid.clone());
            }
        }

        let mut ready: Vec<(String, Task)> = tasks
            .into_iter()
            .filter(|t| !blocked.contains(&t.task_id))
            .filter_map(|t| {
                let p = prefix_by_repo.get(&t.repo_id).cloned()?;
                Some((p, t))
            })
            .collect();
        // Group by kind (epic, feature, bug, task, chore); within each group
        // sort by priority then seq.
        ready.sort_by_key(|(_p, t)| (kind_order(&t.kind), t.priority, t.seq));

        // Walk the sorted list and emit a header row whenever the kind changes.
        let mut grouped: Vec<TaskRow> = Vec::new();
        let mut it = ready.into_iter().peekable();
        while let Some((prefix, task)) = it.next() {
            let kind = task.kind.clone();
            let mut bucket: Vec<(String, Task)> = vec![(prefix, task)];
            while let Some((_, peek)) = it.peek() {
                if peek.kind == kind {
                    if let Some(next) = it.next() {
                        bucket.push(next);
                    }
                } else {
                    break;
                }
            }
            grouped.push(TaskRow::Header {
                kind,
                count: bucket.len(),
            });
            for (p, t) in bucket {
                grouped.push(TaskRow::Task { prefix: p, task: t });
            }
        }
        self.rows = grouped;

        if self.rows.is_empty() {
            self.last_status = format!(
                "no ready tasks in any of {} registered repo(s)",
                repos.len()
            );
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= self.rows.len() {
            self.state.select(Some(self.rows.len() - 1));
        }
        Ok(())
    }

    /// Task ref ("yggdrasil-43") of the selected row, if it's a task row.
    pub fn selected_task_ref(&self) -> Option<String> {
        let i = self.state.selected()?;
        match self.rows.get(i)? {
            TaskRow::Task { prefix, task } => Some(format!("{prefix}-{}", task.seq)),
            _ => None,
        }
    }

    /// Selected task + its prefix (for the detail overlay).
    pub fn selected_task(&self) -> Option<(&crate::models::task::Task, &str)> {
        let i = self.state.selected()?;
        match self.rows.get(i)? {
            TaskRow::Task { prefix, task } => Some((task, prefix.as_str())),
            _ => None,
        }
    }

    pub fn toggle_detail(&mut self) {
        if self.selected_task().is_some() {
            self.detail_open = !self.detail_open;
        }
    }

    /// Short-lived status line shown in the title after a run/add keystroke.
    pub fn set_flash(&mut self, msg: impl Into<String>) {
        self.flash = msg.into();
    }

    pub fn select_prev(&mut self) {
        let Some(target) = self.neighbor_task_row(-1) else {
            return;
        };
        self.state.select(Some(target));
    }

    pub fn select_next(&mut self) {
        let Some(target) = self.neighbor_task_row(1) else {
            return;
        };
        self.state.select(Some(target));
    }

    /// Walk the row list in `dir` (±1), skipping headers. Wraps around.
    fn neighbor_task_row(&self, dir: isize) -> Option<usize> {
        if self.rows.is_empty() {
            return None;
        }
        let len = self.rows.len();
        let start = self.state.selected().unwrap_or(0);
        let mut i = start;
        for _ in 0..len {
            i = ((i as isize + dir).rem_euclid(len as isize)) as usize;
            if matches!(self.rows[i], TaskRow::Task { .. }) {
                return Some(i);
            }
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let base = format!(" Tasks ready ({} across all repos) ", self.rows.len());
        let title = if let Some((_, label)) = &self.pending_delete {
            format!("{base} ·  DELETE {label}? y / any=cancel")
        } else if self.flash.is_empty() {
            base
        } else {
            format!("{base} ·  {}", self.flash)
        };

        if !self.loaded {
            render_tasks_loading(frame, area, &title);
            return;
        }

        if self.rows.is_empty() {
            let lines = vec![
                Line::from(""),
                Line::from("  No ready tasks in any registered repo."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Try "),
                    Span::styled(
                        "ygg task create \"...\" --kind task --priority 2",
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(" from inside a project."),
                ]),
                Line::from(""),
                if !self.last_status.is_empty() {
                    Line::from(vec![
                        Span::styled("  · ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            self.last_status.clone(),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                } else {
                    Line::from("")
                },
            ];
            let para =
                Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from(""), // run-state glyph column
            Cell::from("KIND").style(Style::default().fg(Color::Gray)),
            Cell::from("ID").style(Style::default().fg(Color::Gray)),
            Cell::from("P").style(Style::default().fg(Color::Gray)),
            Cell::from("TITLE").style(Style::default().fg(Color::Gray)),
            Cell::from("AGE").style(Style::default().fg(Color::Gray)),
        ]);

        let rows: Vec<Row> = self
            .rows
            .iter()
            .map(|row| match row {
                TaskRow::Header { kind, count } => {
                    let (color, glyph) = kind_style(kind);
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(format!("{glyph} {}", pluralize_kind(kind)))
                            .style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
                        Cell::from(""),
                        Cell::from(""),
                        Cell::from(format!("({count})"))
                            .style(Style::default().fg(Color::DarkGray)),
                        Cell::from(""),
                    ])
                }
                TaskRow::Task { prefix, task: t } => {
                    // Single source of truth for run/kind/priority/title styling.
                    Row::new(crate::tui::widgets::task_row_cells(t, prefix))
                }
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(3),  // run-state glyph
                Constraint::Length(14), // KIND
                Constraint::Length(16), // ID
                Constraint::Length(4),  // P
                Constraint::Min(20),    // TITLE
                Constraint::Length(5),  // AGE
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        frame.render_stateful_widget(table, area, &mut self.state);

        if self.detail_open
            && let Some((task, prefix)) = self.selected_task()
        {
            crate::tui::dag_view::render_detail_overlay(frame, area, task, prefix);
        }
    }
}

fn prio_style(p: i16) -> Style {
    match p {
        0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        1 => Style::default().fg(Color::Yellow),
        2 => Style::default().fg(Color::White),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn kind_order(k: &TaskKind) -> u8 {
    match k {
        TaskKind::Epic => 0,
        TaskKind::Feature => 1,
        TaskKind::Bug => 2,
        TaskKind::Task => 3,
        TaskKind::Chore => 4,
    }
}

fn kind_style(k: &TaskKind) -> (Color, &'static str) {
    match k {
        TaskKind::Epic => (Color::Magenta, "◉"),
        TaskKind::Feature => (Color::Cyan, "✚"),
        TaskKind::Bug => (Color::Red, "🐞"),
        TaskKind::Task => (Color::White, "○"),
        TaskKind::Chore => (Color::DarkGray, "·"),
    }
}

fn pluralize_kind(k: &TaskKind) -> &'static str {
    match k {
        TaskKind::Epic => "EPICS",
        TaskKind::Feature => "FEATURES",
        TaskKind::Bug => "BUGS",
        TaskKind::Task => "TASKS",
        TaskKind::Chore => "CHORES",
    }
}

/// Painted while the first refresh is in flight. A horizontal row of kind
/// glyphs hints at the "tasks grouped by kind" layout that's coming.
fn render_tasks_loading(frame: &mut Frame, area: Rect, title: &str) {
    let hint = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);
    let sep = Style::default().fg(Color::DarkGray);

    let row: Vec<Span> = vec![
        Span::styled(
            "◉",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ·   ", sep),
        Span::styled("✚", Style::default().fg(Color::Cyan)),
        Span::styled("   ·   ", sep),
        Span::styled("🐞", Style::default().fg(Color::Red)),
        Span::styled("   ·   ", sep),
        Span::styled("○", Style::default().fg(Color::White)),
        Span::styled("   ·   ", sep),
        Span::styled("·", Style::default().fg(Color::DarkGray)),
    ];

    let art: Vec<Line> = vec![
        Line::from(""),
        Line::from(""),
        Line::from(row),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("gathering ready work…", hint)),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let para = Paragraph::new(art)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(para, area);
}
