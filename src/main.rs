//! beadsd — single-writer beads service.
//!
//! Owns ONE central `.beads` SQLite DB for ONE project and is the sole process
//! that runs `br` against it. Exposes the beads verbs over rmcp streamable-HTTP
//! so agents on any host on the trusted network mutate beads through this one
//! process instead of sharing the DB on a filesystem.
//!
//! Why a service and not a shared DB path: SQLite/fsqlite file locking is only
//! safe on a local filesystem — concurrent writers across a network filesystem
//! (NFS) are not. One owning process serializes access and makes cross-host
//! beads correct. The jj-workspace divergence trap (br auto-discovering a stale
//! per-workspace `.beads/`) also disappears, because no working repo holds a DB.
//!
//! Reuse posture (operator policy, beads_rust/CLAUDE.md): assume only the `br`
//! CLI surface + the issues.jsonl export. We therefore SHELL OUT to `br --db
//! <central> --json` rather than link br's internal storage API. The fork's
//! effective-priority behavior comes along for free, and upstream merges of br
//! never touch this code.

mod config;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use config::Config;

use clap::Parser;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::Mutex;

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ReadyReq {
    /// Filter to a specific assignee. Omit for all.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Only show unassigned issues.
    #[serde(default)]
    pub unassigned: bool,
    /// Max issues to return (0 = unlimited). Default br: 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ShowReq {
    /// Issue ID, e.g. `mu-onq8`.
    pub id: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListReq {
    /// Filter by status (open, in_progress, closed, deferred, blocked).
    #[serde(default)]
    pub status: Option<String>,
    /// Filter by assignee.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Minimum priority (0=critical .. 4=backlog).
    #[serde(default)]
    pub priority_min: Option<u8>,
    /// Maximum priority.
    #[serde(default)]
    pub priority_max: Option<u8>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CreateReq {
    /// Issue title.
    pub title: String,
    /// Issue type (task, bug, feature, …).
    #[serde(default)]
    pub issue_type: Option<String>,
    /// Priority (0-4 or P0-P4).
    #[serde(default)]
    pub priority: Option<String>,
    /// Description / body.
    #[serde(default)]
    pub description: Option<String>,
    /// Actor name for the audit trail.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UpdateReq {
    /// Issue ID to update.
    pub id: String,
    /// New status (non-terminal only; use close for closing).
    #[serde(default)]
    pub status: Option<String>,
    /// New assignee (empty string clears).
    #[serde(default)]
    pub assignee: Option<String>,
    /// New priority (0-4 or P0-P4).
    #[serde(default)]
    pub priority: Option<String>,
    /// Actor name for the audit trail.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ClaimReq {
    /// Issue ID to claim.
    pub id: String,
    /// Claiming actor — becomes assignee, status set to in_progress. Required.
    pub actor: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct UnclaimReq {
    /// Issue ID to release (assignee cleared, status set back to open).
    pub id: String,
    /// Actor name for the audit trail.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CloseReq {
    /// Issue ID to close.
    pub id: String,
    /// Close reason.
    #[serde(default)]
    pub reason: Option<String>,
    /// Actor name for the audit trail.
    #[serde(default)]
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExecReq {
    /// Raw br arguments, e.g. ["ready", "--json"] or ["update", "mu-x", "-s",
    /// "in_progress"]. Any caller-supplied `--db`/`--no-db` is stripped; the
    /// central DB is always forced. This is the passthrough the `br` shim uses
    /// to route arbitrary br subcommands to the single writer.
    pub args: Vec<String>,
}

/// Drop any caller-supplied `--db <val>`, `--db=<val>`, and `--no-db` so an
/// exec can never escape the central DB.
fn strip_db_args(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--db" {
            skip_next = true; // also drop its value
            continue;
        }
        if a == "--no-db" || a.starts_with("--db=") {
            continue;
        }
        out.push(a);
    }
    out
}

// ── Server state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BeadsServer {
    br_bin: Arc<String>,
    db_path: Arc<PathBuf>,
    /// Serializes mutating `br` invocations. br takes its own advisory fs lock,
    /// but serializing here keeps ordering clean and avoids lock-timeout churn
    /// under bursts. Reads run without it.
    write_lock: Arc<Mutex<()>>,
}

impl BeadsServer {
    fn new(br_bin: String, db_path: PathBuf) -> Self {
        Self {
            br_bin: Arc::new(br_bin),
            db_path: Arc::new(db_path),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Run `br <args> --db <central> --json`, returning stdout on success or a
    /// JSON error object string on failure. Mutating calls hold the write lock.
    async fn run_br(&self, mut args: Vec<String>, mutation: bool) -> String {
        args.push("--db".into());
        args.push(self.db_path.to_string_lossy().into_owned());
        args.push("--json".into());

        let _guard = if mutation {
            Some(self.write_lock.lock().await)
        } else {
            None
        };

        let output = Command::new(self.br_bin.as_str())
            .args(&args)
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).into_owned()
            }
            Ok(o) => json!({
                "error": "br_failed",
                "exit_code": o.status.code(),
                "stderr": String::from_utf8_lossy(&o.stderr),
                "stdout": String::from_utf8_lossy(&o.stdout),
                "args": args,
            })
            .to_string(),
            Err(e) => json!({
                "error": "br_spawn_failed",
                "message": e.to_string(),
                "br_bin": self.br_bin.as_str(),
            })
            .to_string(),
        }
    }
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[tool_router]
impl BeadsServer {
    #[tool(
        name = "beads_ready",
        description = "List ready (open, unblocked, not deferred) issues as JSON."
    )]
    async fn beads_ready(&self, Parameters(req): Parameters<ReadyReq>) -> String {
        let mut args = vec!["ready".to_string()];
        if req.unassigned {
            args.push("--unassigned".into());
        } else if let Some(a) = req.assignee {
            args.push("--assignee".into());
            args.push(a);
        }
        if let Some(l) = req.limit {
            args.push("--limit".into());
            args.push(l.to_string());
        }
        self.run_br(args, false).await
    }

    #[tool(name = "beads_show", description = "Show one issue's details as JSON.")]
    async fn beads_show(&self, Parameters(req): Parameters<ShowReq>) -> String {
        self.run_br(vec!["show".into(), req.id], false).await
    }

    #[tool(name = "beads_list", description = "List issues with optional filters, as JSON.")]
    async fn beads_list(&self, Parameters(req): Parameters<ListReq>) -> String {
        let mut args = vec!["list".to_string()];
        if let Some(s) = req.status {
            args.push("--status".into());
            args.push(s);
        }
        if let Some(a) = req.assignee {
            args.push("--assignee".into());
            args.push(a);
        }
        if let Some(p) = req.priority_min {
            args.push("--priority-min".into());
            args.push(p.to_string());
        }
        if let Some(p) = req.priority_max {
            args.push("--priority-max".into());
            args.push(p.to_string());
        }
        self.run_br(args, false).await
    }

    #[tool(
        name = "beads_create",
        description = "Create a new issue. Returns the created issue as JSON (incl. its ID)."
    )]
    async fn beads_create(&self, Parameters(req): Parameters<CreateReq>) -> String {
        let mut args = vec!["create".to_string(), req.title];
        if let Some(t) = req.issue_type {
            args.push("--type".into());
            args.push(t);
        }
        if let Some(p) = req.priority {
            args.push("--priority".into());
            args.push(p);
        }
        if let Some(d) = req.description {
            args.push("--description".into());
            args.push(d);
        }
        if let Some(a) = req.actor {
            args.push("--actor".into());
            args.push(a);
        }
        self.run_br(args, true).await
    }

    #[tool(
        name = "beads_update",
        description = "Update an issue's status/assignee/priority (non-terminal). Returns updated issue as JSON."
    )]
    async fn beads_update(&self, Parameters(req): Parameters<UpdateReq>) -> String {
        let mut args = vec!["update".to_string(), req.id];
        if let Some(s) = req.status {
            args.push("--status".into());
            args.push(s);
        }
        if let Some(a) = req.assignee {
            args.push("--assignee".into());
            args.push(a);
        }
        if let Some(p) = req.priority {
            args.push("--priority".into());
            args.push(p);
        }
        if let Some(a) = req.actor {
            args.push("--actor".into());
            args.push(a);
        }
        self.run_br(args, true).await
    }

    #[tool(
        name = "beads_claim",
        description = "Atomically claim an issue (assignee=actor, status=in_progress). Fails if already held by another actor."
    )]
    async fn beads_claim(&self, Parameters(req): Parameters<ClaimReq>) -> String {
        let args = vec![
            "update".to_string(),
            req.id,
            "--claim".into(),
            "--actor".into(),
            req.actor,
        ];
        self.run_br(args, true).await
    }

    #[tool(
        name = "beads_unclaim",
        description = "Release a claim: clears assignee and sets status back to open."
    )]
    async fn beads_unclaim(&self, Parameters(req): Parameters<UnclaimReq>) -> String {
        let mut args = vec![
            "update".to_string(),
            req.id,
            "--assignee".into(),
            String::new(),
            "--status".into(),
            "open".into(),
        ];
        if let Some(a) = req.actor {
            args.push("--actor".into());
            args.push(a);
        }
        self.run_br(args, true).await
    }

    #[tool(name = "beads_close", description = "Close an issue. Returns result as JSON.")]
    async fn beads_close(&self, Parameters(req): Parameters<CloseReq>) -> String {
        let mut args = vec!["close".to_string(), req.id];
        if let Some(r) = req.reason {
            args.push("--reason".into());
            args.push(r);
        }
        if let Some(a) = req.actor {
            args.push("--actor".into());
            args.push(a);
        }
        self.run_br(args, true).await
    }

    #[tool(
        name = "br_exec",
        description = "Run an arbitrary br subcommand against the central DB and return {stdout, stderr, exit_code}. Caller --db is stripped; the central DB is forced. Powers the transparent `br` shim."
    )]
    async fn br_exec(&self, Parameters(req): Parameters<ExecReq>) -> String {
        let mut args = strip_db_args(req.args);
        args.push("--db".into());
        args.push(self.db_path.to_string_lossy().into_owned());

        // Serialize all exec calls: we can't cheaply tell reads from writes by
        // argv, and br ops are fast. fsqlite is concurrency-safe anyway; this
        // just keeps ordering clean.
        let _guard = self.write_lock.lock().await;

        let output = Command::new(self.br_bin.as_str())
            .args(&args)
            .env("RUST_LOG", "error")
            .stdin(Stdio::null())
            .output()
            .await;

        match output {
            Ok(o) => json!({
                "stdout": String::from_utf8_lossy(&o.stdout),
                "stderr": String::from_utf8_lossy(&o.stderr),
                "exit_code": o.status.code().unwrap_or(-1),
            })
            .to_string(),
            Err(e) => json!({
                "stdout": "",
                "stderr": format!("beadsd: failed to spawn br ({}): {e}", self.br_bin),
                "exit_code": 127,
            })
            .to_string(),
        }
    }
}

