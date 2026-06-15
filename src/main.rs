use clap::{Parser, Subcommand};

/// Resolve the canonical agent name. Explicit `--agent` wins; falls back to
/// YGG_AGENT_NAME env, then the current directory's basename, then "ygg".
fn resolve_agent_arg(agent: Option<String>) -> String {
    agent
        .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "ygg".to_string())
        })
}

/// Accept priority as either an integer (0..=4) or a beads-style `P0`..`P4`.
/// Agents used to bd often guess the `P2` form; this keeps them honest
/// without forcing a migration.
fn parse_priority(s: &str) -> Result<i16, String> {
    let trimmed = s.trim();
    let numeric = if let Some(rest) = trimmed.strip_prefix(['P', 'p']) {
        rest
    } else {
        trimmed
    };
    let n: i16 = numeric
        .parse()
        .map_err(|_| format!("priority must be 0..=4 or P0..P4 (got '{s}')"))?;
    if !(0..=4).contains(&n) {
        return Err(format!(
            "priority must be between 0 (critical) and 4 (backlog); got {n}"
        ));
    }
    Ok(n)
}

#[derive(Parser)]
#[command(
    name = "ygg",
    version,
    about = "Yggdrasil — High-density agent orchestrator"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start ygg — open tmux session with dashboard (default when no command given)
    Up,

    /// Bootstrap dependencies (Postgres, migrations, status bar)
    Init {
        /// Show command output for debugging
        #[arg(short, long)]
        verbose: bool,
        /// Skip specific deps (pg, models, statusbar, hooks)
        #[arg(long, value_delimiter = ',')]
        skip: Vec<String>,
        /// Clear saved skip decisions and re-prompt everything
        #[arg(long)]
        reset: bool,
        /// PostgreSQL connection URL (overrides DATABASE_URL)
        #[arg(long)]
        database_url: Option<String>,
    },

    /// Run database migrations
    Migrate {
        /// Exit 0 if up-to-date, exit 1 if pending migrations exist (no changes applied)
        #[arg(long)]
        check: bool,
    },

    /// Agent run loop + task-run lifecycle (claim, finalize, heartbeat, show)
    Run {
        #[command(subcommand)]
        action: RunAction,
    },

    /// Spawn a new agent in a tmux window
    Spawn {
        /// Task description for the agent
        #[arg(short, long)]
        task: String,
        /// Agent name (auto-generated if omitted)
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Launch the TUI dashboard
    Dashboard,

    /// Recover orphaned agents stuck in active states
    Recover {
        /// Staleness threshold in seconds (default 300)
        #[arg(long, default_value = "300")]
        stale_secs: u64,
    },

    /// Start the background watcher daemon
    Watcher,

    /// Resource lock management
    Lock {
        #[command(subcommand)]
        action: LockAction,
    },

    /// Human interrupt controls
    Interrupt {
        #[command(subcommand)]
        action: InterruptAction,
    },

    /// Show agent / system status (quick text output)
    Status {
        /// Specific agent name
        #[arg(short, long)]
        agent: Option<String>,
        /// Show agents across all users
        #[arg(long)]
        all_users: bool,
    },

    /// Live event stream — all hook activity, node writes, locks, digests, similarity hits
    Logs {
        /// Stream live (poll every 300ms)
        #[arg(short, long)]
        follow: bool,
        /// Number of recent events to show on start
        #[arg(long, default_value = "20")]
        tail: i64,
        /// Filter to a specific agent
        #[arg(short, long)]
        agent: Option<String>,
        /// Filter to one or more event kinds (comma-separated)
        #[arg(short, long)]
        kind: Option<String>,
        /// Filter to a specific CC session id
        #[arg(long)]
        session: Option<String>,
    },

    /// Stop-hook enforcement: block spawned-worker session end when work
    /// is unfinished (claimed task still open, uncommitted changes, or
    /// unpushed commits). Emits Claude Code decision:block JSON on stdout
    /// when it wants to block; silent otherwise. Safe on non-worker sessions.
    StopCheck {
        /// Agent name (defaults to YGG_AGENT_NAME env var or current directory name)
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Agent-to-agent messaging on the events bus. Message events have a
    /// recipient_agent_id; each agent carries a cursor; inbox = unread
    /// messages since that cursor. The prompt-submit hook auto-injects
    /// inbox content at the top of the recipient's next turn.
    Msg {
        #[command(subcommand)]
        action: MsgAction,
    },

    /// Pick-and-send wrapper around `msg send`. Open an fzf picker over
    /// current agents (most-recent first), then send the message.
    /// Use `--to` to skip the picker, or `--last` to reuse the most
    /// recent recipient.
    Chat {
        /// Skip the picker — send directly to this agent.
        #[arg(long)]
        to: Option<String>,
        /// Reuse the most recent recipient I messaged.
        #[arg(long)]
        last: bool,
        /// Override sender (defaults to YGG_AGENT_NAME or cwd basename).
        #[arg(long)]
        from: Option<String>,
        /// Also tmux send-keys the body so the recipient sees it now
        /// rather than on their next prompt.
        #[arg(long)]
        push: bool,
        /// Message body — joined with spaces.
        body: Vec<String>,
    },

    /// Purge stale rows from locks / sessions / agents. Safe to cron.
    Reap {
        #[arg(long)]
        locks: bool,
        #[arg(long)]
        sessions: bool,
        /// Archive (not delete) agents with no activity in the window.
        /// Archived agents keep their history but disappear from live views.
        #[arg(long)]
        agents: bool,
        #[arg(long, default_value = "7")]
        older_than_days: i64,
        #[arg(long)]
        dry_run: bool,
    },

    /// Manage agent identities (list / archive / unarchive).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Output agent context as markdown (called by SessionStart and PreCompact hooks)
    Prime {
        /// Agent name (defaults to YGG_AGENT_NAME env var or current directory name)
        #[arg(short, long)]
        agent: Option<String>,
        /// Transcript file path (for estimating context pressure)
        #[arg(long)]
        transcript: Option<String>,
    },

    /// Task tracking (replaces beads for this repo)
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// Integrate Yggdrasil into a project — install / update / remove the managed
    /// block in CLAUDE.md and AGENTS.md so agents in this repo know to use `ygg`.
    Integrate {
        /// Remove the managed block (and delete the file if that's all that remains)
        #[arg(long)]
        remove: bool,
        /// Target directory (defaults to current working directory).
        /// Overrides --global when both are passed.
        #[arg(long)]
        path: Option<String>,
        /// Install at the user-global Claude Code level (~/.claude). The
        /// directive applies to every Claude Code session for this user.
        /// Skips AGENTS.md unless --also-agents is set, and refuses if
        /// the current cwd already carries a managed block (would
        /// double-fire the directives).
        #[arg(long, conflicts_with = "path")]
        global: bool,
        /// When --global, also install ~/.claude/AGENTS.md. Default off
        /// because most non-Claude agents don't read that path; when
        /// they do, opt in explicitly.
        #[arg(long)]
        also_agents: bool,
    },

    /// Orchestrator eval suite — run scripted scenarios against three baselines
    /// (vanilla-single, vanilla-tmux, ygg) and produce comparable numbers.
    /// Specified in docs/eval-benchmarks.md. Drivers land per yggdrasil-103.
    Bench {
        #[command(subcommand)]
        action: BenchAction,
    },

    /// Autonomous task-DAG scheduler. Singleton per host (advisory-lock guarded).
    /// Stage 1 MVP per docs/design/scheduler.md: dispatch_ready + finalize_terminal_runs.
    Scheduler {
        #[command(subcommand)]
        action: SchedulerAction,
    },

    /// Emit the single-line status for Claude Code's statusLine — reads the
    /// harness JSON payload from stdin, shows context %, tokens, cost (2dp),
    /// cache hit rate, recalls/24h.
    Bar,

    /// Manage task worktrees (click-to-do).
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },

    /// Execute tasks with worktrees + CC sessions (click-to-do).
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },

    /// Per-repo activity rollup over a recent window (default: last 7 days).
    Rollup {
        /// Number of days to look back.
        #[arg(short, long, default_value = "7")]
        days: i64,
        /// Restrict to a single repo by task prefix.
        #[arg(short, long)]
        repo: Option<String>,
        /// Output format: text, markdown, or json.
        #[arg(short, long, default_value = "markdown")]
        format: String,
    },

    /// Scoped learnings — CodeRabbit-style rule capture + deterministic match.
    /// Retrieval is SQL predicates on (repo, file_glob, rule_id), not vector
    /// similarity. See ADR 0015.
    Learn {
        #[command(subcommand)]
        action: LearnAction,
    },

    /// Persist a durable note for future sessions. Re-added post-ADR-0015 as a
    /// plain note store — no embeddings, no similarity. Scoped to the current
    /// repo unless --global. Recent notes surface in `ygg prime` (SessionStart)
    /// and `ygg remember --list`.
    Remember {
        /// The note text (omit when using --list).
        #[arg(default_value = "", allow_hyphen_values = true)]
        text: String,
        /// List stored notes instead of writing one.
        #[arg(long)]
        list: bool,
        /// Store (or, with --list, read) at global scope across every repo.
        #[arg(long)]
        global: bool,
        /// With --list: include every repo's notes, not just this one + global.
        #[arg(long)]
        all: bool,
        /// With --list: max notes to show.
        #[arg(long, default_value_t = 20)]
        limit: i64,
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Checkpoint the current session so a fresh one (after `/clear`) resumes
    /// without re-explaining. The saved note auto-surfaces at the top of the
    /// next `ygg prime`. Subcommands: save / show / clear.
    Handoff {
        #[command(subcommand)]
        action: HandoffAction,
    },

    /// Record that the agent is about to invoke a tool — PreToolUse hook.
    AgentTool {
        /// Tool name (Bash, Edit, Read, …)
        tool: String,
        /// Agent name (defaults to env / pwd basename)
        #[arg(short, long)]
        agent: Option<String>,
    },

    /// Handle Claude Code lifecycle hooks natively (replaces shell scripts)
    Hook {
        /// The hook event type
        #[clap(subcommand)]
        action: ygg::cli::hook_cmd::HookAction,
    },
}

