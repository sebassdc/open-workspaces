# ADR 0009: node telemetry over OTLP on the node channel
Date: 2026-10-21 | Status: implemented in 0.9.41; see [node telemetry](../TELEMETRY.md).

Runners need to report host, box and channel health to the controller, so that
an outage and its cause are visible. A stuck reconnect must never again go
unnoticed.

Decisions:

- **Format: OTLP JSON, written by hand.** Runners send standard OTLP data
  (`ExportMetricsServiceRequest`, `ExportLogsServiceRequest`), encoded with
  `serde_json`. The official `opentelemetry-otlp` crate was not used:
  - its stable metrics/logs exporter (0.33.1) was days old;
  - it adds reqwest 0.13 next to our 0.12, plus prost and tonic;
  - it expects an async runtime, while the agent is blocking-thread code.

  The JSON mapping is a stable part of the OTLP specification. The encoder is
  about 200 lines and covered by shape tests. Switching to the SDK later changes
  only the agent's encoder.
- **Transport: frames on the existing node channel.** Telemetry rides the node
  WebSocket as `{"type":"otlp",…}` frames. It does not POST to a new OTLP
  endpoint. The channel already has TLS, the node credential, size limits and
  reconnects, and a second listener would need its own auth path and body limits.
  - Both sides opt in: the agent advertises `telemetry_v1`, and the controller
    offers the `telemetry-v1` extension in its acknowledgement.
  - Neither version breaks the other: old controllers reject unknown frames, and
    old agents reject unknown controller messages.
- **The controller is the only place telemetry is configured.**
  - Settings live in its catalog (`ow config`), versioned and recorded.
  - They ride on the acknowledgement it already sends about every 3 s, so there
    is no push message and no per-runner setting.
  - Off-host export is the controller forwarding to one OTLP/HTTP endpoint;
    runners never talk to a collector.
- **Store: latest values and a short event log.** The controller keeps the latest
  summary per runner (at most 256 series) and the newest 200 events. History
  belongs in an external backend through forwarding.
- **Alerts are pull signals:**
  - `ow health` exit status, `ow nodes` and the dashboard banner;
  - a journal line on each transition;
  - forwarded `ow.alert.active` and `ow.node.up` series.

  No webhook: its URL is a secret and would need a secret store first.
- **Bounds.** At most:
  - 64 queued payloads per runner (oldest dropped and counted);
  - 28 KiB per frame and 32 boxes per sample;
  - 20 frames per 3 s per session, and 64 forwarded payloads queued.

  No secrets or user data are sent: reason classes only, never raw errors, and
  opaque or hashed box IDs.
- **Firecracker's own metrics are deferred.** Per-VM counters need
  `metrics_path` set before boot and again after every snapshot load. That is a
  worker change and a class-2 upgrade (one warm hibernate cycle per runner).
  This release collects host and box data in the agent and leaves the worker
  untouched (class 1).

Not decided here:
- Firecracker per-VM counters and per-box CPU/network counters.
- Telemetry history and traces.
- Webhook alerts.
- Remote `ow config` through the dashboard API.
- Moving the existing deploy-only settings (ROADMAP "Configuration debt") onto
  the same store.