#[tool_handler]
impl ServerHandler for BeadsServer {}

// ── Background git committer ───────────────────────────────────────────────────
//
// br auto-flushes the DB → issues.jsonl on every mutation, so the export is
// always current on disk. What's missing is the git commit that turns it into a
// durable, auditable history in the central repo. This runs ONLY in beadsd —
// br's "never runs git" invariant is preserved.
//
// Debounced: each tick, commit only if issues.jsonl changed since the last
// commit. Plain git on a tree only this process's project touches; concurrent
// per-project beadsd instances share one repo, so a commit that loses an
// index.lock race is simply retried on the next tick.

/// Commit `jsonl` (relative to `repo`) if it has tracked/untracked changes.
/// Returns Ok(true) if a commit was made, Ok(false) if there was nothing to
/// commit, Err on a git failure worth retrying.
async fn git_commit_once(
    repo: &Path,
    rel: &Path,
    msg: &str,
    author_name: &str,
    author_email: &str,
) -> anyhow::Result<bool> {
    // Stage the one path. Tolerate races by surfacing the error for retry.
    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("add")
        .arg("--")
        .arg(rel)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !add.status.success() {
        anyhow::bail!("git add failed: {}", String::from_utf8_lossy(&add.stderr));
    }

    // Commit only this path. Self-contained identity so the service doesn't
    // depend on repo/user git config being present.
    let commit = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg(format!("user.name={author_name}"))
        .arg("-c")
        .arg(format!("user.email={author_email}"))
        .arg("commit")
        .arg("-m")
        .arg(msg)
        .arg("--only")
        .arg("--")
        .arg(rel)
        .stdin(Stdio::null())
        .output()
        .await?;
    if commit.status.success() {
        return Ok(true);
    }
    // "nothing to commit" is success-shaped, not an error.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&commit.stdout),
        String::from_utf8_lossy(&commit.stderr)
    );
    if combined.contains("nothing to commit") || combined.contains("no changes added") {
        return Ok(false);
    }
    anyhow::bail!("git commit failed: {combined}");
}

