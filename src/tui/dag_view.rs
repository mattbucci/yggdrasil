//! Task dependency graph — rolls tasks up into epic → task → deps.
//! This is the "what's the shape of the work?" view, matching how
//! beads renders issues. Different from the flat list in the Tasks
//! pane: here we see roots (epics, unblocked tasks) at the top and
//! their children indented below, with dependency arrows visible.

use crate::db::DbPool;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::models::agent::AgentRepo;
use crate::models::repo::{Repo, RepoRepo};
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DagSort {
    Priority,
    Kind,
    Recent,
}

impl DagSort {
    fn label(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Kind => "kind",
            Self::Recent => "recent",
        }
    }
    fn next(&self) -> Self {
        match self {
            Self::Priority => Self::Kind,
            Self::Kind => Self::Recent,
            Self::Recent => Self::Priority,
        }
    }
}

/// Assignee filter. Cycles `All → each known assignee → Unassigned → All`.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentFilter {
    All,
    Specific(String),
    Unassigned,
}

pub struct DagView {
    pub rows: Vec<RenderRow>,
    pub state: ListState,
    pub repo_name: String,
    /// What happened on the last refresh. Shown in the empty-state so it
    /// is obvious whether we have an error, a registered repo with no
    /// tasks, or a fallback with no conversation either.
    pub last_status: String,
    pub detail_open: bool,
    pub sort: DagSort,
    pub agent_filter: AgentFilter,
    add_active: bool,
    add_buffer: String,
    /// Short-lived status line shown under the title, e.g. "launched yggdrasil-43".
    /// Cleared on the next refresh after a few ticks.
    pub flash: String,
    /// When set, only this task and its descendants (via task_deps) render.
    pub subtree_focus: Option<String>,
    /// Cached assignee list, populated each refresh: (agent_id, agent_name).
    /// Drives the `a` cycle without a DB hit per keypress.
    pub known_assignees: Vec<(String, String)>,
    /// Display label for the subtree focus, shown in the title while active.
    pub focus_label: String,
    /// Flipped true once `refresh` has executed at least once. Until then the
    /// render path shows a themed loading view instead of the "no tasks"
    /// empty-state — otherwise the first paint (before the initial query
    /// completes) reads as "nothing here" rather than "still fetching".
    pub loaded: bool,
    /// Armed by Backspace; next key must be `y` to actually delete.
    /// Stored with the display label so the confirm line stays accurate
    /// even if the selection moves.
    pub pending_delete: Option<(String, String)>,
    // Dependency edge maps — persisted from refresh for the dep tree panel.
    children_of: HashMap<String, Vec<String>>,
    blockers_of: HashMap<String, Vec<String>>,
    tasks_by_id: HashMap<String, (Task, String)>,
}

pub enum RenderRow {
    RepoHeader {
        prefix: String,
        name: String,
        open_count: usize,
    },
    Task {
        task: Task,
        prefix: String,
        depth: usize,
        is_root: bool,
        n_children: usize,
    },
}

impl Default for DagView {
    fn default() -> Self {
        Self::new()
    }
}

impl DagView {
    pub fn new() -> Self {
        let mut st = ListState::default();
        st.select(Some(0));
        Self {
            rows: vec![],
            state: st,
            repo_name: String::new(),
            last_status: String::new(),
            detail_open: false,
            sort: DagSort::Priority,
            agent_filter: AgentFilter::All,
            subtree_focus: None,
            known_assignees: Vec::new(),
            focus_label: String::new(),
            loaded: false,
            add_active: false,
            add_buffer: String::new(),
            flash: String::new(),
            pending_delete: None,
            children_of: HashMap::new(),
            blockers_of: HashMap::new(),
            tasks_by_id: HashMap::new(),
        }
    }

    // Kept for compatibility with existing app.rs entry — a no-op here
    // since we're scoped to the current-cwd repo.
    pub fn set_agent(&mut self, _name: String) {}

