//! `lanius-cli` — the headless command-line entry point for the Lanius gateway.
//!
//! This binary wraps [`lanius_core`], the crate that implements the actual HTTP
//! gateway (OpenAI/Anthropic-compatible routes, upstream Kiro client, streaming
//! parser, auth/account management, etc.). `lanius-cli` itself contains no
//! protocol or business logic — it only parses `argv` (via [`clap`]), wires up
//! a Tokio runtime, and delegates to `lanius_core` APIs. Each subcommand's
//! actual implementation lives in its own module under [`commands`]; this
//! file is limited to argument parsing and dispatch.
//!
//! Supported subcommands (see [`Cli`] for the exact flags/args):
//! - *(no subcommand)* — validate the config and run the gateway in server
//!   mode ([`serve`]), listening until a `Ctrl-C` / SIGINT is received.
//! - `replay <raw-stream-file>` — offline-decode a previously captured raw
//!   upstream byte stream (see [`commands::replay`]), useful for debugging
//!   the SSE/event parser without hitting the network.
//! - `probe [prompt] [--capture <file>]` — perform a live end-to-end request
//!   against the real Kiro backend (see [`commands::probe`]), optionally
//!   writing the raw response bytes to disk so they can later be fed back
//!   into `replay`.
//! - `usage [--json]` — fetch the configured account's raw credit/quota
//!   usage from Kiro (see [`commands::usage`]), printed either as a
//!   human-readable summary or raw JSON.
//! - `models [--json]` — list the models currently resolvable by this
//!   gateway (see [`commands::models`]), including hidden/aliased names and
//!   the Kiro credit-rate multiplier when known.
//! - `update [--check] [-y] [--version <x.y.z>]` — check GitHub Releases for
//!   a newer build and, unless `--check` is given, download and install it
//!   in place (see [`commands::update`]).
//!
//! `--help`/`-h` and `--version`/`-V` (plus the `help` subcommand) are
//! provided automatically by `clap` and exit the process directly (status
//! `0` on success, status `2` on a usage error such as an unknown
//! subcommand).
//!
//! Tip: capture a `replay` fixture with
//! `lanius probe --capture debug_logs/raw.bin`, then prove the parser is
//! byte-boundary agnostic by replaying it with different chunk sizes:
//! `for n in 1 7 64 100000; do REPLAY_CHUNK_SIZE=$n lanius replay <file> | md5; done`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use lanius_core::Config;

mod commands;

/// Top-level CLI definition, parsed from `argv` by [`clap`].
///
/// `version`/`about` are left unset here (note the explicit `long_about =
/// None`, which stops `clap` from falling back to this doc comment) and
/// injected at runtime in [`Cli::parse_from_env`] from
/// `lanius_core::config::{APP_VERSION, APP_DESCRIPTION}` — `clap`'s
/// `#[command(version = ...)]` attribute only accepts string literals, but
/// these values must stay in sync with [`print_banner`] and the HTTP
/// `/health` endpoint, so they can't be hardcoded here.
#[derive(Parser)]
#[command(name = "Lanius", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    /// Parses `argv` the same way [`Parser::parse`] would, except `version`
    /// and `about` are overridden to `lanius_core::config::APP_VERSION` /
    /// `APP_DESCRIPTION` so `--version`/`--help` output can never drift from
    /// the version/description reported elsewhere in the app.
    fn parse_from_env() -> Self {
        // `Command::version` requires a `&'static str`; a `static
        // OnceLock<String>` lets us compute the `"v{APP_VERSION}"` string
        // once at startup and borrow it for `'static` without `unsafe` or
        // leaking memory on every call.
        static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let version = VERSION.get_or_init(|| format!("v{}", lanius_core::config::APP_VERSION));

        let command = Cli::command()
            .version(version.as_str())
            .about(lanius_core::config::APP_DESCRIPTION);
        let matches = command.get_matches();
        // `Cli::command()` above is built from the same `#[derive(Parser)]`
        // definition as `Cli`, so `from_arg_matches` succeeding is an
        // invariant of the derive machinery, not user input; a mismatch
        // here would be a programming error in this function, not a
        // reachable runtime failure.
        Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    }
}

