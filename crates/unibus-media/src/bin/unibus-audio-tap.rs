use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, TrySendError, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unibus::Message;
use unibus::client::{Client, ClientConfig};
use unibus_media::{
    AUDIO_PLAYBACK_CHUNK_CONTRACT, AudioPlaybackChunk, MEDIA_SCHEMA_VERSION, PcmFormat,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    bus: ClientConfig,
    #[serde(default)]
    backend: CaptureBackend,
    #[serde(default = "default_gst")]
    gst_launch: PathBuf,
    #[serde(default = "default_device")]
    capture_device: String,
    #[serde(default = "default_stream")]
    stream_id: String,
    #[serde(default = "default_rate")]
    sample_rate_hz: u32,
    #[serde(default = "default_channels")]
    channels: u16,
    #[serde(default = "default_chunk_ms")]
    chunk_ms: u32,
    #[serde(default = "default_destination")]
    destination: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CaptureBackend {
    #[default]
    Alsa,
    Pipewire,
    Jack,
    Coreaudio,
}

impl CaptureBackend {
    fn source_arguments(&self, reference: &str) -> Result<Vec<String>> {
        ensure!(
            !reference.trim().is_empty()
                && reference.len() <= 512
                && !reference.chars().any(char::is_control),
            "invalid capture reference"
        );
        Ok(match self {
            Self::Alsa => vec!["alsasrc".into(), format!("device={reference}")],
            Self::Pipewire => vec!["pipewiresrc".into(), format!("path={reference}")],
            Self::Jack => vec![
                "jackaudiosrc".into(),
                "connect=explicit".into(),
                format!("port-names={reference}"),
            ],
            Self::Coreaudio => {
                let device = reference
                    .parse::<i32>()
                    .context("CoreAudio capture_device must be a numeric AudioDeviceID")?;
                ensure!(device >= 0, "CoreAudio device ID must be nonnegative");
                vec!["osxaudiosrc".into(), format!("device={device}")]
            }
        })
    }
}

fn default_gst() -> PathBuf {
    PathBuf::from("gst-launch-1.0")
}
fn default_device() -> String {
    "visualizer_capture".into()
}
fn default_stream() -> String {
    "home-playback".into()
}
fn default_rate() -> u32 {
    48_000
}
fn default_channels() -> u16 {
    2
}
fn default_chunk_ms() -> u32 {
    10
}
fn default_destination() -> String {
    "living-room".into()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-audio-tap <config.json>")?;
    let bytes = std::fs::read(&path).with_context(|| format!("read {path}"))?;
    let mut config: Config =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {path}"))?;
    config.bus.resolve_token_file(Path::new(&path));
    config.bus.read_timeout_ms = config.bus.read_timeout_ms.min(250);
    ensure!(
        (5..=100).contains(&config.chunk_ms),
        "chunk_ms must be 5..=100"
    );
    ensure!(
        (8_000..=192_000).contains(&config.sample_rate_hz),
        "invalid PCM sample rate"
    );
    ensure!(
        (1..=8).contains(&config.channels),
        "invalid PCM channel count"
    );
    let sample_milliseconds = config
        .sample_rate_hz
        .checked_mul(config.chunk_ms)
        .context("PCM geometry overflow")?;
    ensure!(
        sample_milliseconds % 1_000 == 0,
        "chunk duration must contain a whole number of PCM frames"
    );
    let frames = sample_milliseconds / 1_000;
    ensure!(frames > 0, "chunk duration produced no samples");
    let chunk_bytes = frames as usize * config.channels as usize * 2;

    let source = config.backend.source_arguments(&config.capture_device)?;
    unibus_media::audio_session::supervise("unibus-audio-tap", || {
        run(&config, &source, frames, chunk_bytes)
    });
}

fn run(config: &Config, source: &[String], frames: u32, chunk_bytes: usize) -> Result<()> {
    // Connect first: an unavailable hub must not leave a capture process behind.
    let client = std::sync::Arc::new(Client::connect(config.bus.clone())?);
    let publication_monitor =
        unibus::publication_monitor::PublicationMonitor::spawn(client.clone())?;
    let mut command = Command::new(&config.gst_launch);
    command
        .arg("-q")
        .args(source)
        .args([
            "!",
            "audioconvert",
            "!",
            "audioresample",
            "!",
            &format!(
                "audio/x-raw,format=S16LE,rate={},channels={},layout=interleaved",
                config.sample_rate_hz, config.channels
            ),
            "!",
            "fdsink",
            "fd=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = unibus_media::audio_session::CaptureProcess::spawn(&mut command)?;
    let stdout = child.take_stdout()?;
    let (sender, receiver) = sync_channel(1);
    std::thread::Builder::new().name("unibus-pcm-capture".into()).spawn(move || {
        let mut capture = BufReader::new(stdout);
        let mut dropped = 0_u64;
        loop {
            let mut payload = vec![0_u8; chunk_bytes];
            if let Err(error) = capture.read_exact(&mut payload) {
                let _ = sender.send(Err(error));
                break;
            }
            match sender.try_send(Ok((Instant::now(), now_ms(), payload))) {
                Ok(()) => {}
                Err(TrySendError::Disconnected(_)) => break,
                Err(TrySendError::Full(_)) => {
                    dropped = dropped.saturating_add(1);
                    if dropped.is_power_of_two() { eprintln!("unibus-audio-tap: dropped {dropped} captured chunks while bus was slow"); }
                }
            }
        }
    })?;
    let mut sequence = 0_u64;
    loop {
        anyhow::ensure!(client.is_healthy(), "Unibus heartbeat stopped");
        publication_monitor.check()?;
        let (captured_at, captured_at_unix_ms, payload) =
            match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(result) => result.context("PCM capture ended")?,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => anyhow::bail!("PCM capture worker stopped"),
            };
        if captured_at.elapsed() > Duration::from_millis(u64::from(config.chunk_ms) * 4) {
            continue;
        }
        let chunk = AudioPlaybackChunk {
            schema_version: MEDIA_SCHEMA_VERSION,
            stream_id: config.stream_id.clone(),
            sequence,
            captured_at_unix_ms,
            sample_rate_hz: config.sample_rate_hz,
            channels: config.channels,
            frames,
            format: PcmFormat::S16Le,
        };
        chunk.validate(payload.len()).map_err(anyhow::Error::msg)?;
        let mut envelope = client.envelope(
            Some(&config.destination),
            Message::new(AUDIO_PLAYBACK_CHUNK_CONTRACT, serde_json::to_value(&chunk)?),
        )?;
        envelope.ttl_ms = Some(config.chunk_ms as u64 * 4);
        envelope.payload_bytes = payload.len();
        client.send(&envelope, &payload)?;
        sequence = sequence.wrapping_add(1);
    }
}

#[allow(dead_code)]
fn _path_is_explicit(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_selection_preserves_explicit_application_ports() {
        let source = CaptureBackend::Jack
            .source_arguments("surge:left,surge:right")
            .unwrap();
        assert!(source.contains(&"connect=explicit".into()));
        assert!(source.contains(&"port-names=surge:left,surge:right".into()));
        assert!(
            CaptureBackend::Coreaudio
                .source_arguments("BlackHole")
                .is_err()
        );
        assert!(CaptureBackend::Coreaudio.source_arguments("-1").is_err());
        assert!(CaptureBackend::Pipewire.source_arguments("").is_err());
    }
}
