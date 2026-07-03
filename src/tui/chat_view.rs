//! Chat panel — real-time message thread view with compose overlay.
//!
//! Shows all recent messages (directed + broadcast) in a scrollable
//! list. Compose with `c` (type `@agent msg` for directed, or just
//! `msg` for broadcast). Claim unclaimed broadcasts with Enter.
//!
//! Features:
//! - **Detail overlay** (Enter on a directed msg): full body, sender,
//!   recipient, timestamp. Esc closes.
//! - **Filter** (`/`): type to filter messages by from_name, to_name,
//!   or body (case-insensitive). Esc clears.
//! - **Unread count**: tracks messages seen while the view is active;
//!   `unread_count()` reports unseen messages for tab badges.

use crate::db::DbPool;
use chrono::{DateTime, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::cli::msg_cmd;

#[derive(Debug, Clone)]
struct ChatMsg {
    id: String,
    from_name: String,
    to_name: Option<String>,
    body: String,
    created_at: DateTime<Utc>,
}

pub struct ChatView {
    messages: Vec<ChatMsg>,
    state: ListState,
    loaded: bool,
    pub compose: Option<ComposeState>,
    pub flash: Option<String>,
    /// UUID of the message shown in the detail overlay (Enter on directed msg).
    detail: Option<String>,
    /// Active filter string — when `Some`, only matching messages are shown.
    filter: Option<String>,
    /// True when the filter input line is focused (typing mode).
    filter_editing: bool,
    /// Timestamp of the newest message seen (updated when view is active).
    last_seen_ts: Option<DateTime<Utc>>,
}

pub struct ComposeState {
    pub buf: String,
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            state: ListState::default(),
            loaded: false,
            compose: None,
            flash: None,
            detail: None,
            filter: None,
            filter_editing: false,
            last_seen_ts: None,
        }
    }

    pub fn composing(&self) -> bool {
        self.compose.is_some()
    }

    pub fn compose_begin(&mut self) {
        self.compose = Some(ComposeState { buf: String::new() });
    }

    pub fn compose_cancel(&mut self) {
        self.compose = None;
    }

    pub fn compose_push(&mut self, c: char) {
        if let Some(cs) = self.compose.as_mut() {
            cs.buf.push(c);
        }
    }

    pub fn compose_pop(&mut self) {
        if let Some(cs) = self.compose.as_mut() {
            cs.buf.pop();
        }
    }

    /// Parse compose buffer and send. `@agent rest` = directed; plain text = broadcast.
    pub async fn compose_commit(&mut self, pool: &DbPool, from_agent: &str) {
        let Some(cs) = self.compose.take() else {
            return;
        };
        let input = cs.buf.trim().to_string();
        if input.is_empty() {
            self.flash = Some("cancelled (empty)".into());
            return;
        }

        if let Some(rest) = input.strip_prefix('@') {
            if let Some(space) = rest.find(' ') {
                let to = &rest[..space];
                let body = rest[space + 1..].trim();
                if body.is_empty() {
                    self.flash = Some("cancelled (empty body)".into());
                    return;
                }
                match msg_cmd::send(pool, from_agent, to, body, true).await {
                    Ok(()) => self.flash = Some(format!("sent to {to}")),
                    Err(e) => self.flash = Some(format!("send failed: {e}")),
                }
            } else {
                self.flash = Some("usage: @agent message".into());
            }
        } else {
            match msg_cmd::broadcast(pool, from_agent, &input).await {
                Ok(_id) => self.flash = Some("broadcast sent".into()),
                Err(e) => self.flash = Some(format!("broadcast failed: {e}")),
            }
        }
    }

    // ── detail overlay ──────────────────────────────────────────────

    /// True when the detail popup is visible.
    pub fn detail_open(&self) -> bool {
        self.detail.is_some()
    }

    /// Open detail overlay for the currently selected message, but only
    /// if it is a directed message (broadcasts use Enter to claim).
    pub fn detail_selected(&mut self) {
        let Some(idx) = self.state.selected() else {
            return;
        };
        let indices = self.visible_indices();
        if let Some(&real) = indices.get(idx)
            && let Some(msg) = self.messages.get(real)
            && msg.to_name.is_some()
        {
            self.detail = Some(msg.id.clone());
        }
    }

    pub fn detail_close(&mut self) {
        self.detail = None;
    }

    // ── filter ──────────────────────────────────────────────────────

    /// True when the filter input line is focused (typing mode).
    pub fn filter_editing(&self) -> bool {
        self.filter_editing
    }

    /// Enter filter-editing mode. If a filter is already set, resume editing it.
    pub fn filter_begin(&mut self) {
        self.filter_editing = true;
        if self.filter.is_none() {
            self.filter = Some(String::new());
        }
    }

    pub fn filter_push(&mut self, c: char) {
        if let Some(f) = self.filter.as_mut() {
            f.push(c);
        }
    }

    pub fn filter_pop(&mut self) {
        if let Some(f) = self.filter.as_mut() {
            f.pop();
        }
    }

    /// Stop editing the filter. If the text is empty, clear the filter entirely.
    pub fn filter_accept(&mut self) {
        self.filter_editing = false;
        if self.filter.as_ref().is_none_or(|f| f.is_empty()) {
            self.filter = None;
        }
        // Reset selection to stay in bounds of the now-visible list.
        self.clamp_selection();
    }

    /// Clear the filter and exit editing mode.
    pub fn filter_clear(&mut self) {
        self.filter = None;
        self.filter_editing = false;
        self.clamp_selection();
    }

    // ── unread count ────────────────────────────────────────────────

    /// Number of messages that arrived since the user last viewed the Chat pane.
    pub fn unread_count(&self) -> usize {
        match self.last_seen_ts {
            Some(ts) => self.messages.iter().filter(|m| m.created_at > ts).count(),
            None => self.messages.len(),
        }
    }

    /// Mark all current messages as seen (call when Chat pane is active).
    pub fn mark_seen(&mut self) {
        if let Some(newest) = self.messages.iter().map(|m| m.created_at).max() {
            self.last_seen_ts = Some(newest);
        }
    }

    // ── claim / scroll / refresh ────────────────────────────────────

    /// Claim the selected broadcast message.
    pub async fn claim_selected(&mut self, pool: &DbPool, agent_name: &str) {
        let Some(idx) = self.state.selected() else {
            return;
        };
        let indices = self.visible_indices();
        let Some(&real) = indices.get(idx) else {
            return;
        };
        let Some(msg) = self.messages.get(real) else {
            return;
        };
        if msg.to_name.is_some() {
            self.flash = Some("not a broadcast — already directed".into());
            return;
        }
        match msg_cmd::claim_broadcast(pool, msg.id.clone(), agent_name).await {
            Ok(()) => self.flash = Some(format!("claimed by {agent_name}")),
            Err(e) => self.flash = Some(format!("claim failed: {e}")),
        }
    }

    pub fn select_next(&mut self) {
        let count = self.visible_indices().len();
        if count > 0 {
            let i = self.state.selected().unwrap_or(0);
            self.state.select(Some((i + 1).min(count - 1)));
        }
    }

    pub fn select_prev(&mut self) {
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(i.saturating_sub(1)));
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        self.loaded = true;
        let msgs = msg_cmd::all_messages(pool, 24, 200).await?;
        self.messages = msgs
            .into_iter()
            .map(|m| ChatMsg {
                id: m.id,
                from_name: m.from_name,
                to_name: m.to_name,
                body: m.body,
                created_at: m.created_at,
            })
            .collect();
        // all_messages returns newest-first; reverse for chronological display
        self.messages.reverse();

        self.clamp_selection();
        Ok(())
    }

    // ── render ───────────────────────────────────────────────────────

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        if !self.loaded {
            let p = Paragraph::new(" loading messages… ")
                .block(Block::default().borders(Borders::ALL).title(" Chat "));
            frame.render_widget(p, area);
            return;
        }

        let visible = self.visible_indices();

        if self.messages.is_empty() || (visible.is_empty() && self.filter.is_none()) {
            let hint = if let Some(ref f) = self.flash {
                format!(" {f} ")
            } else {
                " no messages in last 24h · press [c] to compose ".into()
            };
            let p =
                Paragraph::new(hint).block(Block::default().borders(Borders::ALL).title(" Chat "));
            frame.render_widget(p, area);
            if self.compose.is_some() {
                self.render_compose_overlay(frame, area);
            }
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);

        let items: Vec<ListItem> = visible
            .iter()
            .map(|&i| {
                let m = &self.messages[i];
                let ts = m.created_at.format("%H:%M");
                let arrow = match &m.to_name {
                    Some(to) => format!("{} → {}", m.from_name, to),
                    None => format!("{} → ALL", m.from_name),
                };
                let body = truncate(&m.body, 60);
                let style = if m.to_name.is_none() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{ts}  "), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{arrow:<28}"), style),
                    Span::raw(body),
                ]))
            })
            .collect();

        let title = if self.filter.is_some() {
            format!(
                " Chat · {}/{} msg(s) · 24h ",
                visible.len(),
                self.messages.len()
            )
        } else {
            format!(" Chat · {} msg(s) · 24h ", self.messages.len())
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(list, chunks[0], &mut self.state);

        // Hint bar
        let hint = if let Some(ref f) = self.flash {
            f.clone()
        } else if self.filter_editing {
            let txt = self.filter.as_deref().unwrap_or("");
            format!(" filter: {txt}█  (Enter=accept  Esc=clear)")
        } else if let Some(ref f) = self.filter {
            format!(" filter: {f}  (/=edit  Esc=clear)")
        } else {
            " c=compose  ↑↓=scroll  Enter=detail/claim  /=filter".into()
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );

        if self.compose.is_some() {
            self.render_compose_overlay(frame, area);
        }
        if self.detail.is_some() {
            self.render_detail_overlay(frame, area);
        }
    }

    // ── overlays ────────────────────────────────────────────────────

    fn render_compose_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(cs) = &self.compose else {
            return;
        };
        let w = area.width.min(60);
        let h = 5u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let popup = Rect::new(x, y, w, h);

        Clear.render(popup, frame.buffer_mut());

        let display = format!("{}█", cs.buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" compose (@agent msg = directed, plain = broadcast) ")
            .border_style(Style::default().fg(Color::Cyan));
        let p = Paragraph::new(display)
            .block(block)
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false });
        frame.render_widget(p, popup);
    }

    fn render_detail_overlay(&self, frame: &mut Frame, area: Rect) {
        let Some(msg) = self
            .messages
            .iter()
            .find(|m| self.detail == Some(m.id.clone()))
        else {
            return;
        };

        let w = area.width.min(72);
        let h = area.height.min(16).max(8);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let popup = Rect::new(x, y, w, h);

        Clear.render(popup, frame.buffer_mut());

        let to_label = msg.to_name.as_deref().unwrap_or("ALL");
        let ts = msg.created_at.format("%Y-%m-%d %H:%M:%S UTC");
        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("From: ", Style::default().fg(Color::Cyan)),
                Span::raw(msg.from_name.clone()),
            ]),
            Line::from(vec![
                Span::styled("  To: ", Style::default().fg(Color::Cyan)),
                Span::raw(to_label.to_string()),
            ]),
            Line::from(vec![
                Span::styled("Time: ", Style::default().fg(Color::Cyan)),
                Span::raw(ts.to_string()),
            ]),
            Line::from(""),
        ];
        // Wrap long body into lines that fit the popup width (minus borders).
        // Uses char iteration to avoid panicking on multi-byte UTF-8.
        let body_width = (w as usize).saturating_sub(4).max(1);
        for raw_line in msg.body.lines() {
            let mut chunk = String::new();
            let mut count = 0usize;
            for ch in raw_line.chars() {
                if count == body_width {
                    lines.push(Line::from(std::mem::take(&mut chunk)));
                    count = 0;
                }
                chunk.push(ch);
                count += 1;
            }
            lines.push(Line::from(chunk));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Message Detail ")
            .title_bottom(Line::from(vec![Span::styled(
                "  Esc close  ",
                Style::default().fg(Color::DarkGray),
            )]))
            .border_style(Style::default().fg(Color::Green));
        let p = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(p, popup);
    }

    // ── helpers ─────────────────────────────────────────────────────

    /// Return indices into `self.messages` that pass the current filter.
    fn visible_indices(&self) -> Vec<usize> {
        match &self.filter {
            None => (0..self.messages.len()).collect(),
            Some(f) => {
                let needle = f.to_lowercase();
                (0..self.messages.len())
                    .filter(|&i| {
                        let m = &self.messages[i];
                        m.from_name.to_lowercase().contains(&needle)
                            || m.to_name
                                .as_ref()
                                .is_some_and(|t| t.to_lowercase().contains(&needle))
                            || m.body.to_lowercase().contains(&needle)
                    })
                    .collect()
            }
        }
    }

    /// Keep the selection index within bounds of the visible list.
    fn clamp_selection(&mut self) {
        let count = self.visible_indices().len();
        if count == 0 {
            self.state.select(None);
        } else {
            let i = self.state.selected().unwrap_or(0);
            self.state.select(Some(i.min(count - 1)));
        }
    }
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
