use agent_workboard::{Command, Store, mcp, web};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Issue coordination for agents. JSON on stdout; diagnostics on stderr."
)]
struct Args {
    #[arg(
        long,
        env = "WORKBOARD_DB",
        default_value = "workboard.sqlite3",
        global = true
    )]
    db: PathBuf,
    #[arg(long, env = "WORKBOARD_ACTOR", default_value = "agent", global = true)]
    actor: String,
    /// Authenticate as the key's agent (overrides --actor); prefer the env var.
    #[arg(long, env = "WORKBOARD_API_KEY", global = true, hide_env_values = true)]
    api_key: Option<String>,
    #[command(subcommand)]
    mode: Mode,
}
#[derive(Subcommand)]
enum Mode {
    /// Execute one command JSON; omit JSON or pass '-' to read stdin.
    Run { json: Option<String> },
    /// Print the complete JSON Schema for commands (does not open the database).
    Schema,
    /// Run the MCP JSON-RPC stdio server. Never emits non-protocol stdout.
    Mcp,
    /// Serve the read-only dashboard and authenticated agent HTTP API.
    Web {
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: std::net::SocketAddr,
    },
}
fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("{}", json!({"ok":false,"error":e}));
            ExitCode::FAILURE
        }
    }
}
fn run(mut args: Args) -> Result<(), serde_json::Value> {
    if matches!(args.mode, Mode::Schema) {
        println!("{}", mcp::command_schema());
        return Ok(());
    }
    let mut store = match Store::open(&args.db) {
        Ok(store) => store,
        Err(e) => {
            if matches!(args.mode, Mode::Mcp) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return Err(e.json());
        }
    };
    if let Some(key) = args
        .api_key
        .as_deref()
        .filter(|_| !matches!(args.mode, Mode::Web { .. }))
    {
        match store.authenticate(key) {
            Ok(Some(agent)) => args.actor = agent.id,
            _ => {
                let error = json!({"code":"unauthorized","message":"Valid API key for an active agent required"});
                if matches!(args.mode, Mode::Mcp) {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
                return Err(error);
            }
        }
    }
    match args.mode {
        Mode::Run { json: input } => {
            let input = match input {
                Some(s) if s != "-" => s,
                _ => {
                    let mut s = String::new();
                    io::stdin()
                        .take(8 * 1024 * 1024 + 1)
                        .read_to_string(&mut s)
                        .map_err(|e| json!({"code":"io","message":e.to_string()}))?;
                    if s.len() > 8 * 1024 * 1024 {
                        return Err(json!({"code":"invalid","message":"command exceeds 8 MiB"}));
                    }
                    s
                }
            };
            let command: Command = serde_json::from_str(&input)
                .map_err(|e| json!({"code":"invalid","message":e.to_string()}))?;
            if args.api_key.is_some() && command.is_key_management() {
                return Err(
                    json!({"code":"forbidden","message":"Key administration requires local operator mode without an API key"}),
                );
            }
            let result = store.execute(&args.actor, command).map_err(|e| e.json())?;
            println!("{}", json!({"ok":true,"result":result}));
        }
        Mode::Mcp => {
            if let Err(e) = mcp::serve(
                io::stdin().lock(),
                io::stdout().lock(),
                mcp::Server::new(store, args.actor).with_api_key(args.api_key),
            ) {
                eprintln!("MCP transport: {e}");
                std::process::exit(1);
            }
        }
        Mode::Web { listen } => {
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|e| json!({"code":"io","message":e.to_string()}))?;
            runtime
                .block_on(web::serve(store, listen))
                .map_err(|e| json!({"code":"io","message":e.to_string()}))?;
        }
        Mode::Schema => unreachable!(),
    }
    Ok(())
}
