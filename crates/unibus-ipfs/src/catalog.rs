//! Durable observations and names. Kubo owns blocks, pins and peer transport.
use crate::Kubo;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_ENTRIES: usize = 20_000;
const MAX_SNAPSHOT: u64 = 32 * 1024 * 1024;
const WEBUI: &str = "bafybeiciqeyipumpmhxzlxnbqdbbv6u5uij4hy4wax64dmj7kvrhusiq6y";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub name: String,
    pub rpc: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub access_token_file: PathBuf,
    pub nodes: Vec<Node>,
    pub gateway: String,
    pub public_url: String,
    #[serde(default)]
    pub uploads: Option<crate::uploads::Config>,
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.listen.ip().is_loopback(),
            "library must bind loopback behind an authenticated gateway"
        );
        ensure!(
            self.state_dir.is_absolute() && self.access_token_file.is_absolute(),
            "state and token paths must be absolute"
        );
        ensure!(
            !self.nodes.is_empty() && self.nodes.len() <= 8,
            "configure one to eight nodes"
        );
        let mut names = std::collections::BTreeSet::new();
        for node in &self.nodes {
            ensure!(
                !node.name.is_empty()
                    && node.name.len() <= 48
                    && node
                        .name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "invalid node name"
            );
            ensure!(names.insert(&node.name), "duplicate node name");
            Kubo::new(&node.rpc)?;
        }
        let public = reqwest::Url::parse(&self.public_url)?;
        let gateway = reqwest::Url::parse(&self.gateway)?;
        for url in [&public, &gateway] {
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path() == "/",
                "expected bare HTTPS origin"
            );
        }
        ensure!(
            public.origin() != gateway.origin(),
            "content must use a separate origin"
        );
        if let Some(upload) = &self.uploads {
            upload.validate(self)?;
        }
        Ok(())
    }
    pub fn snapshot(&self) -> PathBuf {
        self.state_dir.join("catalog.json")
    }
    pub fn labels(&self) -> PathBuf {
        self.state_dir.join("labels.json")
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Observation {
    pub last_attempt: u64,
    pub last_success: Option<u64>,
    pub online: bool,
    pub pin_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub cid: String,
    pub names: Vec<String>,
    pub name_source: String,
    pub kind: String,
    pub mime: String,
    pub size: Option<u64>,
    pub nodes: Vec<String>,
    pub text: String,
    pub inspected: bool,
    pub text_checked: bool,
    pub inspection_error: Option<String>,
    #[serde(default)]
    pub metadata_schema: Option<String>,
    #[serde(default)]
    pub source_visibility: Option<String>,
}

impl Entry {
    pub fn display_name(&self) -> String {
        self.names
            .iter()
            .find(|name| {
                !matches!(
                    name.to_lowercase().as_str(),
                    "coarse" | "medium" | "fine" | "model" | "source" | "preview" | "full"
                )
            })
            .or_else(|| self.names.first())
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "{} · {}",
                    if self.kind == "directory" {
                        "Folder"
                    } else if self.mime == "application/pdf" {
                        "PDF document"
                    } else if self.mime == "model/gltf-binary" {
                        "3D model"
                    } else {
                        "Unnamed file"
                    },
                    &self.cid[..self.cid.len().min(16)]
                )
            })
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    pub version: u32,
    pub updated_at: u64,
    pub nodes: BTreeMap<String, Observation>,
    pub entries: BTreeMap<String, Entry>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs()
}

