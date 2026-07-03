//! `ygg memory <action>` — manage and sync the shared agent memory hosted by
//! the **mnemosyne** MCP service (see `agent-sandbox/docs/hermes-gateway.md`
//! "## Agent memory (mnemosyne)" and `docs/adr/0002-agent-memory-hosted-surface.md`).
//!
//! Every command talks to the service over MCP-over-SSE via [`crate::mcp::McpClient`],
//! calling the `mnemosyne_*` tools. Reads accept `--json`; all commands accept
//! `--bank <b>` (default from `MNEMOSYNE_MCP_BANK`, else `"default"`).
//!
//! ## Sync / snapshot design
//! `mnemosyne_export`/`mnemosyne_import` use **server-side file paths** — the
//! service writes/reads JSON on *its own* filesystem, not over the wire. So
//! there are two modes:
//!
//!   * **Shared-host export** (preferred): when `MNEMOSYNE_EXPORT_DIR` is set to
//!     a directory readable by both ygg and the service, `export` calls
//!     `mnemosyne_export` with an `output_path` under it, then copies that file
//!     to the local snapshot dir verbatim — so `import` can place it back and
//!     call `mnemosyne_import`. This is the faithful, full-fidelity round-trip.
//!   * **Recall fallback** (host-agnostic): when the export dir is not set,
//!     `export` synthesises a snapshot by calling `mnemosyne_recall` with a
//!     broad query and a high limit (there is no "list all over the wire" tool,
//!     and `recall` takes no offset, so this is a single high-limit pull), and
//!     `import` replays each record through `mnemosyne_remember`, de-duplicating
//!     by content hash against the current bank.
//!
//! `sync` is a one-way pull-with-diff: export, then report added/removed/changed
//! counts (keyed by memory id) against the previous snapshot. It is deliberately
//! not a bidirectional merge.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::AppConfig;
use crate::mcp::McpClient;

/// Default record cap for broad `recall` pulls (list + export fallback).
const BROAD_LIMIT: u32 = 1000;

