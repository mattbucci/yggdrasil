//! `ygg init-project` — install / update / remove Yggdrasil's managed
//! integration block in a project's `CLAUDE.md` and `AGENTS.md`.
//!
//! Idempotent via versioned HTML-comment markers. Re-running the same
//! binary version on an already-configured repo is a no-op. Running a
//! newer binary refreshes the block. User content outside the markers
//! is never touched.

use std::path::{Path, PathBuf};

// VERSION 4 (2026-06-07, ADR 0015): retired the embedding / similarity
// retrieval layer. Dropped ygg memory / ygg remember / ygg trace and
// the [ygg memory | ...] injection narrative; Yggdrasil is now a pure
// orchestration layer (tasks, locks, spawn, learnings). Existing
// managed blocks pick this up on next `ygg integrate`.
const VERSION: u32 = 4;
const BEGIN: &str = "<!-- BEGIN YGG INTEGRATION";
const END: &str = "<!-- END YGG INTEGRATION -->";

/// The imperative block injected into CLAUDE.md. Keep this in sync with
/// the guidance surfaced by `ygg prime`.
const CLAUDE_BLOCK: &str = r#"## Yggdrasil Agent Coordination

This project uses **Yggdrasil** (`ygg`) for resource coordination and issue
tracking across parallel Claude Code agents. The SessionStart, UserPromptSubmit,
Stop, PreCompact, and PreToolUse hooks are active — they prime agent context,
deliver agent-to-agent messages, record token stats, and track state in
the embedded SQLite database. You will see prime output at the top of each session
(`<!-- ygg:prime -->`).

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in the current repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title" --kind <k> --priority <0-4>   # See priority/kind values below
ygg task claim <ref>                        # Take a task (assign + in_progress)
ygg task show <ref>                         # Full detail for <prefix>-NNN or UUID
ygg task close <ref> [--reason "..."]       # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg task dupes [--all] [--limit N]          # Probable duplicate pairs (string similarity)
```

### Task field values (important — no guessing)

- `--priority <0..4>` — **0 = critical, 1 = high, 2 = medium, 3 = low, 4 = backlog**.
  Also accepts `P0`..`P4`. Do NOT pass strings like "high" / "medium" / "low".
- `--kind <task|bug|feature|chore|epic>` — one of these five. Default is `task`.
- `--status <open|in_progress|blocked|closed>` — for filtering / transitions.
- `--label <a,b,c>` — comma-separated labels. Repeatable.
- `<ref>` is either `<prefix>-<N>` (e.g. `yggdrasil-42`) or a UUID.

### Ticket body structure

Tickets are read by other agents picking up the work. Bodies have **four
sections in this order**, separated by blank lines. No PR-prose walls.

1. **Why** — one sentence. The trigger or observation that justifies the
   work. Cite the source: `Adversarial review:`, `Codebase audit:`,
   `Bench scenario X:`, `Research thread Y:`, `Incident on <date>:`.
2. **What** — one sentence. The concrete change. Use imperative voice.
3. **Acceptance:** — a bulleted list of testable conditions. Each bullet
   is something an autonomous agent can verify when claiming the task as
   done. Avoid vague verbs ("improve", "consider"); pin SHAs, file paths,
   commands, numeric thresholds.
4. **Refs:** *(optional)* — research thread tag, related ticket
   (`yggdrasil-NN`), external URL, ADR number.

Example:

```text
Adversarial review: src/db.rs max_connections(10) starves a fleet of
50+ active agents.

Bump default to 32 and accept YGG_DB_POOL env override.

Acceptance:
- src/db.rs default = 32; YGG_DB_POOL parses to u32, falls back on error
- CLAUDE.md documents the knob in the Build & Test section
- cargo check --all-targets clean

