# Application audio capture

`unibus-audio-tap` selects a GStreamer capture provider using `backend` and
`capture_device`. Existing configurations default to `alsa`.

| backend | capture_device |
| --- | --- |
| alsa | PCM name, e.g. `visualizer_capture` |
| jack | Exact comma-separated source ports, e.g. `surge:left,surge:right` |
| pipewire | PipeWire source node path/ID |
| coreaudio | Numeric CoreAudio input device ID (virtual devices supported) |

The corresponding GStreamer plugin must be installed. Missing plugins and
capture failures surface on stderr and restart the capture session with bounded
exponential backoff (250 ms through 30 s). JACK uses explicit
port connections and requires GStreamer 1.20 or newer. Capture is converted and
resampled to the configured PCM geometry before publication through the existing
`audio.playback_chunk` contract.

This adapter publishes bounded PCM chunks. Studio transport still requires
latency measurements; capture-provider selection alone does not establish
low-latency monitoring. Application discovery, route receipts, and playback
provider selection remain separate work.

Both tap and sink rebuild their bus session after disconnect. Publish-only taps
drain hub replies separately and recover on rejected publication as well as
broken writes. Failed sessions kill and reap GStreamer before replacement.
A one-chunk queue keeps capture independent of slow bus writes; old chunks are
dropped rather than replayed. Relative token paths resolve beside the config.

The v1 PCM contract has no sender epoch. The sink re-establishes its sequence and
geometry baseline after a full freshness window without an admitted chunk, but
only for fresh capture timestamps newer than the previous accepted chunk.
This bounds recovery after a sender restart without admitting delayed old audio.
Within an active session, duplicate/reordered chunks and geometry changes retain
their existing rejection behavior.
