//! beads — thin client for a beadsd service (rmcp streamable-HTTP).
//!
//! Lets shell scripts (sprint-start/sprint-end) and humans drive a remote
//! beadsd from any host on the trusted network, instead of running `br`
//! against a (possibly divergent / cross-host-unsafe) local DB.
//!
//! Explicit verbs (these are NOT plain `br` subcommands, so they stay typed):
//!   beads claim   <id> --actor <name>   atomic claim; br has no `claim`
//!   beads unclaim <id> [--actor <name>]  release; br has no `unclaim`
//!   beads show    <id>                   always JSON (sprint-start/-end parse it)
//!   beads exec -- <br args...>           explicit passthrough; the `br` shim uses it
//!
//! Any OTHER subcommand is forwarded verbatim to the central `br` via beadsd's
//! br_exec, with stdout/stderr/exit replicated faithfully — so the full `br`
//! surface works through `beads`:
//!   beads list --status open -p 0-1 --json
//!   beads count --by status        beads blocked        beads dep tree <id>
//!   beads ready --json             beads graph <id>     beads search <q>
//!   beads create "Title" --type task -p 1
//!   beads update <id> --status in_progress --actor <a>
//!   beads close  <id> --reason "done" --actor <a>
//!
//! --url defaults to $BEADS_REMOTE (e.g. http://host:7777/mcp).
//!
//! On success the br JSON is printed to stdout (exit 0). If beadsd reports a
//! br failure (its `{"error": ...}` envelope — e.g. a claim conflict), the
//! detail is printed to stderr and the process exits 1, so `if ! beads claim`
//! works in scripts.

use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Map, Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "beads",
    about = "Thin relay to a central beadsd (which runs br); run `beads help` for the live br surface."
)]
struct Cli {
    /// beadsd MCP endpoint (http://host:port/mcp). If omitted: $BEADS_REMOTE,
    /// then ~/.config/beads/remotes.env (by current repo name, else `mu`).
    #[arg(long, env = "BEADS_REMOTE")]
    url: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Atomically claim an issue (assignee=actor, status=in_progress).
    /// br has no `claim`; beadsd translates it to `br update --claim`.
    Claim {
        id: String,
        #[arg(long)]
        actor: String,
    },
    /// Release a claim (clear assignee, status back to open).
    /// br has no `unclaim`; beadsd translates it to `br update`.
    Unclaim {
        id: String,
        #[arg(long)]
        actor: Option<String>,
    },
    /// Show one issue (always JSON — sprint-start/sprint-end parse it).
    Show { id: String },
    /// Run an explicit br subcommand against the central DB. The `br` shim uses
    /// this (`beads exec -- <args>`); stdout/stderr/exit replicated faithfully.
    /// Equivalent to the bare-subcommand relay below, minus the `--`.
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Any other br subcommand (list, count, blocked, dep, graph, search,
    /// ready, create, update, close, ...) forwarded verbatim to the central
    /// br via beadsd, with stdout/stderr/exit replicated faithfully.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Map the subcommand to a (tool_name, arguments) pair.
fn to_call(cmd: &Cmd) -> (&'static str, Value) {
    match cmd {
        Cmd::Claim { id, actor } => ("beads_claim", json!({ "id": id, "actor": actor })),
        Cmd::Unclaim { id, actor } => {
            ("beads_unclaim", obj([("id", Some(json!(id))), ("actor", actor.as_ref().map(|a| json!(a)))]))
        }
        Cmd::Show { id } => ("beads_show", json!({ "id": id })),
        // Explicit passthrough and the bare-subcommand relay both go to br_exec.
        Cmd::Exec { args } | Cmd::External(args) => ("br_exec", json!({ "args": args })),
    }
}

/// Build a JSON object, skipping None values.
fn obj<const N: usize>(pairs: [(&str, Option<Value>); N]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

/// Path to ~/.config/beads/remotes.env (XDG-aware).
fn remotes_env_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("beads").join("remotes.env")
}

/// Parse remotes.env into ordered (key, url) pairs; `key=url`, skipping blanks
/// and `#` comments. Returns empty on any read error.
fn parse_remotes(path: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                out.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

/// Repo name for the current directory: the basename of the nearest ancestor
/// containing a `.jj` or `.git` entry, or None outside any repo.
fn repo_key() -> Option<String> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".jj").exists() || dir.join(".git").exists() {
            return dir.file_name().map(|s| s.to_string_lossy().into_owned());
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve the beadsd URL: explicit (--url / $BEADS_REMOTE) wins; else
/// remotes.env, keyed by the current repo, then `default=`/`mu`, then the sole
/// entry. Errors with guidance when nothing resolves.
fn resolve_url(explicit: Option<String>) -> anyhow::Result<String> {
    if let Some(u) = explicit {
        if !u.is_empty() {
            return Ok(u);
        }
    }
    let path = remotes_env_path();
    let remotes = parse_remotes(&path);
    if remotes.is_empty() {
        anyhow::bail!(
            "beads: no --url, no $BEADS_REMOTE, and no usable {} — pass --url <beadsd-mcp-url>",
            path.display()
        );
    }
    let pick = |key: &str| remotes.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    if let Some(u) = repo_key().and_then(|r| pick(&r)) {
        return Ok(u);
    }
    if let Some(u) = pick("default").or_else(|| pick("mu")) {
        return Ok(u);
    }
    if remotes.len() == 1 {
        return Ok(remotes[0].1.clone());
    }
    anyhow::bail!(
        "beads: couldn't pick a beadsd endpoint — set $BEADS_REMOTE, pass --url, or add a \
         `default=` line to {} (have: {})",
        path.display(),
        remotes.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
    );
}

/// beads' own verbs (NOT `br` subcommands): help for these comes from clap, not
/// the relay, since br has no equivalent (claim/unclaim) or beadsd shapes them.
const EXPLICIT_VERBS: [&str; 4] = ["claim", "unclaim", "show", "exec"];

/// Call a beadsd tool over rmcp and return the single text content (br's JSON,
/// or beadsd's `{stdout,stderr,exit_code}` / error envelope).
async fn call_beadsd(url: &str, tool: &str, args: Value) -> anyhow::Result<String> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_string()),
    );
    let client = ClientInfo::default().serve(transport).await?;
    let arg_map: Map<String, Value> = match args {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arg_map))
        .await?;
    let _ = client.cancel().await;
    Ok(result
        .content
        .iter()
        .find_map(|c| serde_json::to_value(c).ok()?.get("text")?.as_str().map(str::to_owned))
        .unwrap_or_default())
}