/// Subcommands accepted by `lanius-cli`. See the module docs for a summary
/// of each; omitting a subcommand runs the gateway in server mode.
#[derive(Subcommand, Debug)]
enum Command {
    /// Decode a previously captured raw upstream byte stream offline
    Replay {
        /// Path to a raw-stream file (as produced by `probe --capture`)
        file: PathBuf,
    },
    /// Perform a live end-to-end request against the real Kiro backend
    Probe {
        /// User message to send as the sole turn of a new conversation
        #[arg(default_value = "Hello")]
        prompt: String,
        /// Write the raw upstream response bytes to this file, for later `replay`
        #[arg(long)]
        capture: Option<PathBuf>,
    },
    /// Show the configured account's credit/quota usage
    Usage {
        /// Print the raw Kiro usage-limits JSON instead of a formatted summary
        #[arg(long)]
        json: bool,
    },
    /// List the models currently resolvable by this gateway
    Models {
        /// Print the model list as JSON instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Check for and install a newer `lanius` release from GitHub
    Update {
        /// Only report whether a newer version is available; don't install it
        #[arg(long)]
        check: bool,
        /// Skip the interactive confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Install a specific version instead of the latest (e.g. for rollback)
        #[arg(long = "version", value_name = "X.Y.Z")]
        pin_version: Option<String>,
    },
}

/// Process entry point: parses `argv` via [`Cli::parse`], dispatches to the
/// requested subcommand, and returns any error up to the process exit path
/// (a non-`Ok` return causes `anyhow`/the default Rust runtime to print the
/// error and exit with a non-zero status).
///
/// Side effects:
/// - Loads a local `.env` file via `dotenvy` (ignored if absent).
/// - Builds the [`Config`] from environment variables.
/// - Initializes the global `tracing` subscriber.
/// - `clap` calls [`std::process::exit`] directly for `--help`/`--version`
///   (status `0`) and for usage errors such as an unknown subcommand
///   (status `2`), bypassing the normal `Result` return path.
fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let config = Config::from_env();
    init_tracing(&config.log_level);

    let cli = Cli::parse_from_env();
    match cli.command {
        Some(Command::Replay { file }) => runtime()?.block_on(commands::replay(&file)),
        Some(Command::Probe { prompt, capture }) => {
            runtime()?.block_on(commands::probe(config, prompt, capture))
        }
        Some(Command::Usage { json }) => runtime()?.block_on(commands::usage(config, json)),
        Some(Command::Models { json }) => runtime()?.block_on(commands::models(config, json)),
        Some(Command::Update {
            check,
            yes,
            pin_version,
        }) => runtime()?.block_on(commands::update(check, yes, pin_version)),
        None => {
            config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
            print_banner(&config);
            runtime()?.block_on(serve(config))
        }
    }
}

/// Runs the gateway in long-lived server mode.
///
/// Delegates to [`lanius_core::server::serve`], passing a shutdown future that
/// resolves when the process receives `Ctrl-C` (SIGINT), so the gateway can
/// perform a graceful shutdown instead of being killed abruptly.
///
/// Side effects: binds and listens on the configured host/port (network I/O)
/// until shutdown; blocks the calling task for the lifetime of the server.
async fn serve(config: Config) -> Result<()> {
    let shutdown = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for shutdown signal: {e}");
        }
        tracing::info!("shutdown signal received");
    };

    lanius_core::server::serve(config, shutdown)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Builds a multi-threaded Tokio runtime used to drive the async subcommands
/// (`serve`, `replay`, `probe`, `usage`, `models`, `update`) from `main`,
/// which is itself synchronous.
///
/// Returns an error if the runtime fails to initialize (e.g. thread spawn
/// failure).
fn runtime() -> Result<tokio::runtime::Runtime> {
    use anyhow::Context;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start tokio runtime")
}

/// Initializes the global `tracing` subscriber for the process.
///
/// Honors the standard `RUST_LOG`-style env filter if set; otherwise falls
/// back to a filter derived from `level` (typically `config.log_level`).
/// Side effect: installs a global subscriber via `fmt().init()`, which
/// panics if a subscriber has already been set.
fn init_tracing(level: &str) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level.to_ascii_lowercase()));

    fmt().with_env_filter(filter).with_target(false).init();
}

