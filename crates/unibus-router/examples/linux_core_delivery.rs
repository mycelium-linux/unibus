//! Real installed-service delivery proof, confined to a private fungOS QEMU clone.
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    fs,
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use unibus::{
    envelope::{Envelope, Message, SCHEMA_VERSION_V2},
    framing::{FrameError, read_frame, write_frame},
    kinds,
};

const UNIT: &str = "unibus-router.service";
const CONFIG: &str = "/etc/unibus/hub.json";
const DROPIN: &str = "/etc/systemd/system/unibus-router.service.d/90-fungos-delivery-proof.conf";
const PRIVATE: &str = "/run/fungos-unibus-delivery-proof";
const KIND: &str = "fungos.qa.delivery";

fn command(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output()?;
    ensure!(
        out.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
fn guard(uid: &str, product: &str, virtualization: &str) -> Result<()> {
    ensure!(
        uid == "0"
            && product == "fungos-mycelium-package-validation"
            && matches!(virtualization, "kvm" | "qemu"),
        "private disposable package-proof QEMU guest required"
    );
    Ok(())
}
fn hash(path: &str) -> Result<String> {
    command("sha256sum", &[path])?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("digest missing")
}
fn token() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).context("private token generation")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
fn ready() -> Result<()> {
    let until = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect("127.0.0.1:18790").is_err() {
        ensure!(
            Instant::now() < until,
            "packaged router did not become ready"
        );
        thread::sleep(Duration::from_millis(100));
    }
    ensure!(
        command("systemctl", &["is-active", UNIT])? == "active",
        "router not active"
    );
    let pid = command(
        "systemctl",
        &["show", UNIT, "--property=MainPID", "--value"],
    )?;
    ensure!(
        pid != "0" && pid.bytes().all(|c| c.is_ascii_digit()),
        "invalid service PID"
    );
    ensure!(
        fs::read_link(format!("/proc/{pid}/exe"))? == Path::new("/usr/bin/unibus-router"),
        "service does not execute owner-installed router"
    );
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let uid = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .context("router UID missing")?;
    ensure!(
        uid.split_whitespace().skip(1).all(|id| id != "0"),
        "router is privileged"
    );
    Ok(())
}
fn stream() -> Result<TcpStream> {
    let stream = TcpStream::connect("127.0.0.1:18790")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}
fn register(installation: &str, producer: &str, token: &str, capability: &str) -> Envelope {
    let mut envelope = Envelope::new(
        installation,
        producer,
        None,
        token,
        Message::new(
            kinds::V1_REGISTER,
            json!({"capabilities":[capability],"zones":["default"],"screen_anchors":[]}),
        ),
    );
    envelope.schema_version = SCHEMA_VERSION_V2;
    envelope
}
fn denied(envelope: &Envelope) -> Result<()> {
    let mut stream = stream()?;
    write_frame(&mut stream, envelope, &[])?;
    match read_frame(&mut stream) {
        Ok((reply, _)) => ensure!(
            reply.message.body["success"] == false,
            "unauthorized registration was not denied"
        ),
        Err(FrameError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        Err(error) => bail!("ambiguous denial result: {error}"),
    }
    Ok(())
}
fn accepted(envelope: &Envelope) -> Result<TcpStream> {
    let mut stream = stream()?;
    write_frame(&mut stream, envelope, &[])?;
    let (reply, _) = read_frame(&mut stream)?;
    ensure!(
        reply.message.body["success"] == true,
        "authorized registration rejected"
    );
    Ok(stream)
}
fn proof(expected: &[String]) -> Result<()> {
    ensure!(
        expected.len() == 2
            && expected.iter().all(|s| s.len() == 64
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))),
        "usage: linux_core_delivery ROUTER_SHA256 CLI_SHA256"
    );
    guard(
        &command("id", &["-u"])?,
        fs::read_to_string("/sys/class/dmi/id/product_name")?.trim(),
        &command("systemd-detect-virt", &[])?,
    )?;
    ensure!(
        Path::new("/opt/fungos-mycelium-package-qa/proof").is_file(),
        "private package-proof marker missing"
    );
    for (path, expected) in [
        ("/usr/bin/unibus-router", &expected[0]),
        ("/usr/bin/unibusctl", &expected[1]),
    ] {
        ensure!(
            hash(path)? == *expected
                && command("dpkg-query", &["-S", path])? == format!("unibus-core: {path}"),
            "owner executable mismatch"
        );
    }
    ready()?;
    command("/usr/bin/unibusctl", &["doctor", "--json"])?;
    let original = fs::read(CONFIG)?;
    let baseline: serde_json::Value = serde_json::from_slice(&original)?;
    ensure!(
        baseline["nodes"] == json!([])
            && baseline["routes"] == json!([])
            && baseline["listen"] == "127.0.0.1:18790"
            && baseline["allow_insecure_lan"] == false,
        "not the fail-closed installed default"
    );
    ensure!(
        !Path::new(DROPIN).exists() && !Path::new(PRIVATE).exists(),
        "refusing reused private fixture state"
    );
    let producer_token = token()?;
    let consumer_token = token()?;
    let health_token = token()?;
    denied(&register(
        "local",
        "qa-producer",
        &producer_token,
        "qa-sender",
    ))?;
    println!("FUNGOS_UNIBUS_EMPTY_GRANTS_DENIED_VERIFIED");
    fs::create_dir(PRIVATE)?;
    fs::set_permissions(PRIVATE, fs::Permissions::from_mode(0o700))?;
    let result = (|| -> Result<()> {
        for (name, token) in [
            ("producer", &producer_token),
            ("consumer", &consumer_token),
            ("health", &health_token),
        ] {
            let path = Path::new(PRIVATE).join(name);
            fs::write(&path, token)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        fs::create_dir_all(Path::new(DROPIN).parent().context("drop-in parent")?)?;
        fs::write(
            DROPIN,
            format!(
                "[Service]\nLoadCredential=producer:{PRIVATE}/producer\nLoadCredential=consumer:{PRIVATE}/consumer\nLoadCredential=health:{PRIVATE}/health\nLoadCredential=health-client:{PRIVATE}/health-client\nEnvironment=UNIBUS_HEALTH_SOCKET=/run/unibus/health.sock\nEnvironment=UNIBUS_HEALTH_CLIENT=/run/credentials/unibus-router.service/health-client\n"
            ),
        )?;
        let installation = format!("qa-{}", fs::read_to_string("/etc/machine-id")?.trim());
        let client = json!({"installation_id":installation,"producer_id":"qa-health","hub_endpoint":"127.0.0.1:18790","token_file":"/run/credentials/unibus-router.service/health","zones":["health"],"schema_version":2,"read_timeout_ms":100});
        let client_path = Path::new(PRIVATE).join("health-client");
        fs::write(&client_path, serde_json::to_vec(&client)?)?;
        fs::set_permissions(&client_path, fs::Permissions::from_mode(0o600))?;
        let config = json!({"installation_id":installation,"listen":"127.0.0.1:18790","allow_insecure_lan":false,"routes":[{"kind":KIND,"target":{"mode":"capability","name":"qa-receiver"}}],"nodes":[{"producer_id":"qa-producer","token_file":"/run/credentials/unibus-router.service/producer","capabilities":["qa-sender"],"zones":["default"],"publishes":[KIND],"publish_zones":["default"]},{"producer_id":"qa-consumer","token_file":"/run/credentials/unibus-router.service/consumer","capabilities":["qa-receiver"],"zones":["default"]},{"producer_id":"qa-health","token_file":"/run/credentials/unibus-router.service/health","capabilities":["health.probe"],"zones":["health"]}]});
        fs::write(CONFIG, serde_json::to_vec_pretty(&config)?)?;
        command("systemctl", &["daemon-reload"])?;
        command("systemctl", &["restart", UNIT])?;
        ready()?;
        use std::io::{Read, Write};
        let mut health = std::os::unix::net::UnixStream::connect("/run/unibus/health.sock")?;
        health.set_read_timeout(Some(Duration::from_secs(2)))?;
        health.set_write_timeout(Some(Duration::from_secs(2)))?;
        health.write_all(b"health\n")?;
        let mut reply = [0; 13];
        health.read_exact(&mut reply)?;
        ensure!(
            &reply == b"READY unibus\n",
            "authenticated owner readiness failed"
        );
        println!("FUNGOS_UNIBUS_OWNER_READINESS_VERIFIED");
        denied(&register(
            &installation,
            "qa-producer",
            &consumer_token,
            "qa-sender",
        ))?;
        denied(&register(
            &installation,
            "qa-producer",
            &producer_token,
            "ungranted-capability",
        ))?;
        let mut consumer = accepted(&register(
            &installation,
            "qa-consumer",
            &consumer_token,
            "qa-receiver",
        ))?;
        let mut producer = accepted(&register(
            &installation,
            "qa-producer",
            &producer_token,
            "qa-sender",
        ))?;
        let mut message = Envelope::new(
            &installation,
            "qa-producer",
            Some("default"),
            &producer_token,
            Message::new(KIND, json!({"proof":"installed-owner-service"})),
        );
        message.schema_version = SCHEMA_VERSION_V2;
        write_frame(&mut producer, &message, &[])?;
        // Schema-2 peers may receive bounded lifecycle advisories before data.
        let mut received = None;
        for _ in 0..16 {
            let (envelope, bytes) = read_frame(&mut consumer)?;
            ensure!(
                envelope.token.is_empty(),
                "credentials escaped through a consumer frame"
            );
            if envelope.message.kind == KIND {
                received = Some((envelope, bytes));
                break;
            }
        }
        let (delivered, bytes) = received.context("no delivery within the bounded frame budget")?;
        ensure!(
            delivered.request_id == message.request_id
                && delivered.message.kind == KIND
                && delivered.message.body == message.message.body
                && delivered.token.is_empty()
                && bytes.is_empty(),
            "delivery correlation/payload or credential redaction failed"
        );
        println!("FUNGOS_UNIBUS_AUTHENTICATED_DELIVERY_VERIFIED");
        println!("FUNGOS_UNIBUS_WRONG_TOKEN_AND_CAPABILITY_DENIED_VERIFIED");
        Ok(())
    })();
    let restore = (|| -> Result<()> {
        fs::write(CONFIG, &original)?;
        if Path::new(DROPIN).exists() {
            fs::remove_file(DROPIN)?;
        }
        command("systemctl", &["daemon-reload"])?;
        command("systemctl", &["restart", UNIT])?;
        ready()?;
        for name in ["producer", "consumer", "health", "health-client"] {
            let path = Path::new(PRIVATE).join(name);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        fs::remove_dir(PRIVATE)?;
        ensure!(fs::read(CONFIG)? == original, "original conffile changed");
        ensure!(
            !Path::new("/run/unibus/health.sock").exists(),
            "fixture readiness listener survived restoration"
        );
        denied(&register(
            "local",
            "qa-producer",
            &producer_token,
            "qa-sender",
        ))?;
        println!("FUNGOS_UNIBUS_DEFAULT_POLICY_RESTORED_VERIFIED");
        Ok(())
    })();
    match (result, restore) {
        (Err(a), Err(b)) => bail!("delivery proof failed: {a}; restoration also failed: {b}"),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}
fn main() -> Result<()> {
    proof(&std::env::args().skip(1).collect::<Vec<_>>())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_hosts_and_wrong_guests() {
        assert!(guard("1000", "fungos-mycelium-package-validation", "kvm").is_err());
        assert!(guard("0", "production", "kvm").is_err());
        assert!(guard("0", "fungos-mycelium-package-validation", "none").is_err());
        assert!(guard("0", "fungos-mycelium-package-validation", "kvm").is_ok());
    }
    #[test]
    fn tokens_are_fresh_and_do_not_escape_registration() {
        let a = token().unwrap();
        let b = token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        let envelope = register("private", "qa-producer", &a, "qa-sender");
        assert_eq!(envelope.schema_version, SCHEMA_VERSION_V2);
        assert_eq!(envelope.token, a);
    }
}
