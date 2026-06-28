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
