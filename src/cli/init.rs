use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::Command;

use crate::db;

// Non-interactive mode (`ygg init --yes`). When set, every prompt resolves to
// its default branch (proceed on yes/no questions, continue on skip questions)
// without reading stdin — so the install script, CI, and spawned agents can run
// `ygg init` unattended without hanging on a TTY.
static NON_INTERACTIVE: AtomicBool = AtomicBool::new(false);

fn non_interactive() -> bool {
    NON_INTERACTIVE.load(Ordering::Relaxed)
}

// Colors
const D: &str = "\x1b[90m";
const O: &str = "\x1b[38;5;208m";
const G: &str = "\x1b[38;5;114m";
const R: &str = "\x1b[38;5;203m";
const Y: &str = "\x1b[38;5;221m";
const B: &str = "\x1b[1m";
const X: &str = "\x1b[0m";

// Tree colors
const G1: &str = "\x1b[38;5;22m";
const G2: &str = "\x1b[38;5;28m";
const G3: &str = "\x1b[38;5;34m";
const G4: &str = "\x1b[38;5;40m";
const G5: &str = "\x1b[38;5;46m";
const TK: &str = "\x1b[38;5;94m";
const RT: &str = "\x1b[38;5;58m";
const FR: &str = "\x1b[38;5;48m";
const EY: &str = "\x1b[38;5;226m";
const TG: &str = "\x1b[38;5;204m";

fn banner() {
    println!();
    println!("       {G5}▄{X}         {FR} ▄▄▄{X}");
    println!("      {G4}▄█▄{X}       {FR}▐{EY}o{FR} {EY}o{FR}▌{X}");
    println!("     {G3}▄███▄{X}      {FR} \\{TG}w{FR}/ {X}");
    println!("    {G2}▄█████▄{X}    {FR}▐▌   ▐▌{X}");
    println!("   {G1}▄███████▄{X}   {FR}▐▌   ▐▌{X}");
    println!("      {TK}▐█▌{X}      {FR}^     ^{X}");
    println!("   {RT}▀▀▀▀█▀▀▀▀{X}");
    println!();
    println!(
        "  {O}{B}Y G G D R A S I L{X} {D}v{}{X}",
        env!("CARGO_PKG_VERSION")
    );
    println!();
}

