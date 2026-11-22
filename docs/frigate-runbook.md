# Frigate NVR events over Unibus

`unibus-frigate` is a read-only Frigate adapter. Frigate keeps MQTT, detection,
recording, and media ownership. Unibus carries two bounded JSON contracts:

- `frigate.event.v1` — normalized `new`, `update`, and `end` detections from
  `<prefix>/events`.
- `frigate.availability.v1` — the retained `online` / `offline` state from
  `<prefix>/available`.

It does not subscribe to snapshot, clip, or camera topics, and it has no
publish-to-MQTT path. Media stays a reference (`has_clip`, `has_snapshot`) on
the control plane.

## Why not the generic MQTT bridge?

`unibus-mqtt` already bridges MQTT to Unibus, but it preserves the raw payload
as binary `payload_bytes` and puts only topic metadata in the JSON body. The
bounded `unibus-sdk` used by Soil deliberately drops non-empty binary payloads,
so a Soil Spore would see `frigate/events` metadata but not the detection.
This adapter lifts the Frigate JSON into the envelope body under a stable kind.
The generic bridge remains the right tool for opaque MQTT mirroring.

## Router configuration

Grant the adapter producer permission to publish both kinds. Merge these
entries into the installation's existing `routes` and `nodes` arrays; do not
replace them.

```json
{
  "routes": [
    { "kind": "frigate.event.v1", "target": { "mode": "any_holder", "name": "frigate.events" } },
    { "kind": "frigate.availability.v1", "target": { "mode": "any_holder", "name": "frigate.events" } }
  ],
  "nodes": [
    {
      "producer_id": "frigate-home",
      "token_file": "frigate-home.token",
      "zones": ["home"],
      "capabilities": ["frigate.events"],
      "publishes": ["frigate.event.v1", "frigate.availability.v1"]
    }
  ]
}
```

Consumers (a Soil automation Spore or a Soil Lisp run) need a node whose grants
allow observing those kinds. A receiver that only observes does not need
`publishes`.

## Adapter configuration

Copy [`config/frigate.example.json`](../config/frigate.example.json). The
`token_file` is resolved relative to this config, so the token and config travel
together. The example uses environment variables for optional MQTT
authentication; omit both to use an anonymous broker.

```json
{
  "bus": {
    "installation_id": "home",
    "producer_id": "frigate-home",
    "hub_endpoint": "127.0.0.1:18790",
    "token_file": "frigate-home.token",
    "zones": ["home"],
    "capabilities": ["frigate.events"],
    "schema_version": 2,
    "read_timeout_ms": 10
  },
  "mqtt": {
    "host": "127.0.0.1",
    "port": 1883,
    "client_id": "frigate-home",
    "tls": false,
    "keep_alive_secs": 30
  },
  "topic_prefix": "frigate"
}
```

`topic_prefix` is Frigate's `mqtt.topic_prefix` (default `frigate`). It may be
one or more `/` segments but may not contain `+`, `#`, NUL, or empty segments.

Build and run:

```sh
cargo test -p unibus-frigate
cargo build --release -p unibus-frigate
unibus-frigate /etc/unibus/frigate.json
```

Run it under a supervisor with restart backoff. A dead MQTT session is a
process-level failure: the adapter exits instead of pretending it is still
observing. The hub session is rebuilt by the host that runs it; the adapter
itself does not queue events.

## Event body

```json
{
  "event_id": "1712345678.123456-abc123",
  "type": "new",
  "camera": "front_door",
  "label": "person",
  "sub_label": null,
  "score": 0.87,
  "top_score": 0.91,
  "zones": ["porch"],
  "entered_zones": ["porch", "walkway"],
  "has_clip": true,
  "has_snapshot": true,
  "start_time": 1712345678.1,
  "end_time": null,
  "snapshot_time": 1712345679.2,
  "stationary": false,
  "observed_at_ms": 1712345679250
}
```

`type` distinguishes the lifecycle. `observed_at_ms` is the adapter's receive
time; Frigate's own timestamps stay in `start_time`, `end_time`, and
`snapshot_time`.

Retained event messages and MQTT duplicate deliveries are ignored so a broker
restart cannot replay an old detection into automation. Availability is
retained and therefore accepted as-is.

## Soil side

A Soil Spore or a Soil Lisp run polls the two exact kinds. There is no wildcard
match in `soil:unibus`; the adapter publishes stable names so the filter does
not drift.

The companion Soil example is
[`soil/crates/soil-lang/examples/frigate_automation.soil`](../../soil/crates/soil-lang/examples/frigate_automation.soil).
It polls `frigate.event.v1`, ignores `update`, dedupes by `event_id` in
`soil:records`, and emits an action record. Run it once:

```sh
soil-lang \
  --records-root ~/.local/share/fungos/records --book home \
  --unibus-config ~/.config/unibus/frigate-home.json \
  spores/crates/soil-lang/bindings/soil-unibus.soil \
  spores/crates/soil-lang/bindings/soil-records.soil \
  spores/crates/soil-lang/bindings/soil-time.soil \
  spores/crates/soil-lang/stdlib/prelude.soil \
  spores/crates/soil-lang/examples/frigate_automation.soil
```

### Poll semantics

`soil:unibus` is a live tail. Any non-empty cursor is `stale`; the router does
not retain history for this path. Dedupe with `soil:records`, keyed by
`event_id`, and restart from the live tail after an outage. Do not treat a
missing page as "no events ever happened".

## Verification

1. Confirm Frigate publishes to `<prefix>/events` with `mosquitto_sub -t
   'frigate/events' -v`.
2. Start the adapter and confirm the hub accepted its registration.
3. Subscribe a test consumer to `frigate.event.v1` and walk past a camera.
4. Confirm `new`, `update`, and `end` arrive with the same `event_id`.
5. Publish a retained event fixture and confirm the adapter logs and ignores it.
6. Restart the broker and confirm the adapter reconnects or exits for the
   supervisor; it must not silently keep claiming delivery.
