use std::path::Path;
use std::time::Duration;

mod provider_worker;
use provider_worker::{Call, Event, Worker};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use unibus::client::{Client, ClientConfig};
use unibus_mcp::client_setup::{self, Client as SetupClient};
use unibus_mcp::http::{McpClient, ProviderConfig};
use unibus_mcp::server::{BridgeConfig, DiscoveryConfig, serve};
use unibus_mcp::{
    AuthorizationVerifier, Claim, INVOKE_KIND, Invocation, InvocationLedger, InvocationOutcome,
    MCP_SCHEMA_VERSION, ProviderAdvertisement, ProviderRegistry, RESULT_KIND, ToolResult,
    catalog_digest, decode_message, require_tool_authorization,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    peer_acl: Option<unibus_mcp::peer_acl::PeerAclConfig>,
    #[serde(default)]
    authorization_policy_file: Option<std::path::PathBuf>,
    #[serde(default)]
    lan_permissive_mutations: bool,
    #[serde(default)]
    unibus: Option<ClientConfig>,
    #[serde(default = "default_lease_ms")]
    lease_ms: u64,
    #[serde(default)]
    providers: Vec<ProviderConfig>,
    #[serde(default)]
    bridge: Option<BridgeConfig>,
    #[serde(default)]
    discovery: Option<DiscoveryConfig>,
}

fn default_lease_ms() -> u64 {
    30_000
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().context(
        "usage: unibus-mcp inspect|publish|serve <config.json> | setup-clients [options]",
    )?;
    if command == "setup-clients" {
        return setup_clients(args.collect());
    }
    ensure!(
        matches!(command.as_str(), "inspect" | "publish" | "serve"),
        "command must be `inspect`, `publish`, `serve`, or `setup-clients`"
    );
    let path = args
        .next()
        .with_context(|| format!("{command} needs <config.json>"))?;
    ensure!(args.next().is_none(), "unexpected argument");
    let config: Config = serde_json::from_slice(
        &std::fs::read(Path::new(&path)).with_context(|| format!("read {path}"))?,
    )?;
    ensure!(
        (5_000..=unibus_mcp::MAX_PROVIDER_LEASE_MS).contains(&config.lease_ms),
        "lease_ms must be 5000..={} ms",
        unibus_mcp::MAX_PROVIDER_LEASE_MS
    );
    match command.as_str() {
        "publish" => {
            ensure!(!config.providers.is_empty(), "no MCP providers configured");
            publish(config)
        }
        "serve" => {
            let bus = config
                .unibus
                .context("serve requires `unibus` client configuration")?;
            let bridge = config
                .bridge
                .context("serve requires `bridge` configuration")?;
            serve(bus, bridge, config.discovery)
        }
        _ => {
            ensure!(!config.providers.is_empty(), "no MCP providers configured");
            println!("{}", serde_json::to_string_pretty(&inspect(&config)?)?);
            Ok(())
        }
    }
}

fn setup_clients(arguments: Vec<String>) -> Result<()> {
    let mut endpoint = "http://127.0.0.1:18791/mcp".to_owned();
    let mut selected = Vec::new();
    let mut write = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--write" => write = true,
            "--endpoint" => {
                index += 1;
                endpoint = arguments
                    .get(index)
                    .context("--endpoint needs a URL")?
                    .clone();
            }
            "--client" => {
                index += 1;
                selected.push(match arguments.get(index).map(String::as_str) {
                    Some("codex") => SetupClient::Codex,
                    Some("claude") | Some("claude-code") => SetupClient::ClaudeCode,
                    Some("opencode") => SetupClient::OpenCode,
                    Some(value) => anyhow::bail!("unsupported MCP client {value}"),
                    None => anyhow::bail!("--client needs codex, claude, or opencode"),
                });
            }
            other => anyhow::bail!("unexpected setup-clients argument {other}"),
        }
        index += 1;
    }
    if selected.is_empty() {
        selected.extend([
            SetupClient::Codex,
            SetupClient::ClaudeCode,
            SetupClient::OpenCode,
        ]);
    }
    let actions = client_setup::plan(&endpoint, &selected)?;
    println!("{}", serde_json::to_string_pretty(&actions)?);
    if write {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        client_setup::apply(&actions, Path::new(&home))?;
    } else {
        eprintln!("preview only; pass --write to apply");
    }
    Ok(())
}

fn inspect(config: &Config) -> Result<Vec<ProviderAdvertisement>> {
    let now = unibus::envelope::now_ms();
    let mut output = Vec::new();
    for provider in &config.providers {
        let mut client = McpClient::connect(provider)
            .with_context(|| format!("initialize {}", provider.provider_id))?;
        let tools = client
            .list_tools(&provider.mutation_overrides)
            .with_context(|| format!("list tools for {}", provider.provider_id))?;
        let digest = catalog_digest(&tools)?;
        let advertisement = ProviderAdvertisement::new(
            provider.provider_id.clone(),
            provider.node_id.clone(),
            provider.producer_id.clone(),
            format!("inspect-{now}"),
            now,
            now + config.lease_ms,
            tools,
        )?;
        debug_assert_eq!(digest, advertisement.catalog_digest);
        output.push(advertisement);
    }
    Ok(output)
}

