//! Locks tab — full table of outstanding resource leases. The dashboard
//! only shows a summary count; this view shows holder, age, TTL, and
//! whether the holder is still alive. Enter releases the selected lock.

use crate::db::DbPool;
use chrono::Utc;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::HashMap;

use crate::lock::{LockManager, ResourceLock};
use crate::models::agent::AgentRepo;

pub struct LocksView {
    pub locks: Vec<ResourceLock>,
    pub agent_name_by_id: HashMap<String, String>,
    /// Agent-ids whose most-recent activity was within the alive window.
    /// Drives the alive/stale column without a separate roundtrip.
    pub alive_ids: std::collections::HashSet<String>,
    pub selected: usize,
    pub flash: Option<String>,
}

impl Default for LocksView {
    fn default() -> Self {
        Self::new()
    }
}

impl LocksView {
    pub fn new() -> Self {
        Self {
            locks: vec![],
            agent_name_by_id: HashMap::new(),
            alive_ids: Default::default(),
            selected: 0,
            flash: None,
        }
    }

    pub async fn refresh(&mut self, pool: &DbPool, ttl_secs: u64) -> Result<(), anyhow::Error> {
        let lock_mgr = LockManager::new(pool, ttl_secs, crate::db::user_id());
        let mut locks = lock_mgr.list_all().await?;
        let now = Utc::now();
        locks.retain(|l| (l.expires_at - now).num_seconds() > 0);
        // Oldest holds first — the "who's stuck?" rows rise to the top.
        locks.sort_by_key(|l| (now - l.acquired_at).num_seconds());
        locks.reverse();
        self.locks = locks;

        let agent_repo = AgentRepo::new(pool, crate::db::user_id());
        let agents = agent_repo.list_all().await?;
        self.agent_name_by_id = agents
            .iter()
            .map(|a| (a.agent_id.clone(), a.agent_name.clone()))
            .collect();
        let cutoff = now - chrono::Duration::minutes(10);
        self.alive_ids = agents
            .iter()
            .filter(|a| a.updated_at >= cutoff)
            .map(|a| a.agent_id.clone())
            .collect();

        if self.selected >= self.locks.len() && !self.locks.is_empty() {
            self.selected = self.locks.len() - 1;
        }
        Ok(())
    }

    pub fn select_next(&mut self) {
        if !self.locks.is_empty() {
            self.selected = (self.selected + 1).min(self.locks.len() - 1);
        }
    }
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Release the selected lock. Returns the flash message the caller
    /// should display. Also triggers a refresh so the row disappears.
    pub async fn release_selected(&mut self, pool: &DbPool, ttl_secs: u64) {
        let Some(lock) = self.locks.get(self.selected).cloned() else {
            return;
        };
        let lock_mgr = LockManager::new(pool, ttl_secs, crate::db::user_id());
        match lock_mgr.release(&lock.resource_key, &lock.agent_id).await {
            Ok(()) => self.flash = Some(format!("released {}", short_resource(&lock.resource_key))),
            Err(e) => self.flash = Some(format!("release failed: {e}")),
        }
        let _ = self.refresh(pool, ttl_secs).await;
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.locks.is_empty() {
            let msg = match self.flash.as_deref() {
                Some(f) => format!("  · {f}"),
                None => "  · no active locks".to_string(),
            };
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(msg, Style::default().fg(Color::DarkGray))),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Locks  ·  r=release  ·  ↑↓ "),
            );
            frame.render_widget(para, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("RESOURCE").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from("HOLDER"),
            Cell::from("HOLDER_STATE"),
            Cell::from("HELD_FOR"),
            Cell::from("TTL"),
        ])
        .height(1);

        let now = Utc::now();
        let selected = self.selected;
        let rows: Vec<Row> = self
            .locks
            .iter()
            .enumerate()
            .map(|(i, lock)| {
                let style = if i == selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let agent = self
                    .agent_name_by_id
                    .get(&lock.agent_id)
                    .cloned()
                    .unwrap_or_else(|| format!("{}…", &lock.agent_id.to_string()[..8]));
                let alive = self.alive_ids.contains(&lock.agent_id);
                let (state_glyph, state_color) = if alive {
                    ("alive", Color::Green)
                } else {
                    ("stale", Color::Yellow)
                };
                let held = (now - lock.acquired_at).num_seconds().max(0);
                let ttl = (lock.expires_at - now).num_seconds().max(0);

                Row::new(vec![
                    Cell::from(short_resource(&lock.resource_key)),
                    Cell::from(agent),
                    Cell::from(state_glyph).style(Style::default().fg(state_color)),
                    Cell::from(humanize_duration(held)),
                    Cell::from(format!("{ttl}s")),
                ])
                .style(style)
            })
            .collect();

        let title = match self.flash.as_deref() {
            Some(f) => format!(" Locks ({}) · {} · r=release ", self.locks.len(), f),
            None => format!(" Locks ({}) · r=release · ↑↓ ", self.locks.len()),
        };
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(40),
                Constraint::Percentage(20),
                Constraint::Percentage(12),
                Constraint::Percentage(14),
                Constraint::Percentage(14),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_widget(table, area);
    }
}

fn short_resource(s: &str) -> String {
    if s.len() <= 48 {
        return s.to_string();
    }
    let tail = &s[s.len().saturating_sub(44)..];
    format!("…{tail}")
}

fn humanize_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}
