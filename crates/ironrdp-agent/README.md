# IronRDP Agent

A CLI-driven, daemon-backed RDP client designed for programmatic (e.g. LLM) consumption.

The single `ironrdp-agent` binary bundles two roles:

- **Daemon** (`ironrdp-agent daemon-start`): a long-lived, foreground process that owns the
  [`ironrdp-client`] engine and one RDP session. It stays alive across many CLI invocations and
  serves requests over a local IPC transport (a Unix domain socket on Unix, a named pipe on
  Windows).
- **CLI** (`ironrdp-agent <op> …`): a short-lived invocation that opens the IPC endpoint, sends a
  single request, prints the response, and exits.

Run `ironrdp-agent --help-agent` for a structured, machine-readable description of every operation.

## Wire format

Messages are encoded with [`ironrdp-core`]'s `Encode`/`DecodeOwned` traits, length-delimited with a
little-endian `u32` byte-count prefix. There is no JSON anywhere. Both ends are the same binary at
the same version, so the format carries no version byte.

Connection configuration travels as a binary-encoded [`PropertySet`][`ironrdp-propertyset`] inside a
strictly-typed `Request::Connect`. Runtime operations (mouse, keyboard, status, logs, …) are
strictly-typed messages.

## Secret redaction

The reader of IPC responses is untrusted for secrets. Every property dump, status, and log path is
routed through `ironrdp_cfg::is_secret_key`, which redacts the values of `ClearTextPassword`,
`GatewayPassword`/`gatewaypassword`, and the RDCleanPath token. Captured log lines are additionally
scrubbed of any registered secret value.

[`ironrdp-client`]: ../ironrdp-client
[`ironrdp-core`]: ../ironrdp-core
[`ironrdp-propertyset`]: ../ironrdp-propertyset