pub fn load<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::File::open(path) {
        Ok(file) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(MAX_SNAPSHOT + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() as u64 <= MAX_SNAPSHOT, "state file too large");
            Ok(serde_json::from_slice(&bytes)
                .context("invalid library state; restore a valid snapshot")?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= MAX_SNAPSHOT,
        "state exceeds size limit"
    );
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn view(config: &Config) -> Result<Snapshot> {
    let mut snapshot: Snapshot = load(&config.snapshot())?;
    let labels: BTreeMap<String, String> = load(&config.labels())?;
    for (cid, name) in labels {
        if let Some(entry) = snapshot.entries.get_mut(&cid) {
            entry.names.insert(0, name);
            entry.names.dedup();
            entry.name_source = "operator".into();
        }
    }
    Ok(snapshot)
}

pub fn label(config: &Config, cid: &str, name: &str) -> Result<()> {
    validate_cid(cid)?;
    let name = clean_name(name)
        .context("name must contain readable text and be at most 240 characters")?;
    let snapshot: Snapshot = load(&config.snapshot())?;
    ensure!(
        snapshot.entries.contains_key(cid),
        "CID is not in the catalog"
    );
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.state_dir.join("labels.lock"))?;
    lock.lock()?;
    let mut labels: BTreeMap<String, String> = load(&config.labels())?;
    labels.insert(cid.into(), name);
    save(&config.labels(), &labels)
}

pub fn validate_cid(value: &str) -> Result<()> {
    ensure!(value.len() <= 128, "CID too long");
    value.parse::<cid::Cid>()?;
    Ok(())
}

pub fn clean_name(value: &str) -> Option<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let value = value
        .trim_matches(|c: char| c.is_control() || c == '/' || c == '\\')
        .to_owned();
    (value.chars().count() >= 3
        && value.chars().count() <= 240
        && value.chars().any(char::is_alphabetic)
        && !value.contains('\u{fffd}'))
    .then_some(value)
}

fn add_name(entry: &mut Entry, name: &str, source: &str) {
    if let Some(name) = clean_name(name) {
        if !entry.names.contains(&name) && entry.names.len() < 12 {
            if source == "directory_entry" || source == "pin_name" {
                entry.names.insert(0, name);
            } else {
                entry.names.push(name);
            }
            entry.name_source = source.into();
        }
    }
}

/// Apply only complete node inventories. Failure retains the previous observations.
pub fn apply_inventory(
    snapshot: &mut Snapshot,
    node: &str,
    pins: Result<BTreeMap<String, String>>,
    timestamp: u64,
) {
    let state = snapshot.nodes.entry(node.into()).or_default();
    state.last_attempt = timestamp;
    match pins {
        Ok(pins) => {
            *state = Observation {
                last_attempt: timestamp,
                last_success: Some(timestamp),
                online: true,
                pin_count: pins.len(),
                error: None,
            };
            for entry in snapshot.entries.values_mut() {
                entry.nodes.retain(|name| name != node);
            }
            for (cid, name) in pins {
                let entry = snapshot
                    .entries
                    .entry(cid.clone())
                    .or_insert_with(|| Entry {
                        cid,
                        ..Entry::default()
                    });
                entry.nodes.push(node.into());
                entry.nodes.sort();
                if !name.is_empty() {
                    add_name(entry, &name, "pin_name");
                }
            }
        }
        Err(error) => {
            state.online = false;
            state.error = Some(format!(
                "inventory unavailable: {}",
                error.to_string().chars().take(160).collect::<String>()
            ));
        }
    }
}

