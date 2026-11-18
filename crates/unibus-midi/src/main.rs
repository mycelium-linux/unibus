use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use midir::{Ignore, MidiInput, MidiInputPort, MidiOutput, MidiOutputPort};
use serde::Deserialize;
use tracing::{debug, info, warn};
use unibus::client::{Client as UnibusClient, ClientConfig};
use unibus::{Message, ReconnectBackoff, ReconnectPolicy};
use unibus_midi::{
    MIDI_INPUT_CAPABILITY, MIDI_OUTPUT_CAPABILITY, MIDI_PACKET_CONTRACT, MIDI_SCHEMA_VERSION,
    MidiPacket, valid_identifier,
};

const DEFAULT_QUEUE_CAPACITY: usize = 1024;
const MAX_EVENT_AGE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    bus: ClientConfig,
    device: DeviceConfig,
    #[serde(default = "default_queue_capacity")]
    queue_capacity: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "direction", deny_unknown_fields)]
enum DeviceConfig {
    Input {
        port: String,
        #[serde(default)]
        additional_ports: Vec<String>,
        destination: String,
    },
    Output {
        port: String,
        #[serde(default)]
        channel_mode: OutputChannelMode,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputChannelMode {
    #[default]
    Preserve,
    All,
}

fn default_queue_capacity() -> usize {
    DEFAULT_QUEUE_CAPACITY
}

impl BridgeConfig {
    fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read MIDI bridge config {}", path.display()))?;
        let mut config = serde_json::from_slice::<Self>(&bytes)
            .with_context(|| format!("parse MIDI bridge config {}", path.display()))?;
        config.bus.resolve_token_file(path);
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.queue_capacity > 0, "queue_capacity must be positive");
        ensure!(
            self.queue_capacity <= 65_536,
            "queue_capacity exceeds 65536 packets"
        );
        ensure!(
            self.bus.schema_version == unibus::SCHEMA_VERSION_V2,
            "MIDI bridge requires Unibus schema_version 2"
        );
        let required_capability = match &self.device {
            DeviceConfig::Input {
                port,
                additional_ports,
                destination,
            } => {
                ensure!(!port.trim().is_empty(), "MIDI input port is empty");
                ensure!(
                    additional_ports.iter().all(|port| !port.trim().is_empty()),
                    "additional MIDI input port is empty"
                );
                ensure!(additional_ports.len() < 128, "too many MIDI input ports");
                ensure!(
                    valid_identifier(destination),
                    "invalid MIDI destination producer"
                );
                MIDI_INPUT_CAPABILITY
            }
            DeviceConfig::Output { port, .. } => {
                ensure!(!port.trim().is_empty(), "MIDI output port is empty");
                MIDI_OUTPUT_CAPABILITY
            }
        };
        ensure!(
            self.bus
                .capabilities
                .iter()
                .any(|capability| capability == required_capability),
            "bus capabilities must include {required_capability}"
        );
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let Some(argument) = std::env::args_os().nth(1) else {
        bail!("usage: unibus-midi <bridge.json> | --list");
    };
    if argument == "--list-json" {
        let inventory =
            unibus_midi::inventory::enumerate("local", unibus::envelope::now_ms(), 6000)?;
        println!("{}", serde_json::to_string_pretty(&inventory)?);
        return Ok(());
    }
    if argument == "--list" {
        return list_ports();
    }
    supervise(BridgeConfig::read(Path::new(&argument))?)
}

fn supervise(config: BridgeConfig) -> Result<()> {
    let mut backoff =
        ReconnectBackoff::new(ReconnectPolicy::default()).map_err(anyhow::Error::msg)?;
    loop {
        let started = Instant::now();
        let error = match run(config.clone()) {
            Ok(()) => anyhow::anyhow!("MIDI bridge session stopped unexpectedly"),
            Err(error) => error,
        };
        let delay = backoff.failed_after(started.elapsed());
        warn!(
            error = %error,
            retry_ms = delay.as_millis(),
            failures = backoff.consecutive_failures(),
            "MIDI bridge session failed; reconnecting"
        );
        std::thread::sleep(delay);
    }
}

fn run(config: BridgeConfig) -> Result<()> {
    match config.device {
        DeviceConfig::Input {
            port,
            additional_ports,
            destination,
        } => {
            let mut ports = vec![port];
            ports.extend(additional_ports);
            run_input(config.bus, &ports, destination, config.queue_capacity)
        }
        DeviceConfig::Output { port, channel_mode } => run_output(config.bus, &port, channel_mode),
    }
}

fn run_input(
    mut bus_config: ClientConfig,
    port_queries: &[String],
    destination: String,
    queue_capacity: usize,
) -> Result<()> {
    let producer_id = bus_config.producer_id.clone();
    bus_config.read_timeout_ms = bus_config.read_timeout_ms.min(250);
    let bus = Arc::new(UnibusClient::connect(bus_config)?);
    let publication_monitor = unibus::publication_monitor::PublicationMonitor::spawn(bus.clone())?;
    let (sender, receiver) = sync_channel::<(Instant, u64, Vec<u8>)>(queue_capacity);
    let overflow = Arc::new(AtomicBool::new(false));
    let mut bindings = (0..port_queries.len()).map(|_| None).collect::<Vec<_>>();
    let mut errors = vec![None; port_queries.len()];
    let mut sequence = 0_u64;
    let mut next_scan = Instant::now();
    loop {
        ensure!(bus.is_healthy(), "Unibus heartbeat stopped");
        publication_monitor.check()?;
        let mut reset = overflow.swap(false, Ordering::AcqRel);
        if Instant::now() >= next_scan {
            next_scan = Instant::now() + Duration::from_secs(1);
            for (index, query) in port_queries.iter().enumerate() {
                let mut midi = MidiInput::new(&format!("unibus-midi-probe-{index}"))?;
                midi.ignore(Ignore::None);
                let selected = select_input_port(&midi, query).map(|port| (port.id(), port));
                let was_bound = bindings[index].is_some();
                let port_sender = sender.clone();
                let port_overflow = overflow.clone();
                let result = reconcile_binding(&mut bindings[index], selected, |port| {
                    midi.connect(
                        &port,
                        &format!("unibus-midi-capture-{index}"),
                        move |timestamp, bytes, ()| {
                            if let Err(TrySendError::Full(_)) =
                                port_sender.try_send((Instant::now(), timestamp, bytes.to_vec()))
                            {
                                // No logging or bus I/O on the platform callback.
                                port_overflow.store(true, Ordering::Release);
                            }
                        },
                        (),
                    )
                    .map_err(|error| anyhow::anyhow!("connect MIDI input {query}: {error}"))
                });
                match result {
                    Ok(changed) => {
                        reset |= changed;
                        if changed {
                            info!(port = %query, %producer_id, "MIDI input bound");
                        }
                        errors[index] = None;
                    }
                    Err(error) => {
                        reset |= was_bound;
                        let error = error.to_string();
                        if errors[index].as_ref() != Some(&error) {
                            warn!(port = %query, %error, "MIDI input unavailable; retrying while other inputs stay active");
                        }
                        errors[index] = Some(error);
                    }
                }
            }
        }
        if reset {
            // Losing a note-off must not leave the remote instrument sounding.
            // Discard the old queue before sending the reset; never replay it.
            reset_capture(&receiver, |payload| {
                publish_input(&bus, &destination, &producer_id, &mut sequence, 0, payload)
            })?;
            warn!(
                "MIDI input changed or queue overflowed; cleared queued events and reset destination"
            );
        }
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok((captured, timestamp, payload)) => {
                if captured.elapsed() > MAX_EVENT_AGE {
                    overflow.store(true, Ordering::Release);
                    continue;
                }
                publish_input(
                    &bus,
                    &destination,
                    &producer_id,
                    &mut sequence,
                    timestamp,
                    &payload,
                )?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("MIDI input channel closed"),
        }
    }
}

fn reset_capture<T>(
    receiver: &std::sync::mpsc::Receiver<T>,
    mut send: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    while receiver.try_recv().is_ok() {}
    for message in reset_messages() {
        send(&message)?;
    }
    Ok(())
}

/// Re-enumeration is authoritative: a missing/ambiguous selection drops the old
/// handle, and a changed OS identity is rebound even when its label is unchanged.
fn reconcile_binding<C, P>(
    binding: &mut Option<(String, C)>,
    selected: Result<(String, P)>,
    connect: impl FnOnce(P) -> Result<C>,
) -> Result<bool> {
    let (id, port) = match selected {
        Ok(selected) => selected,
        Err(error) => {
            *binding = None;
            return Err(error);
        }
    };
    if binding.as_ref().is_some_and(|(current, _)| current == &id) {
        return Ok(false);
    }
    *binding = None;
    *binding = Some((id, connect(port)?));
    Ok(true)
}

fn publish_input(
    bus: &UnibusClient,
    destination: &str,
    origin: &str,
    sequence: &mut u64,
    timestamp: u64,
    payload: &[u8],
) -> Result<()> {
    let packet = MidiPacket {
        schema_version: MIDI_SCHEMA_VERSION,
        origin: origin.into(),
        sequence: *sequence,
        source_timestamp_micros: timestamp,
    };
    packet.validate(payload)?;
    let mut envelope = bus.envelope(
        Some(destination),
        Message::new(MIDI_PACKET_CONTRACT, serde_json::to_value(&packet)?),
    )?;
    envelope.payload_bytes = payload.len();
    envelope.ttl_ms = Some(MAX_EVENT_AGE.as_millis() as u64);
    bus.send(&envelope, payload)?;
    *sequence = sequence.wrapping_add(1);
    if let Some((channel, note, velocity)) = note_on(payload) {
        debug!(channel, note, velocity, "MIDI note captured");
    }
    if let Some((channel, control, value)) = control_change(payload) {
        debug!(channel, control, value, "MIDI control captured");
    }
    Ok(())
}

fn run_output(
    bus_config: ClientConfig,
    port_query: &str,
    channel_mode: OutputChannelMode,
) -> Result<()> {
    let bus = UnibusClient::connect(bus_config)?;
    let midi = MidiOutput::new("unibus-midi-output")?;
    let port = select_output_port(&midi, port_query)?;
    let port_id = port.id();
    let port_name = midi.port_name(&port)?;
    let mut connection = midi
        .connect(&port, "unibus-midi-playback")
        .map_err(|error| anyhow::anyhow!("connect MIDI output: {error}"))?;
    reset_midi_output(&mut connection)?;
    info!(port = %port_name, ?channel_mode, "MIDI output bridge started");
    let result = output_session(&bus, &mut connection, port_query, &port_id, channel_mode);
    if let Err(error) = reset_midi_output(&mut connection) {
        warn!(%error, "failed to reset MIDI output after session loss");
    }
    result
}

fn output_session(
    bus: &UnibusClient,
    connection: &mut midir::MidiOutputConnection,
    port_query: &str,
    port_id: &str,
    channel_mode: OutputChannelMode,
) -> Result<()> {
    let mut next_scan = Instant::now();
    let mut origins = BTreeSet::new();
    loop {
        if Instant::now() >= next_scan {
            next_scan = Instant::now() + Duration::from_secs(1);
            let probe = MidiOutput::new("unibus-midi-output-probe")?;
            let selected = select_output_port(&probe, port_query)?;
            ensure!(
                selected.id() == port_id,
                "MIDI output identity changed; reopening"
            );
        }
        let frame = bus.try_read()?;
        let Some((envelope, payload)) = frame else {
            continue;
        };
        if matches!(
            unibus::kinds::classify(&envelope.message.kind),
            unibus::kinds::Control::Reply
        ) {
            ensure!(
                envelope
                    .message
                    .body
                    .get("success")
                    .and_then(|value| value.as_bool())
                    == Some(true),
                "hub rejected MIDI publication: {:?}",
                envelope.message.body
            );
            continue;
        }
        if envelope.message.kind == unibus::kinds::NODE_STATE {
            if envelope.producer_id == "unibus-hub" && lost_origin(&envelope.message, &mut origins)
            {
                reset_midi_output(connection)?;
            }
            continue;
        }
        if envelope.message.kind != MIDI_PACKET_CONTRACT {
            warn!(kind = %envelope.message.kind, "ignoring unexpected Unibus message");
            continue;
        }
        let packet: MidiPacket =
            serde_json::from_value(serde_json::Value::Object(envelope.message.body))?;
        packet.validate(&payload)?;
        origins.insert(envelope.producer_id.clone());
        if let Some((channel, note, velocity)) = note_on(&payload) {
            debug!(
                sequence = packet.sequence,
                channel,
                note,
                velocity,
                transport_ms =
                    unibus::envelope::now_ms().saturating_sub(envelope.issued_at_unix_ms),
                "MIDI note delivered"
            );
        }
        send_midi(connection, &payload, channel_mode)?;
    }
}

fn lost_origin(message: &Message, origins: &mut BTreeSet<String>) -> bool {
    message
        .body
        .get("online")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
        && message
            .body
            .get("producer_id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|origin| origins.remove(origin))
}

fn reset_midi_output(connection: &mut midir::MidiOutputConnection) -> Result<()> {
    for message in reset_messages() {
        connection.send(&message)?;
    }
    Ok(())
}

fn reset_messages() -> impl Iterator<Item = [u8; 3]> {
    (0_u8..16).flat_map(|channel| {
        [
            [0xb0 | channel, 64, 0],  // sustain off
            [0xb0 | channel, 120, 0], // all sound off
            [0xb0 | channel, 123, 0], // all notes off
        ]
    })
}

fn send_midi(
    connection: &mut midir::MidiOutputConnection,
    payload: &[u8],
    channel_mode: OutputChannelMode,
) -> Result<()> {
    let messages = expanded_messages(payload, channel_mode);
    ensure!(!messages.is_empty(), "empty MIDI payload");
    for message in messages {
        connection.send(&message)?;
    }
    Ok(())
}

fn expanded_messages(payload: &[u8], channel_mode: OutputChannelMode) -> Vec<Vec<u8>> {
    let Some(status) = payload.first().copied() else {
        return Vec::new();
    };
    if channel_mode != OutputChannelMode::All || !(0x80..=0xef).contains(&status) {
        return vec![payload.to_vec()];
    }
    let message_kind = status & 0xf0;
    (0..16)
        .map(|channel| {
            let mut message = payload.to_vec();
            message[0] = message_kind | channel;
            message
        })
        .collect()
}

fn note_on(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let [status, note, velocity, ..] = payload else {
        return None;
    };
    (*status & 0xf0 == 0x90 && *velocity > 0).then_some((*status & 0x0f, *note, *velocity))
}

fn control_change(payload: &[u8]) -> Option<(u8, u8, u8)> {
    let [status, control, value, ..] = payload else {
        return None;
    };
    (*status & 0xf0 == 0xb0).then_some((*status & 0x0f, *control, *value))
}

fn list_ports() -> Result<()> {
    let input = MidiInput::new("unibus-midi-list-input")?;
    println!("MIDI inputs:");
    for port in input.ports() {
        println!("  {}", input.port_name(&port)?);
    }
    let output = MidiOutput::new("unibus-midi-list-output")?;
    println!("MIDI outputs:");
    for port in output.ports() {
        println!("  {}", output.port_name(&port)?);
    }
    Ok(())
}

fn select_input_port(midi: &MidiInput, query: &str) -> Result<MidiInputPort> {
    select_unique(midi.ports(), query, |port| midi.port_name(port))
}

fn select_output_port(midi: &MidiOutput, query: &str) -> Result<MidiOutputPort> {
    select_unique(midi.ports(), query, |port| midi.port_name(port))
}

fn select_unique<T, F>(ports: Vec<T>, query: &str, mut name: F) -> Result<T>
where
    F: FnMut(&T) -> Result<String, midir::PortInfoError>,
{
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    let mut available = Vec::new();
    for port in ports {
        let port_name = name(&port)?;
        if query == "*" || port_name.to_lowercase().contains(&query_lower) {
            matches.push((port_name.clone(), port));
        }
        available.push(port_name);
    }
    match matches.len() {
        1 => Ok(matches.pop().unwrap().1),
        0 => bail!(
            "no MIDI port contains {query:?}; available ports: {}",
            available.join(", ")
        ),
        _ => bail!(
            "MIDI port query {query:?} is ambiguous: {}",
            matches
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> BridgeConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn input_config_requires_destination_and_capability() {
        let config = config(
            r#"{
                "bus":{"installation_id":"home","producer_id":"mac-midi","hub_endpoint":"127.0.0.1:18790","token_env":"UNIBUS_MIDI_TOKEN","zones":["studio"],"capabilities":["midi.input"],"schema_version":2},
                "device":{"direction":"input","port":"Keystation","destination":"pi-midi"}
            }"#,
        );
        config.validate().unwrap();
    }

    #[test]
    fn output_config_requires_output_capability() {
        let mut config = config(
            r#"{
                "bus":{"installation_id":"home","producer_id":"pi-midi","hub_endpoint":"127.0.0.1:18790","token_env":"UNIBUS_MIDI_TOKEN","zones":["studio"],"capabilities":["midi.input"],"schema_version":2},
                "device":{"direction":"output","port":"Scarlett"}
            }"#,
        );
        assert!(config.validate().is_err());
        config.bus.capabilities = vec![MIDI_OUTPUT_CAPABILITY.into()];
        config.validate().unwrap();
    }

    #[test]
    fn wildcard_requires_exactly_one_port() {
        let selected = select_unique(vec!["keyboard"], "*", |port| Ok((*port).to_owned()))
            .expect("single port should match wildcard");
        assert_eq!(selected, "keyboard");
        assert!(select_unique(vec!["one", "two"], "*", |port| Ok((*port).to_owned())).is_err());
    }

    #[test]
    fn note_on_excludes_note_off_and_zero_velocity() {
        assert_eq!(note_on(&[0x92, 60, 7]), Some((2, 60, 7)));
        assert_eq!(note_on(&[0x82, 60, 7]), None);
        assert_eq!(note_on(&[0x92, 60, 0]), None);
        assert_eq!(note_on(&[0x90, 60]), None);
    }

    #[test]
    fn all_channel_mode_fans_out_channel_voice_messages() {
        let messages = expanded_messages(&[0x92, 60, 100], OutputChannelMode::All);
        assert_eq!(messages.len(), 16);
        assert_eq!(messages[0], [0x90, 60, 100]);
        assert_eq!(messages[15], [0x9f, 60, 100]);

        let note_off = expanded_messages(&[0x82, 60, 0], OutputChannelMode::All);
        assert_eq!(note_off.len(), 16);
        assert_eq!(note_off[15], [0x8f, 60, 0]);
    }

    #[test]
    fn preserve_and_system_messages_are_not_duplicated() {
        assert_eq!(
            expanded_messages(&[0x95, 64, 80], OutputChannelMode::Preserve),
            vec![vec![0x95, 64, 80]]
        );
        assert_eq!(
            expanded_messages(&[0xf8], OutputChannelMode::All),
            vec![vec![0xf8]]
        );
    }

    #[test]
    fn reconnect_reset_clears_sustain_sound_and_notes_on_every_channel() {
        let messages = reset_messages().collect::<Vec<_>>();
        assert_eq!(messages.len(), 48);
        assert_eq!(messages[0], [0xb0, 64, 0]);
        assert_eq!(messages[2], [0xb0, 123, 0]);
        assert_eq!(messages[45], [0xbf, 64, 0]);
        assert_eq!(messages[47], [0xbf, 123, 0]);
    }

    #[test]
    fn hotplug_rebinds_changed_identity_and_leaves_other_inputs_bound() {
        let mut keyboard = None;
        let mut pads = None;
        assert!(reconcile_binding(&mut keyboard, Ok(("usb-1".into(), 1)), Ok).unwrap());
        assert!(reconcile_binding(&mut pads, Ok(("usb-2".into(), 2)), Ok).unwrap());
        assert!(
            !reconcile_binding(
                &mut keyboard,
                Ok(("usb-1".into(), 99)),
                |_| -> Result<i32> { panic!("must retain live handle") }
            )
            .unwrap()
        );
        assert!(
            reconcile_binding::<_, i32>(&mut keyboard, Err(anyhow::anyhow!("unplugged")), Ok)
                .is_err()
        );
        assert!(keyboard.is_none());
        assert_eq!(pads.as_ref().unwrap().1, 2);
        assert!(reconcile_binding(&mut keyboard, Ok(("usb-3".into(), 3)), Ok).unwrap());
        assert_eq!(keyboard.unwrap(), ("usb-3".into(), 3));
    }

    #[test]
    fn failed_rebind_never_keeps_the_dead_handle() {
        let mut binding = Some(("old".into(), 1));
        assert!(
            reconcile_binding(&mut binding, Ok(("new".into(), ())), |_| Err(
                anyhow::anyhow!("open failed")
            ))
            .is_err()
        );
        assert!(binding.is_none());
    }

    #[test]
    fn queue_loss_discards_stale_note_events_before_sending_a_complete_reset() {
        let (sender, receiver) = sync_channel(1);
        sender.try_send(vec![0x90, 60, 100]).unwrap();
        assert!(matches!(
            sender.try_send(vec![0x80, 60, 0]),
            Err(TrySendError::Full(_))
        ));
        let mut delivered = Vec::new();
        reset_capture(&receiver, |message| {
            delivered.push(message.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(receiver.try_recv().is_err());
        assert_eq!(delivered.len(), 48);
        assert!(!delivered.iter().any(|message| message[0] & 0xf0 == 0x90));
        assert!(delivered.contains(&vec![0xb0, 64, 0]));
        assert!(delivered.contains(&vec![0xbf, 123, 0]));
    }

    #[test]
    fn losing_an_active_publisher_resets_notes_but_unrelated_nodes_do_not() {
        let mut origins = BTreeSet::from(["keys".into()]);
        let message = |origin: &str, online| {
            Message::new(
                unibus::kinds::NODE_STATE,
                serde_json::json!({"producer_id":origin,"online":online}),
            )
        };
        assert!(!lost_origin(&message("other", false), &mut origins));
        assert!(!lost_origin(&message("keys", true), &mut origins));
        assert!(lost_origin(&message("keys", false), &mut origins));
        assert!(!lost_origin(&message("keys", false), &mut origins));
    }
}
