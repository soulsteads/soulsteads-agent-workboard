use agent_workboard::{Command, Store, mcp};
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
fn run(args: Args) -> Result<(), serde_json::Value> {
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
            let result = store.execute(&args.actor, command).map_err(|e| e.json())?;
            println!("{}", json!({"ok":true,"result":result}));
        }
        Mode::Mcp => {
            if let Err(e) = mcp::serve(
                io::stdin().lock(),
                io::stdout().lock(),
                mcp::Server::new(store, args.actor),
            ) {
                eprintln!("MCP transport: {e}");
                std::process::exit(1);
            }
        }
        Mode::Schema => unreachable!(),
    }
    Ok(())
}