#[derive(Subcommand)]
pub enum MemoryAction {
    /// List memories in a bank (broad `mnemosyne_recall`; there is no list-all
    /// tool, so this is a high-limit recall over an empty query).
    List {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        bank: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search memories by relevance (`mnemosyne_recall`).
    Search {
        query: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
        #[arg(long)]
        bank: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one memory (`mnemosyne_get`).
    Show {
        memory_id: String,
        #[arg(long)]
        bank: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add a memory (`mnemosyne_remember`); prints the new id.
    Add {
        content: String,
        #[arg(long)]
        importance: Option<f64>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long = "valid-until")]
        valid_until: Option<String>,
        /// Extract entities from the content server-side.
        #[arg(long)]
        entities: bool,
        /// Run the server-side extraction pipeline.
        #[arg(long)]
        extract: bool,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Edit a memory's content and/or importance (`mnemosyne_update`).
    Update {
        memory_id: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        importance: Option<f64>,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Hard-delete a memory (`mnemosyne_forget`).
    Rm {
        memory_id: String,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Soft-supersede a memory, optionally pointing at a replacement
    /// (`mnemosyne_invalidate`).
    Invalidate {
        memory_id: String,
        #[arg(long)]
        replacement: Option<String>,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Store counts / tier stats (`mnemosyne_stats`).
    Stats {
        #[arg(long)]
        bank: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the MCP tools the service exposes (`tools/list`; introspection).
    Tools {
        #[arg(long)]
        json: bool,
    },
    /// Snapshot memories to a local JSON file (shared-host export or recall
    /// fallback — see module docs).
    Export {
        /// Snapshot path (default `$XDG_DATA_HOME/ygg/memory-snapshots/<bank>-<ts>.json`).
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Restore memories from a local snapshot (server-side import when the
    /// export dir is set, else replay via `mnemosyne_remember`).
    Import {
        path: PathBuf,
        #[arg(long)]
        bank: Option<String>,
    },
    /// Export to the snapshot dir, then report drift vs. the previous snapshot.
    Sync {
        #[arg(long)]
        bank: Option<String>,
    },
}

/// Dispatch a `ygg memory` subcommand. Opens a fresh MCP session per call.
pub async fn handle(config: &AppConfig, action: MemoryAction) -> anyhow::Result<()> {
    match action {
        MemoryAction::List { limit, bank, json } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let result = recall(&client, "", limit, &bank).await?;
            let records = extract_records(&result);
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else {
                print_record_table(&records);
            }
            Ok(())
        }

        MemoryAction::Search {
            query,
            limit,
            bank,
            json,
        } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let result = recall(&client, &query, limit, &bank).await?;
            let records = extract_records(&result);
            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else {
                print_record_table(&records);
            }
            Ok(())
        }

        MemoryAction::Show {
            memory_id,
            bank,
            json,
        } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let result = client
                .call_tool(
                    "mnemosyne_get",
                    with_bank(json!({"memory_id": memory_id}), &bank),
                )
                .await?;
            print_value(&result, json);
            Ok(())
        }

        MemoryAction::Add {
            content,
            importance,
            source,
            scope,
            valid_until,
            entities,
            extract,
            bank,
        } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let mut args = json!({"content": content});
            set_opt(&mut args, "importance", importance.map(Into::into));
            set_opt(&mut args, "source", source.map(Value::String));
            set_opt(&mut args, "scope", scope.map(Value::String));
            set_opt(&mut args, "valid_until", valid_until.map(Value::String));
            if entities {
                args["extract_entities"] = Value::Bool(true);
            }
            if extract {
                args["extract"] = Value::Bool(true);
            }
            let result = client
                .call_tool("mnemosyne_remember", with_bank(args, &bank))
                .await?;
            match record_id(&result) {
                Some(id) => println!("remembered {id}"),
                None => println!("remembered: {}", compact(&result)),
            }
            Ok(())
        }

        MemoryAction::Update {
            memory_id,
            content,
            importance,
            bank,
        } => {
            if content.is_none() && importance.is_none() {
                bail!("nothing to update — pass --content and/or --importance");
            }
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let mut args = json!({"memory_id": memory_id});
            set_opt(&mut args, "content", content.map(Value::String));
            set_opt(&mut args, "importance", importance.map(Into::into));
            client
                .call_tool("mnemosyne_update", with_bank(args, &bank))
                .await?;
            println!("updated {memory_id}");
            Ok(())
        }

        MemoryAction::Rm { memory_id, bank } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            client
                .call_tool(
                    "mnemosyne_forget",
                    with_bank(json!({"memory_id": memory_id}), &bank),
                )
                .await?;
            println!("forgot {memory_id}");
            Ok(())
        }

        MemoryAction::Invalidate {
            memory_id,
            replacement,
            bank,
        } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let mut args = json!({"memory_id": memory_id});
            set_opt(&mut args, "replacement_id", replacement.map(Value::String));
            client
                .call_tool("mnemosyne_invalidate", with_bank(args, &bank))
                .await?;
            println!("invalidated {memory_id}");
            Ok(())
        }

        MemoryAction::Stats { bank, json } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let result = client
                .call_tool("mnemosyne_stats", with_bank(json!({}), &bank))
                .await?;
            print_value(&result, json);
            Ok(())
        }

        MemoryAction::Tools { json } => {
            let client = connect(config).await?;
            let names = client.list_tools().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&names)?);
            } else if names.is_empty() {
                println!("(no tools reported)");
            } else {
                println!("{} tool(s):", names.len());
                for n in names {
                    println!("  {n}");
                }
            }
            Ok(())
        }

        MemoryAction::Export { out, bank } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            let path = match out {
                Some(p) => p,
                None => default_snapshot_path(&bank)?,
            };
            let (count, strategy) = export_to(config, &client, &bank, &path).await?;
            println!(
                "exported {count} record(s) via {strategy} -> {}",
                path.display()
            );
            Ok(())
        }

        MemoryAction::Import { path, bank } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            import_from(config, &client, &bank, &path).await
        }

        MemoryAction::Sync { bank } => {
            let bank = resolve_bank(config, bank);
            let client = connect(config).await?;
            sync(config, &client, &bank).await
        }
    }
}

