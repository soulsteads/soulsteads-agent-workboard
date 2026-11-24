use agent_workboard::{Command, Store, mcp, web};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    ffi::CString,
    fs::{self, OpenOptions},
    io::Write,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    name = "soulsteads-workboard",
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
    Run {
        json: Option<String>,
        /// Make a retry safe: repeating the same command with the same key returns the
        /// original result (with "replayed":true) instead of applying it twice.
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Print the complete JSON Schema for commands (does not open the database).
    Schema,
    /// Run the MCP JSON-RPC stdio server. Never emits non-protocol stdout.
    Mcp,
    /// Serve the read-only dashboard and authenticated agent HTTP API.
    Web {
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: std::net::SocketAddr,
    },
    /// Install a systemd unit for the foreground dashboard server.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Write a new unit only; it never enables, starts, or opens the database.
    Install {
        /// Install into the invoking user's systemd unit directory (the default).
        #[arg(long)]
        user: bool,
        /// Install into /etc/systemd/system; requires --run-as and root privileges.
        #[arg(long)]
        system: bool,
        /// Non-root account used by a system unit. Required with --system.
        #[arg(long)]
        run_as: Option<String>,
        /// Print the unit instead of writing it.
        #[arg(long)]
        dry_run: bool,
    },
}

const UNIT_NAME: &str = "soulsteads-workboard.service";

fn invalid(message: impl Into<String>) -> serde_json::Value {
    json!({"code":"invalid","message":message.into()})
}

fn io_error(error: impl std::fmt::Display) -> serde_json::Value {
    json!({"code":"io","message":error.to_string()})
}

fn systemd_argument(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '"' => encoded.push_str("\\\""),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            '%' => encoded.push_str("%%"),
            '$' => encoded.push_str("$$"),
            c if c.is_control() => {
                for byte in c.to_string().bytes() {
                    encoded.push_str(&format!("\\x{byte:02X}"));
                }
            }
            c => encoded.push(c),
        }
    }
    encoded.push('"');
    encoded
}

fn user_unit_directory() -> Result<PathBuf, serde_json::Value> {
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        let config = PathBuf::from(config);
        if !config.is_absolute() {
            return Err(invalid("XDG_CONFIG_HOME must be an absolute path"));
        }
        return Ok(config.join("systemd/user"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("HOME is required when XDG_CONFIG_HOME is unset"))?;
    if !home.is_absolute() {
        return Err(invalid("HOME must be an absolute path"));
    }
    Ok(home.join(".config/systemd/user"))
}

fn validated_system_account(name: &str) -> Result<(), serde_json::Value> {
    if name.is_empty()
        || !(name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(invalid("--run-as must name an existing local account"));
    }
    let name = CString::new(name).map_err(|_| invalid("--run-as must not contain NUL"))?;
    // getpwnam is only used to validate a requested local account before its name is
    // placed in the unit. The returned record is read immediately and not retained.
    let record = unsafe { libc::getpwnam(name.as_ptr()) };
    if record.is_null() {
        return Err(invalid("--run-as must name an existing local account"));
    }
    let uid = unsafe { (*record).pw_uid };
    if uid == 0 {
        return Err(invalid("--run-as must name a non-root account"));
    }
    Ok(())
}

fn rendered_unit(
    executable: &Path,
    database: &Path,
    actor: &str,
    system_user: Option<&str>,
) -> Result<String, serde_json::Value> {
    let executable = executable
        .to_str()
        .ok_or_else(|| invalid("current executable path must be valid UTF-8"))?;
    let database = database
        .to_str()
        .ok_or_else(|| invalid("service database path must be valid UTF-8"))?;
    let mut unit = String::from(
        "[Unit]\nDescription=Soulsteads Workboard dashboard\nAfter=network.target\n\n[Service]\nType=simple\n",
    );
    if let Some(user) = system_user {
        unit.push_str("User=");
        unit.push_str(user);
        unit.push('\n');
    }
    unit.push_str("UMask=0077\nRestart=on-failure\nRestartSec=5s\nExecStart=");
    for argument in [
        executable,
        "--db",
        database,
        "--actor",
        actor,
        "web",
        "--listen",
        "127.0.0.1:7878",
    ] {
        unit.push_str(&systemd_argument(argument));
        unit.push(' ');
    }
    unit.pop();
    unit.push_str("\n\n[Install]\nWantedBy=");
    unit.push_str(if system_user.is_some() {
        "multi-user.target"
    } else {
        "default.target"
    });
    unit.push('\n');
    Ok(unit)
}

fn write_new_unit(path: &Path, unit: &str) -> Result<(), serde_json::Value> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                invalid(format!(
                    "refusing to overwrite existing unit {}",
                    path.display()
                ))
            } else {
                io_error(error)
            }
        })?;
    file.write_all(unit.as_bytes()).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn install_service(
    database: &Path,
    actor: &str,
    user: bool,
    system: bool,
    run_as: Option<&str>,
    dry_run: bool,
) -> Result<(), serde_json::Value> {
    if user && system {
        return Err(invalid("choose at most one of --user and --system"));
    }
    if !database.is_absolute() {
        return Err(invalid("service install requires an absolute --db path"));
    }
    let executable = std::env::current_exe().map_err(io_error)?;
    if !executable.is_absolute() {
        return Err(io_error("current executable path is not absolute"));
    }
    let (target, system_user) = if system {
        let account = run_as.ok_or_else(|| invalid("--system requires --run-as USER"))?;
        validated_system_account(account)?;
        if !dry_run && unsafe { libc::geteuid() } != 0 {
            return Err(invalid("--system installation requires root privileges"));
        }
        (
            PathBuf::from("/etc/systemd/system").join(UNIT_NAME),
            Some(account),
        )
    } else {
        if run_as.is_some() {
            return Err(invalid("--run-as is only valid with --system"));
        }
        (user_unit_directory()?.join(UNIT_NAME), None)
    };
    let unit = rendered_unit(&executable, database, actor, system_user)?;
    if dry_run {
        print!("{unit}");
        return Ok(());
    }
    if !system {
        fs::create_dir_all(
            target
                .parent()
                .expect("a systemd unit target always has a parent"),
        )
        .map_err(io_error)?;
    }
    write_new_unit(&target, &unit)?;
    println!("installed {}", target.display());
    Ok(())
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
    if let Mode::Service {
        command:
            ServiceCommand::Install {
                user,
                system,
                run_as,
                dry_run,
            },
    } = args.mode
    {
        return install_service(
            &args.db,
            &args.actor,
            user,
            system,
            run_as.as_deref(),
            dry_run,
        );
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
        Mode::Run {
            json: input,
            idempotency_key,
        } => {
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
            if args.api_key.is_some() && command.is_operator_only() {
                return Err(
                    json!({"code":"forbidden","message":"Key and webhook administration requires local operator mode without an API key"}),
                );
            }
            let (result, replayed) = store
                .execute_idempotent(&args.actor, idempotency_key.as_deref(), command)
                .map_err(|e| e.json())?;
            if replayed {
                println!("{}", json!({"ok":true,"result":result,"replayed":true}));
            } else {
                println!("{}", json!({"ok":true,"result":result}));
            }
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
        Mode::Schema | Mode::Service { .. } => unreachable!(),
    }
    Ok(())
}