fn publish(config: Config) -> Result<()> {
    if let Some(acl) = &config.peer_acl {
        acl.validate_feature()?;
    }
    let unibus = config
        .unibus
        .clone()
        .context("publish requires `unibus` client configuration")?;
    ensure!(
        unibus
            .capabilities
            .iter()
            .any(|value| value == unibus_mcp::PROVIDER_CAPABILITY),
        "publisher must declare mcp.provider capability"
    );
    let bus = Client::connect(unibus)?;
    let verifier = std::sync::Arc::new(unibus_mcp::authorization::PolicyVerifier::configured(
        config.authorization_policy_file.clone(),
        config.lan_permissive_mutations,
    )?);
    let peer_acl = std::sync::Arc::new(config.peer_acl);
    let refresh = Duration::from_millis(config.lease_ms / 2);
    let (events, received) = std::sync::mpsc::sync_channel(128);
    let mut providers = config
        .providers
        .iter()
        .enumerate()
        .map(|(index, provider)| {
            let verifier = verifier.clone();
            let acl = peer_acl.clone();
            let destination = provider.clone();
            let authorize: provider_worker::Authorizer = std::sync::Arc::new(move |call, tool| {
                let now = unibus::envelope::now_ms();
                if let Some(acl) = acl.as_ref() {
                    acl.authorize(&call.producer, &destination, now / 1000)?;
                }
                verifier
                    .verify_invocation(&call.invocation, tool, now)
                    .map(|_| ())
            });
            Worker::start(index, provider.clone(), refresh, events.clone(), authorize)
        })
        .collect::<Result<Vec<_>>>()?;
    drop(events);
    let mut registry = ProviderRegistry::default();
    let mut ledger = InvocationLedger::new(1024)?;
    let mut generation = 0_u64;
    loop {
        for _ in 0..64 {
            let event = match received.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("all MCP provider workers stopped")
                }
            };
            match event {
                Event::Catalog {
                    index,
                    tools,
                    observed_at,
                } => {
                    let worker = &mut providers[index];
                    let provider = &worker.config;
                    let now = unibus::envelope::now_ms();
                    // Never renew a delayed observation as if it were fresh.
                    if observed_at.saturating_add(config.lease_ms) <= now {
                        continue;
                    }
                    let advertisement = ProviderAdvertisement::new(
                        provider.provider_id.clone(),
                        provider.node_id.clone(),
                        provider.producer_id.clone(),
                        format!("{}-{generation}", provider.provider_id),
                        observed_at,
                        observed_at.saturating_add(config.lease_ms),
                        tools,
                    )?;
                    registry.upsert(advertisement.clone(), now)?;
                    let envelope = bus.envelope(None, advertisement.into_message()?)?;
                    bus.send(&envelope, &[])?;
                    if !worker.ready {
                        eprintln!("MCP provider {} ready", provider.provider_id);
                    }
                    worker.ready = true;
                    generation = generation.wrapping_add(1);
                }
                Event::Failed {
                    index,
                    error,
                    retry,
                } => {
                    providers[index].ready = false;
                    eprintln!(
                        "MCP provider {} unavailable: {}; retry in {}s",
                        providers[index].config.provider_id,
                        error,
                        retry.as_secs()
                    );
                }
                Event::Completed {
                    invocation,
                    producer,
                    result,
                } => {
                    ledger.complete(&invocation.invocation_id);
                    send_result(&bus, &invocation, &producer, result)?;
                }
            }
        }
        if let Some((envelope, _)) = bus.try_read()? {
            if envelope.message.kind == INVOKE_KIND {
                let invocation: Invocation = decode_message(&envelope.message, INVOKE_KIND)?;
                let result = execute_invocation(
                    &mut providers,
                    &registry,
                    &*verifier,
                    &mut ledger,
                    &invocation,
                    peer_acl.as_ref().as_ref(),
                    &envelope.producer_id,
                );
                if let Err(error) = result {
                    send_result(&bus, &invocation, &envelope.producer_id, Err(error))?;
                }
            }
        }
        ensure!(bus.is_healthy(), "Unibus connection is unhealthy");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn send_result(
    bus: &Client,
    invocation: &Invocation,
    producer: &str,
    result: Result<serde_json::Value>,
) -> Result<()> {
    let reply = ToolResult {
        schema_version: MCP_SCHEMA_VERSION,
        invocation_id: invocation.invocation_id.clone(),
        outcome: match result {
            Ok(result) => InvocationOutcome::Success { result },
            Err(error) => InvocationOutcome::Error {
                code: "invocation_failed".into(),
                message: format!("{error:#}"),
                data: None,
            },
        },
    };
    reply.validate()?;
    let message = unibus::Message::new(RESULT_KIND, serde_json::to_value(reply)?);
    bus.send(&bus.envelope(Some(producer), message)?, &[])?;
    Ok(())
}

fn execute_invocation(
    providers: &mut [Worker],
    registry: &ProviderRegistry,
    verifier: &unibus_mcp::authorization::PolicyVerifier,
    ledger: &mut InvocationLedger,
    invocation: &Invocation,
    peer_acl: Option<&unibus_mcp::peer_acl::PeerAclConfig>,
    authenticated_producer: &str,
) -> Result<()> {
    if let Some(acl) = peer_acl {
        let provider = providers
            .iter()
            .find(|worker| worker.config.provider_id == invocation.provider_id)
            .context("provider is not configured")?;
        acl.authorize(
            authenticated_producer,
            &provider.config,
            unibus::envelope::now_ms() / 1000,
        )?;
    }
    require_tool_authorization(verifier, registry, invocation, unibus::envelope::now_ms())?;
    let worker = providers
        .iter()
        .find(|worker| worker.config.provider_id == invocation.provider_id)
        .context("provider is not configured")?;
    ensure!(worker.ready, "provider is unavailable");
    ensure!(
        ledger.claim(invocation)? == Claim::New,
        "duplicate invocation"
    );
    let result = worker.submit(Call {
        invocation: invocation.clone(),
        producer: authenticated_producer.to_owned(),
    });
    if result.is_err() {
        ledger.complete(&invocation.invocation_id);
    }
    result
}
