use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Find the newest Claude Code JSONL transcript for the current working
/// directory, matching how Claude Code slugifies project paths under
/// `~/.claude/projects/<slug>/`.
pub fn find_latest_transcript() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let slug = cwd.to_string_lossy().replace('/', "-");
    let home = std::env::var("HOME").ok()?;
    let project_dir = PathBuf::from(&home).join(".claude/projects").join(&slug);
    if !project_dir.exists() {
        return None;
    }
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&project_dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(mtime) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        match &newest {
            None => newest = Some((mtime, p)),
            Some((t, _)) if mtime > *t => newest = Some((mtime, p)),
            _ => {}
        }
    }
    newest.map(|(_, p)| p.to_string_lossy().to_string())
}

/// Token usage from a single Claude Code assistant message.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// A parsed turn from a JSONL transcript.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: String,
    pub usage: TokenUsage,
    pub tool_names: Vec<String>,
    pub message_id: String,
}

#[derive(Deserialize)]
struct JsonlMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    usage: Option<JsonlUsage>,
    #[serde(default)]
    message: Option<JsonlNestedMessage>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Deserialize)]
struct JsonlNestedMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    usage: Option<JsonlUsage>,
}

#[derive(Deserialize, Clone)]
struct JsonlUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

/// Discover Claude Code session files for a project.
pub fn discover_sessions(_project_path: &Path) -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/vscode".into());
    let claude_dir = Path::new(&home).join(".claude").join("projects");

    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&claude_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Ok(files) = std::fs::read_dir(&path)
            {
                for file in files.flatten() {
                    let fp = file.path();
                    if fp.extension().is_some_and(|e| e == "jsonl") {
                        sessions.push(fp);
                    }
                }
            }
        }
    }

    sessions
}

/// Parse a single JSONL session file into turns.
pub fn parse_session(path: &Path) -> Vec<Turn> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut turns = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    for line in content.lines() {
        let msg: JsonlMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Dedup by message ID
        let msg_id = msg.uuid.unwrap_or_default();
        if !msg_id.is_empty() && !seen_ids.insert(msg_id.clone()) {
            continue;
        }

        // Check top-level usage first, fall back to message.usage.
        let raw_usage = msg
            .usage
            .or_else(|| msg.message.as_ref().and_then(|m| m.usage.clone()));
        let usage = raw_usage
            .map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_write: u.cache_creation_input_tokens,
            })
            .unwrap_or_default();

        // Prefer top-level role, fall back to message.role.
        let role = if msg.role.is_empty() {
            msg.message
                .as_ref()
                .and_then(|m| m.role.clone())
                .unwrap_or_default()
        } else {
            msg.role
        };

        // Extract tool names from content array
        let tool_names = extract_tool_names(&msg.content);

        turns.push(Turn {
            role,
            usage,
            tool_names,
            message_id: msg_id,
        });
    }

    turns
}

/// Extract tool_use names from a content array.
fn extract_tool_names(content: &Option<serde_json::Value>) -> Vec<String> {
    let Some(arr) = content.as_ref().and_then(|c| c.as_array()) else {
        return vec![];
    };

    arr.iter()
        .filter_map(|item| {
            if item.get("type")?.as_str()? == "tool_use" {
                item.get("name")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Aggregate total usage from a set of turns.
pub fn aggregate_usage(turns: &[Turn]) -> TokenUsage {
    let mut total = TokenUsage::default();
    for turn in turns {
        total.input_tokens += turn.usage.input_tokens;
        total.output_tokens += turn.usage.output_tokens;
        total.cache_read += turn.usage.cache_read;
        total.cache_write += turn.usage.cache_write;
    }
    total
}