async fn run_committer(
    repo: PathBuf,
    jsonl: PathBuf,
    interval: Duration,
    author_name: String,
    author_email: String,
) {
    let rel = jsonl
        .strip_prefix(&repo)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| jsonl.clone());
    let project = rel
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let msg = format!("beadsd: snapshot {project}");
    let mut last: Option<SystemTime> = None;
    tracing::info!(repo = %repo.display(), path = %rel.display(), "committer started");
    loop {
        tokio::time::sleep(interval).await;
        let mtime = match std::fs::metadata(&jsonl).and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(_) => continue, // jsonl not present yet / unreadable; skip this tick
        };
        if last == Some(mtime) {
            continue; // unchanged since last commit
        }
        match git_commit_once(&repo, &rel, &msg, &author_name, &author_email).await {
            Ok(true) => {
                last = Some(mtime);
                tracing::info!(path = %rel.display(), "committed snapshot");
            }
            Ok(false) => last = Some(mtime), // nothing to commit; don't re-check same mtime
            Err(e) => tracing::warn!(error = %e, "snapshot commit failed; will retry"),
        }
    }
}

// ── Entrypoint ────────────────────────────────────────────────────────────────

/// CLI flags. Config comes from layered TOML/env (see `config`); any flag set
/// here overrides the loaded config as the final layer.
#[derive(Parser, Debug)]
#[command(name = "beadsd", about = "Single-writer beads (br) service over rmcp streamable-HTTP")]
struct Args {
    /// Per-instance config file (e.g. ~/.config/beadsd/mu.toml), layered over
    /// the system/user defaults.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Override: path to the central project DB this instance owns.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Override: address to bind, e.g. 0.0.0.0:7771.
    #[arg(long)]
    listen: Option<String>,

