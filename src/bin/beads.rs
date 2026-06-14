//! beads — thin client for a beadsd service (rmcp streamable-HTTP).
//!
//! Lets shell scripts (sprint-start/sprint-end) and humans drive a remote
//! beadsd from any host on the trusted network, instead of running `br`
//! against a (possibly divergent / cross-host-unsafe) local DB.
//!
//!   beads claim   <id> --actor <name>
//!   beads unclaim <id> [--actor <name>]
//!   beads close   <id> [--reason <r>] [--actor <name>]
//!   beads ready   [--assignee <a>] [--unassigned] [--limit <n>]
//!   beads show    <id>
//!   beads create  <title> [--type <t>] [--priority <p>] [--description <d>] [--actor <a>]
//!   beads update  <id> [--status <s>] [--assignee <a>] [--priority <p>] [--actor <a>]
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
#[command(name = "beads", about = "Client for a beadsd service (rmcp over HTTP)")]
struct Cli {
    /// beadsd MCP endpoint, e.g. http://host:7777/mcp. Defaults to $BEADS_REMOTE.
    #[arg(long, env = "BEADS_REMOTE")]
    url: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Atomically claim an issue (assignee=actor, status=in_progress).
    Claim {
        id: String,
        #[arg(long)]
        actor: String,
    },
    /// Release a claim (clear assignee, status back to open).
    Unclaim {
        id: String,
        #[arg(long)]
        actor: Option<String>,
    },
    /// Close an issue.
    Close {
        id: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        actor: Option<String>,
    },
    /// List ready (open, unblocked, not deferred) issues.
    Ready {
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        unassigned: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Show one issue.
    Show { id: String },
    /// Create a new issue.
    Create {
        title: String,
        #[arg(long = "type")]
        issue_type: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        actor: Option<String>,
    },
    /// Update an issue (non-terminal).
    Update {
        id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        actor: Option<String>,
    },
    /// Run an arbitrary br subcommand against the central DB (the `br` shim
    /// uses this). stdout/stderr/exit code are replicated faithfully.
    /// Example: `beads --url <u> exec -- ready --json`.
    Exec {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Map the subcommand to a (tool_name, arguments) pair.
fn to_call(cmd: &Cmd) -> (&'static str, Value) {
    match cmd {
        Cmd::Claim { id, actor } => ("beads_claim", json!({ "id": id, "actor": actor })),
        Cmd::Unclaim { id, actor } => {
            ("beads_unclaim", obj([("id", Some(json!(id))), ("actor", actor.as_ref().map(|a| json!(a)))]))
        }
        Cmd::Close { id, reason, actor } => (
            "beads_close",
            obj([
                ("id", Some(json!(id))),
                ("reason", reason.as_ref().map(|r| json!(r))),
                ("actor", actor.as_ref().map(|a| json!(a))),
            ]),
        ),
        Cmd::Ready { assignee, unassigned, limit } => (
            "beads_ready",
            obj([
                ("assignee", assignee.as_ref().map(|a| json!(a))),
                ("unassigned", if *unassigned { Some(json!(true)) } else { None }),
                ("limit", limit.map(|l| json!(l))),
            ]),
        ),
        Cmd::Show { id } => ("beads_show", json!({ "id": id })),
        Cmd::Create { title, issue_type, priority, description, actor } => (
            "beads_create",
            obj([
                ("title", Some(json!(title))),
                ("issue_type", issue_type.as_ref().map(|v| json!(v))),
                ("priority", priority.as_ref().map(|v| json!(v))),
                ("description", description.as_ref().map(|v| json!(v))),
                ("actor", actor.as_ref().map(|v| json!(v))),
            ]),
        ),
        Cmd::Update { id, status, assignee, priority, actor } => (
            "beads_update",
            obj([
                ("id", Some(json!(id))),
                ("status", status.as_ref().map(|v| json!(v))),
                ("assignee", assignee.as_ref().map(|v| json!(v))),
                ("priority", priority.as_ref().map(|v| json!(v))),
                ("actor", actor.as_ref().map(|v| json!(v))),
            ]),
        ),
        Cmd::Exec { args } => ("br_exec", json!({ "args": args })),
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let (tool, args) = to_call(&cli.cmd);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(cli.url.clone()),
    );
    let client = ClientInfo::default().serve(transport).await?;

    let arg_map: Map<String, Value> = match args {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arg_map))
        .await?;
    let _ = client.cancel().await;

    // The tool returns one text content holding br's JSON (or beadsd's error
    // envelope). Pull the text out of the MCP content wire shape.
    let text = result
        .content
        .iter()
        .find_map(|c| serde_json::to_value(c).ok()?.get("text")?.as_str().map(str::to_owned))
        .unwrap_or_default();

    // exec passthrough: replicate br's stdout/stderr and exit code faithfully.
    if matches!(cli.cmd, Cmd::Exec { .. }) {
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
