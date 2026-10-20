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
    synchronize(config)
}

fn synchronize(config: Config) -> Result<()> {
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
    let grant_observed = grants.iter().any(|record| {
        serde_json::to_value(&record.statement)
            .is_ok_and(|statement| statement.get("grant_id") == expected.get("grant_id"))
    });
    let revocations: Vec<AccessRecord> = serde_json::from_value(
        view.get("revocations")
            .context("missing revocations")?
            .clone(),
    )?;
    let next = reconcile(&current, &grants, &revocations, &config.issuers)?;
    let bytes = serde_json::to_vec_pretty(&next)?;
    ensure!(bytes.len() <= 65536, "retained trust exceeds 64 KiB");
    if serde_json::to_value(&current)? == serde_json::to_value(&next)? {
        ensure!(
            grant_observed,
            "expected service grant absent from local gossip"
        );
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
    // A partial gossip view must not delay an independently verified revocation.
    // Persist it first, then report the missing grant so the operator still sees
    // the degraded carrier. Absence never extends the retained grant's lifetime.
    ensure!(
        grant_observed,
        "expected service grant absent from local gossip; verified revocations retained"
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
    };
    use unibus_iroh::grant::{GrantVerifier, permit_scope};

    fn fixture() -> (tempfile::TempDir, Config, TrustDocument, SigningKey) {
        let directory = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[71; 32]);
        let permit = serde_json::from_value(json!({
            "audience":"private-services", "installation":"fleet",
            "initiator":{"producer_id":"a", "key_binding":format!("iroh:{}", "1".repeat(64)), "candidates":[]},
            "responder":{"producer_id":"b", "key_binding":format!("iroh:{}", "2".repeat(64)), "candidates":[]},
            "kinds":["unibus.transport.udp.v1"], "max_payload_bytes":65507
        })).unwrap();
        let grant = AccessRecord::sign(&key, serde_json::from_value(json!({
            "kind":"grant", "grant_id":"test-service", "principal":"test-client", "serial":1,
            "roles":[], "scopes":[permit_scope(&permit).unwrap()], "unix_users":[], "ssh_public_keys":[],
            "not_before":100, "not_after":1000
        })).unwrap()).unwrap();
        let config = Config {
            socket: directory.path().join("authority.sock"),
            trust_file: directory.path().join("trust.json"),
            issuers: BTreeSet::from([grant.signer.clone()]),
        };
        let current = TrustDocument {
            grant,
            revocations: vec![],
            permit,
        };
        std::fs::write(&config.trust_file, serde_json::to_vec(&current).unwrap()).unwrap();
        (directory, config, current, key)
    }

    fn authority(socket: &Path, view: serde_json::Value) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
                "access_list"
            );
            writeln!(stream, "{}", json!({"ok":true,"result":view})).unwrap();
        })
    }

    #[test]
    fn missing_grant_still_persists_revocation_and_reports_degraded_view() {
        let (_directory, config, current, key) = fixture();
        let revocation = AccessRecord::sign(&key, serde_json::from_value(json!({
            "kind":"revoke", "revocation_id":"stop", "grant_id":"test-service", "revoked_at":150, "reason":"test"
        })).unwrap()).unwrap();
        let server = authority(
            &config.socket,
            json!({"grants":[],"revocations":[revocation]}),
        );
        let path = config.trust_file.clone();
        let issuers = config.issuers.clone();
        let error = synchronize(config).unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("verified revocations retained"));
        let retained: TrustDocument = serde_json::from_slice(&read(&path).unwrap()).unwrap();
        assert_eq!(retained.grant, current.grant);
        assert_eq!(retained.revocations, vec![revocation]);
        assert!(
            GrantVerifier::new(path, issuers)
                .unwrap()
                .assertion(200)
                .is_err()
        );
    }

    #[test]
    fn empty_view_preserves_retained_document_and_reports_failure() {
        let (_directory, config, _current, _key) = fixture();
        let before = read(&config.trust_file).unwrap();
        let path = config.trust_file.clone();
        let server = authority(&config.socket, json!({"grants":[],"revocations":[]}));
        assert!(synchronize(config).is_err());
        server.join().unwrap();
        assert_eq!(read(&path).unwrap(), before);
    }

    #[test]
    fn observed_renewal_updates_retained_grant() {
        let (_directory, config, current, key) = fixture();
        let mut statement = serde_json::to_value(&current.grant.statement).unwrap();
        statement["serial"] = 2.into();
        statement["not_after"] = 2000.into();
        let renewed = AccessRecord::sign(&key, serde_json::from_value(statement).unwrap()).unwrap();
        let server = authority(
            &config.socket,
            json!({"grants":[{"record":renewed}],"revocations":[]}),
        );
        let path = config.trust_file.clone();
        synchronize(config).unwrap();
        server.join().unwrap();
        let retained: TrustDocument = serde_json::from_slice(&read(&path).unwrap()).unwrap();
        assert_eq!(retained.grant, renewed);
    }
}