fn inventory(kubo: &Kubo) -> Result<BTreeMap<String, String>> {
    let mut pins = BTreeMap::new();
    for kind in ["recursive", "direct"] {
        let value: Value = serde_json::from_slice(&kubo.catalog_rpc(
            "pin/ls",
            &[("type", kind.into()), ("names", "true".into())],
            8 * 1024 * 1024,
        )?)?;
        // Kubo v0.43.1 returns {} when the requested pin class is empty.
        // Accept that exact shape; do not hide malformed or error responses.
        if value.as_object().is_some_and(|object| object.is_empty()) {
            continue;
        }
        let keys = value["Keys"]
            .as_object()
            .context("pin inventory has no Keys map")?;
        for (cid, pin) in keys {
            validate_cid(cid)?;
            pins.insert(
                cid.clone(),
                pin["Name"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(240)
                    .collect(),
            );
        }
        ensure!(
            pins.len() <= MAX_ENTRIES,
            "pin inventory exceeds catalog limit"
        );
    }
    Ok(pins)
}

pub fn refresh(config: &Config) -> Result<Snapshot> {
    fs::create_dir_all(&config.state_dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.state_dir.join("refresh.lock"))?;
    lock.try_lock().context("another refresh is running")?;
    let mut snapshot: Snapshot = load(&config.snapshot())?;
    let timestamp = now();
    let mut kubos = BTreeMap::new();
    for node in &config.nodes {
        let kubo = Kubo::new(&node.rpc)?;
        let pins = inventory(&kubo);
        if pins.is_ok() {
            kubos.insert(node.name.clone(), kubo);
        } else if let Err(error) = &pins {
            eprintln!(
                "IPFS catalog: node {} inventory failed: {error:#}",
                node.name
            );
        }
        apply_inventory(&mut snapshot, &node.name, pins, timestamp);
    }
    ensure!(
        snapshot.entries.len() <= MAX_ENTRIES,
        "combined catalog exceeds entry limit"
    );
    if snapshot.version < 2 {
        for entry in snapshot.entries.values_mut() {
            if entry.name_source == "suggested_heading"
                && (entry.text.trim_start().starts_with('{')
                    || entry.text.trim_start().starts_with('['))
            {
                entry.names.clear();
                entry.name_source.clear();
            }
            entry.inspected = false;
            entry.text_checked = false;
        }
    }
    snapshot.version = 2;
    snapshot.updated_at = timestamp;
    // Commit membership before optional extraction, so readers never wait for PDFs.
    save(&config.snapshot(), &snapshot)?;
    let mut byte_budget = 128 * 1024 * 1024_u64;
    let mut inspected = 0;
    let ids: Vec<_> = snapshot.entries.keys().cloned().collect();
    for cid in ids {
        let entry = snapshot
            .entries
            .get_mut(&cid)
            .expect("catalog entry disappeared");
        if entry.nodes.is_empty() || (entry.inspected && entry.text_checked) {
            continue;
        }
        if cid == WEBUI {
            add_name(entry, "IPFS node administration interface", "system");
            entry.kind = "system".into();
            entry.inspected = true;
            entry.text_checked = true;
            continue;
        }
        let Some(kubo) = entry.nodes.iter().find_map(|node| kubos.get(node)) else {
            continue;
        };
        if inspected >= 96 {
            break;
        }
        inspected += 1;
        match inspect(kubo, entry, &mut byte_budget) {
            Ok(children) => {
                entry.inspection_error = None;
                let source = if entry.mime == "application/json" {
                    "asset_manifest"
                } else {
                    "directory_entry"
                };
                for (child, name) in children {
                    if let Some(entry) = snapshot.entries.get_mut(&child) {
                        add_name(entry, &name, source);
                    }
                }
            }
            Err(error) => {
                entry.inspection_error = Some(error.to_string().chars().take(160).collect());
            }
        }
        // Bound lost progress if a process is interrupted during extraction.
        if inspected % 12 == 0 {
            save(&config.snapshot(), &snapshot)?;
        }
    }
    save(&config.snapshot(), &snapshot)?;
    Ok(snapshot)
}

fn inspect(kubo: &Kubo, entry: &mut Entry, budget: &mut u64) -> Result<Vec<(String, String)>> {
    let stat: Value = serde_json::from_slice(&kubo.catalog_rpc(
        "files/stat",
        &[("arg", format!("/ipfs/{}", entry.cid))],
        8192,
    )?)?;
    entry.kind = stat["Type"].as_str().context("missing file type")?.into();
    entry.size = stat["Size"].as_u64();
    entry.inspected = true;
    if entry.kind == "directory" {
        entry.mime = "inode/directory".into();
        let links: Value = serde_json::from_slice(&kubo.catalog_rpc(
            "ls",
            &[("arg", entry.cid.clone())],
            1024 * 1024,
        )?)?;
        let children = links["Objects"][0]["Links"]
            .as_array()
            .context("directory has no Links array")?
            .iter()
            .filter_map(|link| {
                Some((
                    link["Hash"].as_str()?.to_string(),
                    link["Name"].as_str()?.to_string(),
                ))
            })
            .filter(|(cid, name)| validate_cid(cid).is_ok() && clean_name(name).is_some())
            .take(1000)
            .collect();
        entry.text_checked = true;
        return Ok(children);
    }
    if entry.kind != "file" {
        entry.text_checked = true;
        return Ok(Vec::new());
    }
    let prefix = kubo.catalog_rpc(
        "cat",
        &[("arg", entry.cid.clone()), ("length", "4096".into())],
        4096,
    )?;
    entry.mime = detect_mime(&prefix).into();
    if entry.mime == "application/json" {
        let size = entry.size.context("missing JSON size")?;
        if size <= 1024 * 1024 && size <= *budget {
            *budget -= size;
            let bytes = if size <= prefix.len() as u64 {
                prefix.clone()
            } else {
                kubo.catalog_rpc(
                    "cat",
                    &[("arg", entry.cid.clone()), ("length", size.to_string())],
                    size,
                )?
            };
            let value: Value = serde_json::from_slice(&bytes)?;
            let references = json_names(entry, &value);
            entry.text = serde_json::to_string_pretty(&value)?
                .chars()
                .take(24_000)
                .collect();
            entry.text_checked = true;
            return Ok(references);
        }
    }
    if entry.mime == "application/pdf" {
        let size = entry.size.context("missing file size")?;
        if size > 16 * 1024 * 1024 {
            entry.text_checked = true;
            return Ok(Vec::new());
        }
        if size > *budget {
            return Ok(Vec::new());
        }
        *budget -= size;
        let bytes = kubo.catalog_rpc(
            "cat",
            &[("arg", entry.cid.clone()), ("length", size.to_string())],
            size,
        )?;
        ensure!(bytes.len() as u64 == size, "incomplete PDF download");
        extract_pdf(entry, &bytes)?;
    } else if entry.mime == "text/plain" {
        let text = String::from_utf8_lossy(&prefix).into_owned();
        if let Some(name) = suggested_heading(&text) {
            add_name(entry, &name, "suggested_heading");
        }
        entry.text = text.chars().take(4096).collect();
    }
    entry.text_checked = true;
    Ok(Vec::new())
}

pub fn detect_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"glTF") {
        "model/gltf-binary"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"PK\x03\x04") {
        "application/zip"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        "video/mp4"
    } else if !bytes.is_empty()
        && std::str::from_utf8(bytes).is_ok()
        && bytes
            .iter()
            .all(|b| !b.is_ascii_control() || matches!(b, b'\n' | b'\r' | b'\t'))
    {
        if std::str::from_utf8(bytes)
            .is_ok_and(|s| s.trim_start().starts_with('{') || s.trim_start().starts_with('['))
        {
            "application/json"
        } else {
            "text/plain"
        }
    } else {
        "application/octet-stream"
    }
}

