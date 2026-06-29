//! The long-lived daemon: owns the [`RdpClient`] engine and one RDP session, and serves IPC
//! requests until shut down.
//!
//! One daemon serves one RDP session (multi-session is out of scope for V1). It is started
//! explicitly with `daemon-start` and runs in the foreground; the caller is expected to background
//! it. On a clean shutdown the Unix socket file is removed (see [`crate::transport`]).

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use ironrdp_cfg::is_secret_key;
use ironrdp_client::config::{ConfigBuilder, MissingField};
use ironrdp_client::rdp::{RdpClient, RdpInputEvent, RdpOutputEvent};
use ironrdp_input::{Database, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_propertyset::{PropertySet, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::ipc::{
    ConnState, KeyFilter, Payload, PropValue, PropertyDump, PropertyEntry, REDACTED, Request, Response, StatusInfo,
};
use crate::logbuf::{self, LogBuffer};
use crate::transport::{Endpoint, Listener, read_message, write_message};

/// Binds the IPC endpoint and serves requests until a shutdown signal is received.
pub async fn run(endpoint: Endpoint) -> anyhow::Result<()> {
    // On Unix a leftover socket file would make `bind` fail; clear it if no daemon is alive.
    #[cfg(unix)]
    if endpoint.0.exists() {
        if crate::transport::connect(&endpoint).await.is_ok() {
            anyhow::bail!("a daemon already appears to be running at {endpoint}");
        }
        let _ = std::fs::remove_file(&endpoint.0);
    }

    let logs = LogBuffer::new();
    logbuf::install(Arc::clone(&logs));

    let listener = Listener::bind(&endpoint).with_context(|| format!("bind IPC endpoint {endpoint}"))?;
    let daemon = Daemon::new(logs);

    info!(%endpoint, "Daemon listening");

    loop {
        tokio::select! {
            result = listener.accept() => {
                let stream = result.context("accept IPC connection")?;
                if let Err(error) = handle_connection(stream, &daemon).await {
                    debug!(error = format!("{error:#}"), "IPC connection error");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, stopping");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_connection<S>(mut stream: S, daemon: &Daemon) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request: Request = read_message(&mut stream).await?;
    let response = daemon.handle(request);
    write_message(&mut stream, &response).await?;
    Ok(())
}

/// The daemon's mutable state: the (single) current session, plus the shared log buffer.
struct Daemon {
    state: Mutex<Option<Session>>,
    logs: Arc<LogBuffer>,
}

/// Per-session state owned by the request handler.
struct Session {
    input_tx: mpsc::UnboundedSender<RdpInputEvent>,
    input_db: Database,
    destination: String,
    live: Arc<Mutex<Live>>,
}

/// Per-session state shared with the output-consumer task.
struct Live {
    /// Live property bag, seeded from `Config::properties` and updated on (re)negotiation.
    properties: PropertySet,
    state: ConnState,
    error: Option<String>,
    /// Most recent frame dimensions (the raw framebuffer is intentionally dropped for V1).
    frame_size: Option<(u16, u16)>,
}

impl Daemon {
    fn new(logs: Arc<LogBuffer>) -> Self {
        Self {
            state: Mutex::new(None),
            logs,
        }
    }

    fn handle(&self, request: Request) -> Response {
        match request {
            Request::Connect(properties) => self.connect(properties),
            Request::Disconnect => self.disconnect(),
            Request::Status => self.status(),
            Request::DumpProperties { filter } => self.dump_properties(filter.as_ref()),
            Request::QueryLogs { substring, last } => self.query_logs(substring.as_deref(), last),
            Request::Screenshot => self.screenshot(),
            Request::MouseMove { x, y } => self.input(Operation::MouseMove(MousePosition { x, y })),
            Request::MouseButton { button, pressed } => self.input(if pressed {
                Operation::MouseButtonPressed(button)
            } else {
                Operation::MouseButtonReleased(button)
            }),
            Request::Wheel { delta, horizontal } => self.input(Operation::WheelRotations(WheelRotations {
                is_vertical: !horizontal,
                rotation_units: delta,
            })),
            Request::KeyScancode { scancode, pressed } => {
                let scancode = Scancode::from_u16(scancode);
                self.input(if pressed {
                    Operation::KeyPressed(scancode)
                } else {
                    Operation::KeyReleased(scancode)
                })
            }
            Request::KeyUnicode { ch, pressed } => self.input(if pressed {
                Operation::UnicodeKeyPressed(ch)
            } else {
                Operation::UnicodeKeyReleased(ch)
            }),
        }
    }

    fn connect(&self, properties: PropertySet) -> Response {
        let builder = match ConfigBuilder::from_property_set(&properties) {
            Ok(builder) => builder,
            Err(error) => return Response::error(format!("invalid configuration: {error:#}")),
        };

        // Derive the headless client identity. These fields are never representable as `.rdp`
        // properties and are never prompted; the daemon supplies them itself.
        let builder = builder
            .with_client_build(client_build())
            .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
            .with_platform(current_platform())
            .with_client_name(client_name());

        let missing = builder.missing();
        if !missing.is_empty() {
            return Response::error(format!(
                "missing required fields: {}",
                missing
                    .iter()
                    .map(MissingField::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        let config = match builder.build() {
            Ok(config) => config,
            Err(error) => return Response::error(format!("{error:#}")),
        };

        let live_seed = config.properties().clone();
        let destination = config.destination().to_string();

        // Register secret values so they can never appear verbatim in retained log lines.
        self.logs.register_secrets(&live_seed);

        let (output_tx, output_rx) = mpsc::channel(16);
        let client = RdpClient::new(config, output_tx);
        let input_tx = client.input_sender();

        let live = Arc::new(Mutex::new(Live {
            properties: live_seed,
            state: ConnState::Connecting,
            error: None,
            frame_size: None,
        }));

        // The RDP client engine runs on its own thread with a current-thread runtime, mirroring
        // `ironrdp-viewer`. This sidesteps any `Send` requirement on the connection future.
        let spawn_result = std::thread::Builder::new()
            .name("ironrdp-agent-session".to_owned())
            .spawn(
                move || match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime.block_on(client.run()),
                    Err(error) => error!(%error, "Failed to build the session runtime"),
                },
            );
        if let Err(error) = spawn_result {
            return Response::error(format!("failed to spawn session thread: {error}"));
        }

        tokio::spawn(consume_output(output_rx, Arc::clone(&live)));

        info!(%destination, "Started RDP session");

        *self.state.lock().expect("daemon state poisoned") = Some(Session {
            input_tx,
            input_db: Database::new(),
            destination,
            live,
        });

        Response::ok()
    }

    fn disconnect(&self) -> Response {
        let mut guard = self.state.lock().expect("daemon state poisoned");
        match guard.as_mut() {
            None => Response::error("no active session"),
            Some(session) => {
                // Request a graceful shutdown; ignore send errors (the session may already be gone).
                let _ = session.input_tx.send(RdpInputEvent::Close);
                session.live.lock().expect("session live state poisoned").state = ConnState::Disconnected;
                Response::ok()
            }
        }
    }

    fn status(&self) -> Response {
        let guard = self.state.lock().expect("daemon state poisoned");
        let info = match guard.as_ref() {
            None => StatusInfo {
                state: ConnState::NoSession,
                destination: None,
                width: None,
                height: None,
                message: None,
            },
            Some(session) => {
                let live = session.live.lock().expect("session live state poisoned");
                let (width, height) = match live.frame_size {
                    Some((width, height)) => (Some(width), Some(height)),
                    None => (None, None),
                };
                StatusInfo {
                    state: live.state,
                    destination: Some(session.destination.clone()),
                    width,
                    height,
                    message: live.error.clone(),
                }
            }
        };
        Response::Ok(Payload::Status(info))
    }

    fn dump_properties(&self, filter: Option<&KeyFilter>) -> Response {
        let guard = self.state.lock().expect("daemon state poisoned");
        let Some(session) = guard.as_ref() else {
            return Response::error("no active session");
        };
        let live = session.live.lock().expect("session live state poisoned");

        let mut entries = Vec::new();
        for (key, value) in live.properties.iter() {
            let key = key.as_ref();
            if filter.is_some_and(|filter| !filter.matches(key)) {
                continue;
            }
            // INVARIANT: every dumped value whose key names a secret is redacted.
            let value = if is_secret_key(key) {
                PropValue::Str(REDACTED.to_owned())
            } else {
                match value {
                    Value::Int(value) => PropValue::Int(*value),
                    Value::Str(value) => PropValue::Str(value.clone()),
                }
            };
            entries.push(PropertyEntry {
                key: key.to_owned(),
                value,
                description: property_description(key).map(str::to_owned),
            });
        }

        Response::Ok(Payload::Properties(PropertyDump { entries }))
    }

    fn query_logs(&self, substring: Option<&str>, last: Option<u32>) -> Response {
        let mut lines = self.logs.query(substring);
        if let Some(last) = last {
            let last = usize::try_from(last).unwrap_or(usize::MAX);
            if last < lines.len() {
                lines.drain(0..lines.len() - last);
            }
        }
        Response::Ok(Payload::Logs(lines))
    }

    fn screenshot(&self) -> Response {
        let guard = self.state.lock().expect("daemon state poisoned");
        let Some(session) = guard.as_ref() else {
            return Response::error("no active session");
        };
        let live = session.live.lock().expect("session live state poisoned");
        match live.frame_size {
            Some((width, height)) => Response::Ok(Payload::Screenshot { width, height }),
            None => Response::error("no frame available yet"),
        }
    }

    fn input(&self, operation: Operation) -> Response {
        let mut guard = self.state.lock().expect("daemon state poisoned");
        let Some(session) = guard.as_mut() else {
            return Response::error("no active session");
        };
        let events = session.input_db.apply([operation]);
        if events.is_empty() {
            return Response::ok();
        }
        match session.input_tx.send(RdpInputEvent::FastPath(events)) {
            Ok(()) => Response::ok(),
            Err(_) => Response::error("session input channel is closed"),
        }
    }
}

/// Consumes the bounded output-event stream, keeping the live state current.
async fn consume_output(mut output_rx: mpsc::Receiver<RdpOutputEvent>, live: Arc<Mutex<Live>>) {
    while let Some(event) = output_rx.recv().await {
        let mut guard = live.lock().expect("session live state poisoned");
        match event {
            RdpOutputEvent::Image { width, height, .. } => {
                let width = width.get();
                let height = height.get();
                guard.properties.insert("desktopwidth", width);
                guard.properties.insert("desktopheight", height);
                guard.frame_size = Some((width, height));
                guard.state = ConnState::Connected;
                guard.error = None;
            }
            RdpOutputEvent::ConnectionFailure(error) => {
                guard.state = ConnState::Failed;
                guard.error = Some(format!("{error}"));
            }
            RdpOutputEvent::Terminated(Ok(reason)) => {
                guard.state = ConnState::Disconnected;
                guard.error = Some(format!("{reason:?}"));
            }
            RdpOutputEvent::Terminated(Err(error)) => {
                guard.state = ConnState::Failed;
                guard.error = Some(format!("{error}"));
            }
            // Pointer events carry no live state we track for V1.
            _ => {}
        }
    }
}

/// Optional short LLM-facing descriptions for a handful of properties.
fn property_description(key: &str) -> Option<&'static str> {
    match key {
        "full address" => Some("RDP target host and port"),
        "username" => Some("RDP account user name"),
        "desktopwidth" => Some("negotiated remote framebuffer width in pixels"),
        "desktopheight" => Some("negotiated remote framebuffer height in pixels"),
        "ironrdp_colordepth" => Some("color depth in bits per pixel (16 or 32)"),
        _ => None,
    }
}

/// Derives a build number from the crate version (`major*100 + minor*10 + patch`).
fn client_build() -> u32 {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    major
        .saturating_mul(100)
        .saturating_add(minor.saturating_mul(10))
        .saturating_add(patch)
}

fn client_name() -> String {
    whoami::hostname().unwrap_or_else(|_| "ironrdp-agent".to_owned())
}

fn current_platform() -> MajorPlatformType {
    match whoami::platform() {
        whoami::Platform::Windows => MajorPlatformType::WINDOWS,
        whoami::Platform::Linux => MajorPlatformType::UNIX,
        whoami::Platform::Mac => MajorPlatformType::MACINTOSH,
        whoami::Platform::Ios => MajorPlatformType::IOS,
        whoami::Platform::Android => MajorPlatformType::ANDROID,
        _ => MajorPlatformType::UNSPECIFIED,
    }
}
