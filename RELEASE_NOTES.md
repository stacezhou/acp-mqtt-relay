# Release Notes

## v0.1.0 - 2026-04-07

Highlights:

- Renamed the binary and package from `acp-mqtt-relay` to `amr`.
- Simplified the CLI to a single entrypoint:
  - user side: `amr <node-id> --broker <broker>`
  - work side: `amr --serve <node-id> --broker <broker> --command <cmd>`
- Made `node_id` a required positional argument instead of optional config state.
- Kept compatibility with the legacy config path `~/.config/acp-mqtt-relay/config.yaml` while preferring `~/.config/amr/config.yaml`.
- Verified the flow with `cargo fmt`, `cargo test`, `cargo check`, and a local Docker Mosquitto end-to-end loopback using `cat`.

Known gaps:

- No release-build end-to-end verification recorded yet.
- No authenticated broker end-to-end verification recorded yet.
