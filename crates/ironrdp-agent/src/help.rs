//! The `--help-agent` guide: a concise, structured, LLM-friendly description of every operation.

/// Structured guide printed by `ironrdp-agent --help-agent`.
pub(crate) const AGENT_GUIDE: &str = r#"# ironrdp-agent

A CLI-driven, daemon-backed RDP client. One binary plays two roles:

- DAEMON: `ironrdp-agent daemon-start` runs a long-lived foreground process that owns the RDP
  engine and one RDP session. Background it yourself (e.g. `ironrdp-agent daemon-start &`).
- CLI: every other subcommand opens the local IPC endpoint, sends one request, prints the
  response, and exits.

The daemon stays alive across CLI invocations. One daemon serves one RDP session.

## Endpoint

Unix: `$XDG_RUNTIME_DIR/ironrdp-agent-<uid>.sock` (falls back to `/tmp/ironrdp-agent-<uid>.sock`).
Windows: `\\.\pipe\ironrdp-agent-<user>`.
Override with `--endpoint <PATH-OR-PIPE>` on any subcommand.

## Lifecycle

- `daemon-start`                 Start the daemon (foreground). Run this first.
- `connect [--rdp-file F] [--server H[:PORT]] [-u USER] [-p PASS] [-d DOMAIN]`
                                 Merge an optional .rdp file with CLI overrides into one config and
                                 open a session. CLI flags win over the .rdp file. The config is
                                 pre-validated locally before being sent.
- `disconnect`                   Tear down the current session (daemon keeps running).
- `status`                       Report connection state, destination, and last frame size.

## Inspection

- `dump-properties [--filter SUBSTR] [--prefix PREFIX]`
                                 Print the live session property bag, one `key = value` per line.
                                 Secret values (passwords, tokens) are redacted to `<redacted>`.
                                 `--filter` matches keys by substring; `--prefix` by prefix
                                 (both case-insensitive).
- `query-logs [--substring S] [--last N]`
                                 Print retained daemon log lines (a bounded in-memory ring buffer).
                                 `--substring` filters to matching lines; `--last N` keeps the last
                                 N. Raise verbosity with the `IRONRDP_LOG` env var (default `debug`).
- `screenshot`                   Print the most recent frame dimensions (`frame WxH`). Errors with
                                 `no frame available yet` until the first frame arrives.

## Input (require an active session)

- `mouse-move --x X --y Y`                       Move the pointer to an absolute position.
- `mouse-button --button <left|middle|right|x1|x2> --pressed <true|false>`
- `wheel --delta N [--horizontal]`               Rotate the wheel (negative N scrolls down/left).
- `key-scancode --scancode <0x1D|29> --pressed <true|false>`
- `key-unicode --char C --pressed <true|false>`  Type by Unicode character.

## Errors

Failures print a single lowercase message (no trailing punctuation) and exit non-zero. A failed
`connect` carries the list of missing required fields.
"#;