Refs: yggdrasil-141, adversarial-review note 2026-04-23
```

### Terse for AI-tracking fields

When writing content that only agents consume — `ygg task create`
titles/descriptions/acceptance/design/notes, `ygg learn` rules — be
terse. Drop filler (really/just/basically/actually/very). Drop articles
(`a`/`an`/`the`) when meaning survives.
Prefer one sentence per field where content allows. **Preserve
verbatim**: identifiers (snake_case, CamelCase), paths, commands,
numbers, URLs, and modal keywords (always/never/must/should/cannot/
don't/may/shall).

Does **NOT** apply to commit messages, PR descriptions, code comments,
or chat responses — those are human-facing and full fidelity is correct.

Example:
```bash
ygg task create "fix migration ordering" --kind bug --priority 1 --label migrations,sqlx

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock coordination.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in a native TodoList; cross-session work lives in `ygg task`.
- **Do NOT** use `bd` / beads. This project uses `ygg task` instead.

### Dedup + learnings

- **Before `ygg task create`** → run `ygg task dupes --limit 5` (or
  `--all` for cross-repo). Dupe detection is token-set string similarity
  on title+description (no embeddings). If a pair surfaces near the top,
  prefer claiming/extending the existing task over filing a new one.

- **For recurring engineering corrections** → `ygg learn add` with a file
  glob. Learnings re-fire deterministically when an agent touches a
  matching file — use for "every time someone edits X, also check Y"
  rules. Retrieval is SQL predicates on (repo, file_glob, rule_id), not
  similarity.

## Session Completion

Work is NOT complete until `git push` succeeds.

1. Run quality gates if code changed (tests, linters, build/type-check).
2. Release any locks you still hold (`ygg lock list` → `ygg lock release <key>`).
3. Push:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
4. If push fails, resolve and retry until it succeeds.

**Never** stop before pushing; **never** say "ready to push when you are" — you push.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
"#;

/// The block for AGENTS.md — same semantics, slightly terser narrative,
/// intended for non-Claude CLI agents that read AGENTS.md instead of CLAUDE.md.
const AGENTS_BLOCK: &str = r#"## Yggdrasil Coordination

This project uses **Yggdrasil** (`ygg`) for resource coordination and issue
tracking across parallel agents. Hooks fire at Claude Code lifecycle events;
you do not invoke them manually.

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in this repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title"                     # New task
ygg task claim <ref>                        # Take a task
ygg task close <ref>                        # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg task dupes [--all]                      # Probable duplicate pairs (string similarity)

