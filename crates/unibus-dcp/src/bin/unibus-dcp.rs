use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use unibus::client::{Client, ClientConfig};
use unibus_dcp::{
    BINDING_SCHEMA_VERSION, CatalogAdvertisement, EXECUTE_KIND, Execution, ExecutionReceipt,
    MAX_PROVIDER_LEASE_MS, PROVIDER_CAPABILITY, ProviderRegistry, RECEIPT_KIND, decode_message,
    http::{DcpHttpClient, ProviderConfig},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    unibus: ClientConfig,
    #[serde(default = "default_lease_ms")]
    lease_ms: u64,
    providers: Vec<ProviderConfig>,
}

fn default_lease_ms() -> u64 {
    30_000
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .context("usage: unibus-dcp inspect|publish <config.json>")?;
    ensure!(
        matches!(command.as_str(), "inspect" | "publish"),
        "command must be `inspect` or `publish`"
    );
    let path = args
        .next()
        .with_context(|| format!("{command} needs <config.json>"))?;
    ensure!(args.next().is_none(), "unexpected argument");
    let config: Config = serde_json::from_slice(&std::fs::read(Path::new(&path))?)?;
    validate_config(&config)?;
    if command == "inspect" {
        println!("{}", serde_json::to_string_pretty(&inspect(&config, 0)?)?);
        return Ok(());
    }
    publish(config)
}

fn validate_config(config: &Config) -> Result<()> {
    ensure!(!config.providers.is_empty(), "no DCP providers configured");
    ensure!(
        (5_000..=MAX_PROVIDER_LEASE_MS).contains(&config.lease_ms),
        "invalid lease_ms"
    );
    ensure!(
        config
            .unibus
            .capabilities
            .iter()
            .any(|capability| capability == PROVIDER_CAPABILITY),
        "publisher must declare dcp.provider capability"
    );
    Ok(())
}

fn inspect(config: &Config, generation: u64) -> Result<Vec<CatalogAdvertisement>> {
    let now = unibus::envelope::now_ms();
    config
        .providers
        .iter()
        .map(|provider| inspect_provider(provider, generation, now, config.lease_ms))
        .collect()
}

fn inspect_provider(
    provider: &ProviderConfig,
    generation: u64,
    now: u64,
    lease_ms: u64,
) -> Result<CatalogAdvertisement> {
    let client = DcpHttpClient::connect(provider)?.with_timeout(Duration::from_secs(3))?;
    inspect_provider_with_client(provider, &client, generation, now, lease_ms)
}

fn inspect_provider_with_client(
    provider: &ProviderConfig,
    client: &DcpHttpClient,
    generation: u64,
    _started_at: u64,
    lease_ms: u64,
) -> Result<CatalogAdvertisement> {
    let discovery = client.discovery()?;
    let catalog = client.catalog_at(&discovery.endpoints.catalog)?;
    ensure!(
        discovery.provider.id == provider.provider_id,
        "configured provider_id differs from discovery"
    );
    // A slow fetch must not consume the lease before it is advertised.
    let now = unibus::envelope::now_ms();
    let advertisement = CatalogAdvertisement {
        binding_schema_version: BINDING_SCHEMA_VERSION,
        node_id: provider.node_id.clone(),
        producer_id: provider.producer_id.clone(),
        lease_id: format!("{}-{generation}", provider.provider_id),
        issued_at_unix_ms: now,
        expires_at_unix_ms: now + lease_ms,
        discovery,
        catalog,
    };
    advertisement.validate(now)?;
    Ok(advertisement)
}

/// One bounded fetch loop per provider. A stalled owner cannot stall the bus
/// or another owner's lease. The wake channel refreshes state after execution.
fn spawn_refresh_worker<T: Send + 'static>(
    provider_id: String,
    refresh: Duration,
    results: std::sync::mpsc::SyncSender<(String, T)>,
    mut fetch: impl FnMut() -> T + Send + 'static,
) -> Result<std::sync::mpsc::SyncSender<()>> {
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(format!("dcp-refresh-{provider_id}"))
        .spawn(move || {
            loop {
                let started = std::time::Instant::now();
                if results.send((provider_id.clone(), fetch())).is_err() {
                    break;
                }
                let wait = refresh.saturating_sub(started.elapsed());
                if let Err(std::sync::mpsc::RecvTimeoutError::Disconnected) =
                    wake_rx.recv_timeout(wait)
                {
                    break;
                }
            }
        })?;
    Ok(wake_tx)
}

