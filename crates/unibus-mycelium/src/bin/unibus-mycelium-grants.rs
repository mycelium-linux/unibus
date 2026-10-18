//! One-shot adapter: signed Mycelium access view -> consumer-owned trust cache.
//! Run under a protected service account; never expose this as a remote write API.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use unibus_iroh::grant::{AccessRecord, TrustDocument, reconcile};
use unibus_mycelium::access::MyceliumAccessAuthority;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    socket: PathBuf,
    trust_file: PathBuf,
    issuers: BTreeSet<String>,
}
fn read(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "document exceeds 64 KiB");
    Ok(bytes)
}
fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: unibus-mycelium-grants CONFIG.json")?;
    let config: Config = serde_json::from_slice(&read(Path::new(&path))?)?;
    ensure!(
        !config.issuers.is_empty() && config.issuers.len() <= 16,
        "explicit issuers required"
    );
    let old = read(&config.trust_file)?;
    let current: TrustDocument = serde_json::from_slice(&old)?;
    let view = MyceliumAccessAuthority::new(config.socket).access_snapshot()?;
    let grants: Vec<AccessRecord> = view
        .get("grants")
        .and_then(|v| v.as_array())
        .context("missing grants")?
        .iter()
        .map(|v| serde_json::from_value(v["record"].clone()))
        .collect::<std::result::Result<_, _>>()?;
    let expected = serde_json::to_value(&current.grant.statement)?;
    ensure!(
        grants
            .iter()
            .any(|record| serde_json::to_value(&record.statement)
                .is_ok_and(|statement| statement.get("grant_id") == expected.get("grant_id"))),
        "expected service grant absent from local gossip; retained trust unchanged"
    );
    let revocations: Vec<AccessRecord> = serde_json::from_value(
        view.get("revocations")
            .context("missing revocations")?
            .clone(),
    )?;
    let next = reconcile(&current, &grants, &revocations, &config.issuers)?;
    let bytes = serde_json::to_vec_pretty(&next)?;
    ensure!(bytes.len() <= 65536, "retained trust exceeds 64 KiB");
    if serde_json::to_value(&current)? == serde_json::to_value(&next)? {
        println!("retained service grant unchanged");
        return Ok(());
    }
    let temporary = config
        .trust_file
        .with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options.open(&temporary)?;
    let result = file
        .write_all(&bytes)
        .and_then(|_| file.sync_all())
        .and_then(|_| std::fs::rename(&temporary, &config.trust_file));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    println!("retained signed service grant and revocations updated");
    Ok(())
}