/// First `--url <v>` / `--url=v` in the raw args (used before clap runs).
fn url_from_raw(raw: &[String]) -> Option<String> {
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        if a == "--url" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--url=") {
            return Some(v.to_string());
        }
    }
    None
}

/// If this is a top-level help request not scoped to a beads-own verb, return
/// the `br` args whose help to relay — so the command surface in `beads --help`
/// comes live from br and never drifts. None => clap dispatches normally
/// (`beads <relayed-cmd> --help` already relays via the external passthrough;
/// `beads claim --help` shows clap's own help for the beads verb).
fn toplevel_help(raw: &[String]) -> Option<Vec<String>> {
    let mut rest: Vec<&str> = Vec::new();
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        if a == "--url" {
            it.next();
            continue;
        }
        if a.starts_with("--url=") {
            continue;
        }
        rest.push(a);
    }
    let first = match rest.first() {
        None => return Some(vec!["--help".into()]), // bare `beads`
        Some(f) => *f,
    };
    if first == "help" {
        return match rest.get(1) {
            Some(&s) if !EXPLICIT_VERBS.contains(&s) => Some(vec![s.into(), "--help".into()]),
            Some(_) => None, // `help <beads-verb>` -> clap's own help
            None => Some(vec!["--help".into()]),
        };
    }
    // Root `-h` / `--help` with no subcommand token -> relay br's root help.
    if (first == "-h" || first == "--help") && rest.iter().all(|a| a.starts_with('-')) {
        return Some(vec!["--help".into()]);
    }
    None
}

/// Relay `br <args>` help from beadsd, prefixed with the beads-specific notes.
/// Falls back to clap's built-in help if the endpoint can't be resolved/reached.
async fn print_relayed_help(explicit: Option<String>, br_args: Vec<String>) -> anyhow::Result<()> {
    use clap::CommandFactory;
    let explicit = explicit.or_else(|| std::env::var("BEADS_REMOTE").ok().filter(|s| !s.is_empty()));
    let url = match resolve_url(explicit) {
        Ok(u) => u,
        Err(_) => {
            Cli::command().print_help().ok();
            println!();
            return Ok(());
        }
    };
    print!(
        "beads — thin relay to a central beadsd (which runs br). Any br subcommand works:\n  \
         beads <br-subcommand> [args]     e.g. beads list --json, beads count --by status\n\
         Special beads verbs (not br): claim, unclaim (atomic claim/release), show (always\n  \
         --json), exec (-- passthrough). Endpoint: --url, else $BEADS_REMOTE, else\n  \
         ~/.config/beads/remotes.env (by repo, else mu).\n"
    );
    let text = call_beadsd(&url, "br_exec", json!({ "args": br_args }))
        .await
        .unwrap_or_default();
    let stdout = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("stdout").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    if stdout.trim().is_empty() {
        println!("\n(could not reach beadsd at {url} for the live br help — beads built-ins only)\n");
        Cli::command().print_help().ok();
        println!();
    } else {
        println!("\nLive br command surface (from beadsd):\n");
        print!("{stdout}");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // Help is RELAYED from br (beads runs nothing locally), so the surface stays
    // in sync with br instead of a hand-maintained list.
    if let Some(br_args) = toplevel_help(&raw) {
        return print_relayed_help(url_from_raw(&raw), br_args).await;
    }

    let cli = Cli::parse();
    let url = resolve_url(cli.url.clone())?;
    let (tool, args) = to_call(&cli.cmd);
    let text = call_beadsd(&url, tool, args).await?;

    // exec / bare-subcommand passthrough: replicate br's stdout/stderr and exit
    // code faithfully.
    if matches!(cli.cmd, Cmd::Exec { .. } | Cmd::External(..)) {
        use std::io::Write;
        if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&text) {
            if let Some(s) = o.get("stdout").and_then(Value::as_str) {
                print!("{s}");
                let _ = std::io::stdout().flush();
            }
            if let Some(s) = o.get("stderr").and_then(Value::as_str) {
                eprint!("{s}");
                let _ = std::io::stderr().flush();
            }
            let code = o.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
            std::process::exit(code as i32);
        }
        eprintln!("{text}");
        std::process::exit(1);
    }

    // Detect beadsd's failure envelope: {"error": "...", ...}.
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(&text) {
        if o.contains_key("error") {
            eprintln!("{text}");
            std::process::exit(1);
        }
    }

    println!("{text}");
    Ok(())
}