fn spin(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!("  {BR}│{X} {{spinner}} {{msg}}"))
            .unwrap()
            .tick_strings(&["◜", "◠", "◝", "◞", "◡", "◟"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

// Border color — forest green to match the tree theme
const BR: &str = "\x1b[38;5;34m";

fn ok(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {BR}│{X}  {label} {D}{d}{X} {G}{state}{X}");
}

fn bad(label: &str, state: &str) {
    let dots = 40usize.saturating_sub(label.len() + state.len());
    let d: String = std::iter::repeat_n('·', dots).collect();
    println!("  {BR}│{X}  {label} {D}{d}{X} {R}{state}{X}");
}

fn hint(msg: &str) {
    println!("  {BR}│{X}  {D}{msg}{X}");
}

fn head(title: &str) {
    println!("  {BR}│{X}");
    println!("  {BR}├─ {G4}{B}{title}{X}");
    println!("  {BR}│{X}");
}

fn prompt_yes(msg: &str) -> bool {
    use std::io::{self, BufRead, Write};
    println!("  {BR}│{X}");
    println!("  {BR}│{X}  {Y}{msg} [Y/n]{X}");
    if non_interactive() {
        println!("  {BR}│{X}  {D}> (--yes) yes{X}");
        return true;
    }
    print!("  {BR}│{X}  > ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

async fn offer_curl_install(name: &str, script_url: &str) {
    hint(&format!("install script: {script_url}"));
    if prompt_yes(&format!("run install script for {name}?")) {
        let installed = run_show("sh", &["-c", &format!("curl -fsSL {script_url} | sh")]).await;
        if installed && has(name).await {
            ok(name, "installed");
        } else {
            bad(name, "install script failed");
            hint(&format!("try manually: curl -fsSL {script_url} | sh"));
        }
    } else if !prompt_skip(name) {
        std::process::exit(1);
    }
}

fn prompt_skip(name: &str) -> bool {
    use std::io::{self, BufRead, Write};
    println!("  {BR}│{X}");
    println!("  {BR}│{X}  {Y}skip {name} and continue? [Y/n]{X}");
    // In --yes mode, continue past the failed dep but DON'T persist the skip —
    // an unattended run shouldn't silently teach future interactive runs to
    // skip this dep forever.
    if non_interactive() {
        println!("  {BR}│{X}  {D}> (--yes) skip and continue{X}");
        return true;
    }
    println!("  {BR}│{X}  {D}(choice will be remembered for future runs){X}");
    print!("  {BR}│{X}  > ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();
    let skip = a.is_empty() || a == "y" || a == "yes";
    if skip {
        // Save the decision — use sync write since we're in a sync fn
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let config_dir = std::path::Path::new(&home).join(".config/ygg");
        let path = config_dir.join("skips.json");
        let mut skips: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or_default();
        let key = name.to_lowercase();
        if !skips.contains(&key) {
            skips.push(key);
            let _ = std::fs::write(
                &path,
                serde_json::to_string_pretty(&skips).unwrap_or_default(),
            );
        }
    }
    skip
}

/// Find a binary by checking known paths, then PATH.
fn find_bin(name: &str) -> Option<String> {
    for dir in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
    ] {
        let p = format!("{dir}/{name}");
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    // Check HOME-relative paths
    if let Ok(home) = std::env::var("HOME") {
        for sub in [".local/bin", ".cargo/bin"] {
            let p = format!("{home}/{sub}/{name}");
            if Path::new(&p).exists() {
                return Some(p);
            }
        }
    }
    // Fallback: which — return the absolute path it resolves, not the bare name.
    if let Ok(o) = std::process::Command::new("which")
        .arg(name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        && o.status.success()
    {
        let resolved = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !resolved.is_empty() {
            return Some(resolved);
        }
    }
    None
}

async fn has(name: &str) -> bool {
    find_bin(name).is_some()
}

async fn run_show(cmd: &str, args: &[&str]) -> bool {
    let bin = find_bin(cmd).unwrap_or_else(|| cmd.to_string());
    Command::new(&bin)
        .args(args)
        .status()
        .await
        .is_ok_and(|s| s.success())
}

// ─── init ────────────────────────────────────────────────

pub async fn execute_with_options(
    _verbose: bool,
    skip: &[String],
    non_interactive: bool,
) -> Result<(), anyhow::Error> {
    init(skip, non_interactive).await
}

pub async fn execute() -> Result<(), anyhow::Error> {
    init(&[], false).await
}

fn skipping(list: &[String], name: &str) -> bool {
    list.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Load saved skip decisions from ~/.config/ygg/skips.json
async fn load_saved_skips(config_dir: &Path) -> Vec<String> {
    let path = config_dir.join("skips.json");
    if let Ok(data) = tokio::fs::read_to_string(&path).await {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        vec![]
    }
}

async fn init(skips: &[String], yes: bool) -> Result<(), anyhow::Error> {
    NON_INTERACTIVE.store(yes, Ordering::Relaxed);

    // Config lives in ~/.config/ygg/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let config_dir = Path::new(&home).join(".config").join("ygg");
    tokio::fs::create_dir_all(&config_dir).await.ok();

    // Merge CLI skips with saved skips
    let saved_skips = load_saved_skips(&config_dir).await;
    let all_skips: Vec<String> = skips.iter().cloned().chain(saved_skips).collect();

    // Ensure we're in a valid directory — brew/apt fail if cwd is gone
    if std::env::current_dir().is_err() {
        let _ = std::env::set_current_dir(&home);
    }

    banner();

    let has_brew = has("brew").await;
    let has_apt = has("apt-get").await;
    let pkg = if has_brew {
        "brew"
    } else if has_apt {
        "apt"
    } else {
        "—"
    };

    let env_path = config_dir.join(".env");

    // SQLite: no connection URL to prompt for. The database lives at
    // YGG_DB_PATH > DATABASE_URL (sqlite://...) > $XDG_DATA_HOME/ygg/ygg.db.
    let config_changed = if env_path.exists() {
        dotenvy::from_path(&env_path).ok();
        false
    } else {
        let env_content = "# Database location (optional). Default: $XDG_DATA_HOME/ygg/ygg.db\n\
             # YGG_DB_PATH=/path/to/ygg.db\n\
             CONTEXT_LIMIT_TOKENS=250000\n\
             CONTEXT_HARD_CAP_TOKENS=300000\n\
             LOCK_TTL_SECS=300\n\
             HEARTBEAT_INTERVAL_SECS=60\n\
             WATCHER_INTERVAL_SECS=30\n\
             RTK_BINARY_PATH=rtk\n\
             RUST_LOG=ygg=info\n";
        let _ = tokio::fs::write(&env_path, env_content).await;
        true
    };

    let db_url = db::resolve_database_url();
    let db_show = db_url.clone();

    println!("  {D}pkg{X}     {pkg}");
    println!("  {D}db{X}      {db_show}");
    println!();
    println!("  {BR}╭─────────────────────────────────────────────╮{X}");

    // Collect missing deps that need manual install
    let mut missing: Vec<(&str, &str)> = Vec::new();

    // ── deps ──
    head("dependencies");

    // Tools we can brew install (no sudo needed)
    for (name, brew_pkg) in [("tmux", "tmux"), ("jq", "jq")] {
        if has(name).await {
            ok(name, "found");
        } else if has_brew {
            let pb = spin(&format!("brew install {brew_pkg}..."));
            let installed = run_show("brew", &["install", brew_pkg]).await;
            pb.finish_and_clear();
            if installed {
                ok(name, "installed");
            } else {
                bad(name, "brew install failed");
                if !prompt_skip(name) {
                    std::process::exit(1);
                }
            }
        } else {
            // Can't auto-install without brew — tell user what to run
            bad(name, "not found");
            if has_apt {
                hint(&format!("run: sudo apt-get install -y {name}"));
            }
            missing.push((name, brew_pkg));
            if !prompt_skip(name) {
                std::process::exit(1);
            }
        }
    }

    // Tools with install scripts — prompt before running
    if !has("rtk").await {
        bad("rtk", "not found");
        if has_brew {
            hint("install: brew install rtk");
            if prompt_yes("install rtk via brew?") {
                let pb = spin("brew install rtk...");
                let installed = run_show("brew", &["install", "rtk"]).await;
                pb.finish_and_clear();
                if installed && has("rtk").await {
                    ok("rtk", "installed");
                } else {
                    bad("rtk", "brew install failed, trying install script...");
                    offer_curl_install(
                        "rtk",
                        "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh",
                    )
                    .await;
                }
            } else if !prompt_skip("rtk") {
                std::process::exit(1);
            }
        } else {
            offer_curl_install(
                "rtk",
                "https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh",
            )
            .await;
        }
    } else {
        ok("rtk", "found");
    }

    // ── database (SQLite) ──
    head("database");

    if skipping(&all_skips, "migrations") {
        ok("migrations", "skipped");
    } else {
        let pb = spin("opening database + running migrations...");
        match async {
            let pool = db::create_pool(&db_url).await?;
            db::run_migrations(&pool).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await
        {
            Result::Ok(()) => {
                pb.finish_and_clear();
                ok("database", &db_url);
                ok("migrations", "applied");
            }
            Err(e) => {
                pb.finish_and_clear();
                bad("migrations", "failed");
                hint(&format!("{e}"));
                hint("check that the database path is writable");
                if !prompt_skip("migrations") {
                    std::process::exit(1);
                }
                hint("to retry later: ygg migrate");
            }
        }
    }

    // ── config ──
    head("config");

    if config_changed {
        ok(&format!("{}", env_path.display()), "created");
    } else {
        ok(&format!("{}", env_path.display()), "exists");
    }

    // ── hooks + status bar ──
    if !skipping(&all_skips, "hooks") {
        head("hooks");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let claude_dir = Path::new(&home).join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.ok();

        // Install status bar script
        let status_dest = claude_dir.join("ygg-status.sh");
        tokio::fs::write(&status_dest, include_str!("../../scripts/ygg-status.sh")).await?;
        Command::new("chmod")
            .args(["+x", status_dest.to_str().unwrap()])
            .status()
            .await
            .ok();
        ok("ygg-status.sh", "installed");

        // Install hook scripts
        let hooks_dir = claude_dir.join("ygg-hooks");
        tokio::fs::create_dir_all(&hooks_dir).await.ok();

        for (name, content) in [
            (
                "session-start.sh",
                include_str!("../../scripts/hooks/session-start.sh"),
            ),
            (
                "pre-tool-use.sh",
                include_str!("../../scripts/hooks/pre-tool-use.sh"),
            ),
            (
                "prompt-submit.sh",
                include_str!("../../scripts/hooks/prompt-submit.sh"),
            ),
            (
                "pre-compact.sh",
                include_str!("../../scripts/hooks/pre-compact.sh"),
            ),
            ("stop.sh", include_str!("../../scripts/hooks/stop.sh")),
        ] {
            let dest = hooks_dir.join(name);
            tokio::fs::write(&dest, content).await?;
            Command::new("chmod")
                .args(["+x", dest.to_str().unwrap()])
                .status()
                .await
                .ok();
        }
        ok("hook scripts", "installed");

        // Install slash commands into ~/.claude/commands/
        let commands_dir = claude_dir.join("commands");
        tokio::fs::create_dir_all(&commands_dir).await.ok();
        for (name, content) in [
            (
                "ygg-status.md",
                include_str!("../../scripts/commands/ygg-status.md"),
            ),
            (
                "ygg-spawn.md",
                include_str!("../../scripts/commands/ygg-spawn.md"),
            ),
            (
                "ygg-lock.md",
                include_str!("../../scripts/commands/ygg-lock.md"),
            ),
        ] {
            let dest = commands_dir.join(name);
            tokio::fs::write(&dest, content).await?;
        }
        ok("slash commands", "installed");

        // Install hooks into Claude Code settings — use native `ygg hook`
        // subcommand instead of shell scripts. The shell scripts are still
        // installed above for backwards compatibility / manual use.
        let settings_path = claude_dir.join("settings.json");

        // Resolve the ygg binary path so the hook command is absolute.
        let ygg_bin = find_bin("ygg").unwrap_or_else(|| "ygg".to_string());

        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{ygg_bin} hook session-start")}]}],
                "PreToolUse": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{ygg_bin} hook pre-tool-use")}]}],
                "UserPromptSubmit": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{ygg_bin} hook prompt-submit")}]}],
                "PreCompact": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{ygg_bin} hook pre-compact")}]}],
                "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": format!("{ygg_bin} hook stop")}]}]
            },
            "statusLine": {
                "type": "command",
                "command": format!("{}", status_dest.to_string_lossy()),
                "refreshInterval": 3
            }
        });

        // Merge with existing settings if present
        let final_settings = if settings_path.exists() {
            let existing = tokio::fs::read_to_string(&settings_path)
                .await
                .unwrap_or_default();
            if let Ok(mut existing_json) = serde_json::from_str::<serde_json::Value>(&existing) {
                // Merge hooks and statusLine into existing
                if let Some(obj) = existing_json.as_object_mut() {
                    obj.insert("hooks".to_string(), settings["hooks"].clone());
                    obj.insert("statusLine".to_string(), settings["statusLine"].clone());
                }
                existing_json
            } else {
                settings
            }
        } else {
            settings
        };

        tokio::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&final_settings)?,
        )
        .await?;
        ok("claude settings.json", "hooks registered");

        hint("hooks will fire automatically in Claude Code sessions");
        hint("no manual ygg commands needed — just use Claude normally");
    }

    // ── project integration ──
    if !skipping(&all_skips, "project")
        && let Ok(cwd) = std::env::current_dir()
    {
        head("project integration");
        if super::init_project::has_any_content(&cwd) {
            hint("CLAUDE.md or AGENTS.md already has content — skipping auto-install");
            hint("run `ygg integrate` to install the managed block, `--remove` to strip it");
        } else {
            match super::init_project::install(&cwd) {
                Ok(report) => {
                    for (path, action) in &report.files {
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                        ok(name, &action.to_string());
                    }
                }
                Err(e) => {
                    bad("project integration", &format!("{e}"));
                }
            }
        }
    }

    // ── done ──
    println!("  {BR}│{X}");
    println!("  {BR}╰─────────────────────────────────────────────╯{X}");
    print!("\x1b[?25h");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Show manual install commands if anything was missing
    if !missing.is_empty() {
        println!();
        println!("  {Y}manual installs needed:{X}");
        for (name, pkg) in &missing {
            if has_apt {
                println!("    sudo apt-get install -y {pkg}");
            } else {
                println!("    # install {name} manually");
            }
        }
        println!("  {D}then re-run: ygg init{X}");
    }

    println!();
    println!("  {G}{B}ready{X}");
    println!();
    println!("  {D}next:{X}");
    println!("    {O}ygg spawn{X} --task {D}\"your task\"{X}");
    println!("    {O}ygg dashboard{X}");
    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    /// yggdrasil-107: hook drift regression. The PreToolUse hook MUST bump
    /// `task_runs.heartbeat_at` so the scheduler doesn't reap live agents as
    /// crashed (yggdrasil-99). Drop this line from scripts/hooks and the
    /// scheduler stops working as soon as a user runs `ygg init`.
    #[test]
    fn pre_tool_use_hook_includes_heartbeat() {
        let content = include_str!("../../scripts/hooks/pre-tool-use.sh");
        assert!(
            content.contains("ygg run heartbeat"),
            "pre-tool-use.sh must invoke `ygg run heartbeat` (see yggdrasil-99); installed hooks come from this file"
        );
    }

    /// yggdrasil-107: same drift class. Stop hook owns the run-terminal
    /// transition + commit/branch capture (yggdrasil-97). If this line goes
    /// missing, runs never finalize and the scheduler reaper has to clean up
    /// (slower, lossier).
    #[test]
    fn stop_hook_includes_capture_outcome() {
        let content = include_str!("../../scripts/hooks/stop.sh");
        assert!(
            content.contains("ygg run capture-outcome"),
            "stop.sh must invoke `ygg run capture-outcome` (see yggdrasil-97)"
        );
    }
}