    pub fn scroll_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state
            .select(Some(if i == 0 { self.rows.len() - 1 } else { i - 1 }));
    }

    pub fn scroll_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1) % self.rows.len()));
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
    }

    /// Cycle `All → assignee1 → ... → assigneeN → Unassigned → All`. Assignees
    /// come from the last refresh's `known_assignees` list.
    pub fn cycle_agent_filter(&mut self) {
        self.agent_filter = match &self.agent_filter {
            AgentFilter::All => {
                if let Some((id, _)) = self.known_assignees.first() {
                    AgentFilter::Specific(id.clone())
                } else {
                    AgentFilter::Unassigned
                }
            }
            AgentFilter::Specific(cur) => {
                let idx = self.known_assignees.iter().position(|(id, _)| id == cur);
                match idx {
                    Some(i) if i + 1 < self.known_assignees.len() => {
                        AgentFilter::Specific(self.known_assignees[i + 1].0.clone())
                    }
                    _ => AgentFilter::Unassigned,
                }
            }
            AgentFilter::Unassigned => AgentFilter::All,
        };
    }

    /// Toggle subtree-focus on the currently selected task. Second press on
    /// the same row clears focus; pressing on a different row replaces it.
    pub fn toggle_subtree_focus(&mut self) {
        let Some((task, prefix)) = self.selected_task() else {
            return;
        };
        let label = format!(
            "{}-{} {}",
            prefix,
            task.seq,
            task.title.chars().take(40).collect::<String>()
        );
        match &self.subtree_focus {
            Some(id) if *id == task.task_id => {
                self.subtree_focus = None;
                self.focus_label.clear();
            }
            _ => {
                self.subtree_focus = Some(task.task_id.clone());
                self.focus_label = label;
            }
        }
    }

    pub fn clear_filters(&mut self) {
        self.subtree_focus = None;
        self.focus_label.clear();
        self.agent_filter = AgentFilter::All;
    }

    /// One-line summary of active filters, for title/empty-state display.
    pub fn filter_description(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match &self.agent_filter {
            AgentFilter::All => {}
            AgentFilter::Specific(id) => {
                let name = self
                    .known_assignees
                    .iter()
                    .find(|(a, _)| a == id)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_else(|| id.to_string()[..8].to_string());
                parts.push(format!("owner={name}"));
            }
            AgentFilter::Unassigned => parts.push("owner=none".into()),
        }
        if !self.focus_label.is_empty() {
            parts.push(format!("focus={}", self.focus_label));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  ·  {}", parts.join("  ·  "))
        }
    }

    /// Toggle the detail overlay for the selected task row. Does nothing for
    /// repo-header rows since there's no task to show.
    pub fn toggle_detail(&mut self) {
        if let Some(i) = self.state.selected()
            && matches!(self.rows.get(i), Some(RenderRow::Task { .. }))
        {
            self.detail_open = !self.detail_open;
        }
    }

    fn selected_task(&self) -> Option<(&Task, &str)> {
        let i = self.state.selected()?;
        match self.rows.get(i)? {
            RenderRow::Task { task, prefix, .. } => Some((task, prefix.as_str())),
            _ => None,
        }
    }

    /// task_id + display label ("yggdrasil-42") for the selected row, if any.
    /// app.rs uses this for the backspace-delete confirm flow.
    pub fn selected_task_id(&self) -> Option<(String, String)> {
        self.selected_task()
            .map(|(t, p)| (t.task_id.clone(), format!("{p}-{}", t.seq)))
    }

    /// Arm delete confirmation for the currently selected task. Next key must
    /// be `y` in handle_key; anything else cancels via `delete_cancel`.
    pub fn delete_begin(&mut self) {
        self.pending_delete = self.selected_task_id();
    }
    pub fn delete_cancel(&mut self) {
        self.pending_delete = None;
    }
    pub fn take_pending_delete(&mut self) -> Option<String> {
        self.pending_delete.take().map(|(id, _)| id)
    }

    /// Public accessor for app.rs key handlers that need to dispatch by ref
    /// (e.g. `r` launches `ygg plan run <ref>`).
    pub fn selected_task_ref(&self) -> Option<String> {
        self.selected_task().map(|(t, p)| format!("{p}-{}", t.seq))
    }

    /// Inline-input overlay state for 'n' (add child). When active, key
    /// events flow into `add_buffer` and Enter commits a new task.
    pub fn add_mode(&self) -> bool {
        self.add_active
    }
    pub fn add_begin(&mut self) {
        self.add_active = true;
        self.add_buffer.clear();
    }
    pub fn add_cancel(&mut self) {
        self.add_active = false;
        self.add_buffer.clear();
    }
    pub fn add_push(&mut self, c: char) {
        self.add_buffer.push(c);
    }
    pub fn add_pop(&mut self) {
        self.add_buffer.pop();
    }
    pub fn add_buffer(&self) -> &str {
        &self.add_buffer
    }

    /// Returns (parent_ref_if_task, title). Caller feeds this into
    /// plan_cmd::add — if parent is None, use a top-level create.
    pub fn add_commit(&mut self) -> Option<(Option<String>, String)> {
        let title = self.add_buffer.trim().to_string();
        self.add_active = false;
        self.add_buffer.clear();
        if title.is_empty() {
            return None;
        }
        let parent = self.selected_task_ref();
        Some((parent, title))
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        // Whole-system view: show all open tasks across every repo, grouped
        // by repo header. Deliberately NOT cwd-scoped — the TUI is a
        // cross-project dashboard; the CLI `ygg task ready` is the
        // in-repo quick view.
        self.repo_name.clear();
        self.last_status.clear();
        self.loaded = true;

        let repos = RepoRepo::new(pool).list().await.unwrap_or_default();

        // All open tasks across every repo, keyed by repo_id.
        let unfiltered_open: Vec<Task> = TaskRepo::new(pool)
            .list(None, None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .collect();

        if unfiltered_open.is_empty() {
            self.rows.clear();
            self.known_assignees.clear();
            self.last_status =
                format!("no open tasks in any of {} registered repo(s)", repos.len());
            self.state.select(None);
            return Ok(());
        }

        // "Owner" for filter purposes = assignee if set, else created_by.
        // Without this fallback almost every task registers as unassigned
        // because `ygg task create` doesn't set an assignee, so the `a`
        // cycle was effectively dead.
        let mut owner_ids: HashSet<String> = HashSet::new();
        for t in &unfiltered_open {
            let owner = t.assignee.clone().or(t.created_by.clone());
            if let Some(id) = owner {
                owner_ids.insert(id);
            }
        }
        let all_agents = AgentRepo::new(pool, crate::db::user_id())
            .list()
            .await
            .unwrap_or_default();
        let mut assignees: Vec<(String, String)> = all_agents
            .into_iter()
            .filter(|a| owner_ids.contains(&a.agent_id))
            .map(|a| (a.agent_id.clone(), a.agent_name))
            .collect();
        assignees.sort_by(|a, b| a.1.cmp(&b.1));
        self.known_assignees = assignees;

        // Preload all dep edges once — need them both for filtering (subtree
        // descendants) and rendering (children badges). Do this BEFORE the
        // assignee/subtree filter so descendants are computed from the full
        // graph, not a post-filter subset.
        let every_id: Vec<String> = unfiltered_open.iter().map(|t| t.task_id.clone()).collect();
        let edges_all: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
            "SELECT task_id, blocker_id FROM task_deps
             WHERE task_id IN (SELECT value FROM json_each($1))
                OR blocker_id IN (SELECT value FROM json_each($1))",
        )
        .bind(serde_json::to_string(&every_id).unwrap_or_default())
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        // Apply filters. Subtree focus comes first — it restricts the
        // universe of visible task_ids, then agent filter narrows further.
        let mut allowed: Option<HashSet<String>> = None;
        if let Some(root) = self.subtree_focus.clone() {
            let mut descendants: HashMap<String, Vec<String>> = HashMap::new();
            for (t, b) in &edges_all {
                descendants.entry(b.clone()).or_default().push(t.clone());
            }
            let mut keep: HashSet<String> = HashSet::new();
            let mut frontier = vec![root];
            while let Some(id) = frontier.pop() {
                if !keep.insert(id.clone()) {
                    continue;
                }
                if let Some(children) = descendants.get(&id) {
                    frontier.extend(children.iter().cloned());
                }
            }
            allowed = Some(keep);
        }

        let all_open: Vec<Task> = unfiltered_open
            .into_iter()
            .filter(|t| {
                let owner = t.assignee.clone().or(t.created_by.clone());
                match &self.agent_filter {
                    AgentFilter::All => true,
                    AgentFilter::Specific(a) => owner.as_ref() == Some(a),
                    AgentFilter::Unassigned => owner.is_none(),
                }
            })
            .filter(|t| {
                allowed
                    .as_ref()
                    .map(|s| s.contains(&t.task_id))
                    .unwrap_or(true)
            })
            .collect();

        if all_open.is_empty() {
            self.rows.clear();
            let filter_desc = self.filter_description();
            self.last_status = format!("no tasks match the current filters{}", filter_desc);
            self.state.select(None);
            return Ok(());
        }

        // Bucket by repo.
        let mut by_repo: HashMap<String, Vec<Task>> = HashMap::new();
        for t in all_open {
            by_repo.entry(t.repo_id.clone()).or_default().push(t);
        }

        // Build the forward/backward edge maps from the preloaded edges.
        // Persist on self so the dep-tree panel can render them.
        self.children_of.clear();
        self.blockers_of.clear();
        for (t, b) in &edges_all {
            self.children_of
                .entry(b.clone())
                .or_default()
                .push(t.clone());
            self.blockers_of
                .entry(t.clone())
                .or_default()
                .push(b.clone());
        }
        let children_of = &self.children_of;
        let blockers_of = &self.blockers_of;

        // Build tasks_by_id lookup for the dep-tree panel.
        self.tasks_by_id.clear();
        for tasks in by_repo.values() {
            for t in tasks {
                let prefix = repos
                    .iter()
                    .find(|r| r.repo_id == t.repo_id)
                    .map(|r| r.task_prefix.clone())
                    .unwrap_or_default();
                self.tasks_by_id
                    .insert(t.task_id.clone(), (t.clone(), prefix));
            }
        }

        // Stable repo order: by name asc.
        let mut repo_order: Vec<&Repo> = repos
            .iter()
            .filter(|r| by_repo.contains_key(&r.repo_id))
            .collect();
        repo_order.sort_by(|a, b| a.name.cmp(&b.name));

        let mut rows: Vec<RenderRow> = Vec::new();
        for repo in repo_order {
            let tasks = by_repo.remove(&repo.repo_id).unwrap_or_default();
            rows.push(RenderRow::RepoHeader {
                prefix: repo.task_prefix.clone(),
                name: repo.name.clone(),
                open_count: tasks.len(),
            });

            let by_id: BTreeMap<String, &Task> =
                tasks.iter().map(|t| (t.task_id.clone(), t)).collect();

            // When a subtree is focused, only THAT task is a root in its
            // repo — everything else is either a descendant or filtered out.
            let mut roots: Vec<&Task> = if let Some(focus) = self.subtree_focus.clone() {
                tasks.iter().filter(|t| t.task_id == focus).collect()
            } else {
                tasks
                    .iter()
                    .filter(|t| {
                        let no_blockers = blockers_of
                            .get(&t.task_id)
                            .map(|bs| bs.iter().all(|b| !by_id.contains_key(b)))
                            .unwrap_or(true);
                        matches!(t.kind, TaskKind::Epic) || no_blockers
                    })
                    .collect()
            };
            sort_tasks(&mut roots, self.sort);

            let mut visited: HashSet<String> = HashSet::new();
            for r in &roots {
                walk(
                    r.task_id.clone(),
                    0,
                    &repo.task_prefix,
                    children_of,
                    &by_id,
                    &mut visited,
                    &mut rows,
                    self.sort,
                );
            }
            // Cycle orphans and anything we missed.
            for t in &tasks {
                if !visited.contains(&t.task_id) {
                    let n_children = children_of.get(&t.task_id).map(|v| v.len()).unwrap_or(0);
                    rows.push(RenderRow::Task {
                        task: t.clone(),
                        prefix: repo.task_prefix.clone(),
                        depth: 0,
                        is_root: true,
                        n_children,
                    });
                    visited.insert(t.task_id.clone());
                }
            }
        }

        self.rows = rows;

        if self.rows.is_empty() {
            self.state.select(None);
        } else if self.state.selected().unwrap_or(0) >= self.rows.len() {
            self.state.select(Some(0));
        }
        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let title = format!(
            " Task graph — {} open across all repos  ·  sort: {}  (s=sort · a=agent · f=focus · c=clear){} ",
            self.rows
                .iter()
                .filter(|r| matches!(r, RenderRow::Task { .. }))
                .count(),
            self.sort.label(),
            self.filter_description()
        );

        if !self.loaded {
            render_dag_loading(frame, area, &title);
            return;
        }

        if self.rows.is_empty() {
            let lines: Vec<Line> = vec![
                Line::from(""),
                Line::from("  No open tasks in any registered repo yet."),
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
            let para = ratatui::widgets::Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| match r {
                RenderRow::RepoHeader {
                    prefix,
                    name,
                    open_count,
                } => ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  ▸ {name}"),
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ({prefix}, {open_count} open)"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])),
                RenderRow::Task {
                    task: t,
                    prefix,
                    depth,
                    is_root: _,
                    n_children,
                } => {
                    let indent = "  ".repeat(depth + 1);
                    let connector = if *depth == 0 { "" } else { "└─ " };

                    let (kind_color, kind_glyph) = match t.kind {
                        TaskKind::Epic => (Color::Magenta, "◉"),
                        TaskKind::Feature => (Color::Cyan, "✚"),
                        TaskKind::Bug => (Color::Red, "🐞"),
                        TaskKind::Chore => (Color::DarkGray, "·"),
                        TaskKind::Task => (Color::White, "○"),
                    };

                    // Run-state glyph: not-started / active / blocked / done.
                    // "Open" means filed but nothing has worked on it yet —
                    // reads as empty-circle, not hourglass (which implies
                    // work in flight). Running tasks get bold + a filled
                    // arrow so they visibly stand out.
                    let (status_color, status_label, run_glyph, glyph_bold) = match t.status {
                        TaskStatus::Open => (Color::DarkGray, "open", "◯", false),
                        TaskStatus::InProgress => (Color::Green, "wip ", "▶", true),
                        TaskStatus::Blocked => (Color::Red, "blkd", "⏸", false),
                        TaskStatus::Closed => {
                            let failed = t
                                .close_reason
                                .as_deref()
                                .map(|r| r.to_lowercase().contains("fail"))
                                .unwrap_or(false);
                            if failed {
                                (Color::Red, "fail", "✗", false)
                            } else {
                                (Color::DarkGray, "done", "✓", false)
                            }
                        }
                    };

                    let prio_style = match t.priority {
                        0 => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        1 => Style::default().fg(Color::Yellow),
                        2 => Style::default().fg(Color::White),
                        _ => Style::default().fg(Color::DarkGray),
                    };

                    let children_badge = if *n_children > 0 {
                        format!(" {}↓", n_children)
                    } else {
                        String::new()
                    };

                    let id = format!("{}-{}", prefix, t.seq);
                    let age = humanize_age(t.updated_at);
                    // Older rows dim the age so the eye lands on fresh work first;
                    // a week+ gets a Yellow nudge so the "stale" case is obvious.
                    let age_style = {
                        let secs = (chrono::Utc::now() - t.updated_at).num_seconds().max(0);
                        if secs >= 7 * 86400 {
                            Style::default().fg(Color::Yellow)
                        } else if secs >= 86400 {
                            Style::default().fg(Color::DarkGray)
                        } else {
                            Style::default().fg(Color::Gray)
                        }
                    };

                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::raw(connector),
                        Span::styled(
                            format!("{run_glyph} "),
                            if glyph_bold {
                                Style::default()
                                    .fg(status_color)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(status_color)
                            },
                        ),
                        Span::styled(format!("{kind_glyph} "), Style::default().fg(kind_color)),
                        Span::styled(status_label, Style::default().fg(status_color)),
                        Span::raw(" "),
                        Span::styled(format!("P{}", t.priority), prio_style),
                        Span::raw(" "),
                        Span::styled(id, Style::default().fg(Color::DarkGray)),
                        Span::raw("  "),
                        Span::styled(
                            truncate(&t.title, 70),
                            if matches!(t.status, TaskStatus::InProgress) {
                                Style::default()
                                    .fg(Color::Green)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(children_badge, Style::default().fg(Color::Cyan)),
                        Span::styled(format!("  {age}"), age_style),
                    ]))
                }
            })
            .collect();

        let title = if let Some((_, label)) = &self.pending_delete {
            format!("{title}  ·  DELETE {label}? y / any=cancel")
        } else if !self.flash.is_empty() {
            format!("{title}  ·  {}", self.flash)
        } else {
            title
        };

        // Split: dep tree panel on top, task list on bottom.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(7), Constraint::Percentage(60)])
            .split(area);

        self.render_dep_tree(frame, chunks[0]);

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, chunks[1], &mut self.state);

        // Detail overlay floats over the full area.
        if self.detail_open
            && let Some((task, prefix)) = self.selected_task()
        {
            render_detail_overlay(frame, area, task, prefix);
        }

        // Inline-input overlay for 'n' (add child). Centered, small.
        if self.add_active {
            render_add_overlay(
                frame,
                area,
                &self.add_buffer,
                self.selected_task_ref().as_deref(),
            );
        }
    }

    fn render_dep_tree(&self, frame: &mut Frame, area: Rect) {
        let selected = match self.selected_task() {
            Some((task, prefix)) => (task.clone(), prefix.to_string()),
            None => {
                let para = ratatui::widgets::Paragraph::new(vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        "  select a task to see its dependency tree",
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Dependencies "),
                );
                frame.render_widget(para, area);
                return;
            }
        };
        let (task, prefix) = &selected;
        let task_ref = format!("{}-{}", prefix, task.seq);

        let blockers: Vec<&(Task, String)> = self
            .blockers_of
            .get(&task.task_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.tasks_by_id.get(id))
                    .collect()
            })
            .unwrap_or_default();
        let dependents: Vec<&(Task, String)> = self
            .children_of
            .get(&task.task_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.tasks_by_id.get(id))
                    .collect()
            })
            .unwrap_or_default();

        let title = format!(" Dependencies · {task_ref}: {} ", truncate(&task.title, 40));

        if blockers.is_empty() && dependents.is_empty() {
            let total_edges = self
                .blockers_of
                .get(&task.task_id)
                .map(|v| v.len())
                .unwrap_or(0)
                + self
                    .children_of
                    .get(&task.task_id)
                    .map(|v| v.len())
                    .unwrap_or(0);
            let msg = if total_edges > 0 {
                format!("  {total_edges} dep(s) hidden by current filter")
            } else {
                "  no dependencies — this task is independent".to_string()
            };
            let para = ratatui::widgets::Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))),
            ])
            .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
            return;
        }

        // Two-column layout: blockers left, dependents right.
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left: blocked by
        let mut left_lines: Vec<Line> = vec![Line::from(Span::styled(
            " blocked by:",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))];
        if blockers.is_empty() {
            left_lines.push(Line::from(Span::styled(
                "   (none)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (t, p) in &blockers {
            left_lines.push(dep_line(t, p));
        }
        let left = ratatui::widgets::Paragraph::new(left_lines)
            .block(Block::default().borders(Borders::ALL).title(title.clone()));
        frame.render_widget(left, cols[0]);

        // Right: blocks
        let mut right_lines: Vec<Line> = vec![Line::from(Span::styled(
            " blocks:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        if dependents.is_empty() {
            right_lines.push(Line::from(Span::styled(
                "   (none)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (t, p) in &dependents {
            right_lines.push(dep_line(t, p));
        }
        let right = ratatui::widgets::Paragraph::new(right_lines)
            .block(Block::default().borders(Borders::ALL).title(" Dependents "));
        frame.render_widget(right, cols[1]);
    }
}

fn dep_line(t: &Task, prefix: &str) -> Line<'static> {
    let (status_color, glyph) = match t.status {
        TaskStatus::Open => (Color::DarkGray, "◯"),
        TaskStatus::InProgress => (Color::Green, "▶"),
        TaskStatus::Blocked => (Color::Red, "⏸"),
        TaskStatus::Closed => (Color::DarkGray, "✓"),
    };
    let prio_color = match t.priority {
        0 => Color::Red,
        1 => Color::Yellow,
        2 => Color::White,
        _ => Color::DarkGray,
    };
    let id = format!("{}-{}", prefix, t.seq);
    Line::from(vec![
        Span::styled(format!("   {glyph} "), Style::default().fg(status_color)),
        Span::styled(format!("P{} ", t.priority), Style::default().fg(prio_color)),
        Span::styled(format!("{id} "), Style::default().fg(Color::DarkGray)),
        Span::styled(truncate(&t.title, 35), Style::default()),
    ])
}

fn render_add_overlay(frame: &mut Frame, area: Rect, buffer: &str, parent: Option<&str>) {
    let w = area.width.saturating_sub(8).min(80);
    let h = 6u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);

    let hint = match parent {
        Some(p) => format!(" add child of {p}  ·  Enter=create · Esc=cancel "),
        None => " add new task at top level  ·  Enter=create · Esc=cancel ".to_string(),
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                buffer.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
    ];
    let para = ratatui::widgets::Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(hint)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(para, popup);
}

pub fn render_detail_overlay(frame: &mut Frame, area: Rect, task: &Task, prefix: &str) {
    // Center a popup inside the pane area.
    let popup_w = area.width.saturating_sub(8).min(90);
    let popup_h = area.height.saturating_sub(4).min(24);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    frame.render_widget(ratatui::widgets::Clear, popup);

    let id = format!("{}-{}", prefix, task.seq);
    let kind = format!("{:?}", task.kind).to_lowercase();
    let status = format!("{:?}", task.status).to_lowercase();

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!(" {id} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(&task.title, Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![Span::styled(
            format!("  {kind} · P{} · {status}", task.priority),
            Style::default().fg(Color::DarkGray),
        )]),
        Line::from(""),
    ];

    if !task.description.is_empty() {
        lines.push(Line::from(Span::styled(
            "description",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for l in task.description.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
        lines.push(Line::from(""));
    }
    if let Some(a) = task.acceptance.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled(
            "acceptance",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for l in a.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
        lines.push(Line::from(""));
    }
    if let Some(d) = task.design.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled(
            "design",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for l in d.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
        lines.push(Line::from(""));
    }
    if let Some(n) = task.notes.as_ref().filter(|s| !s.is_empty()) {
        lines.push(Line::from(Span::styled(
            "notes",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        for l in n.lines() {
            lines.push(Line::from(format!("  {l}")));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" task detail — Enter/Esc to close ")
        .border_style(Style::default().fg(Color::Cyan));
    let para = ratatui::widgets::Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(para, popup);
}

fn walk(
    id: String,
    depth: usize,
    prefix: &str,
    children_of: &HashMap<String, Vec<String>>,
    by_id: &BTreeMap<String, &Task>,
    visited: &mut HashSet<String>,
    rows: &mut Vec<RenderRow>,
    sort: DagSort,
) {
    if !visited.insert(id.clone()) {
        return;
    }
    let Some(task) = by_id.get(&id).copied() else {
        return;
    };
    let children = children_of.get(&id).cloned().unwrap_or_default();
    let n_children = children.iter().filter(|c| by_id.contains_key(*c)).count();

    rows.push(RenderRow::Task {
        task: task.clone(),
        prefix: prefix.to_string(),
        depth,
        is_root: depth == 0,
        n_children,
    });

    let mut sorted: Vec<&Task> = children
        .iter()
        .filter_map(|c| by_id.get(c).copied())
        .collect();
    sort_tasks(&mut sorted, sort);

    for child in sorted {
        walk(
            child.task_id.clone(),
            depth + 1,
            prefix,
            children_of,
            by_id,
            visited,
            rows,
            sort,
        );
    }
}

fn sort_tasks(tasks: &mut Vec<&Task>, sort: DagSort) {
    match sort {
        DagSort::Priority => tasks.sort_by_key(|t| (t.priority, t.seq)),
        DagSort::Kind => tasks.sort_by_key(|t| (kind_order(&t.kind), t.priority, t.seq)),
        DagSort::Recent => tasks.sort_by_key(|t| std::cmp::Reverse(t.updated_at)),
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn humanize_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 7 * 86400 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / (7 * 86400))
    }
}

/// Painted while the first refresh is in flight. A tiny epic-with-three-children
/// ASCII DAG makes it obvious what's being fetched.
fn render_dag_loading(frame: &mut Frame, area: Rect, title: &str) {
    let dim = Style::default().fg(Color::DarkGray);
    let epic = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let feat = Style::default().fg(Color::Cyan);
    let tsk = Style::default().fg(Color::White);
    let chr = Style::default().fg(Color::DarkGray);
    let hint = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    let art: Vec<Line> = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("╭─────╮", dim)),
        Line::from(vec![
            Span::styled("│  ", dim),
            Span::styled("◉", epic),
            Span::styled("  │", dim),
        ]),
        Line::from(Span::styled("╰──┬──╯", dim)),
        Line::from(Span::styled("   │", dim)),
        Line::from(Span::styled("┌──┼──┐", dim)),
        Line::from(Span::styled("│  │  │", dim)),
        Line::from(vec![
            Span::styled("", dim),
            Span::styled("✚", feat),
            Span::styled("  ", dim),
            Span::styled("○", tsk),
            Span::styled("  ", dim),
            Span::styled("·", chr),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled("growing the task graph…", hint)),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let para = ratatui::widgets::Paragraph::new(art)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(para, area);
}