/// Prints the startup banner shown when the gateway launches in server mode
/// (no subcommand given): app name/version, listen address, region, and
/// debug mode.
fn print_banner(config: &Config) {
    println!(
        "\n{} v{}\n  listening : http://{}:{}\n  region    : {}\n  debug     : {:?}\n",
        lanius_core::config::APP_TITLE,
        lanius_core::config::APP_VERSION,
        config.server_host,
        config.server_port,
        config.region,
        config.debug_mode,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing with no arguments must select server mode (no subcommand),
    /// which is what routes `main` into `serve` rather than `replay`/`probe`.
    #[test]
    fn no_args_selects_server_mode() {
        let cli = Cli::try_parse_from(["lanius"]).expect("bare invocation must parse");
        assert!(cli.command.is_none());
    }

    /// `replay <file>` must capture the file path verbatim, and omitting it
    /// must be a usage error rather than silently defaulting.
    #[test]
    fn replay_requires_file_argument() {
        let cli = Cli::try_parse_from(["lanius", "replay", "capture.bin"]).expect("must parse");
        match cli.command {
            Some(Command::Replay { file }) => assert_eq!(file, PathBuf::from("capture.bin")),
            other => panic!("expected Replay, got {other:?}"),
        }

        assert!(
            Cli::try_parse_from(["lanius", "replay"]).is_err(),
            "replay with no file must be a usage error"
        );
    }

    /// `probe` with no prompt must default to `"Hello"` and no capture path;
    /// an explicit prompt and `--capture <file>` must both be threaded through.
    #[test]
    fn probe_prompt_defaults_and_capture_flag() {
        let bare = Cli::try_parse_from(["lanius", "probe"]).expect("must parse");
        match bare.command {
            Some(Command::Probe { prompt, capture }) => {
                assert_eq!(prompt, "Hello");
                assert_eq!(capture, None);
            }
            other => panic!("expected Probe, got {other:?}"),
        }

        let with_args =
            Cli::try_parse_from(["lanius", "probe", "custom prompt", "--capture", "out.bin"])
                .expect("must parse");
        match with_args.command {
            Some(Command::Probe { prompt, capture }) => {
                assert_eq!(prompt, "custom prompt");
                assert_eq!(capture, Some(PathBuf::from("out.bin")));
            }
            other => panic!("expected Probe, got {other:?}"),
        }
    }

    /// An unrecognized subcommand must be rejected as a usage error, matching
    /// the `main` dispatch path that exits with status `2`.
    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["lanius", "bogus"]).is_err());
    }

    /// `usage` with no flags must default to the formatted summary
    /// (`json: false`); `--json` must switch to raw JSON output.
    #[test]
    fn usage_json_flag_defaults_and_parses() {
        let bare = Cli::try_parse_from(["lanius", "usage"]).expect("must parse");
        match bare.command {
            Some(Command::Usage { json }) => assert!(!json),
            other => panic!("expected Usage, got {other:?}"),
        }

        let with_flag = Cli::try_parse_from(["lanius", "usage", "--json"]).expect("must parse");
        match with_flag.command {
            Some(Command::Usage { json }) => assert!(json),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    /// `models` with no flags must default to the formatted table
    /// (`json: false`); `--json` must switch to JSON output.
    #[test]
    fn models_json_flag_defaults_and_parses() {
        let bare = Cli::try_parse_from(["lanius", "models"]).expect("must parse");
        match bare.command {
            Some(Command::Models { json }) => assert!(!json),
            other => panic!("expected Models, got {other:?}"),
        }

        let with_flag = Cli::try_parse_from(["lanius", "models", "--json"]).expect("must parse");
        match with_flag.command {
            Some(Command::Models { json }) => assert!(json),
            other => panic!("expected Models, got {other:?}"),
        }
    }

    /// `update` with no flags must default to a real (non-check-only)
    /// update, no `-y`, and no version pin; every flag must be threaded
    /// through when given explicitly.
    #[test]
    fn update_flags_default_and_parse() {
        let bare = Cli::try_parse_from(["lanius", "update"]).expect("must parse");
        match bare.command {
            Some(Command::Update {
                check,
                yes,
                pin_version,
            }) => {
                assert!(!check);
                assert!(!yes);
                assert_eq!(pin_version, None);
            }
            other => panic!("expected Update, got {other:?}"),
        }

        let with_flags =
            Cli::try_parse_from(["lanius", "update", "--check", "-y", "--version", "1.2.3"])
                .expect("must parse");
        match with_flags.command {
            Some(Command::Update {
                check,
                yes,
                pin_version,
            }) => {
                assert!(check);
                assert!(yes);
                assert_eq!(pin_version, Some("1.2.3".to_string()));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }
}