pub fn suggested_heading(text: &str) -> Option<String> {
    if text.trim_start().starts_with('{') || text.trim_start().starts_with('[') {
        return None;
    }
    text.lines().take(40).filter_map(clean_name).find(|line| {
        line.chars().count() >= 8
            && line.chars().count() <= 160
            && !line.starts_with("http")
            && ![
                "abstract",
                "contents",
                "table of contents",
                "untitled",
                "introduction",
                "page",
            ]
            .contains(&line.to_lowercase().as_str())
    })
}

/// Recover display labels from metadata. These labels are not original filenames.
fn json_names(entry: &mut Entry, value: &Value) -> Vec<(String, String)> {
    entry.metadata_schema = value["schema"]
        .as_str()
        .map(|s| s.chars().take(128).collect());
    entry.source_visibility = value["access"]["visibility"]
        .as_str()
        .map(|s| s.chars().take(48).collect());
    let part = value["part"].as_str().and_then(clean_name);
    let title = [
        "title", "filename", "file", "name", "label", "vehicle", "model",
    ]
    .iter()
    .find_map(|key| value[*key].as_str().and_then(clean_name));
    let base = part.clone().or(title.clone());
    if let Some(part) = &part {
        add_name(entry, &format!("{part} · asset manifest"), "asset_manifest");
    } else if let Some(title) = title {
        add_name(entry, &title, "json_metadata");
    } else if let Some(file) = value["artifacts"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["file"].as_str())
        .and_then(clean_name)
    {
        add_name(entry, &format!("Model package · {file}"), "asset_manifest");
    }
    let mut refs = Vec::new();
    if let (Some(part), Some(cid)) = (base.as_deref(), value["source_cid"].as_str()) {
        if validate_cid(cid).is_ok() {
            refs.push((cid.to_string(), format!("{part} · source model")));
        }
    }
    fn walk(value: &Value, base: Option<&str>, depth: usize, out: &mut Vec<(String, String)>) {
        if depth > 8 || out.len() >= 1000 {
            return;
        }
        match value {
            Value::Object(object) => {
                if let Some(cid) = object
                    .get("cid")
                    .and_then(Value::as_str)
                    .filter(|cid| validate_cid(cid).is_ok())
                {
                    let name = ["file", "filename", "name", "title", "id"]
                        .iter()
                        .find_map(|key| {
                            object
                                .get(*key)
                                .and_then(Value::as_str)
                                .and_then(clean_name)
                        });
                    if let Some(name) = name {
                        out.push((
                            cid.into(),
                            match base {
                                Some(base) if !name.contains(base) => format!("{base} · {name}"),
                                _ => name,
                            },
                        ));
                    }
                }
                for nested in object.values() {
                    walk(nested, base, depth + 1, out);
                }
            }
            Value::Array(items) => {
                for nested in items.iter().take(1000) {
                    walk(nested, base, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    walk(value, base.as_deref(), 0, &mut refs);
    refs
}

fn extract_pdf(entry: &mut Entry, bytes: &[u8]) -> Result<()> {
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("document.pdf");
    fs::write(&path, bytes)?;
    let info = dir.path().join("info.txt");
    let text = dir.path().join("text.txt");
    let status = Command::new("timeout")
        .args(["8", "pdfinfo"])
        .arg(&path)
        .stdout(fs::File::create(&info)?)
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "PDF metadata extraction failed");
    let metadata = fs::read_to_string(&info)?;
    if let Some(title) = metadata
        .lines()
        .find_map(|line| line.strip_prefix("Title:"))
    {
        if let Some(title) = clean_name(title).filter(|title| {
            !["untitled", "document", "microsoft word"]
                .iter()
                .any(|bad| title.to_lowercase().starts_with(bad))
        }) {
            add_name(entry, &title, "pdf_metadata");
        }
    }
    let status = Command::new("timeout")
        .args(["10", "pdftotext", "-f", "1", "-l", "5", "-nopgbrk"])
        .arg(&path)
        .arg(&text)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "PDF text extraction failed");
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(text)?
        .take(32 * 1024)
        .read_to_end(&mut bytes)?;
    entry.text = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(24_000)
        .collect();
    if entry.names.is_empty() {
        if let Some(name) = suggested_heading(&entry.text) {
            add_name(entry, &name, "suggested_heading");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CID: &str = "bafkreid4xhlzm7ppdjvigsyevb7grissmrmx4kx3wgd6m4of7mifopcnc4";
    #[test]
    fn asset_manifest_recovers_part_and_variant_names_without_guessing_json_headings() {
        let mut entry = Entry::default();
        let refs = json_names(
            &mut entry,
            &serde_json::json!({"schema":"holodeck.assets.v1","part":"starbreaker-tooth-1","source_cid":CID,"access":{"visibility":"private"},"variants":[{"id":"coarse","cid":CID}]}),
        );
        assert_eq!(entry.display_name(), "starbreaker-tooth-1 · asset manifest");
        assert_eq!(entry.source_visibility.as_deref(), Some("private"));
        assert!(
            refs.iter()
                .any(|(cid, name)| cid == CID && name == "starbreaker-tooth-1 · coarse")
        );
        assert!(suggested_heading("{\n  \"access\": {\n").is_none());
        assert_eq!(detect_mime(b"glTF\x02\x00\x00\x00"), "model/gltf-binary");
        entry.names = vec!["coarse".into(), "starbreaker-tooth-1 · coarse".into()];
        assert_eq!(entry.display_name(), "starbreaker-tooth-1 · coarse");
    }
    fn kubo_fixture(bodies: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut b = [0];
                    stream.read_exact(&mut b).unwrap();
                    header.push(b[0]);
                    assert!(header.len() < 8192);
                }
                assert!(String::from_utf8_lossy(&header).starts_with("POST /api/v0/pin/ls?"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (origin, thread)
    }
    #[test]
    fn real_kubo_empty_pin_class_does_not_discard_nonempty_inventory() {
        let (origin, server) = kubo_fixture(vec![
            serde_json::json!({"Keys":{CID:{"Type":"recursive","Name":"Practical IPFS guide"}}})
                .to_string(),
            "{}".into(),
        ]);
        let pins = inventory(&Kubo::new(&origin).unwrap()).unwrap();
        assert_eq!(pins[CID], "Practical IPFS guide");
        server.join().unwrap();
        let (origin, server) = kubo_fixture(vec![r#"{"Message":"unexpected response"}"#.into()]);
        assert!(inventory(&Kubo::new(&origin).unwrap()).is_err());
        server.join().unwrap();
    }
    #[test]
    fn failed_inventory_preserves_pins_success_removes_obsolete_membership() {
        let mut s = Snapshot::default();
        apply_inventory(
            &mut s,
            "one",
            Ok(BTreeMap::from([(CID.into(), "Guide.pdf".into())])),
            10,
        );
        apply_inventory(&mut s, "one", Err(anyhow::anyhow!("offline")), 20);
        assert_eq!(s.entries[CID].nodes, ["one"]);
        assert_eq!(s.nodes["one"].last_success, Some(10));
        assert!(!s.nodes["one"].online);
        apply_inventory(&mut s, "one", Ok(BTreeMap::new()), 30);
        assert!(s.entries[CID].nodes.is_empty());
    }
    #[test]
    fn labels_survive_atomic_refresh_and_do_not_change_cids() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            listen: "127.0.0.1:8788".parse().unwrap(),
            state_dir: dir.path().into(),
            access_token_file: dir.path().join("token"),
            nodes: vec![Node {
                name: "one".into(),
                rpc: "http://127.0.0.1:5001".into(),
            }],
            gateway: "https://content.example".into(),
            public_url: "https://library.example".into(),
            uploads: None,
        };
        let mut s = Snapshot::default();
        apply_inventory(
            &mut s,
            "one",
            Ok(BTreeMap::from([(CID.into(), String::new())])),
            10,
        );
        save(&config.snapshot(), &s).unwrap();
        label(&config, CID, "Family guide.pdf").unwrap();
        save(&config.snapshot(), &s).unwrap();
        let view = view(&config).unwrap();
        assert_eq!(view.entries[CID].cid, CID);
        assert_eq!(view.entries[CID].display_name(), "Family guide.pdf");
        assert_eq!(view.entries[CID].name_source, "operator");
        assert!(label(&config, "../../secret", "bad").is_err());
    }
    #[test]
    fn damaged_state_fails_loudly_and_inference_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        fs::write(&path, "{broken").unwrap();
        assert!(load::<Snapshot>(&path).is_err());
        assert_eq!(detect_mime(b"%PDF-1.7"), "application/pdf");
        assert_eq!(
            suggested_heading("\nAbstract\nA practical guide to IPFS\n"),
            Some("A practical guide to IPFS".into())
        );
        let mut e = Entry::default();
        add_name(&mut e, "A practical guide to IPFS", "suggested_heading");
        assert_eq!(e.name_source, "suggested_heading");
    }
}
