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
    {
        if o.status.success() {
            let resolved = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !resolved.is_empty() {
                return Some(resolved);
            }
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

async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .is_ok()
}

async fn host_port_open(host: &str, port: u16) -> bool {
    tokio::net::TcpStream::connect(format!("{host}:{port}"))
        .await
        .is_ok()
}

async fn wait_port(port: u16, secs: u64) -> bool {
    for _ in 0..secs {
        if port_open(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

/// Parse (user, host, port, password) from a postgres URL.
/// Falls back to `fallback_user` when no userinfo is present.
fn parse_pg_url_parts(url: &str, fallback_user: &str) -> (String, String, u16, Option<String>) {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(url);
    let (userinfo, hostdb) = rest.split_once('@').unwrap_or(("", rest));
    let (user, pass) = if userinfo.is_empty() {
        (fallback_user.to_string(), None)
    } else if let Some((u, p)) = userinfo.split_once(':') {
        (u.to_string(), Some(p.to_string()))
    } else {
        (userinfo.to_string(), None)
    };
    let (hostport, _) = hostdb.split_once('/').unwrap_or((hostdb, "ygg"));
    let (host, port) = if let Some((h, p)) = hostport.rsplit_once(':') {
        (h.to_string(), p.parse().unwrap_or(5432))
    } else {
        (hostport.to_string(), 5432u16)
    };
    (user, host, port, pass)
}

/// Try to create the given PG role by connecting as a superuser.
/// Attempts `postgres` first, then the system user, then the target user itself.
async fn pg_ensure_role(
    role: &str,
    pass: Option<&str>,
    host: &str,
    port: u16,
    sys_user: &str,
) -> bool {
    let port_s = port.to_string();
    let bin = find_bin("psql").unwrap_or_else(|| "psql".to_string());

    // Check if role already works
    {
        let mut cmd = Command::new(&bin);
        cmd.args([
            "-U", role, "-h", host, "-p", &port_s, "-d", "postgres", "-c", "SELECT 1", "-q",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        if let Some(p) = pass {
            cmd.env("PGPASSWORD", p);
        }
        if cmd.status().await.is_ok_and(|s| s.success()) {
            return true;
        }
    }

    let esc_id = role.replace('"', "\"\"");
    let quoted_role = format!("\"{esc_id}\"");
    let create_sql = if let Some(p) = pass {
        let esc_pass = p.replace('\'', "''");
        format!(
            "DO $$ BEGIN \
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{esc_id}') THEN \
             CREATE ROLE {quoted_role} WITH LOGIN PASSWORD '{esc_pass}' CREATEDB; \
             END IF; \
             END $$"
        )
    } else {
        format!(
            "DO $$ BEGIN \
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{esc_id}') THEN \
             CREATE ROLE {quoted_role} WITH LOGIN CREATEDB; \
             END IF; \
             END $$"
        )
    };
    let grant_sql = format!("GRANT ALL ON SCHEMA public TO {quoted_role}");

    // Try connecting as common superusers to create the role
    let candidates: Vec<&str> = ["postgres", sys_user]
        .iter()
        .copied()
        .filter(|u| *u != role)
        .collect();

    for su in &candidates {
        let mut cmd = Command::new(&bin);
        cmd.args([
            "-U",
            su,
            "-h",
            host,
            "-p",
            &port_s,
            "-d",
            "postgres",
            "-c",
            &create_sql,
            "-c",
            &grant_sql,
            "-q",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        if cmd.status().await.is_ok_and(|s| s.success()) {
            return true;
        }
    }

    false
}

/// Run `createdb` against the configured postgres instance.
async fn pg_createdb(user: &str, host: &str, port: u16, pass: Option<&str>) -> bool {
    let port_s = port.to_string();
    let bin = find_bin("createdb").unwrap_or_else(|| "createdb".to_string());
    let mut cmd = Command::new(&bin);
    cmd.args(["-U", user, "-h", host, "-p", &port_s, "ygg"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(p) = pass {
        cmd.env("PGPASSWORD", p);
    }
    cmd.status().await.is_ok_and(|s| s.success())
}

/// Detect which postgresql@XX version is running via brew services or pg_config.
async fn detect_pg_version() -> String {
    // Try pg_config first
    if let Ok(output) = Command::new("pg_config")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // "PostgreSQL 16.4" → "postgresql@16"
            if let Some(major) = ver
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.split('.').next())
            {
                return format!("postgresql@{major}");
            }
        }
    }

    // Fallback: check which brew services are running
    if let Ok(output) = Command::new("brew")
        .args(["services", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if line.contains("postgresql@") && line.contains("started") {
                    if let Some(name) = line.split_whitespace().next() {
                        return name.to_string();
                    }
                }
            }
        }
    }

    // Fallback: check common install locations
    let prefixes = ["/opt/homebrew/opt", "/usr/local/opt", "/usr/lib/postgresql"];
    for prefix in prefixes {
        for ver in ["18", "17", "16", "15", "14"] {
            let path = format!("{prefix}/postgresql@{ver}");
            let path2 = format!("{prefix}/{ver}");
            if Path::new(&path).exists() {
                return format!("postgresql@{ver}");
            }
            if Path::new(&path2).exists() {
                return format!("postgresql@{ver}");
            }
        }
    }

    "postgresql@18".to_string()
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
    let all_skips: Vec<String> = skips
        .iter()
        .cloned()
        .chain(saved_skips.into_iter())
        .collect();

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

    // Detect system username via whoami (most reliable)
    let sys_user = if let Ok(output) = Command::new("whoami")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "postgres".into())
        }
    } else {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "postgres".into())
    };

    let default_db_url = format!("postgres://{sys_user}@localhost:5432/ygg");
    let env_path = config_dir.join(".env");

    // Load existing config or prompt for database URL.
    let (db_url, config_changed) = if env_path.exists() {
        dotenvy::from_path(&env_path).ok();
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| default_db_url.clone());
        (url, false)
    } else {
        use std::io::{self, BufRead, Write};
        println!("  {BR}│{X}  {B}PostgreSQL connection{X}");
        println!("  {BR}│{X}  {D}default uses system user '{sys_user}', no password{X}");
        println!("  {BR}│{X}  {D}default: {default_db_url}{X}");
        println!("  {BR}│{X}");
        println!("  {BR}│{X}  {Y}use default? [Y/n]{X}");
        // --yes: take the default URL without prompting.
        let a = if non_interactive() {
            println!("  {BR}│{X}  {D}> (--yes) default{X}");
            String::new()
        } else {
            print!("  {BR}│{X}  > ");
            io::stdout().flush().ok();
            let mut answer = String::new();
            io::stdin().lock().read_line(&mut answer).ok();
            answer.trim().to_lowercase()
        };

        let url = if a.is_empty() || a == "y" || a == "yes" {
            default_db_url.clone()
        } else {
            println!(
                "  {BR}│{X}  {D}enter postgres URL (e.g. postgres://user:pass@host:5432/ygg){X}"
            );
            print!("  {BR}│{X}  > ");
            io::stdout().flush().ok();
            let mut custom = String::new();
            io::stdin().lock().read_line(&mut custom).ok();
            let custom = custom.trim().to_string();
            if custom.is_empty() {
                default_db_url.clone()
            } else {
                custom
            }
        };

        // Write config immediately so it's saved
        let env_content = format!(
            "DATABASE_URL={url}\n\
             CONTEXT_LIMIT_TOKENS=250000\n\
             CONTEXT_HARD_CAP_TOKENS=300000\n\
             LOCK_TTL_SECS=300\n\
             HEARTBEAT_INTERVAL_SECS=60\n\
             WATCHER_INTERVAL_SECS=30\n\
             RTK_BINARY_PATH=rtk\n\
             RUST_LOG=ygg=info\n"
        );
        let _ = tokio::fs::write(&env_path, &env_content).await;

        (url, true)
    };

    // Always force the correct DATABASE_URL in env
    unsafe {
        std::env::set_var("DATABASE_URL", &db_url);
    }

    // Parse pg connection details — used for all createdb/psql calls so we
    // connect to the right host/port/user regardless of where postgres lives.
    let (pg_user, pg_host, pg_port, pg_pass) = parse_pg_url_parts(&db_url, &sys_user);
    let pg_is_local = pg_host == "localhost" || pg_host == "127.0.0.1";

    let db_show = db_url
        .find('@')
        .and_then(|at| {
            db_url[..at]
                .rfind(':')
                .map(|c| format!("{}:***@{}", &db_url[..c], &db_url[at + 1..]))
        })
        .unwrap_or_else(|| db_url.clone());

    println!("  {D}pkg{X}     {pkg}");
    println!("  {D}pg{X}      {db_show}");
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

    // ── pg ──
    head("postgresql");

    if skipping(&all_skips, "pg") {
        ok("postgresql", "skipped");
    } else if host_port_open(&pg_host, pg_port).await {
        ok("postgresql", "running");
    } else if pg_is_local {
        // Local postgres not responding — try to start or install it
        if has("psql").await {
            ok("postgresql", "installed");
            let pg_ver = detect_pg_version().await;
            if has_brew {
                run_show("brew", &["services", "start", &pg_ver]).await;
            }
            if wait_port(5432, 10).await {
                ok("postgresql", "started");
            } else {
                bad("postgresql", "not responding on :5432");
                if has_brew {
                    hint(&format!("try: brew services restart {pg_ver}"));
                }
                if !prompt_skip("postgresql") {
                    std::process::exit(1);
                }
            }
        } else {
            bad("postgresql", "not installed");
            if has_brew {
                hint("run: brew install postgresql@18");
                hint("then: brew services start postgresql@18");
            } else if has_apt {
                hint("run: sudo apt-get install -y postgresql postgresql-client");
                hint("then: sudo systemctl start postgresql");
            } else {
                hint("install: https://www.postgresql.org/download/");
            }
            if !prompt_skip("postgresql") {
                std::process::exit(1);
            }
        }
    } else {
        // Remote postgres not reachable
        bad("postgresql", &format!("cannot reach {pg_host}:{pg_port}"));
        hint("check that your port-forward or remote host is accessible");
        if !prompt_skip("postgresql") {
            std::process::exit(1);
        }
    }

    // database + pgvector (only if pg is up)
    if !skipping(&all_skips, "pg") && host_port_open(&pg_host, pg_port).await {
        // Ensure the configured role exists
        if pg_ensure_role(&pg_user, pg_pass.as_deref(), &pg_host, pg_port, &sys_user).await {
            ok(&format!("role '{pg_user}'"), "ready");
        } else {
            bad(&format!("role '{pg_user}'"), "cannot create");
            hint(&format!(
                "run: psql -U postgres -h {pg_host} -p {pg_port} -d postgres -c \"CREATE ROLE {pg_user} WITH LOGIN CREATEDB;\""
            ));
            hint(&format!(
                "then: psql -U postgres -h {pg_host} -p {pg_port} -d postgres -c \"GRANT ALL ON SCHEMA public TO {pg_user};\""
            ));
            hint("then re-run: ygg init");
            // Same skip-or-exit contract as the other postgres failure paths
            // above — without this, --yes (and a plain skip) couldn't get past
            // an existing-but-unmanageable role (e.g. Postgres.app, where the
            // superuser is the OS user, not `postgres`).
            if !prompt_skip("postgresql") {
                std::process::exit(1);
            }
        }

        // Create database
        if pg_createdb(&pg_user, &pg_host, pg_port, pg_pass.as_deref()).await {
            ok("database 'ygg'", "created");
        } else {
            ok("database 'ygg'", "exists");
        }
    }

    // ── config ──
    head("config");

    if config_changed {
        ok(&format!("{}", env_path.display()), "created");
    } else {
        ok(&format!("{}", env_path.display()), "exists");
    }

    if !skipping(&all_skips, "pg") && host_port_open(&pg_host, pg_port).await {
        // Ensure database exists
        pg_createdb(&pg_user, &pg_host, pg_port, pg_pass.as_deref()).await;

        // Pre-flight psql connection check before attempting migrations
        let probe_ok = if skipping(&all_skips, "migrations") {
            true
        } else {
            let port_s = pg_port.to_string();
            let bin = find_bin("psql").unwrap_or_else(|| "psql".to_string());
            let mut cmd = Command::new(&bin);
            cmd.args([
                "-U", &pg_user, "-h", &pg_host, "-p", &port_s, "-d", "ygg", "-c", "SELECT 1", "-q",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
            if let Some(ref p) = pg_pass {
                cmd.env("PGPASSWORD", p);
            }
            match cmd.output().await {
                Ok(output) if output.status.success() => true,
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    bad("connection", "psql probe failed");
                    if !stderr.is_empty() {
                        hint(&format!("psql: {stderr}"));
                    }
                    if stderr.contains("role") && stderr.contains("does not exist") {
                        hint(&format!("configured URL: {db_show}"));
                        hint("create the role or update DATABASE_URL in .env");
                    } else if stderr.contains("password authentication failed") {
                        hint("check the password in DATABASE_URL");
                    } else if stderr.contains("Connection refused") {
                        hint("postgres appears down or not listening on the configured port");
                    } else if stderr.contains("TimeZone") {
                        hint("postgres cannot resolve timezone data — reinstall or check config");
                    }
                    false
                }
                Err(e) => {
                    bad("connection", "psql probe failed");
                    hint(&format!("could not run psql: {e}"));
                    false
                }
            }
        };

        if !probe_ok {
            if prompt_skip("migrations") {
                // skip migrations entirely
            } else {
                std::process::exit(1);
            }
        }

        if probe_ok && !skipping(&all_skips, "migrations") {
            let pb = spin("running migrations...");
            match async {
                let pool = db::create_pool(&db_url).await?;
                db::run_migrations(&pool).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await
            {
                Result::Ok(()) => {
                    pb.finish_and_clear();
                    ok("migrations", "applied");
                }
                Err(e) => {
                    pb.finish_and_clear();
                    let err_str = format!("{e}");
                    bad("migrations", "failed");

                    if err_str.contains("TimeZone") {
                        hint("PostgreSQL cannot resolve timezone data");
                        hint("this is common with brew postgresql@17/18 installs");
                        hint("");
                        if has_brew {
                            let pg_ver = detect_pg_version().await;
                            hint(&format!("try: brew reinstall {pg_ver}"));
                            hint(&format!("then: brew services restart {pg_ver}"));
                        } else {
                            hint("ensure timezone data is installed (postgresql-common on apt)");
                            hint("check: psql -c \"SHOW timezone\"");
                        }
                    } else if err_str.contains("role") && err_str.contains("does not exist") {
                        hint("the configured role doesn't exist on this postgres server");
                        hint(&format!("current URL: {db_show}"));
                        hint("");
                        // Offer to reconfigure the URL in-place. Skipped under
                        // --yes — there's no new URL to read unattended, so just
                        // print the manual-fix hints below.
                        if !non_interactive() && prompt_yes("reconfigure the database URL now?") {
                            use std::io::{self, BufRead, Write};
                            hint(&format!("your system user is: {sys_user}"));
                            println!(
                                "  {BR}│{X}  {D}enter postgres URL (e.g. postgres://user:pass@host:port/ygg){X}"
                            );
                            print!("  {BR}│{X}  > ");
                            io::stdout().flush().ok();
                            let mut new_url = String::new();
                            io::stdin().lock().read_line(&mut new_url).ok();
                            let new_url = new_url.trim().to_string();
                            if !new_url.is_empty() {
                                let new_content = format!(
                                    "DATABASE_URL={new_url}\n\
                                 CONTEXT_LIMIT_TOKENS=250000\n\
                                 CONTEXT_HARD_CAP_TOKENS=300000\n\
                                 LOCK_TTL_SECS=300\n\
                                 HEARTBEAT_INTERVAL_SECS=60\n\
                                 WATCHER_INTERVAL_SECS=30\n\
                                 RTK_BINARY_PATH=rtk\n\
                                 RUST_LOG=ygg=info\n"
                                );
                                if tokio::fs::write(&env_path, &new_content).await.is_ok() {
                                    ok("config", "updated — re-run: ygg init");
                                }
                            }
                        } else {
                            hint(&format!("  rm {}", env_path.display()));
                            hint("  ygg init");
                        }
                    } else if err_str.contains("does not exist") && err_str.contains("database") {
                        hint("database 'ygg' doesn't exist");
                        hint(&format!("  createdb -U {sys_user} ygg"));
                    } else {
                        hint(&format!("{e}"));
                    }

                    if !prompt_skip("migrations") {
                        std::process::exit(1);
                    }
                    hint("to retry later: ygg migrate");
                }
            }
        } // if probe_ok
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
    if !skipping(&all_skips, "project") {
        if let Ok(cwd) = std::env::current_dir() {
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

    use super::parse_pg_url_parts;

    #[test]
    fn parse_full_url_with_user_pass_host_port() {
        let (user, host, port, pass) = parse_pg_url_parts(
            "postgres://alice:s3cret@db.example.com:5433/ygg",
            "fallback",
        );
        assert_eq!(user, "alice");
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 5433);
        assert_eq!(pass, Some("s3cret".to_string()));
    }

    #[test]
    fn parse_user_without_password() {
        let (user, host, port, pass) =
            parse_pg_url_parts("postgres://bob@localhost:5432/ygg", "fallback");
        assert_eq!(user, "bob");
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
        assert_eq!(pass, None);
    }

    #[test]
    fn parse_no_userinfo_uses_fallback() {
        let (user, host, port, pass) =
            parse_pg_url_parts("postgres://localhost:5432/ygg", "sysuser");
        assert_eq!(user, "sysuser");
        assert_eq!(host, "localhost");
        assert_eq!(port, 5432);
        assert_eq!(pass, None);
    }

    #[test]
    fn parse_postgresql_prefix() {
        let (user, host, port, pass) =
            parse_pg_url_parts("postgresql://admin:pw@pghost:5555/mydb", "fallback");
        assert_eq!(user, "admin");
        assert_eq!(host, "pghost");
        assert_eq!(port, 5555);
        assert_eq!(pass, Some("pw".to_string()));
    }

    #[test]
    fn parse_default_port_when_omitted() {
        let (user, host, port, pass) =
            parse_pg_url_parts("postgres://alice@myhost/ygg", "fallback");
        assert_eq!(user, "alice");
        assert_eq!(host, "myhost");
        assert_eq!(port, 5432);
        assert_eq!(pass, None);
    }

    #[test]
    fn parse_non_standard_port() {
        let (user, host, port, pass) =
            parse_pg_url_parts("postgres://dev:devpass@127.0.0.1:15432/ygg", "fallback");
        assert_eq!(user, "dev");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 15432);
        assert_eq!(pass, Some("devpass".to_string()));
    }
}
