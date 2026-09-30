//! `lanius-cli` — the headless command-line entry point for the Lanius gateway.
//!
//! This binary wraps [`lanius_core`], the crate that implements the actual HTTP
//! gateway (OpenAI/Anthropic-compatible routes, upstream Kiro client, streaming
//! parser, auth/account management, etc.). `lanius-cli` itself contains no
//! protocol or business logic — it only parses `argv` (via [`clap`]), wires up
//! a Tokio runtime, and delegates to `lanius_core` APIs.
//!
//! Supported subcommands (see [`Cli`] for the exact flags/args):
//! - *(no subcommand)* — validate the config and run the gateway in server
//!   mode ([`serve`]), listening until a `Ctrl-C` / SIGINT is received.
//! - `replay <raw-stream-file>` — offline-decode a previously captured raw
//!   upstream byte stream (see [`replay`]), useful for debugging the SSE/event
//!   parser without hitting the network.
//! - `probe [prompt] [--capture <file>]` — perform a live end-to-end request
//!   against the real Kiro backend (see [`probe`]), optionally writing the raw
//!   response bytes to disk so they can later be fed back into `replay`.
//! - `update [--check] [-y] [--version <x.y.z>]` — check GitHub Releases for
//!   a newer build and, unless `--check` is given, download and install it
//!   in place (see the `update` module).
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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use futures_util::StreamExt;
use lanius_core::Config;
use lanius_core::auth::AuthManager;
use lanius_core::upstream::{KiroEvent, KiroEventType, KiroHttpClient, parse_kiro_stream};

mod update;

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
        Some(Command::Replay { file }) => runtime()?.block_on(replay(&file)),
        Some(Command::Probe { prompt, capture }) => {
            runtime()?.block_on(probe(config, prompt, capture))
        }
        Some(Command::Update {
            check,
            yes,
            pin_version,
        }) => runtime()?.block_on(update::run(check, yes, pin_version)),
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
/// (`serve`, `replay`, `probe`) from `main`, which is itself synchronous.
///
/// Returns an error if the runtime fails to initialize (e.g. thread spawn
/// failure).
fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start tokio runtime")
}

/// Implements the `replay <raw-stream-file>` subcommand: reads a previously
/// captured raw upstream byte stream from disk and feeds it through the same
/// SSE/event parser ([`parse_kiro_stream`]) used for live traffic, entirely
/// offline (no network access).
///
/// Parameters:
/// - `path`: filesystem path to the raw capture file (as produced by
///   `probe --capture <file>`).
///
/// Side effects: reads the file from disk and prints decoded events plus a
/// summary to stdout.
///
/// Errors if the file cannot be read or if the stream fails to parse.
async fn replay(path: &Path) -> Result<()> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    println!("replaying {} ({} bytes)\n", path.display(), bytes.len());

    // Allow overriding the chunk size via env var so the same fixture can be
    // replayed with different byte-boundary framing (see the framing
    // independence tip in `print_usage`) — this is how we prove the parser
    // is byte-boundary agnostic and doesn't rely on chunks lining up with
    // SSE event boundaries. Falls back to a fixed default (64) if unset,
    // invalid, or non-positive.
    let chunk_size: usize = std::env::var("REPLAY_CHUNK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(64);
    println!("chunk size: {chunk_size}\n");

    // Re-chunk the captured bytes into artificial "network reads" of
    // `chunk_size`, wrapped in `Ok` to mimic the `Result<Bytes, reqwest::Error>`
    // item type that a real `reqwest` byte stream would yield.
    let chunks: Vec<std::result::Result<bytes::Bytes, reqwest::Error>> = bytes
        .chunks(chunk_size)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();

    let stream = futures_util::stream::iter(chunks);
    let events = parse_kiro_stream(stream, Duration::from_secs(5), Duration::from_secs(5));

    let summary = print_events(events).await?;
    println!("\n{summary}");
    Ok(())
}