fn publish(config: Config) -> Result<()> {
    let bus = Client::connect(config.unibus.clone())?;
    let refresh = Duration::from_millis(config.lease_ms / 2);
    let mut registry = ProviderRegistry::default();
    let clients = config
        .providers
        .iter()
        .map(|provider| {
            Ok((
                provider.provider_id.clone(),
                DcpHttpClient::connect(provider)?.with_timeout(Duration::from_secs(8))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let (catalog_tx, catalog_rx) = std::sync::mpsc::sync_channel(config.providers.len().max(1));
    let mut refreshers = std::collections::BTreeMap::new();
    for provider in &config.providers {
        let provider = provider.clone();
        let client = clients
            .iter()
            .find(|(id, _)| id == &provider.provider_id)
            .unwrap()
            .1
            .clone()
            .with_timeout(Duration::from_secs(3))?;
        let catalog_tx = catalog_tx.clone();
        let lease_ms = config.lease_ms;
        let mut generation = 0_u64;
        let provider_id = provider.provider_id.clone();
        let refresh_tx =
            spawn_refresh_worker(provider_id.clone(), refresh, catalog_tx, move || {
                let result = inspect_provider_with_client(
                    &provider,
                    &client,
                    generation,
                    unibus::envelope::now_ms(),
                    lease_ms,
                );
                generation = generation.wrapping_add(1);
                result
            })?;
        refreshers.insert(provider_id, refresh_tx);
    }
    drop(catalog_tx);
    loop {
        // Fetches run independently; the bus always remains available for receipts.
        while let Ok((provider_id, result)) = catalog_rx.try_recv() {
            match result {
                Ok(advertisement) => {
                    registry.upsert(advertisement.clone(), unibus::envelope::now_ms())?;
                    bus.send(&bus.envelope(None, advertisement.into_message()?)?, &[])?;
                }
                Err(error) => {
                    eprintln!("DCP provider {provider_id} catalog refresh failed: {error:#}")
                }
            }
        }
        if let Some((envelope, _)) = bus.try_read()? {
            if envelope.message.kind == EXECUTE_KIND {
                let dispatched = (|| -> Result<()> {
                    let execution: Execution = decode_message(&envelope.message, EXECUTE_KIND)?;
                    let now = unibus::envelope::now_ms();
                    registry.resolve_execution(&execution, now)?;
                    let execute_path = registry
                        .provider(&execution.provider_id, now)
                        .and_then(|provider| provider.discovery.endpoints.execute.as_deref())
                        .context("DCP provider does not advertise execution")?
                        .to_owned();
                    let (_, client) = clients
                        .iter()
                        .find(|(provider_id, _)| provider_id == &execution.provider_id)
                        .context("DCP provider is not configured")?;
                    let receipt = client.execute_at(&execute_path, &execution.request)?;
                    if let Some(refresh) = refreshers.get(&execution.provider_id) {
                        let _ = refresh.try_send(());
                    }
                    let result = ExecutionReceipt {
                        binding_schema_version: BINDING_SCHEMA_VERSION,
                        provider_id: execution.provider_id,
                        receipt,
                    };
                    result.validate()?;
                    bus.send(
                        &bus.envelope(
                            Some(&envelope.producer_id),
                            unibus::Message::new(RECEIPT_KIND, serde_json::to_value(result)?),
                        )?,
                        &[],
                    )?;
                    Ok(())
                })();
                if let Err(error) = dispatched {
                    // A stale or unavailable target does not tear down other
                    // providers' leases. No success receipt is fabricated.
                    eprintln!("DCP execution rejected or failed: {error:#}");
                }
            }
        }
        ensure!(bus.is_healthy(), "Unibus connection is unhealthy");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn slow_owner_does_not_hold_fast_catalog_or_execution_refresh() -> Result<()> {
        let (results_tx, results_rx) = mpsc::sync_channel(4);
        let (release_tx, release_rx) = mpsc::channel();
        let slow = spawn_refresh_worker(
            "slow".into(),
            Duration::from_secs(60),
            results_tx.clone(),
            move || {
                release_rx.recv().unwrap();
                0
            },
        )?;
        let mut generation = 0;
        let fast = spawn_refresh_worker(
            "fast".into(),
            Duration::from_secs(60),
            results_tx,
            move || {
                generation += 1;
                generation
            },
        )?;
        assert_eq!(
            results_rx.recv_timeout(Duration::from_secs(5))?,
            ("fast".into(), 1)
        );
        fast.send(())?;
        assert_eq!(
            results_rx.recv_timeout(Duration::from_secs(5))?,
            ("fast".into(), 2)
        );
        drop(fast);
        drop(slow);
        release_tx.send(())?;
        assert_eq!(
            results_rx.recv_timeout(Duration::from_secs(5))?,
            ("slow".into(), 0)
        );
        Ok(())
    }
}
