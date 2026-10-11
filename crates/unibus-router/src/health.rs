//! Opt-in, owner-local readiness. Uses the normal client registration path;
//! no management-provider dependency or unauthenticated network health port.
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use unibus::client::{Client, ClientConfig};

pub fn start_from_env() -> Result<()> {
    let Some(path) = std::env::var_os("UNIBUS_HEALTH_SOCKET") else {
        ensure!(
            std::env::var_os("UNIBUS_HEALTH_CLIENT").is_none(),
            "UNIBUS_HEALTH_CLIENT requires UNIBUS_HEALTH_SOCKET"
        );
        return Ok(());
    };
    let config_path = std::env::var_os("UNIBUS_HEALTH_CLIENT")
        .context("UNIBUS_HEALTH_SOCKET requires UNIBUS_HEALTH_CLIENT")?;
    let mut bytes = Vec::new();
    std::fs::File::open(config_path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "health config exceeds 1 MiB");
    let config: ClientConfig = serde_json::from_slice(&bytes)?;
    validate(&config)?;
    let path = Path::new(&path);
    ensure!(path.is_absolute(), "health socket must be absolute");
    let parent = std::fs::metadata(path.parent().context("health socket needs parent")?)?;
    ensure!(
        parent.is_dir() && parent.mode() & 0o077 == 0,
        "health socket needs an owner-only directory"
    );
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket() && metadata.uid() == parent.uid(),
                "unsafe existing health socket"
            );
            match UnixStream::connect(path) {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?
                }
                _ => anyhow::bail!("health socket is active or cannot be safely replaced"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    std::thread::Builder::new()
        .name("unibus-health".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                if let Err(error) = handle(&mut stream, &config) {
                    tracing::debug!("local readiness check failed: {error:#}");
                }
            }
        })?;
    Ok(())
}

fn validate(config: &ClientConfig) -> Result<()> {
    let address: std::net::SocketAddr = config.hub_endpoint.parse()?;
    ensure!(
        address.ip().is_loopback(),
        "health client must use loopback"
    );
    ensure!(
        config.capabilities.is_empty() && config.screen_anchors.is_empty(),
        "health client must not claim application capabilities"
    );
    ensure!(
        config.schema_version == 2,
        "health client requires fail-closed schema 2"
    );
    ensure!(
        (1..=100).contains(&config.read_timeout_ms),
        "health client read timeout must be 1..=100ms"
    );
    Ok(())
}

fn handle(stream: &mut UnixStream, config: &ClientConfig) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
    let mut request = [0; 7];
    stream.read_exact(&mut request)?;
    ensure!(&request == b"health\n", "unknown health request");
    // Registration acknowledges transport, parsing, authentication and grants.
    // Dropping this read-only client leaves no publishing authority behind.
    let client = Client::connect(config.clone())?;
    ensure!(client.is_healthy(), "health registration is unhealthy");
    stream.write_all(b"READY unibus\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_client_is_local_and_unprivileged() {
        let mut config: ClientConfig = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "producer_id":"health", "hub_endpoint":"127.0.0.1:1234",
            "token_file":"token", "zones":["health"], "schema_version":2, "read_timeout_ms":100
        }))
        .unwrap();
        assert!(validate(&config).is_ok());
        config.capabilities.push("display".into());
        assert!(validate(&config).is_err());
        config.capabilities.clear();
        config.schema_version = 1;
        assert!(validate(&config).is_err());
        config.schema_version = 2;
        config.read_timeout_ms = 101;
        assert!(validate(&config).is_err());
        config.read_timeout_ms = 100;
        config.screen_anchors.push("ungranted-screen".into());
        assert!(validate(&config).is_err());
        config.screen_anchors.clear();
        config.hub_endpoint = "192.0.2.1:1234".into();
        assert!(validate(&config).is_err());
    }
    #[test]
    fn readiness_requires_successful_authenticated_registration() {
        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("token");
        crate_token(&token_path);
        let hub_config = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "listen":"127.0.0.1:0",
            "nodes":[{"producer_id":"health", "token_file":token_path, "zones":["health"], "capabilities":["health.probe"]}]
        })).unwrap();
        let hub = std::sync::Arc::new(unibus_router::Hub::new(hub_config).unwrap());
        let address = hub.start().unwrap();
        let config: ClientConfig = serde_json::from_value(serde_json::json!({
            "installation_id":"test", "producer_id":"health", "hub_endpoint":address.to_string(),
            "token_file":token_path, "zones":["health"], "schema_version":2, "read_timeout_ms":100
        }))
        .unwrap();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_all(b"health\n").unwrap();
        handle(&mut server, &config).unwrap();
        let mut reply = [0; 13];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"READY unibus\n");
        let mut wrong = config.clone();
        wrong.producer_id = "ungranted".into();
        client.write_all(b"health\n").unwrap();
        assert!(handle(&mut server, &wrong).is_err());
    }
    fn crate_token(path: &Path) {
        unibus_router::auth::issue_token(path, &unibus_router::auth::generate_token().unwrap())
            .unwrap();
    }
}