#[derive(Subcommand)]
enum PlanAction {
    /// Create a plan (epic) in the current repo.
    Create {
        title: String,
        #[arg(short, long)]
        description: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Add a child task under a plan, optionally with dependencies.
    Add {
        epic: String,
        title: String,
        #[arg(short, long)]
        description: Option<String>,
        #[arg(short, long)]
        kind: Option<String>,
        #[arg(long, value_delimiter = ',')]
        deps: Vec<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Execute a single task: worktree + claim + tmux + CC session.
    Run {
        task: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Supervise an epic: walk deps, spawn CC sessions for ready tasks,
    /// poll for status changes, exit when no open tasks remain.
    Supervise {
        epic: String,
        #[arg(short, long, default_value = "1")]
        parallelism: usize,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "5")]
        poll_secs: u64,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Pause an epic's supervisor — no new tasks spawn until resumed.
    Pause { epic: String },
    /// Resume a paused epic.
    Resume { epic: String },
    /// Abort: tear down in-progress descendants (archive worktrees,
    /// revert to open) and pause the epic. Recoverable.
    Abort {
        epic: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorktreeAction {
    /// Create (or return existing) worktree for a task.
    Ensure { task: String },
    /// Tear down a task's worktree.
    Rm {
        task: String,
        /// Policy: keep / archive / delete (default: archive)
        #[arg(long, default_value = "archive")]
        policy: String,
        /// Force removal even with uncommitted changes.
        #[arg(long)]
        force: bool,
    },
    /// Show the on-disk root + existing worktrees for this host.
    List,
}

#[derive(Subcommand)]
enum MsgAction {
    /// Send a message to another agent. Writes an event on the bus;
    /// recipient's next user turn sees it via prompt-submit injection.
    /// `--push` additionally interrupts the recipient via tmux send-keys
    /// for instant delivery (may disrupt a mid-thought turn).
    Send {
        #[arg(long)]
        to: String,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        push: bool,
        /// Don't auto-spawn a worker if the recipient is inactive.
        #[arg(long)]
        no_spawn: bool,
        body: Vec<String>,
    },
    /// Show unread messages for this agent. `--all` ignores the cursor.
    Inbox {
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Advance the cursor so inbox is empty until the next message arrives.
    /// Called by the prompt-submit hook after injection.
    MarkRead {
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Send a broadcast message (no recipient). Any agent can claim it.
    Broadcast {
        #[arg(long)]
        from: Option<String>,
        body: Vec<String>,
    },
    /// Claim an unclaimed broadcast message for this agent.
    Claim {
        /// Event UUID of the broadcast message
        id: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum SchedulerAction {
    /// Start the daemon in the foreground. Holds the singleton advisory lock.
    Run,
    /// Run one tick synchronously and exit. Prints stats as JSON.
    Tick,
    /// Print whether a scheduler is running and what's queued.
    Status,
    /// Print what `tick` would do without writing.
    DryRun,
    /// Synthesize task_runs rows for tasks that predate ADR 0016. Idempotent.
    Backfill,
}

#[derive(Subcommand)]
enum RunAction {
    /// Open a run for the given task reference. Used by spawned agents whose
    /// SessionStart hook claims their bound task.
    Claim {
        /// Task reference (e.g. yggdrasil-142)
        task_ref: String,
        /// Override agent name (defaults to env / pwd basename).
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Bump heartbeat_at on a running run. Called by PreToolUse hook.
    Heartbeat {
        /// Specific run id (default: latest running run for this agent).
        #[arg(long)]
        run_id: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Finalize a task's current run with the given terminal state.
    Finalize {
        task_ref: String,
        /// succeeded | failed | crashed | cancelled | poison
        #[arg(long, default_value = "succeeded")]
        state: String,
        /// run_reason (default 'ok')
        #[arg(long, default_value = "ok")]
        reason: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Print one run's detail.
    Show { run_id: String },
    /// Print run history for a task.
    List { task_ref: String },
    /// Stop-hook handoff: capture commits + branch into the agent's latest
    /// running run, transition state heuristically (succeeded if a commit
    /// landed since started_at, else failed). Idempotent.
    CaptureOutcome {
        #[arg(short, long)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum BenchAction {
    /// List known scenarios and implementation status.
    List,
    /// Run a scenario against a baseline. Drivers land per yggdrasil-103;
    /// today this records a bench_runs row and exits non-zero.
    Run {
        /// Scenario id (see `ygg bench list`).
        scenario: String,
        /// Baseline: vanilla-single | vanilla-tmux | ygg.
        #[arg(long, default_value = "ygg")]
        baseline: String,
        /// Override default parallelism for the scenario.
        #[arg(long)]
        parallelism: Option<i32>,
        /// Model id passed through to the driver.
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        /// Optional seed (recorded for reproducibility audit; not yet used).
        #[arg(long)]
        seed: Option<i64>,
    },
    /// Print a single bench run's summary.
    Report { run_id: String },
    /// Pairwise compare two runs of the same scenario.
    Diff { a: String, b: String },
    /// CI gate — run the tier's scenarios, exit non-zero on regression.
    Ci {
        #[arg(long, default_value = "smoke")]
        tier: String,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// List agents with last-activity age. By default hides archived.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Archive an agent (hides from live views, keeps history).
    Archive { name: String },
    /// Restore a previously-archived agent.
    Unarchive { name: String },
    /// Rename an agent. Preserves agent_id so events/nodes/sessions stay linked.
    /// Rejects if the new name already exists under the same persona.
    Rename { old: String, new_name: String },
    /// Show agents that would be archived by `ygg reap --agents` at the
    /// given staleness threshold. Never mutates.
    Stale {
        #[arg(long, default_value = "14")]
        older_than_days: i64,
    },
}

#[derive(Subcommand)]
enum LearnAction {
    /// Capture a new learning. Defaults to current repo scope.
    Create {
        /// The learning text
        text: String,
        /// Apply to every repo, not just the current one
        #[arg(long)]
        global: bool,
        /// File-glob this learning applies to (e.g. "terraform/*.tf")
        #[arg(long)]
        file_glob: Option<String>,
        /// External rule id (e.g. "CKV_AWS_337", "clippy::too_many_lines")
        #[arg(long)]
        rule_id: Option<String>,
        /// Freeform provenance: PR link, commit hash, quote
        #[arg(long)]
        context: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
        /// Scope tag: global, agent=<name>, kind=<task-kind>. Repeatable.
        #[arg(long, value_name = "SCOPE")]
        scope: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// List learnings whose scope matches the given filters. No filters = all
    /// learnings visible from the current repo. Deterministic SQL match, not
    /// similarity search.
    List {
        /// A file path to test against each learning's file_glob
        #[arg(long)]
        file: Option<String>,
        /// Exact rule_id match
        #[arg(long)]
        rule_id: Option<String>,
        /// Scan every repo (default: current repo + global)
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a learning by id (full UUID or short prefix not supported yet)
    Delete { id: String },
}

#[derive(Subcommand)]
enum LockAction {
    /// Acquire a resource lock
    Acquire {
        /// Resource key (e.g. "file:src/auth/")
        resource: String,
        /// Agent name performing the lock
        #[arg(short, long)]
        agent: String,
    },
    /// Release a resource lock
    Release {
        /// Resource key
        resource: String,
        /// Agent name releasing the lock
        #[arg(short, long)]
        agent: String,
    },
    /// List all active locks. Pass --stale to restrict to locks held
    /// longer than `secs` (default 600 = 10 min) — useful for triaging
    /// abandoned leases.
    List {
        #[arg(long)]
        stale: bool,
        #[arg(long, default_value = "600")]
        stale_secs: i64,
    },
}

#[derive(Subcommand)]
enum HandoffAction {
    /// Save a resume note for this repo+agent (replaces any prior one). Pass
    /// the text as an argument, or omit it / pass `-` to read from stdin.
    Save {
        /// The handoff text. Omit or use `-` to read stdin.
        #[arg(default_value = "", allow_hyphen_values = true)]
        text: String,
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the current handoff for this repo+agent.
    Show {
        #[arg(short, long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete the current handoff for this repo+agent.
    Clear {
        #[arg(short, long)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum TaskAction {
    /// Create a new task in the current repo
    Create {
        /// Title (required unless --file/--stdin/--body-file is used)
        #[arg(default_value = "")]
        title: String,
        // allow_hyphen_values so "-- item" / "* foo" etc. don't get silently
        // interpreted as new flags (yggdrasil-21).
        #[arg(short, long, allow_hyphen_values = true)]
        description: Option<String>,
        #[arg(short, long)]
        kind: Option<String>,
        #[arg(short, long, value_parser = parse_priority)]
        priority: Option<i16>,
        #[arg(long, allow_hyphen_values = true)]
        acceptance: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        design: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        notes: Option<String>,
        #[arg(short, long, value_delimiter = ',')]
        label: Vec<String>,
        #[arg(short, long)]
        agent: Option<String>,
        /// Link to an external issue tracker (gh-123, jira-PROJ-42, URL, etc.)
        #[arg(long)]
        external_ref: Option<String>,
        /// Emit the created task(s) as JSON (for agent consumption)
        #[arg(long)]
        json: bool,
        /// Parse a markdown file into a task tree (H1=epic, H2=feature, H3/4=task).
        /// Body under each header becomes the description. Parent→child dep edges
        /// are auto-linked so `ygg task ready` surfaces leaves first.
        #[arg(short = 'f', long, value_name = "FILE")]
        file: Option<std::path::PathBuf>,
        /// Read the description body from a file (single-task mode). Useful when
        /// agents write long specs and shell-escaping gets painful.
        #[arg(long, value_name = "FILE")]
        body_file: Option<std::path::PathBuf>,
        /// Read the description body from stdin (single-task mode).
        #[arg(long)]
        stdin: bool,
    },
    /// List tasks (defaults to current repo; pass --all for every repo)
    List {
        #[arg(long)]
        all: bool,
        #[arg(short, long)]
        status: Option<String>,
        /// Filter to tasks with ALL of these labels (AND)
        #[arg(short, long, value_delimiter = ',')]
        label: Vec<String>,
        /// Filter to tasks with ANY of these labels (OR)
        #[arg(long, value_delimiter = ',')]
        label_any: Vec<String>,
        /// Emit results as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Show tasks with no unsatisfied blockers
    Ready {
        /// Emit results as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Show tasks blocked by another open task
    Blocked {
        /// Emit results as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Surface probable duplicate task pairs via token-set (Jaccard)
    /// string similarity on title+description.
    Dupes {
        /// Scan every repo (default: current repo only)
        #[arg(long)]
        all: bool,
        /// Minimum token-set (Jaccard) similarity on title+description
        /// (0.0–1.0). Default 0.5 — Jaccard is stricter than the old
        /// embedding cosine, so 0.5 catches reworded dupes while keeping
        /// false positives down.
        #[arg(long, default_value_t = 0.5)]
        min_similarity: f64,
        /// Max pairs to return
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Emit results as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Surface tasks that haven't been touched recently — useful for
    /// triage of abandoned in_progress claims.
    Stale {
        /// Age threshold in days (default 30)
        #[arg(long, default_value_t = 30)]
        days: i32,
        /// Scan every repo instead of just the current one
        #[arg(long)]
        all: bool,
        /// Filter to a specific status (e.g. in_progress)
        #[arg(short, long)]
        status: Option<String>,
        /// Emit results as JSON array
        #[arg(long)]
        json: bool,
    },
    /// Show a task by "<prefix>-<seq>" or UUID
    Show {
        reference: String,
        /// Emit the task as JSON (includes labels, deps, links)
        #[arg(long)]
        json: bool,
    },
    /// Update task fields
    Update {
        reference: String,
        #[arg(long, allow_hyphen_values = true)]
        title: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        description: Option<String>,
        #[arg(long, value_parser = parse_priority)]
        priority: Option<i16>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        acceptance: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        design: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        notes: Option<String>,
        /// Set the external ref. Pass empty string to clear.
        #[arg(long)]
        external_ref: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Claim a task (assignee + in_progress)
    Claim {
        reference: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Close a task
    Close {
        reference: String,
        #[arg(short, long)]
        reason: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Change a task's status
    Status {
        reference: String,
        status: String,
        #[arg(short, long)]
        reason: Option<String>,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Add a dependency: <task> depends on <blocker>
    Dep { task: String, blocker: String },
    /// Remove a dependency
    Undep { task: String, blocker: String },
    /// Label management (add/remove/list/list-all)
    Label {
        #[command(subcommand)]
        action: LabelAction,
    },
    /// Bump a task's relevance by a signed delta (clamped 0..100).
    /// Use when a task turns out to be more (or less) load-bearing than first filed.
    Bump {
        reference: String,
        /// Integer delta (+5, -10, etc). Accepts bare numbers too.
        #[arg(allow_hyphen_values = true)]
        delta: i32,
    },
    /// Record a non-blocking relationship between two tasks
    /// (see-also / superseded-by / duplicate-of / related).
    Link {
        from: String,
        to: String,
        #[arg(short, long, default_value = "see-also")]
        kind: String,
    },
    /// Count open/in_progress/blocked/closed
    Stats {
        #[arg(long)]
        all: bool,
        /// Emit stats as JSON
        #[arg(long)]
        json: bool,
    },
    /// Approve a task gated on approve_plan / approve_completion. Sets
    /// approved_at = now() so the scheduler can dispatch.
    Approve {
        reference: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Clear poison state and let the scheduler retry. Resets the poisoned
    /// run to 'failed' and reopens the task.
    Unpoison { reference: String },
    /// Toggle the `runnable` flag — controls whether the scheduler will
    /// auto-spawn an agent for this task. Default is on; pass --off to
    /// disable. yggdrasil-108.
    Runnable {
        reference: String,
        /// Disable scheduler dispatch for this task.
        #[arg(long)]
        off: bool,
    },
    /// Walk task_deps and surface any pre-existing cycles. add_dep
    /// already rejects new cycles; this catches rows that pre-date the
    /// guard or were inserted via raw SQL. Exit non-zero on any cycle.
    Lint {
        /// Scan every repo (default: current repo only).
        #[arg(long)]
        all: bool,
        /// Emit cycles as a JSON array of task-ref arrays.
        #[arg(long)]
        json: bool,
    },
    /// Reassign a task to another repo by its task_prefix — for work filed in
    /// the wrong repo. Keeps the task_id; renumbers the human ref into the
    /// destination (e.g. `yggdrasil-42` → `canairy-7`).
    Move {
        /// Task ref (UUID, ygg-<shortuuid>, or <prefix>-<seq>)
        reference: String,
        /// Destination repo's task_prefix (e.g. `canairy`)
        repo: String,
        #[arg(short, long)]
        agent: Option<String>,
    },
    /// Move a task to the trash. Hidden from list/ready/blocked/dupes
    /// until restored or hard-deleted by `ygg task purge`.
    Delete { reference: String },
    /// Pull a task back out of the trash.
    Restore { reference: String },
    /// List tasks currently in the trash.
    Trash {
        /// Scan every repo (default: current repo only).
        #[arg(long)]
        all: bool,
        /// Emit results as JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Hard-delete trashed tasks older than `days`. Intended for cron;
    /// safe to run interactively. Default 30 days.
    Purge {
        #[arg(long, default_value_t = 30)]
        days: i32,
        /// Purge across every repo (default: current repo only).
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum LabelAction {
    /// Attach a label to a task
    Add { reference: String, label: String },
    /// Remove a label from a task
    Remove { reference: String, label: String },
    /// List labels on a specific task
    List {
        reference: String,
        #[arg(long)]
        json: bool,
    },
    /// List every label in the current repo (or --all repos) with usage counts
    ListAll {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum InterruptAction {
    /// Take over an agent's session
    TakeOver {
        /// Agent name
        agent: String,
    },
    /// Hand back control to the agent
    HandBack {
        /// Agent name
        agent: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ygg=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let user_id = ygg::db::resolve_user();

    let command = cli.command.unwrap_or(Commands::Up);

    match command {
        Commands::Up => {
            // Start the ygg tmux session with dashboard
            if !ygg::tmux::TmuxManager::is_available().await {
                eprintln!("tmux is not installed. Run: ygg init");
                std::process::exit(1);
            }
            ygg::tmux::TmuxManager::ensure_session().await?;
            println!("ygg session ready. Attach with: tmux attach -t ygg");

            // If already in tmux, just attach
            if std::env::var("TMUX").is_ok() {
                println!("Already in tmux. Switch to ygg: Ctrl-b s → ygg");
            } else {
                // Attach to the session
                let _ = tokio::process::Command::new("tmux")
                    .args(["attach", "-t", "ygg"])
                    .status()
                    .await;
            }
        }
        Commands::Init {
            verbose,
            skip,
            reset,
            database_url,
        } => {
            if reset {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                let skips_path = std::path::Path::new(&home).join(".config/ygg/skips.json");
                let _ = std::fs::remove_file(&skips_path);
                println!("Saved skip decisions cleared.");
            }
            if let Some(ref url) = database_url {
                unsafe {
                    std::env::set_var("DATABASE_URL", url);
                }
            }
            ygg::cli::init::execute_with_options(verbose, &skip).await?;
        }
        Commands::Migrate { check } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            if check {
                let pending = ygg::db::pending_migrations(&pool).await?;
                if pending.is_empty() {
                    println!("Schema is up to date.");
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "pending_count": pending.len(),
                            "pending": pending,
                        })
                    );
                    std::process::exit(1);
                }
            } else {
                let pending = ygg::db::pending_migrations(&pool).await?;
                let count = pending.len();
                ygg::db::run_migrations(&pool).await?;
                if count > 0 {
                    println!("Applied {count} migration(s).");
                } else {
                    println!("Schema already up to date.");
                }
            }
        }
        Commands::Run { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                RunAction::Claim { task_ref, agent } => {
                    let agent = resolve_agent_arg(agent);
                    ygg::cli::run_cmd::claim_cli(&pool, &task_ref, &agent).await?;
                }
                RunAction::Heartbeat { run_id, agent } => {
                    let agent = resolve_agent_arg(agent);
                    let id = run_id
                        .map(|s| {
                            uuid::Uuid::parse_str(&s)
                                .map_err(|e| anyhow::anyhow!("invalid run-id: {e}"))
                        })
                        .transpose()?;
                    ygg::cli::run_cmd::heartbeat_cli(&pool, id, &agent).await?;
                }
                RunAction::Finalize {
                    task_ref,
                    state,
                    reason,
                    agent,
                } => {
                    let agent = resolve_agent_arg(agent);
                    ygg::cli::run_cmd::finalize_cli(&pool, &task_ref, &state, &reason, &agent)
                        .await?;
                }
                RunAction::Show { run_id } => {
                    let id = uuid::Uuid::parse_str(&run_id)
                        .map_err(|e| anyhow::anyhow!("invalid run-id: {e}"))?;
                    ygg::cli::run_cmd::show_cli(&pool, id).await?;
                }
                RunAction::List { task_ref } => {
                    ygg::cli::run_cmd::list_cli(&pool, &task_ref).await?;
                }
                RunAction::CaptureOutcome { agent } => {
                    let agent = resolve_agent_arg(agent);
                    ygg::cli::run_cmd::capture_outcome_cli(&pool, &agent, None).await?;
                }
            }
        }
        Commands::Spawn { task, name } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::spawn::execute(&pool, &config, &task, name.as_deref()).await?;
        }
        Commands::Dashboard => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::dashboard_cmd::execute(&pool, &config).await?;
        }
        Commands::Recover { stale_secs } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::recover::execute(&pool, Some(stale_secs)).await?;
        }
        Commands::Watcher => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::watcher_cmd::execute(&pool, &config).await?;
        }
        Commands::Lock { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                LockAction::Acquire { resource, agent } => {
                    ygg::cli::lock_cmd::acquire(&pool, &config, &resource, &agent).await?;
                }
                LockAction::Release { resource, agent } => {
                    ygg::cli::lock_cmd::release(&pool, &config, &resource, &agent).await?;
                }
                LockAction::List { stale, stale_secs } => {
                    ygg::cli::lock_cmd::list(&pool, &config, stale, stale_secs).await?;
                }
            }
        }
        Commands::Interrupt { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                InterruptAction::TakeOver { agent } => {
                    ygg::cli::interrupt_cmd::execute_take_over(&pool, &config, &agent).await?;
                }
                InterruptAction::HandBack { agent } => {
                    ygg::cli::interrupt_cmd::execute_hand_back(&pool, &config, &agent).await?;
                }
            }
        }
        Commands::Status { agent, all_users } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::status_cmd::execute(&pool, agent.as_deref(), all_users).await?;
        }
        Commands::Logs {
            follow,
            tail,
            agent,
            kind,
            session,
        } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let kinds: Vec<String> = kind
                .map(|k| {
                    k.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            ygg::cli::logs_cmd::execute(
                &pool,
                follow,
                tail,
                agent.as_deref(),
                if kinds.is_empty() { None } else { Some(kinds) },
                session.as_deref(),
            )
            .await?;
        }
        Commands::Chat {
            to,
            last,
            from,
            push,
            body,
        } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let from_name = from.unwrap_or_else(|| {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            });
            let joined = body.join(" ");
            ygg::cli::chat_cmd::execute(&pool, &from_name, &joined, to.as_deref(), last, push)
                .await?;
        }
        Commands::Msg { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let default_agent = || {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            };
            match action {
                MsgAction::Send {
                    to,
                    from,
                    push,
                    no_spawn,
                    body,
                } => {
                    let from_name = from.unwrap_or_else(default_agent);
                    let joined = body.join(" ");
                    if joined.trim().is_empty() {
                        anyhow::bail!("empty body — pass the message as trailing args");
                    }
                    ygg::cli::msg_cmd::send_inner(&pool, &from_name, &to, &joined, push, no_spawn)
                        .await?;
                }
                MsgAction::Inbox { agent, all } => {
                    let name = agent.unwrap_or_else(default_agent);
                    let msgs = ygg::cli::msg_cmd::inbox(&pool, &name, all).await?;
                    ygg::cli::msg_cmd::print_inbox(&msgs);
                }
                MsgAction::MarkRead { agent } => {
                    let name = agent.unwrap_or_else(default_agent);
                    ygg::cli::msg_cmd::mark_read(&pool, &name).await?;
                }
                MsgAction::Broadcast { from, body } => {
                    let from_name = from.unwrap_or_else(default_agent);
                    let joined = body.join(" ");
                    if joined.trim().is_empty() {
                        anyhow::bail!("empty body — pass the message as trailing args");
                    }
                    ygg::cli::msg_cmd::broadcast(&pool, &from_name, &joined).await?;
                }
                MsgAction::Claim { id, agent } => {
                    let name = agent.unwrap_or_else(default_agent);
                    let event_id: uuid::Uuid = id
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid UUID: {id}"))?;
                    ygg::cli::msg_cmd::claim_broadcast(&pool, event_id, &name).await?;
                }
            }
        }
        Commands::StopCheck { agent } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::stop_check::execute(&pool, &agent_name).await?;
        }
        Commands::Reap {
            locks,
            sessions,
            agents,
            older_than_days,
            dry_run,
        } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            // Default to everything when no specific flag is set.
            let all = !(locks || sessions || agents);
            let mut total: i64 = 0;

            if all || locks {
                let sql = "DELETE FROM locks WHERE expires_at < now() - ($1 || ' days')::interval";
                let n = if dry_run {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*)::bigint FROM locks WHERE expires_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).fetch_one(&pool).await.unwrap_or(0)
                } else {
                    sqlx::query(sql)
                        .bind(older_than_days.to_string())
                        .execute(&pool)
                        .await?
                        .rows_affected() as i64
                };
                println!(
                    "locks:    {} {}",
                    if dry_run { "would delete" } else { "deleted" },
                    n
                );
                total += n;
            }
            if all || sessions {
                // Close abandoned sessions (no ended_at but stale updated_at)
                // before we delete. Leaves a digest trail intact.
                let n_closed = if dry_run {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*)::bigint FROM sessions
                          WHERE ended_at IS NULL AND updated_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).fetch_one(&pool).await.unwrap_or(0)
                } else {
                    sqlx::query(
                        "UPDATE sessions SET ended_at = updated_at
                          WHERE ended_at IS NULL AND updated_at < now() - ($1 || ' days')::interval"
                    ).bind(older_than_days.to_string()).execute(&pool).await?.rows_affected() as i64
                };
                println!(
                    "sessions: {} {} abandoned (auto-closed)",
                    if dry_run { "would close" } else { "closed" },
                    n_closed
                );
                total += n_closed;
            }
            if all || agents {
                // Archive-not-delete: keep history intact, just hide from
                // live views. Staleness = no events + no sessions + no
                // live locks in the window.
                let repo = ygg::models::agent::AgentRepo::new(&pool, &user_id);
                let stale = repo.find_stale(older_than_days).await.unwrap_or_default();
                let n = stale.len() as i64;
                if !dry_run {
                    for a in &stale {
                        let _ = repo.archive(a.agent_id).await;
                    }
                }
                println!(
                    "agents:   {} {} stale (no activity in {} days)",
                    if dry_run { "would archive" } else { "archived" },
                    n,
                    older_than_days
                );
                for a in stale.iter().take(5) {
                    println!(
                        "  · {}  ({})",
                        a.agent_name,
                        chrono::Utc::now()
                            .signed_duration_since(a.updated_at)
                            .num_days()
                    );
                }
                total += n;
            }
            println!("total:    {}", total);
        }
        Commands::Agent { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let repo = ygg::models::agent::AgentRepo::new(&pool, &user_id);
            match action {
                AgentAction::List { all } => {
                    let agents = if all {
                        repo.list_all().await?
                    } else {
                        repo.list().await?
                    };
                    if agents.is_empty() {
                        println!("no agents");
                    } else {
                        println!(
                            "{:<24} {:<12} {:<10} {:<10}",
                            "NAME", "PERSONA", "STATE", "AGE"
                        );
                        let now = chrono::Utc::now();
                        for a in agents {
                            let age_days = now.signed_duration_since(a.updated_at).num_days();
                            let age = if age_days == 0 {
                                "<1d".to_string()
                            } else {
                                format!("{age_days}d")
                            };
                            let persona = a.persona.as_deref().unwrap_or("—");
                            println!(
                                "{:<24} {:<12} {:<10} {:<10}",
                                a.agent_name, persona, a.current_state, age
                            );
                        }
                    }
                }
                AgentAction::Archive { name } => {
                    let agent = repo
                        .get_by_name(&name)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;
                    repo.archive(agent.agent_id).await?;
                    println!("archived '{name}'");
                }
                AgentAction::Unarchive { name } => {
                    let agent = repo
                        .get_by_name(&name)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;
                    repo.unarchive(agent.agent_id).await?;
                    println!("restored '{name}'");
                }
                AgentAction::Rename { old, new_name } => {
                    let agent = repo
                        .get_by_name(&old)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("agent '{old}' not found"))?;
                    if old == new_name {
                        println!("noop: '{old}' already named that");
                    } else if let Err(e) = repo.rename(agent.agent_id, &new_name).await {
                        // 23505 = unique_violation; surface as friendly error.
                        let msg = e
                            .as_database_error()
                            .and_then(|d| d.code().map(|c| c.to_string()));
                        if msg.as_deref() == Some("23505") {
                            anyhow::bail!("agent '{new_name}' already exists (with same persona)");
                        } else {
                            return Err(e.into());
                        }
                    } else {
                        println!("renamed '{old}' → '{new_name}'");
                    }
                }
                AgentAction::Stale { older_than_days } => {
                    let stale = repo.find_stale(older_than_days).await?;
                    if stale.is_empty() {
                        println!("no stale agents (threshold: {older_than_days}d)");
                    } else {
                        println!(
                            "{} stale agent(s) (no activity in {older_than_days}d):",
                            stale.len()
                        );
                        let now = chrono::Utc::now();
                        for a in stale {
                            let age = now.signed_duration_since(a.updated_at).num_days();
                            println!("  · {:<24} idle {age}d", a.agent_name);
                        }
                    }
                }
            }
        }
        Commands::Prime { agent, transcript } => {
            let agent_name = agent
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            ygg::cli::prime::execute(&agent_name, transcript.as_deref()).await?;
        }
        Commands::Task { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let default_agent = || {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            };
            match action {
                TaskAction::Create {
                    title,
                    description,
                    kind,
                    priority,
                    acceptance,
                    design,
                    notes,
                    label,
                    agent,
                    external_ref,
                    json,
                    file,
                    body_file,
                    stdin,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    // Mode dispatch — mutually exclusive.
                    if let Some(path) = file {
                        if !title.is_empty() {
                            anyhow::bail!("--file and a positional title are mutually exclusive");
                        }
                        ygg::cli::task_cmd::create_from_markdown(&pool, &path, &agent_name, json)
                            .await?;
                    } else {
                        // Single-task mode. Body precedence: --stdin > --body-file > --description.
                        let body: Option<String> = if stdin {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf)?;
                            Some(buf)
                        } else if let Some(p) = body_file {
                            Some(std::fs::read_to_string(&p)?)
                        } else {
                            description
                        };
                        if title.is_empty() {
                            anyhow::bail!("title is required unless --file is used");
                        }
                        ygg::cli::task_cmd::create(
                            &pool,
                            ygg::cli::task_cmd::CreateOpts {
                                title: &title,
                                description: body.as_deref(),
                                kind: kind.as_deref(),
                                priority,
                                acceptance: acceptance.as_deref(),
                                design: design.as_deref(),
                                notes: notes.as_deref(),
                                labels: &label,
                                agent_name: &agent_name,
                                external_ref: external_ref.as_deref(),
                                json,
                            },
                        )
                        .await?;
                    }
                }
                TaskAction::List {
                    all,
                    status,
                    label,
                    label_any,
                    json,
                } => {
                    ygg::cli::task_cmd::list(
                        &pool,
                        all,
                        status.as_deref(),
                        &label,
                        &label_any,
                        json,
                    )
                    .await?;
                }
                TaskAction::Ready { json } => {
                    ygg::cli::task_cmd::ready(&pool, json).await?;
                }
                TaskAction::Blocked { json } => {
                    ygg::cli::task_cmd::blocked(&pool, json).await?;
                }
                TaskAction::Stale {
                    days,
                    all,
                    status,
                    json,
                } => {
                    ygg::cli::task_cmd::stale(&pool, days, all, status.as_deref(), json).await?;
                }
                TaskAction::Dupes {
                    all,
                    min_similarity,
                    limit,
                    json,
                } => {
                    ygg::cli::task_cmd::dupes(&pool, all, min_similarity, limit, json).await?;
                }
                TaskAction::Show { reference, json } => {
                    ygg::cli::task_cmd::show(&pool, &reference, json).await?;
                }
                TaskAction::Update {
                    reference,
                    title,
                    description,
                    priority,
                    kind,
                    acceptance,
                    design,
                    notes,
                    external_ref,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    // Empty-string external_ref clears the column; None leaves it alone.
                    let ext_update = external_ref
                        .as_deref()
                        .map(|s| if s.is_empty() { None } else { Some(s) });
                    ygg::cli::task_cmd::update(
                        &pool,
                        &reference,
                        title.as_deref(),
                        description.as_deref(),
                        priority,
                        kind.as_deref(),
                        acceptance.as_deref(),
                        design.as_deref(),
                        notes.as_deref(),
                        ext_update,
                        &agent_name,
                    )
                    .await?;
                }
                TaskAction::Claim { reference, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::claim(&pool, &reference, &agent_name).await?;
                }
                TaskAction::Close {
                    reference,
                    reason,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::close(&pool, &reference, reason.as_deref(), &agent_name)
                        .await?;
                }
                TaskAction::Status {
                    reference,
                    status,
                    reason,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::set_status(
                        &pool,
                        &reference,
                        &status,
                        reason.as_deref(),
                        &agent_name,
                    )
                    .await?;
                }
                TaskAction::Dep { task, blocker } => {
                    ygg::cli::task_cmd::add_dep(&pool, &task, &blocker).await?;
                }
                TaskAction::Undep { task, blocker } => {
                    ygg::cli::task_cmd::remove_dep(&pool, &task, &blocker).await?;
                }
                TaskAction::Bump { reference, delta } => {
                    ygg::cli::task_cmd::bump(&pool, &reference, delta).await?;
                }
                TaskAction::Link { from, to, kind } => {
                    ygg::cli::task_cmd::link(&pool, &from, &to, &kind).await?;
                }
                TaskAction::Label { action } => match action {
                    LabelAction::Add { reference, label } => {
                        ygg::cli::task_cmd::label_add(&pool, &reference, &label).await?;
                    }
                    LabelAction::Remove { reference, label } => {
                        ygg::cli::task_cmd::label_remove(&pool, &reference, &label).await?;
                    }
                    LabelAction::List { reference, json } => {
                        ygg::cli::task_cmd::label_list(&pool, &reference, json).await?;
                    }
                    LabelAction::ListAll { all, json } => {
                        ygg::cli::task_cmd::label_list_all(&pool, all, json).await?;
                    }
                },
                TaskAction::Stats { all, json } => {
                    ygg::cli::task_cmd::stats(&pool, all, json).await?;
                }
                TaskAction::Approve { reference, agent } => {
                    let task = ygg::cli::task_cmd::resolve_task_public(&pool, &reference).await?;
                    let agent = resolve_agent_arg(agent);
                    let agent_id = ygg::models::agent::AgentRepo::new(&pool, &user_id)
                        .get_by_name(&agent)
                        .await?
                        .map(|a| a.agent_id);
                    ygg::scheduler::approve(&pool, task.task_id, agent_id).await?;
                    println!("{reference} approved");
                }
                TaskAction::Unpoison { reference } => {
                    let task = ygg::cli::task_cmd::resolve_task_public(&pool, &reference).await?;
                    ygg::scheduler::unpoison(&pool, task.task_id).await?;
                    println!(
                        "{reference} unpoisoned (status reset to open; latest run flipped failed)"
                    );
                }
                TaskAction::Lint { all, json } => {
                    ygg::cli::task_cmd::lint(&pool, all, json).await?;
                }
                TaskAction::Move {
                    reference,
                    repo,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::task_cmd::move_to(&pool, &reference, &repo, &agent_name).await?;
                }
                TaskAction::Delete { reference } => {
                    ygg::cli::task_cmd::delete(&pool, &reference).await?;
                }
                TaskAction::Restore { reference } => {
                    ygg::cli::task_cmd::restore(&pool, &reference).await?;
                }
                TaskAction::Trash { all, json } => {
                    ygg::cli::task_cmd::trash(&pool, all, json).await?;
                }
                TaskAction::Purge { days, all } => {
                    ygg::cli::task_cmd::purge(&pool, days, all).await?;
                }
                TaskAction::Runnable { reference, off } => {
                    let task = ygg::cli::task_cmd::resolve_task_public(&pool, &reference).await?;
                    let new_value = !off;
                    sqlx::query(
                        "UPDATE tasks SET runnable = $1, updated_at = now() WHERE task_id = $2",
                    )
                    .bind(new_value)
                    .bind(task.task_id)
                    .execute(&pool)
                    .await?;
                    println!(
                        "{reference} runnable={}",
                        if new_value { "TRUE" } else { "FALSE" }
                    );
                }
            }
        }
        Commands::Integrate {
            remove,
            path,
            global,
            also_agents,
        } => {
            // Resolve target directory + opts. --global pins to ~/.claude
            // and (by default) only touches CLAUDE.md. Explicit --path
            // overrides --global; clap already enforces "not both."
            let (target, opts, mode_label) = if let Some(p) = path {
                (
                    std::path::PathBuf::from(p),
                    ygg::cli::init_project::IntegrateOpts::default(),
                    "path",
                )
            } else if global {
                let home = std::env::var("HOME")
                    .map_err(|_| anyhow::anyhow!("--global requires $HOME to be set"))?;
                let target = std::path::PathBuf::from(home).join(".claude");
                // Refuse on conflict: if cwd has its own managed block
                // already, installing globally would double-fire every
                // directive (~/.claude/CLAUDE.md loads first).
                if !remove {
                    let cwd = std::env::current_dir().expect("cwd");
                    if ygg::cli::init_project::has_managed_block(&cwd) {
                        anyhow::bail!(
                            "cwd ({}) already carries a managed Yggdrasil block; \
                             --global would double-fire the directives.\n\
                             Run `ygg integrate --remove` from cwd first, then re-run \
                             `ygg integrate --global`.",
                            cwd.display()
                        );
                    }
                }
                let opts = ygg::cli::init_project::IntegrateOpts {
                    skip_agents_md: !also_agents,
                };
                (target, opts, "global")
            } else {
                (
                    std::env::current_dir().expect("cwd"),
                    ygg::cli::init_project::IntegrateOpts::default(),
                    "cwd",
                )
            };

            if remove {
                let report = ygg::cli::init_project::remove_with(&target, opts)?;
                println!(
                    "Removing Yggdrasil integration block in {} ({mode_label}):",
                    target.display()
                );
                ygg::cli::init_project::print_report(&report);
            } else {
                let report = ygg::cli::init_project::install_with(&target, opts)?;
                println!(
                    "Yggdrasil integration in {} ({mode_label}):",
                    target.display()
                );
                ygg::cli::init_project::print_report(&report);
                if global {
                    println!();
                    println!("note: ~/.claude/CLAUDE.md loads BEFORE any project CLAUDE.md.");
                    println!(
                        "      The directives now apply to every Claude Code session for this user."
                    );
                    if !also_agents {
                        println!(
                            "      AGENTS.md was skipped; pass --also-agents if you want non-Claude tools to see it too."
                        );
                    }
                }
            }
        }
        Commands::Bench { action } => match action {
            BenchAction::List => {
                ygg::cli::bench_cmd::list();
            }
            BenchAction::Run {
                scenario,
                baseline,
                parallelism,
                model,
                seed,
            } => {
                let config = ygg::config::AppConfig::from_env()?;
                let pool = ygg::db::create_pool(&config.database_url).await?;
                let baseline: ygg::bench::Baseline =
                    baseline.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                let parallelism = parallelism.unwrap_or_else(|| {
                    ygg::bench::scenarios::find(&scenario)
                        .map(|s| s.default_parallelism as i32)
                        .unwrap_or(1)
                });
                ygg::cli::bench_cmd::run(&pool, &scenario, baseline, parallelism, &model, seed)
                    .await?;
            }
            BenchAction::Report { run_id } => {
                let id = uuid::Uuid::parse_str(&run_id)
                    .map_err(|e| anyhow::anyhow!("invalid run-id: {e}"))?;
                let config = ygg::config::AppConfig::from_env()?;
                let pool = ygg::db::create_pool(&config.database_url).await?;
                ygg::cli::bench_cmd::report(&pool, id).await?;
            }
            BenchAction::Diff { a, b } => {
                let a = uuid::Uuid::parse_str(&a)
                    .map_err(|e| anyhow::anyhow!("invalid run-id a: {e}"))?;
                let b = uuid::Uuid::parse_str(&b)
                    .map_err(|e| anyhow::anyhow!("invalid run-id b: {e}"))?;
                let config = ygg::config::AppConfig::from_env()?;
                let pool = ygg::db::create_pool(&config.database_url).await?;
                ygg::cli::bench_cmd::diff(&pool, a, b).await?;
            }
            BenchAction::Ci { tier } => {
                let tier: ygg::bench::Tier =
                    tier.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                let config = ygg::config::AppConfig::from_env()?;
                let pool = ygg::db::create_pool(&config.database_url).await?;
                ygg::cli::bench_cmd::ci(&pool, tier).await?;
            }
        },
        Commands::Scheduler { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                SchedulerAction::Run => {
                    ygg::cli::scheduler_cmd::run(pool, &config).await?;
                }
                SchedulerAction::Tick => {
                    ygg::cli::scheduler_cmd::tick(&pool, &config).await?;
                }
                SchedulerAction::Status => {
                    ygg::cli::scheduler_cmd::status(&pool).await?;
                }
                SchedulerAction::DryRun => {
                    ygg::cli::scheduler_cmd::dry_run(&pool).await?;
                }
                SchedulerAction::Backfill => {
                    let stats = ygg::scheduler::backfill(&pool).await?;
                    println!("{}", serde_json::to_string_pretty(&stats)?);
                }
            }
        }
        Commands::Bar => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            ygg::cli::bar_cmd::execute(&pool).await?;
        }
        Commands::Plan { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let default_agent = || {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            };
            match action {
                PlanAction::Create {
                    title,
                    description,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    let _ = ygg::cli::plan_cmd::create(
                        &pool,
                        &title,
                        description.as_deref(),
                        &agent_name,
                    )
                    .await?;
                }
                PlanAction::Add {
                    epic,
                    title,
                    description,
                    kind,
                    deps,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    let _ = ygg::cli::plan_cmd::add(
                        &pool,
                        &epic,
                        &title,
                        description.as_deref(),
                        kind.as_deref(),
                        &deps,
                        &agent_name,
                    )
                    .await?;
                }
                PlanAction::Run {
                    task,
                    dry_run,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::run(&pool, &task, &agent_name, dry_run).await?;
                }
                PlanAction::Supervise {
                    epic,
                    parallelism,
                    dry_run,
                    poll_secs,
                    agent,
                } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::supervise(
                        &pool,
                        &epic,
                        &agent_name,
                        parallelism.max(1),
                        dry_run,
                        poll_secs,
                    )
                    .await?;
                }
                PlanAction::Pause { epic } => {
                    ygg::cli::plan_cmd::pause(&pool, &epic).await?;
                }
                PlanAction::Resume { epic } => {
                    ygg::cli::plan_cmd::resume(&pool, &epic).await?;
                }
                PlanAction::Abort { epic, agent } => {
                    let agent_name = agent.unwrap_or_else(default_agent);
                    ygg::cli::plan_cmd::abort(&pool, &epic, &agent_name).await?;
                }
            }
        }
        Commands::Worktree { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            // Mirrors the task-cmd resolver so `ygg worktree ensure ygg-abcd`
            // or `yggdrasil-42` both work.
            async fn resolve_id(pool: &sqlx::PgPool, r: &str) -> Result<uuid::Uuid, anyhow::Error> {
                if let Ok(u) = uuid::Uuid::parse_str(r) {
                    return Ok(u);
                }
                let hex = r.strip_prefix("ygg-").unwrap_or(r);
                if hex.len() >= 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    let m: Vec<uuid::Uuid> = sqlx::query_scalar(
                        "SELECT task_id FROM tasks WHERE task_id::text LIKE $1 LIMIT 5",
                    )
                    .bind(format!("{hex}%"))
                    .fetch_all(pool)
                    .await?;
                    match m.len() {
                        0 => {}
                        1 => return Ok(m[0]),
                        n => anyhow::bail!("ambiguous '{r}' ({n} matches)"),
                    }
                }
                let (prefix, seq) = r
                    .rsplit_once('-')
                    .ok_or_else(|| anyhow::anyhow!("expected <prefix>-<seq> or ygg-<hex>"))?;
                let seq: i32 = seq.parse().map_err(|_| anyhow::anyhow!("bad seq: {seq}"))?;
                let repo = ygg::models::repo::RepoRepo::new(pool)
                    .get_by_prefix(prefix)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no repo '{prefix}'"))?;
                let t = ygg::models::task::TaskRepo::new(pool)
                    .get_by_ref(repo.repo_id, seq)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no task {r}"))?;
                Ok(t.task_id)
            }
            match action {
                WorktreeAction::Ensure { task } => {
                    let id = resolve_id(&pool, &task).await?;
                    let wt = ygg::worktree::ensure(&pool, id).await?;
                    println!("{} → {}", wt.task_ref, wt.path.display());
                    println!("branch: {}", wt.branch);
                    println!("base:   {}", wt.base_path.display());
                }
                WorktreeAction::Rm {
                    task,
                    policy,
                    force,
                } => {
                    let id = resolve_id(&pool, &task).await?;
                    let policy = ygg::worktree::parse_policy(&policy)?;
                    ygg::worktree::teardown(&pool, id, policy, force).await?;
                    println!("removed worktree for {task} ({policy:?})");
                }
                WorktreeAction::List => {
                    let root = ygg::worktree::worktree_root()?;
                    println!("root: {}", root.display());
                    if !root.exists() {
                        println!("(no worktrees created yet)");
                    } else {
                        for entry in std::fs::read_dir(&root)? {
                            let entry = entry?;
                            println!("  {}", entry.file_name().to_string_lossy());
                        }
                    }
                }
            }
        }
        Commands::Rollup { days, repo, format } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let fmt = match format.as_str() {
                "text" | "txt" => ygg::cli::rollup_cmd::Format::Text,
                "json" => ygg::cli::rollup_cmd::Format::Json,
                _ => ygg::cli::rollup_cmd::Format::Markdown,
            };
            ygg::cli::rollup_cmd::execute(&pool, days, repo.as_deref(), fmt).await?;
        }
        Commands::Learn { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let agent_name_default = || {
                std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                })
            };
            match action {
                LearnAction::Create {
                    text,
                    global,
                    file_glob,
                    rule_id,
                    context,
                    agent,
                    scope,
                    json,
                } => {
                    let agent_name = agent.unwrap_or_else(agent_name_default);
                    let scope_tags = ygg::cli::learning_cmd::parse_scope_tags(&scope)?;
                    ygg::cli::learning_cmd::create(
                        &pool,
                        &text,
                        global,
                        file_glob.as_deref(),
                        rule_id.as_deref(),
                        context.as_deref(),
                        &agent_name,
                        &scope_tags,
                        json,
                    )
                    .await?;
                }
                LearnAction::List {
                    file,
                    rule_id,
                    all,
                    json,
                } => {
                    ygg::cli::learning_cmd::list(
                        &pool,
                        file.as_deref(),
                        rule_id.as_deref(),
                        all,
                        json,
                    )
                    .await?;
                }
                LearnAction::Delete { id } => {
                    let uuid = uuid::Uuid::parse_str(&id)
                        .map_err(|_| anyhow::anyhow!("invalid uuid: {id}"))?;
                    ygg::cli::learning_cmd::delete(&pool, uuid).await?;
                }
            }
        }
        Commands::Remember {
            text,
            list,
            global,
            all,
            limit,
            agent,
            json,
        } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            if list {
                ygg::cli::remember_cmd::list(&pool, all || global, limit, json).await?;
            } else {
                let agent_name = agent.unwrap_or_else(|| {
                    std::env::var("YGG_AGENT_NAME").ok().unwrap_or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                            .unwrap_or_else(|| "ygg".to_string())
                    })
                });
                ygg::cli::remember_cmd::remember(&pool, &text, global, &agent_name, json).await?;
            }
        }
        Commands::Handoff { action } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            match action {
                HandoffAction::Save { text, agent, json } => {
                    let agent_name = resolve_agent_arg(agent);
                    // Empty or `-` reads the note from stdin (pipe-friendly).
                    let text = if text.is_empty() || text == "-" {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf)?;
                        buf
                    } else {
                        text
                    };
                    ygg::cli::handoff_cmd::save(&pool, &text, &agent_name, json).await?;
                }
                HandoffAction::Show { agent, json } => {
                    let agent_name = resolve_agent_arg(agent);
                    ygg::cli::handoff_cmd::show(&pool, &agent_name, json).await?;
                }
                HandoffAction::Clear { agent } => {
                    let agent_name = resolve_agent_arg(agent);
                    ygg::cli::handoff_cmd::clear(&pool, &agent_name).await?;
                }
            }
        }
        Commands::AgentTool { tool, agent } => {
            let config = ygg::config::AppConfig::from_env()?;
            let pool = ygg::db::create_pool(&config.database_url).await?;
            let agent_name = agent
                .clone()
                .or_else(|| std::env::var("YGG_AGENT_NAME").ok())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| "ygg".to_string())
                });
            ygg::cli::agent_cmd::set_tool(&pool, &agent_name, &tool).await?;
        }
        Commands::Hook { action } => {
            ygg::cli::hook_cmd::handle(action).await?;
        }
    }

    Ok(())
}
