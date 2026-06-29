//! The short-lived CLI: parse arguments, build a request (merging a `.rdp` file with overrides for
//! `connect`), send it to the daemon, and print the response.
//!
//! The CLI operates purely at the [`PropertySet`] level for connection config — it never calls
//! typed `ConfigBuilder` setters and never ships `argv` to the daemon.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};
use ironrdp_client::config::{ConfigBuilder, MissingField};
use ironrdp_input::MouseButton;
use ironrdp_propertyset::PropertySet;

use crate::ipc::{KeyFilter, Payload, PropValue, Request, Response};
use crate::transport::{self, Endpoint};

/// IronRDP agent: a CLI-driven, daemon-backed RDP client.
#[derive(Parser, Debug)]
#[command(name = "ironrdp-agent", version, about, long_about = None)]
pub struct Cli {
    /// Print a structured, LLM-friendly guide to every operation and exit.
    #[arg(long, global = true)]
    help_agent: bool,

    /// Override the IPC endpoint (defaults to the per-user socket/pipe).
    #[arg(long, global = true)]
    endpoint: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the long-lived daemon in the foreground (owns the RDP session).
    DaemonStart,
    /// Open an RDP session from a .rdp file and/or CLI overrides.
    Connect(ConnectArgs),
    /// Tear down the current RDP session (the daemon keeps running).
    Disconnect,
    /// Report the current session status.
    Status,
    /// Dump the live session properties (secret values are redacted).
    DumpProperties(DumpArgs),
    /// Print retained daemon log lines.
    QueryLogs(QueryLogsArgs),
    /// Print the most recent frame dimensions.
    Screenshot,
    /// Move the mouse pointer to an absolute position.
    MouseMove {
        #[arg(long)]
        x: u16,
        #[arg(long)]
        y: u16,
    },
    /// Press or release a mouse button.
    MouseButton {
        #[arg(long, value_enum)]
        button: CliMouseButton,
        #[arg(long, action = clap::ArgAction::Set)]
        pressed: bool,
    },
    /// Rotate the mouse wheel (negative delta scrolls down/left).
    Wheel {
        #[arg(long, allow_hyphen_values = true)]
        delta: i16,
        #[arg(long)]
        horizontal: bool,
    },
    /// Press or release a key identified by its RDP scancode.
    KeyScancode {
        #[arg(long, value_parser = parse_scancode)]
        scancode: u16,
        #[arg(long, action = clap::ArgAction::Set)]
        pressed: bool,
    },
    /// Press or release a key identified by a Unicode character.
    KeyUnicode {
        #[arg(long = "char")]
        character: char,
        #[arg(long, action = clap::ArgAction::Set)]
        pressed: bool,
    },
}

#[derive(Args, Debug)]
struct ConnectArgs {
    /// Path to a .rdp file to read the base configuration from.
    #[arg(long)]
    rdp_file: Option<PathBuf>,
    /// RDP server address (host[:port]). Overrides the .rdp file.
    #[arg(long)]
    server: Option<String>,
    /// RDP account user name. Overrides the .rdp file.
    #[arg(short, long)]
    username: Option<String>,
    /// RDP account password. Overrides the .rdp file.
    #[arg(short, long)]
    password: Option<String>,
    /// RDP account domain. Overrides the .rdp file.
    #[arg(short, long)]
    domain: Option<String>,
}

#[derive(Args, Debug)]
struct DumpArgs {
    /// Only show keys containing this substring (case-insensitive).
    #[arg(long, conflicts_with = "prefix")]
    filter: Option<String>,
    /// Only show keys starting with this prefix (case-insensitive).
    #[arg(long)]
    prefix: Option<String>,
}

#[derive(Args, Debug)]
struct QueryLogsArgs {
    /// Only show lines containing this substring.
    #[arg(long)]
    substring: Option<String>,
    /// Only show the last N retained lines.
    #[arg(long)]
    last: Option<u32>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliMouseButton {
    Left,
    Middle,
    Right,
    X1,
    X2,
}

impl CliMouseButton {
    fn into_button(self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
            Self::X1 => MouseButton::X1,
            Self::X2 => MouseButton::X2,
        }
    }
}

/// Parses an RDP scancode in decimal or `0x`-prefixed hexadecimal.
fn parse_scancode(input: &str) -> Result<u16, core::num::ParseIntError> {
    if let Some(hex) = input.strip_prefix("0x").or_else(|| input.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        input.parse()
    }
}