/// Implements the `probe [prompt] [--capture <file>]` subcommand: performs a
/// live end-to-end request against the real Kiro backend using the same
/// [`AuthManager`] / [`KiroHttpClient`] machinery as the gateway itself, then
/// streams and decodes the response with [`parse_kiro_stream`].
///
/// Parameters:
/// - `config`: validated gateway configuration (region, auth, timeouts, etc.).
/// - `prompt`: the user message text sent as the sole turn of a new
///   conversation.
/// - `capture`: optional filesystem path; when present, the raw upstream
///   response bytes are buffered in memory while streaming and written to
///   this path once the stream completes, producing a fixture consumable by
///   the `replay` subcommand.
///
/// Side effects:
/// - Network I/O: obtains/refreshes an access token, then issues a POST to
///   the Kiro `generateAssistantResponse` endpoint and reads the streamed
///   response.
/// - Prints auth/account diagnostics and decoded stream events to stdout.
/// - File I/O: if `capture` is set, creates parent directories as needed and
///   writes the captured raw bytes to disk.
///
/// Errors if config validation fails, auth/token retrieval fails, the
/// upstream request fails, the response stream fails to parse, or the
/// capture file cannot be created/written.
async fn probe(config: Config, prompt: String, capture: Option<PathBuf>) -> Result<()> {
    config.validate().map_err(|e| anyhow::anyhow!("{e}"))?;

    let auth = Arc::new(AuthManager::new(config.clone()).map_err(|e| anyhow::anyhow!("{e}"))?);

    println!("auth type   : {:?}", auth.auth_type().await);
    println!("region      : {}", auth.region().await);
    println!("fingerprint : {}", auth.fingerprint());
    match auth.profile_arn().await {
        Some(arn) => println!("profile arn : {arn}"),
        None => println!("profile arn : (none — Builder ID account)"),
    }

    let token = auth
        .access_token()
        .await
        .map_err(|e| anyhow::anyhow!("failed to obtain access token: {e}"))?;
    println!("access token: obtained ({} chars)\n", token.len());

    let client = KiroHttpClient::new(auth.clone(), &config).map_err(|e| anyhow::anyhow!("{e}"))?;

    let conversation_state = serde_json::json!({
        "chatTriggerType": "MANUAL",
        "conversationId": uuid_v4(),
        "currentMessage": {
            "userInputMessage": {
                "content": prompt,
                "modelId": "auto",
                "origin": lanius_core::utils::KIRO_ORIGIN,
            }
        },
        "history": [],
    });
    let mut payload = serde_json::json!({ "conversationState": conversation_state });
    if let Some(arn) = auth.profile_arn().await.filter(|arn| !arn.is_empty()) {
        payload["profileArn"] = serde_json::Value::String(arn);
    }

    let url = config.generate_assistant_response_url();
    println!("POST {url}\n");

    let response = client
        .request_with_retry(reqwest::Method::POST, &url, Some(payload), None, true)
        .await
        .map_err(|e| anyhow::anyhow!("upstream request failed: {e}"))?;

    println!("HTTP {}\n", response.status());

    let byte_stream = response.bytes_stream();

    // Only allocate a capture buffer when `--capture` was requested, so a
    // plain probe run has no extra memory/CPU overhead.
    let captured = capture
        .is_some()
        .then(|| Arc::new(std::sync::Mutex::new(Vec::<u8>::new())));
    let sink = captured.clone();
    // Tee each raw chunk into the capture buffer (if any) as it flows through
    // the stream, without altering what's forwarded to the parser below.
    // The lock can only be poisoned if a previous holder panicked while
    // holding it, but the critical section here is just an infallible
    // `Vec::extend_from_slice`, so poisoning is unreachable in practice;
    // recovering via `into_inner` (rather than `.expect`) means we still
    // salvage whatever bytes were captured even if that invariant is ever
    // violated, instead of turning a benign panic into probe/replay failure.
    let byte_stream = byte_stream.inspect(move |chunk| {
        if let (Some(sink), Ok(bytes)) = (sink.as_ref(), chunk) {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
        }
    });

    let events = parse_kiro_stream(
        byte_stream,
        config.first_token_timeout,
        config.streaming_read_timeout,
    );

    let summary = print_events(events).await?;
    println!("\n{summary}");

    // Flush the captured raw bytes to disk only after the stream has fully
    // drained, so the fixture file reflects the complete response. See the
    // note above `byte_stream.inspect` on why `unwrap_or_else(into_inner)`
    // (rather than `.expect`) is used to read the lock here too.
    if let (Some(path), Some(sink)) = (capture, captured) {
        let bytes = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, bytes.as_slice())
            .with_context(|| format!("failed to write {}", path.display()))?;
        println!("captured {} raw bytes to {}", bytes.len(), path.display());
    }

    Ok(())
}

