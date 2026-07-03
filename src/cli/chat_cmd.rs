//! `ygg chat` — pick-and-send wrapper over `ygg msg send`.
//!
//! Lets you message any agent from any terminal without remembering
//! agent names. Three modes:
//!
//!   ygg chat <body>              — fzf picker over current agents
//!   ygg chat --to <name> <body>  — explicit recipient (alias for `msg send`)
//!   ygg chat --last <body>       — re-use the last recipient I messaged
//!
//! No embedding/LLM-based routing — that contradicts ADR 0015. The
//! picker shows enough context (state · age · persona) to pick by
//! eye, and fzf's own typeahead handles the "I sort of know who"
//! case.
//!
//! Falls back to a numbered text picker if `fzf` isn't on PATH.

use crate::db::DbPool;
use crate::models::agent::AgentRepo;
use chrono::Utc;
use std::io::Write;
use std::process::{Command, Stdio};

pub async fn execute(
    pool: &DbPool,
    from: &str,
    body: &str,
    to: Option<&str>,
    last: bool,
    push: bool,
) -> Result<(), anyhow::Error> {
    if body.trim().is_empty() {
        anyhow::bail!("empty body — pass the message as trailing args");
    }

    let recipient: String = if let Some(t) = to {
        t.to_string()
    } else if last {
        last_recipient(pool, from)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no prior recipient — drop --last and pick"))?
    } else {
        pick_recipient(pool).await?
    };

    super::msg_cmd::send(pool, from, &recipient, body, push).await
}

/// Most recent recipient this `from` agent sent a message to. Read
/// straight from the events table — no extra state file needed.
async fn last_recipient(pool: &DbPool, from: &str) -> Result<Option<String>, anyhow::Error> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        r#"SELECT recipient_agent_id
             FROM events
            WHERE event_kind = 'message'
              AND agent_name = $1
              AND recipient_agent_id IS NOT NULL
            ORDER BY created_at DESC
            LIMIT 1"#,
    )
    .bind(from)
    .fetch_optional(pool)
    .await?;

    let Some((Some(rid),)) = row else {
        return Ok(None);
    };
    let name: Option<String> =
        sqlx::query_scalar("SELECT agent_name FROM agents WHERE agent_id = $1")
            .bind(rid)
            .fetch_optional(pool)
            .await?;
    Ok(name)
}

/// Build a row list (most-recent first) and run the user through a picker.
async fn pick_recipient(pool: &DbPool) -> Result<String, anyhow::Error> {
    let mut agents = AgentRepo::new(pool, crate::db::user_id()).list().await?;
    if agents.is_empty() {
        anyhow::bail!("no registered agents");
    }
    agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    let now = Utc::now();
    let lines: Vec<String> = agents
        .iter()
        .map(|a| {
            let age = humanize_age(now - a.updated_at);
            let persona = a.persona.as_deref().unwrap_or("-");
            // Tab-separated: name in column 1 makes parse-back trivial.
            format!(
                "{}\t{}\t{}\t{}",
                a.agent_name, a.current_state, age, persona
            )
        })
        .collect();

    let picked_line = if which("fzf") {
        run_fzf(&lines)?
    } else {
        run_text_picker(&lines)?
    };

    let name = picked_line
        .split('\t')
        .next()
        .ok_or_else(|| anyhow::anyhow!("picker returned empty selection"))?
        .trim()
        .to_string();
    if name.is_empty() {
        anyhow::bail!("cancelled");
    }
    Ok(name)
}

fn run_fzf(lines: &[String]) -> Result<String, anyhow::Error> {
    let mut child = Command::new("fzf")
        .args([
            "--height=40%",
            "--reverse",
            "--prompt=to> ",
            "--header=  agent  state  age  persona   (Esc to cancel)",
            "--with-nth=1,2,3,4",
            "--delimiter=\t",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("fzf stdin"))?;
        for l in lines {
            writeln!(stdin, "{l}")?;
        }
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("cancelled");
    }
    Ok(String::from_utf8(out.stdout)?.trim_end().to_string())
}

fn run_text_picker(lines: &[String]) -> Result<String, anyhow::Error> {
    eprintln!("  #  agent                state           age   persona");
    for (i, l) in lines.iter().enumerate().take(40) {
        let cols: Vec<&str> = l.split('\t').collect();
        eprintln!(
            "  {:>2}  {:<20} {:<14}  {:<5} {}",
            i,
            cols.first().unwrap_or(&""),
            cols.get(1).unwrap_or(&""),
            cols.get(2).unwrap_or(&""),
            cols.get(3).unwrap_or(&""),
        );
    }
    eprint!("pick #: ");
    std::io::stderr().flush().ok();
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let idx: usize = buf
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("not a number"))?;
    let line = lines
        .get(idx)
        .ok_or_else(|| anyhow::anyhow!("out of range"))?;
    Ok(line.clone())
}

fn which(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn humanize_age(d: chrono::Duration) -> String {
    let s = d.num_seconds();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}