// ── connection + argument helpers ───────────────────────────────────────────

async fn connect(config: &AppConfig) -> anyhow::Result<McpClient> {
    McpClient::connect_from_config(config).await
}

/// Resolve the effective bank: `--bank` > `MNEMOSYNE_MCP_BANK` > `"default"`.
fn resolve_bank(config: &AppConfig, arg: Option<String>) -> String {
    arg.or_else(|| config.mnemosyne_mcp_bank.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// Attach `bank` to a tool argument object.
fn with_bank(mut args: Value, bank: &str) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.insert("bank".to_string(), Value::String(bank.to_string()));
    }
    args
}

/// Insert `key` only when the value is present (skips JSON nulls).
fn set_opt(args: &mut Value, key: &str, value: Option<Value>) {
    if let (Some(obj), Some(v)) = (args.as_object_mut(), value) {
        obj.insert(key.to_string(), v);
    }
}

/// One `mnemosyne_recall` call.
async fn recall(client: &McpClient, query: &str, limit: u32, bank: &str) -> anyhow::Result<Value> {
    client
        .call_tool(
            "mnemosyne_recall",
            with_bank(json!({"query": query, "limit": limit}), bank),
        )
        .await
}

// ── record shape helpers (defensive — server shapes vary) ───────────────────

/// Pull the record list out of a tool result. Handles a bare array, common
/// envelope keys (`results`/`memories`/`items`/`records`/`data`), and the
/// `ygg` snapshot wrapper.
fn extract_records(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    for key in ["records", "results", "memories", "items", "data"] {
        if let Some(arr) = value.get(key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    // A single object is treated as a one-record list.
    if value.is_object() {
        return vec![value.clone()];
    }
    Vec::new()
}

fn record_id(record: &Value) -> Option<String> {
    for key in ["memory_id", "id"] {
        if let Some(s) = record.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

fn record_content(record: &Value) -> String {
    for key in ["content", "text", "value"] {
        if let Some(s) = record.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

fn record_importance(record: &Value) -> Option<f64> {
    record.get("importance").and_then(Value::as_f64)
}

fn record_scope(record: &Value) -> String {
    record
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

// ── printing ────────────────────────────────────────────────────────────────

fn print_record_table(records: &[Value]) {
    if records.is_empty() {
        println!("(no memories)");
        return;
    }
    println!("{:<38} {:>4} {:<8} CONTENT", "ID", "IMP", "SCOPE");
    for r in records {
        let id = record_id(r).unwrap_or_else(|| "-".to_string());
        let imp = record_importance(r)
            .map(|i| format!("{i:.2}"))
            .unwrap_or_else(|| "-".to_string());
        let scope = record_scope(r);
        let content = record_content(r).replace('\n', " ");
        println!(
            "{:<38} {:>4} {:<8} {}",
            truncate(&id, 38),
            imp,
            truncate(&scope, 8),
            truncate(&content, 80),
        );
    }
    println!("({} record(s))", records.len());
}

fn print_value(value: &Value, _json: bool) {
    // The get/stats shapes are server-defined; pretty-print in both modes.
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(_) => println!("{value}"),
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{taken}…")
}

// ── snapshot paths ──────────────────────────────────────────────────────────

fn snapshot_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .ok_or_else(|| anyhow!("cannot resolve a data dir (set XDG_DATA_HOME or HOME)"))?;
    Ok(base.join("ygg/memory-snapshots"))
}

fn default_snapshot_path(bank: &str) -> anyhow::Result<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    Ok(snapshot_dir()?.join(format!("{bank}-{ts}.json")))
}

// ── export ──────────────────────────────────────────────────────────────────

/// Produce a snapshot at `path`. Returns `(record_count, strategy_label)`.
async fn export_to(
    config: &AppConfig,
    client: &McpClient,
    bank: &str,
    path: &Path,
) -> anyhow::Result<(usize, &'static str)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating snapshot dir {}", parent.display()))?;
    }

    if let Some(export_dir) = config.mnemosyne_export_dir.as_deref() {
        // Shared-host export: the service writes native JSON to a path we can
        // read, then we copy it verbatim so `import` can round-trip it.
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let server_path = PathBuf::from(export_dir).join(format!("ygg-export-{bank}-{ts}.json"));
        client
            .call_tool(
                "mnemosyne_export",
                with_bank(json!({"output_path": server_path.to_string_lossy()}), bank),
            )
            .await
            .context("mnemosyne_export failed")?;
        let bytes = std::fs::read(&server_path).with_context(|| {
            format!(
                "reading server export at {} — is MNEMOSYNE_EXPORT_DIR shared and readable?",
                server_path.display()
            )
        })?;
        std::fs::write(path, &bytes)
            .with_context(|| format!("writing snapshot {}", path.display()))?;
        let count = serde_json::from_slice::<Value>(&bytes)
            .map(|v| extract_records(&v).len())
            .unwrap_or(0);
        Ok((count, "server-export"))
    } else {
        // Recall fallback: a single high-limit broad pull (recall has no offset).
        let result = recall(client, "", BROAD_LIMIT, bank).await?;
        let records = extract_records(&result);
        let snapshot = json!({
            "bank": bank,
            "exported_at": chrono::Utc::now().to_rfc3339(),
            "strategy": "recall",
            "count": records.len(),
            "records": records,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&snapshot)?)
            .with_context(|| format!("writing snapshot {}", path.display()))?;
        Ok((records.len(), "recall"))
    }
}

// ── import ──────────────────────────────────────────────────────────────────

async fn import_from(
    config: &AppConfig,
    client: &McpClient,
    bank: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading snapshot {}", path.display()))?;

    if let Some(export_dir) = config.mnemosyne_export_dir.as_deref() {
        // Shared-host import: place the snapshot where the service can read it,
        // then let it ingest natively. Works cleanly for snapshots produced by
        // shared-host `export` (native mnemosyne format).
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let server_path = PathBuf::from(export_dir).join(format!("ygg-import-{bank}-{ts}.json"));
        std::fs::write(&server_path, &bytes)
            .with_context(|| format!("staging import at {}", server_path.display()))?;
        client
            .call_tool(
                "mnemosyne_import",
                with_bank(json!({"input_path": server_path.to_string_lossy()}), bank),
            )
            .await
            .context("mnemosyne_import failed")?;
        println!(
            "imported via server-side mnemosyne_import from {}",
            path.display()
        );
        Ok(())
    } else {
        // Replay fallback: re-`remember` each record, skipping content already
        // present in the bank (best-effort dedup against a broad recall).
        let snapshot: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing snapshot {}", path.display()))?;
        let records = extract_records(&snapshot);

        let existing = recall(client, "", BROAD_LIMIT, bank).await?;
        let mut seen: HashSet<String> = extract_records(&existing)
            .iter()
            .map(|r| content_hash(&record_content(r)))
            .collect();

        let (mut added, mut skipped) = (0usize, 0usize);
        for r in &records {
            let content = record_content(r);
            if content.is_empty() {
                skipped += 1;
                continue;
            }
            let h = content_hash(&content);
            if !seen.insert(h) {
                skipped += 1;
                continue;
            }
            let mut args = json!({"content": content});
            set_opt(
                &mut args,
                "importance",
                record_importance(r).map(Into::into),
            );
            if let Some(scope) = r.get("scope").and_then(Value::as_str) {
                args["scope"] = Value::String(scope.to_string());
            }
            if let Some(source) = r.get("source").and_then(Value::as_str) {
                args["source"] = Value::String(source.to_string());
            }
            client
                .call_tool("mnemosyne_remember", with_bank(args, bank))
                .await
                .with_context(|| format!("re-remembering record {:?}", record_id(r)))?;
            added += 1;
        }
        println!(
            "imported via replay: {added} added, {skipped} skipped (dedup by content) from {}",
            path.display()
        );
        Ok(())
    }
}

fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    let digest = h.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

// ── sync (export + one-way diff) ────────────────────────────────────────────

async fn sync(config: &AppConfig, client: &McpClient, bank: &str) -> anyhow::Result<()> {
    let prev = latest_snapshot(bank)?;
    let path = default_snapshot_path(bank)?;
    let (count, strategy) = export_to(config, client, bank, &path).await?;
    println!(
        "snapshot: {count} record(s) via {strategy} -> {}",
        path.display()
    );

    let Some(prev_path) = prev else {
        println!("baseline snapshot created (no prior snapshot to diff)");
        return Ok(());
    };

    let new_map = load_id_content_map(&path)?;
    let old_map = load_id_content_map(&prev_path)?;

    let (mut added, mut removed, mut changed) = (0usize, 0usize, 0usize);
    for (id, content) in &new_map {
        match old_map.get(id) {
            None => added += 1,
            Some(old) if old != content => changed += 1,
            Some(_) => {}
        }
    }
    for id in old_map.keys() {
        if !new_map.contains_key(id) {
            removed += 1;
        }
    }
    println!(
        "drift vs {}: +{added} added  -{removed} removed  ~{changed} changed",
        prev_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    Ok(())
}

/// Most recent snapshot for `bank` under the snapshot dir (excludes none — the
/// caller diffs the newly written one against this). Returns `None` if there is
/// no earlier snapshot.
fn latest_snapshot(bank: &str) -> anyhow::Result<Option<PathBuf>> {
    let dir = snapshot_dir()?;
    if !dir.exists() {
        return Ok(None);
    }
    let prefix = format!("{bank}-");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".json"))
        })
        .collect();
    // Timestamped names sort lexicographically by time.
    files.sort();
    Ok(files.pop())
}

fn load_id_content_map(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let bytes = std::fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let mut map = HashMap::new();
    for r in extract_records(&value) {
        if let Some(id) = record_id(&r) {
            map.insert(id, record_content(&r));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_records_handles_shapes() {
        assert_eq!(extract_records(&json!([{"id": "a"}])).len(), 1);
        assert_eq!(
            extract_records(&json!({"results": [{"id": "a"}, {"id": "b"}]})).len(),
            2
        );
        assert_eq!(extract_records(&json!({"records": []})).len(), 0);
        // A bare object is one record.
        assert_eq!(extract_records(&json!({"id": "solo"})).len(), 1);
        assert_eq!(extract_records(&json!("nope")).len(), 0);
    }

    #[test]
    fn record_accessors() {
        let r = json!({"memory_id": "m1", "content": "hi", "importance": 0.7, "scope": "global"});
        assert_eq!(record_id(&r).as_deref(), Some("m1"));
        assert_eq!(record_content(&r), "hi");
        assert_eq!(record_importance(&r), Some(0.7));
        assert_eq!(record_scope(&r), "global");
        // Fallback id key.
        assert_eq!(record_id(&json!({"id": "x"})).as_deref(), Some("x"));
    }

    #[test]
    fn with_bank_injects() {
        let v = with_bank(json!({"query": "q"}), "team");
        assert_eq!(v.get("bank").and_then(Value::as_str), Some("team"));
        assert_eq!(v.get("query").and_then(Value::as_str), Some("q"));
    }

    #[test]
    fn resolve_bank_precedence() {
        let mut cfg = base_cfg();
        cfg.mnemosyne_mcp_bank = Some("env-bank".into());
        assert_eq!(resolve_bank(&cfg, Some("arg-bank".into())), "arg-bank");
        assert_eq!(resolve_bank(&cfg, None), "env-bank");
        cfg.mnemosyne_mcp_bank = None;
        assert_eq!(resolve_bank(&cfg, None), "default");
    }

    #[test]
    fn content_hash_is_stable_and_distinct() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    fn base_cfg() -> AppConfig {
        AppConfig {
            database_url: "x".into(),
            context_limit_tokens: 1,
            context_hard_cap_tokens: 1,
            lock_ttl_secs: 1,
            heartbeat_interval_secs: 1,
            watcher_interval_secs: 1,
            rtk_binary_path: "rtk".into(),
            hermes_gateway_url: None,
            hermes_gateway_token: None,
            hermes_dashboard_token: None,
            mnemosyne_mcp_url: None,
            mnemosyne_mcp_token: None,
            mnemosyne_mcp_bank: None,
            mnemosyne_export_dir: None,
        }
    }
}