    /// Override: path to the br binary.
    #[arg(long)]
    br_bin: Option<String>,

    /// Override: central git repo root for the snapshot committer.
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Override: seconds between debounced snapshot commits (0 disables).
    #[arg(long)]
    commit_interval_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "beadsd=info".into()),
        )
        .init();

    let args = Args::parse();

    // Layered config, then apply CLI overrides as the final layer.
    let mut cfg = Config::load(args.config.as_deref())?;
    if let Some(v) = args.db {
        cfg.db = Some(v);
    }
    if let Some(v) = args.listen {
        cfg.listen = v;
    }
    if let Some(v) = args.br_bin {
        cfg.br_bin = v;
    }
    if args.repo.is_some() {
        cfg.repo = args.repo;
    }
    if let Some(v) = args.commit_interval_secs {
        cfg.commit_interval_secs = v;
    }

    let db = cfg
        .db
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no db configured (set `db` in config or pass --db)"))?;
    if !db.exists() {
        anyhow::bail!("db path does not exist: {}", db.display());
    }
    let db_path = std::fs::canonicalize(&db)?;
    let db_display = db_path.display().to_string();

    // Optional background committer: snapshot <db dir>/issues.jsonl into the
    // central repo on a debounced interval.
    if let Some(repo) = cfg.repo.clone() {
        if cfg.commit_interval_secs > 0 {
            let repo = std::fs::canonicalize(&repo)
                .map_err(|e| anyhow::anyhow!("repo {}: {e}", repo.display()))?;
            let jsonl = db_path
                .parent()
                .map(|d| d.join("issues.jsonl"))
                .ok_or_else(|| anyhow::anyhow!("db path has no parent dir"))?;
            let interval = Duration::from_secs(cfg.commit_interval_secs);
            tokio::spawn(run_committer(
                repo,
                jsonl,
                interval,
                cfg.git_author_name.clone(),
                cfg.git_author_email.clone(),
            ));
        }
    }

    let server = BeadsServer::new(cfg.br_bin.clone(), db_path);

    let service: StreamableHttpService<BeadsServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            Default::default(),
            StreamableHttpServerConfig::default(),
        );

    let app = axum::Router::new()
        .route(&cfg.health_path, axum::routing::get(|| async { "ok" }))
        .nest_service(&cfg.mcp_path, service);

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        db = %db_display, br = %cfg.br_bin, addr = %bound,
        mcp = %cfg.mcp_path, "beadsd serving"
    );

    axum::serve(listener, app).await?;
    Ok(())
}