/// Running tally of decoded stream events, accumulated by [`print_events`]
/// and printed as a one-line summary once the stream ends.
#[derive(Default)]
struct Summary {
    content_events: usize,
    thinking_events: usize,
    tool_calls: usize,
    usage_events: usize,
    context_usage_events: usize,
    content_chars: usize,
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "summary: {} content ({} chars), {} thinking, {} tool_use, {} usage, {} context_usage",
            self.content_events,
            self.content_chars,
            self.thinking_events,
            self.tool_calls,
            self.usage_events,
            self.context_usage_events,
        )
    }
}

/// Drains a decoded [`KiroEvent`] stream (from either `replay` or `probe`),
/// printing a human-readable line per event to stdout and classifying each
/// event by [`KiroEventType`] into a running [`Summary`].
///
/// This is the shared "sink" for both offline replay and live probing, so
/// the two subcommands produce identically formatted output and can be
/// diffed against each other.
///
/// Returns the accumulated [`Summary`] once the stream ends, or an error if
/// any individual event in the stream is an `Err` (i.e. a parser/transport
/// failure, distinct from the in-band `KiroEventType::Error` event which is
/// merely printed).
async fn print_events(
    events: impl futures_util::Stream<Item = lanius_core::Result<KiroEvent>>,
) -> Result<Summary> {
    let mut events = std::pin::pin!(events);
    let mut summary = Summary::default();

    while let Some(event) = events.next().await {
        let event = event.map_err(|e| anyhow::anyhow!("stream error: {e}"))?;
        match event.event_type {
            KiroEventType::Content => {
                let c = event.content.unwrap_or_default();
                summary.content_events += 1;
                summary.content_chars += c.chars().count();
                println!("[content]       {c:?}");
            }
            KiroEventType::Thinking => {
                summary.thinking_events += 1;
                if let Some(c) = event.thinking_content {
                    println!("[thinking]      {c:?}");
                }
                if let Some(signature) = event.thinking_signature {
                    println!("[thinking sig]  {} chars", signature.chars().count());
                }
            }
            KiroEventType::ToolUse => {
                summary.tool_calls += 1;
                if let Some(tc) = event.tool_use {
                    println!(
                        "[tool_use]      id={:?} name={} args={}",
                        tc.id, tc.name, tc.arguments
                    );
                    if let Some(t) = tc.truncation {
                        println!(
                            "                !! TRUNCATED by upstream: {} ({} bytes)",
                            t.reason, t.size_bytes
                        );
                    }
                }
            }
            KiroEventType::Usage => {
                summary.usage_events += 1;
                println!("[usage]         {:?}", event.usage);
            }
            KiroEventType::ContextUsage => {
                summary.context_usage_events += 1;
                println!("[context_usage] {:?}", event.context_usage_percentage);
            }
            KiroEventType::Error => {
                println!("[error]         {:?}", event.content);
            }
        }
    }

    Ok(summary)
}

/// Generates a fresh random conversation id for the `probe` subcommand by
/// delegating to `lanius_core`'s conversation-id generator with no seed
/// components.
fn uuid_v4() -> String {
    lanius_core::utils::generate_conversation_id(&[])
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
