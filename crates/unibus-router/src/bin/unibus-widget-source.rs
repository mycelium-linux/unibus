//! Trusted-config telemetry collector. No browser-supplied commands or URLs.
use anyhow::{ensure, Context, Result};
use coherence_widget_contract::{
    Metric, ModelCatalog, ServedModel, Snapshot, MAX_HISTORY, MESSAGE_KIND,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use unibus::{
    client::{Client, ClientConfig},
    envelope::now_ms,
    Message,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    client: ClientConfig,
    zone: String,
    poll_seconds: u64,
    feeds: Vec<Feed>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Feed {
    id: String,
    sources: Vec<Source>,
    #[serde(default)]
    model_catalog_command: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    command: Vec<String>,
    metrics: Vec<Selection>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    id: String,
    label: String,
    unit: String,
    /// An exact Prometheus series selector, or a CSV column index.
    #[serde(default)]
    series: Option<String>,
    /// Aggregate the exact named Prometheus family, including labeled samples.
    #[serde(default)]
    sum: bool,
    /// Divide by another exact series; zero/missing denominator is unavailable.
    #[serde(default)]
    divide_by: Option<String>,
    #[serde(default)]
    column: Option<usize>,
    #[serde(default)]
    subtract: Option<String>,
    #[serde(default)]
    maximum_series: Option<String>,
    #[serde(default)]
    maximum: Option<f32>,
    #[serde(default)]
    maximum_column: Option<usize>,
    #[serde(default = "one")]
    divisor: f64,
}
fn one() -> f64 {
    1.
}
fn served_models(text: &str) -> Result<ModelCatalog> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<ServedModel>,
    }
    // OpenAI-compatible records may include object/capabilities; project only
    // explicitly supported fields rather than leaking arbitrary provider JSON.
    let value: serde_json::Value = serde_json::from_str(text)?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .context("missing model list")?;
    ensure!(data.len() <= 128, "too many served models");
    let projected = serde_json::json!({"data": data.iter().map(|entry| serde_json::json!({
        "id": entry.get("id"), "owned_by": entry.get("owned_by").unwrap_or(&serde_json::Value::Null)
    })).collect::<Vec<_>>()});
    let response: Response = serde_json::from_value(projected)?;
    let catalog = ModelCatalog {
        available: true,
        entries: response.data,
    };
    let validation = Snapshot {
        schema_version: 1,
        widget_id: "catalog".into(),
        observed_at_unix_ms: 1,
        expires_at_unix_ms: 2,
        metrics: vec![],
        model_catalog: Some(catalog.clone()),
    };
    validation.validate().map_err(anyhow::Error::msg)?;
    Ok(catalog)
}
fn scalar(text: &str, selector: &str) -> Option<f64> {
    let mut values = text.lines().filter_map(|line| {
        let line = line.trim();
        if !line.starts_with(selector)
            || !line
                .as_bytes()
                .get(selector.len())
                .is_some_and(u8::is_ascii_whitespace)
        {
            return None;
        }
        line[selector.len()..]
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    });
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}
fn sum_family(text: &str, family: &str) -> Option<f64> {
    let mut total = 0.;
    let mut found = false;
    for line in text.lines().map(str::trim) {
        if !line.starts_with(family) {
            continue;
        }
        let suffix = &line[family.len()..];
        if !suffix.starts_with('{')
            && !suffix
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }
        let value = line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
        if !value.is_finite() {
            return None;
        }
        total += value;
        found = true;
    }
    found.then_some(total).filter(|v| v.is_finite())
}
fn selected(text: &str, metric: &Selection) -> (Option<f32>, Option<f32>) {
    let value = match (&metric.series, metric.column) {
        (Some(series), None) if metric.sum => sum_family(text, series),
        (Some(series), None) => scalar(text, series),
        (None, Some(column)) if text.lines().count() == 1 => text
            .trim()
            .split(',')
            .nth(column)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite()),
        _ => None,
    };
    let value = if let Some(subtract) = &metric.subtract {
        value.zip(scalar(text, subtract)).map(|(a, b)| a - b)
    } else {
        value
    };
    let value = if let Some(denominator) = &metric.divide_by {
        value
            .zip(scalar(text, denominator).filter(|d| *d > 0.))
            .map(|(n, d)| n / d)
    } else {
        value
    };
    let maximum = metric
        .maximum
        .or_else(|| {
            metric
                .maximum_column
                .and_then(|column| {
                    text.trim()
                        .split(',')
                        .nth(column)?
                        .trim()
                        .parse::<f32>()
                        .ok()
                })
                .map(|v| (f64::from(v) / metric.divisor) as f32)
        })
        .or_else(|| {
            metric
                .maximum_series
                .as_deref()
                .and_then(|s| scalar(text, s))
                .map(|v| (v / f64::from(metric.divisor)) as f32)
        })
        .filter(|v| *v > 0. && v.is_finite());
    (
        value
            .map(|v| (v / f64::from(metric.divisor)) as f32)
            .filter(|v| v.is_finite()),
        maximum,
    )
}
fn collect(arguments: &[String]) -> Result<String> {
    ensure!(
        !arguments.is_empty() && arguments.len() <= 32,
        "invalid collector command"
    );
    let mut command = Command::new(&arguments[0]);
    command
        .args(&arguments[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("start configured collector")?;
    let mut stdout = child.stdout.take().context("collector stdout missing")?;
    let (send, receive) = mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .by_ref()
            .take(256 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = send.send(result);
    });
    let started = Instant::now();
    let output = loop {
        if let Ok(output) = receive.try_recv() {
            break Some(output);
        }
        if started.elapsed() > Duration::from_secs(5) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(unix)]
    if output.is_none() {
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    // Reap on all paths; never join a potentially blocked reader on a timeout.
    let mut success = false;
    loop {
        if let Some(status) = child.try_wait()? {
            success = status.success();
            break;
        }
        if started.elapsed() > Duration::from_secs(5) {
            #[cfg(unix)]
            {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    rustix::process::kill_process_group(pid, rustix::process::Signal::KILL)?;
                }
            }
            #[cfg(not(unix))]
            child.kill()?;
            child.wait()?;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ensure!(success, "collector failed or timed out");
    let bytes = output.context("collector timed out")??;
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("collector reader panicked"))?;
    ensure!(
        bytes.len() <= 256 * 1024,
        "collector output exceeds byte budget"
    );
    String::from_utf8(bytes).context("collector output is not UTF-8")
}
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-widget-source CONFIG")?;
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.len() <= 64 * 1024,
        "source config exceeds byte budget"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "source config must be private"
        );
    }
    let mut config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    ensure!(
        (5..=30).contains(&config.poll_seconds)
            && config.feeds.len() <= 16
            && !config.feeds.is_empty(),
        "invalid polling cadence or feed count"
    );
    config
        .client
        .resolve_token_file(std::path::Path::new(&path));
    ensure!(
        config.client.schema_version == 2 && config.client.zones.contains(&config.zone),
        "widget sources require schema v2 and an explicit granted zone"
    );
    for feed in &config.feeds {
        ensure!(
            coherence_widget_contract::identifier(&feed.id)
                && feed.sources.len() <= 8
                && feed.sources.iter().map(|s| s.metrics.len()).sum::<usize>() <= 16,
            "invalid feed bounds"
        );
        for source in &feed.sources {
            for metric in &source.metrics {
                ensure!(
                    metric.divisor.is_finite()
                        && metric.divisor > 0.
                        && (!metric.sum || (metric.series.is_some() && metric.column.is_none())),
                    "invalid metric divisor"
                );
            }
        }
    }
    let client = Client::connect(config.client)?;
    let mut history = BTreeMap::<(String, String), Vec<f32>>::new();
    loop {
        let start = Instant::now();
        for feed in &config.feeds {
            let mut metrics = Vec::new();
            for source in &feed.sources {
                let output = match collect(&source.command) {
                    Ok(text) => text,
                    Err(_) => {
                        eprintln!("widget {} collector unavailable", feed.id);
                        String::new()
                    }
                };
                for selection in &source.metrics {
                    let (value, maximum) = selected(&output, selection);
                    let samples = history
                        .entry((feed.id.clone(), selection.id.clone()))
                        .or_default();
                    if let Some(value) = value {
                        samples.push(value);
                        if samples.len() > MAX_HISTORY {
                            samples.remove(0);
                        }
                    }
                    metrics.push(Metric {
                        id: selection.id.clone(),
                        label: selection.label.clone(),
                        unit: selection.unit.clone(),
                        value,
                        maximum,
                        history: samples.clone(),
                    });
                }
            }
            let now = now_ms();
            let model_catalog = feed.model_catalog_command.as_ref().map(|command| {
                match collect(command).and_then(|text| served_models(&text)) {
                    Ok(catalog) => catalog,
                    Err(error) => {
                        eprintln!("widget {} model catalog unavailable: {error}", feed.id);
                        ModelCatalog {
                            available: false,
                            entries: vec![],
                        }
                    }
                }
            });
            let snapshot = Snapshot {
                schema_version: 1,
                widget_id: feed.id.clone(),
                observed_at_unix_ms: now,
                expires_at_unix_ms: now + config.poll_seconds * 3000,
                metrics,
                model_catalog,
            };
            snapshot.validate().map_err(anyhow::Error::msg)?;
            let envelope = client.envelope(
                Some(&config.zone),
                Message::new(MESSAGE_KIND, serde_json::to_value(snapshot)?),
            )?;
            client.send(&envelope, &[])?;
        }
        std::thread::sleep(
            Duration::from_secs(config.poll_seconds).saturating_sub(start.elapsed()),
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_catalog_projects_only_bounded_unique_provider_identities() {
        let catalog = served_models(r#"{"data":[{"id":"org/model","owned_by":"owner","object":"model","private_path":"/secret"}]}"#).unwrap();
        assert_eq!(catalog.entries[0].id, "org/model");
        assert!(served_models(
            r#"{"data":[{"id":"same","owned_by":"a"},{"id":"same","owned_by":"b"}]}"#
        )
        .is_err());
        assert!(served_models("{}").is_err());
        assert!(served_models(r#"{"data":[{"id":"bad\nname","owned_by":"a"}]}"#).is_err());
        assert!(served_models(r#"{"data":[]}"#).unwrap().entries.is_empty());
    }
    #[test]
    fn exact_series_only_and_missing_is_not_zero() {
        assert_eq!(scalar("vm_count 0\nvm_count_extra 9", "vm_count"), Some(0.));
        assert_eq!(scalar("vm_count_extra 9", "vm_count"), None);
        assert_eq!(scalar("vm_count NaN", "vm_count"), None);
        assert_eq!(scalar("vm_count 1\nvm_count 2", "vm_count"), None);
        assert_eq!(
            scalar("load{interval=\"1m\"} 3", "load{interval=\"1m\"}"),
            Some(3.)
        );
    }
    #[test]
    fn family_sum_and_ratio_do_not_invent_missing_observations() {
        assert_eq!(
            sum_family(
                "vm{state=\"ready\"} 1\nvm{state=\"stale\"} 2\nvm_extra 99",
                "vm"
            ),
            Some(3.)
        );
        assert_eq!(sum_family("vm_extra 0", "vm"), None);
        assert_eq!(sum_family("vm{state=\"ready\"} NaN", "vm"), None);
        let selection: Selection = serde_json::from_str(r#"{"id":"latency","label":"Mean latency","unit":"ms","series":"sum","divide_by":"count","divisor":0.001}"#).unwrap();
        assert_eq!(selected("sum 1\ncount 2", &selection).0, Some(500.));
        assert_eq!(selected("sum 0\ncount 0", &selection).0, None);
        assert_eq!(selected("sum 1", &selection).0, None);
    }
}