/// Entry point shared by the binary: dispatches the parsed [`Cli`].
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    if cli.help_agent {
        print!("{}", crate::help::AGENT_GUIDE);
        return Ok(());
    }

    let endpoint = endpoint_from_arg(cli.endpoint);

    let Some(command) = cli.command else {
        let _ = Cli::command().print_help();
        println!();
        return Ok(());
    };

    let request = match command {
        Command::DaemonStart => return crate::daemon::run(endpoint).await,
        Command::Connect(args) => build_connect_request(args)?,
        Command::Disconnect => Request::Disconnect,
        Command::Status => Request::Status,
        Command::DumpProperties(args) => Request::DumpProperties {
            filter: args
                .filter
                .map(KeyFilter::Substring)
                .or_else(|| args.prefix.map(KeyFilter::Prefix)),
        },
        Command::QueryLogs(args) => Request::QueryLogs {
            substring: args.substring,
            last: args.last,
        },
        Command::Screenshot => Request::Screenshot,
        Command::MouseMove { x, y } => Request::MouseMove { x, y },
        Command::MouseButton { button, pressed } => Request::MouseButton {
            button: button.into_button(),
            pressed,
        },
        Command::Wheel { delta, horizontal } => Request::Wheel { delta, horizontal },
        Command::KeyScancode { scancode, pressed } => Request::KeyScancode { scancode, pressed },
        Command::KeyUnicode { character, pressed } => Request::KeyUnicode { ch: character, pressed },
    };

    let response = transport::send_request(&endpoint, &request).await?;
    print_response(response)
}

/// Builds a `Connect` request by merging an optional `.rdp` file with CLI overrides into one
/// [`PropertySet`], then pre-validating it locally.
fn build_connect_request(args: ConnectArgs) -> anyhow::Result<Request> {
    let mut properties = PropertySet::new();

    if let Some(path) = &args.rdp_file {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if let Err(errors) = ironrdp_rdpfile::load(&mut properties, &text) {
            for error in &errors {
                eprintln!("warning: skipped entry in {}: {error}", path.display());
            }
        }
    }

    // CLI overrides win: plain inserts with canonical .rdp keys.
    if let Some(server) = &args.server {
        properties.insert("full address", server.as_str());
    }
    if let Some(username) = &args.username {
        properties.insert("username", username.as_str());
    }
    if let Some(password) = &args.password {
        properties.insert("ClearTextPassword", password.as_str());
    }
    if let Some(domain) = &args.domain {
        properties.insert("domain", domain.as_str());
    }

    // Pre-validate locally so a misconfigured connect fails fast without contacting the daemon.
    // Only the user-supplied fields are checked here; the daemon derives the client identity.
    let builder = ConfigBuilder::from_property_set(&properties).context("validate configuration")?;
    let missing: Vec<MissingField> = builder.missing().into_iter().filter(is_user_field).collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "missing required fields: {}",
            missing
                .iter()
                .map(MissingField::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(Request::Connect(properties))
}

/// Returns `true` for missing fields the user is expected to supply (the daemon fills the rest).
fn is_user_field(field: &MissingField) -> bool {
    matches!(
        field,
        MissingField::ServerAddress
            | MissingField::Username
            | MissingField::Password
            | MissingField::GatewayUsername
            | MissingField::GatewayPassword
    )
}

fn print_response(response: Response) -> anyhow::Result<()> {
    match response {
        Response::Ok(payload) => {
            print_payload(payload);
            Ok(())
        }
        Response::Err(message) => anyhow::bail!("{message}"),
    }
}

fn print_payload(payload: Payload) {
    match payload {
        Payload::Empty => println!("ok"),
        Payload::Status(status) => {
            println!("state: {:?}", status.state);
            if let Some(destination) = status.destination {
                println!("destination: {destination}");
            }
            if let (Some(width), Some(height)) = (status.width, status.height) {
                println!("resolution: {width}x{height}");
            }
            if let Some(message) = status.message {
                println!("detail: {message}");
            }
        }
        Payload::Properties(dump) => {
            for entry in dump.entries {
                let value = match entry.value {
                    PropValue::Int(value) => value.to_string(),
                    PropValue::Str(value) => value,
                };
                match entry.description {
                    Some(description) => println!("{} = {value}  # {description}", entry.key),
                    None => println!("{} = {value}", entry.key),
                }
            }
        }
        Payload::Logs(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Payload::Screenshot { width, height } => println!("frame {width}x{height}"),
    }
}

#[cfg(unix)]
fn endpoint_from_arg(arg: Option<String>) -> Endpoint {
    match arg {
        Some(value) => Endpoint(PathBuf::from(value)),
        None => transport::default_endpoint(),
    }
}

#[cfg(windows)]
fn endpoint_from_arg(arg: Option<String>) -> Endpoint {
    match arg {
        Some(value) => Endpoint(value),
        None => transport::default_endpoint(),
    }
}