ygg status                                  # Agents + outstanding locks
ygg lock acquire <key> / release <key> / list
ygg spawn --task "..."                      # Parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over another agent
ygg logs --follow                           # Live event stream
```

### Rules

- Acquire a lock before editing a resource another agent might touch. Release when done.
- Prefer `ygg spawn` over a native Task/Agent tool for parallel work.
- Check `ygg status` before assuming you're working alone.
- Use `ygg task` for cross-session work tracking.
- Before `ygg task create`, run `ygg task dupes` to surface near-dups.
- For recurring engineering corrections, `ygg learn add` with a file glob.
- Do NOT use `bd` / beads.

### Ticket body structure

Tickets are read by other agents picking up the work. Bodies have four
sections in this order, separated by blank lines: **Why** (one sentence,
trigger or observation), **What** (one sentence, imperative change),
**Acceptance:** (bulleted testable conditions, no vague verbs — pin
SHAs, paths, commands, numeric thresholds), **Refs:** (optional —
related ticket, ADR, URL).

Be terse in `ygg task create` titles/descriptions/acceptance/notes and
`ygg learn` rules. Drop filler and articles when meaning survives.
Preserve identifiers, paths, commands, numbers, URLs, and modal
keywords (always/never/must/should/cannot/don't) verbatim. Does NOT
apply to commit messages, PR descriptions, or chat — those stay
human-prose.

## Session Completion

Work is not complete until `git push` succeeds. Release held locks, run quality gates, rebase, push, verify `git status` shows up-to-date.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
"#;

/// Compute a stable content hash of the template at this version.
/// We only need to detect "the block needs updating"; any change to the
/// template bytes will flip this.
fn block_hash(body: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn begin_marker(hash: &str) -> String {
    format!("{BEGIN} v:{VERSION} hash:{hash} -->")
}

enum FoundBlock {
    Missing,
    UpToDate,
    Stale,
}

/// Returns the [begin..=end] byte range of an existing managed block,
/// or None if not present.
fn find_block(content: &str) -> Option<(usize, usize)> {
    let start = content.find(BEGIN)?;
    let end_from = start + BEGIN.len();
    let rel_end = content[end_from..].find(END)?;
    let end = end_from + rel_end + END.len();
    Some((start, end))
}

fn classify(content: &str, current_hash: &str) -> FoundBlock {
    match find_block(content) {
        None => FoundBlock::Missing,
        Some((s, e)) => {
            let block = &content[s..e];
            if block.contains(&format!("hash:{current_hash}")) {
                FoundBlock::UpToDate
            } else {
                FoundBlock::Stale
            }
        }
    }
}

/// Install or update the block in `content`. Returns the new content.
fn install_block(content: &str, body: &str) -> String {
    let hash = block_hash(body);
    // The managed block intentionally restates sections (Quick Reference,
    // Rules, Session Completion, …) that hand-authored content above it may
    // also use, so it stays self-contained. Wrap it in a scoped MD024 disable
    // so markdownlint doesn't flag those by-design duplicate headings; the
    // hand-authored region above the block is still linted.
    let managed = format!(
        "{}\n<!-- markdownlint-disable MD024 -->\n{}\n<!-- markdownlint-enable MD024 -->\n{}",
        begin_marker(&hash),
        body.trim_end(),
        END
    );

    if let Some((s, e)) = find_block(content) {
        // Replace existing block in place.
        let mut out = String::with_capacity(content.len() + managed.len());
        out.push_str(&content[..s]);
        out.push_str(&managed);
        out.push_str(&content[e..]);
        out
    } else if content.trim().is_empty() {
        // Empty file: block only.
        format!("{managed}\n")
    } else {
        // Append, preceded by a blank line.
        let mut out = String::with_capacity(content.len() + managed.len() + 2);
        out.push_str(content.trim_end());
        out.push_str("\n\n");
        out.push_str(&managed);
        out.push('\n');
        out
    }
}

/// Remove the managed block from `content` (if present). Returns the new content.
fn remove_block(content: &str) -> String {
    match find_block(content) {
        None => content.to_string(),
        Some((s, e)) => {
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..s]);
            // Eat a trailing newline from the block end if present
            let rest = &content[e..];
            out.push_str(rest.trim_start_matches('\n'));
            // Collapse leading blank lines the user left before the block
            out.trim_end().to_string() + "\n"
        }
    }
}

/// Options that control which files `install` / `remove` touch.
#[derive(Debug, Clone, Copy, Default)]
pub struct IntegrateOpts {
    /// When `true`, skip writing AGENTS.md. Used by `--global` so the
    /// directives only land in `~/.claude/CLAUDE.md`. Local installs
    /// default this to `false`.
    pub skip_agents_md: bool,
}

/// Public entry point. Target files are `<dir>/CLAUDE.md` and (unless
/// `opts.skip_agents_md`) `<dir>/AGENTS.md`.
pub fn install(dir: &Path) -> Result<InstallReport, anyhow::Error> {
    install_with(dir, IntegrateOpts::default())
}

pub fn install_with(dir: &Path, opts: IntegrateOpts) -> Result<InstallReport, anyhow::Error> {
    let mut report = InstallReport::default();
    let mut targets: Vec<(&str, &str)> = vec![("CLAUDE.md", CLAUDE_BLOCK)];
    if !opts.skip_agents_md {
        targets.push(("AGENTS.md", AGENTS_BLOCK));
    }
    for (filename, body) in targets {
        let path = dir.join(filename);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let hash = block_hash(body);
        let action = match (classify(&existing, &hash), path.exists()) {
            (FoundBlock::UpToDate, _) => ActionTaken::Unchanged,
            (FoundBlock::Stale, _) => ActionTaken::Updated,
            (FoundBlock::Missing, true) => ActionTaken::Appended,
            (FoundBlock::Missing, false) => ActionTaken::Created,
        };
        if !matches!(action, ActionTaken::Unchanged) {
            // mkdir -p the parent so `--global` works on a fresh
            // machine where `~/.claude` doesn't exist yet.
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            let new_content = install_block(&existing, body);
            std::fs::write(&path, new_content)?;
        }
        report.files.push((path, action));
    }
    Ok(report)
}

pub fn remove(dir: &Path) -> Result<InstallReport, anyhow::Error> {
    remove_with(dir, IntegrateOpts::default())
}

pub fn remove_with(dir: &Path, opts: IntegrateOpts) -> Result<InstallReport, anyhow::Error> {
    let mut report = InstallReport::default();
    let mut targets: Vec<&str> = vec!["CLAUDE.md"];
    if !opts.skip_agents_md {
        targets.push("AGENTS.md");
    }
    for filename in targets {
        let path = dir.join(filename);
        if !path.exists() {
            report.files.push((path, ActionTaken::Unchanged));
            continue;
        }
        let existing = std::fs::read_to_string(&path)?;
        let action = match find_block(&existing) {
            None => ActionTaken::Unchanged,
            Some(_) => ActionTaken::Removed,
        };
        if matches!(action, ActionTaken::Removed) {
            let new_content = remove_block(&existing);
            // If after removal only the block-less file is empty → delete file.
            if new_content.trim().is_empty() {
                std::fs::remove_file(&path)?;
            } else {
                std::fs::write(&path, new_content)?;
            }
        }
        report.files.push((path, action));
    }
    Ok(report)
}

/// True iff a managed Yggdrasil block exists in `<dir>/CLAUDE.md`.
/// `--global` consults this on cwd to refuse installing globally when
/// the directives would double-fire from a per-project block.
pub fn has_managed_block(dir: &Path) -> bool {
    let path = dir.join("CLAUDE.md");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    find_block(&content).is_some()
}

/// Check whether either target file already exists and has content outside
/// a managed block — used to decide whether `ygg init` should auto-invoke.
pub fn has_any_content(cwd: &Path) -> bool {
    for filename in ["CLAUDE.md", "AGENTS.md"] {
        let path = cwd.join(filename);
        if path.exists()
            && let Ok(c) = std::fs::read_to_string(&path)
        {
            // Content exists if there's anything outside the managed block.
            let stripped = remove_block(&c);
            if !stripped.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub files: Vec<(PathBuf, ActionTaken)>,
}

#[derive(Debug, Clone, Copy)]
pub enum ActionTaken {
    Created,
    Appended,
    Updated,
    Unchanged,
    Removed,
}

impl std::fmt::Display for ActionTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Appended => write!(f, "block appended"),
            Self::Updated => write!(f, "block updated"),
            Self::Unchanged => write!(f, "up to date"),
            Self::Removed => write!(f, "block removed"),
        }
    }
}

pub fn print_report(report: &InstallReport) {
    for (path, action) in &report.files {
        let display = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        println!("  {display:<12} {action}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_into_empty() {
        let body = "hello";
        let out = install_block("", body);
        assert!(out.starts_with(BEGIN));
        assert!(out.contains("hello"));
        assert!(out.trim_end().ends_with(END));
    }

    #[test]
    fn install_appends_when_file_has_content() {
        let existing = "# My Project\n\nSome notes.\n";
        let out = install_block(existing, "block");
        assert!(out.starts_with("# My Project"));
        assert!(out.contains(BEGIN));
        assert!(out.contains("block"));
    }

    #[test]
    fn install_replaces_existing_block() {
        let start = install_block("# Title\n", "old body");
        let replaced = install_block(&start, "new body");
        assert!(replaced.contains("new body"));
        assert!(!replaced.contains("old body"));
        assert!(replaced.starts_with("# Title"));
    }

    #[test]
    fn remove_strips_block() {
        let with_block = install_block("# Title\n\nUser text.\n", "managed");
        let stripped = remove_block(&with_block);
        assert!(!stripped.contains("managed"));
        assert!(stripped.contains("# Title"));
        assert!(stripped.contains("User text."));
    }

    #[test]
    fn classify_detects_same_version() {
        let body = "same";
        let h = block_hash(body);
        let content = install_block("", body);
        assert!(matches!(classify(&content, &h), FoundBlock::UpToDate));
    }

    #[test]
    fn classify_detects_stale_hash() {
        let content = install_block("", "old");
        let new_hash = block_hash("new");
        assert!(matches!(classify(&content, &new_hash), FoundBlock::Stale));
    }
}
