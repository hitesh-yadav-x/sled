use clap::{Parser, Subcommand};
use session_ledger::{config::Config, hook, install, paths::Paths, server, sweep};
use std::io::Read;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "sled", about = "A ledger of Claude Code sessions")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Session hook entry point (reads the hook payload from stdin)
    Hook,
    /// Scan session files and update the ledger
    Sweep {
        /// Re-read every session whose file still exists with the current reading rules
        #[arg(long)]
        rebuild: bool,
    },
    /// Register the hook in Claude Code settings
    Install {
        /// Set Claude Code's session retention (cleanupPeriodDays) to N days
        #[arg(long, value_name = "N", conflicts_with = "keep_retention")]
        retention_days: Option<u32>,
        /// Leave the retention setting alone, without asking
        #[arg(long)]
        keep_retention: bool,
    },
    /// Remove the hook from Claude Code settings
    Uninstall,
    /// Serve the ledger page on 127.0.0.1
    Serve {
        /// Port to listen on (default: the config file's port, else 7878)
        #[arg(long, value_name = "N")]
        port: Option<u16>,
        /// Replace project names, folders, branches, titles, prompts and notes with
        /// made-up text, for showing the page to other people. Numbers stay real.
        #[arg(long)]
        demo: bool,
    },
}

fn main() -> ExitCode {
    // The hook must never fail, so it is dispatched before clap sees the arguments.
    if std::env::args_os().nth(1).is_some_and(|a| a == "hook") {
        run_hook();
        return ExitCode::SUCCESS;
    }
    let cli = Cli::parse();
    match cli.command {
        // Unreachable: `hook` is dispatched above, before clap parses anything. The
        // variant exists so `sled --help` lists the command.
        Command::Hook => ExitCode::SUCCESS,
        Command::Sweep { rebuild } => match Paths::from_env().and_then(|p| run_sweep(&p, rebuild)) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("sled: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Install { retention_days, keep_retention } => {
            let retention = match (retention_days, keep_retention) {
                (Some(n), _) => install::Retention::Set(n),
                (None, true) => install::Retention::Keep,
                (None, false) => install::Retention::Ask,
            };
            finish(Paths::from_env().and_then(|p| {
                let exe = std::env::current_exe()?;
                install::run(&p, &exe, retention)
            }))
        }
        Command::Uninstall => finish(Paths::from_env().and_then(|p| install::uninstall(&p))),
        Command::Serve { port, demo } => finish(serve(port, demo)),
    }
}

fn run_sweep(paths: &Paths, rebuild: bool) -> anyhow::Result<ExitCode> {
    let mut failed = false;
    if rebuild {
        let r = sweep::rebuild(paths)?;
        println!("Rebuilt {} sessions, kept {} whose files are gone.", r.rebuilt, r.kept);
        failed = r.failed > 0;
    }
    let (r, refreshed) = sweep::run_keeping_counts(paths)?;
    println!("Indexed {} sessions ({} new).", r.sessions, r.new);
    if let Err(e) = refreshed {
        eprintln!("sled: could not refresh missing-file flags: {e:#}");
        failed = true;
    }
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

fn serve(port: Option<u16>, demo: bool) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    paths.tighten_permissions();
    let mut cfg = Config::load(&paths.config_file())?;
    cfg.demo = demo;
    let server = server::bind(port.unwrap_or(cfg.port))?;
    let bound = server.server_addr().to_ip().map(|a| a.port()).unwrap_or(0);
    println!("Session ledger is running at http://127.0.0.1:{bound}");
    if demo {
        println!("Demo mode: names, folders, branches, titles, prompts and notes are made up. Numbers are real.");
    }
    // A handler panic is reported by the server in one line; the default hook would add more.
    std::panic::set_hook(Box::new(|_| {}));
    server::run(server, paths, cfg)
}

fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sled: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Reads stdin and ingests; silent in every case, including panics.
fn run_hook() {
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(|| {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        if let Ok(paths) = Paths::from_env() {
            hook::run(&paths, &input);
        }
    });
}
