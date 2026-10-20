//! Explicit write capability. Reader and MCP credentials never authorize imports.
use crate::{
    catalog,
    library::{App, failure, protected_response},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};
use subtle::ConstantTimeEq;

pub const CHUNK: usize = 8 * 1024 * 1024;
pub const FORM_LIMIT: usize = 90 * 1024 * 1024;
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const QUOTA: u64 = 8 * MAX_BYTES;
const RESERVE: u64 = 2 * MAX_BYTES;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node: String,
    pub token_file: PathBuf,
    pub storage_path: PathBuf,
}
impl Config {
    pub fn validate(&self, config: &catalog::Config) -> Result<()> {
        ensure!(
            config.nodes.iter().any(|node| node.name == self.node),
            "upload node is not configured"
        );
        ensure!(
            self.token_file.is_absolute() && self.storage_path.is_absolute(),
            "upload paths must be absolute"
        );
        ensure!(
            self.token_file != config.access_token_file,
            "upload and reader credentials must differ"
        );
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Uploads {
    pub config: Config,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub path: String,
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub folder: bool,
    pub files: Vec<File>,
}

pub fn valid_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 1024,
        "path is empty or too long"
    );
    let parts: Vec<_> = path.split('/').collect();
    ensure!(parts.len() <= 16, "folder depth exceeds 16");
    ensure!(
        parts.iter().all(|part| !part.is_empty()
            && *part != "."
            && *part != ".."
            && part.len() <= 240
            && !part.chars().any(|c| c.is_control() || "\\%:#?".contains(c))),
        "unsafe file path"
    );
    Ok(())
}
impl Manifest {
    fn validate(&self) -> Result<u64> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 240
                && !self.name.contains('/')
                && !self.name.chars().any(char::is_control),
            "invalid upload name"
        );
        ensure!(
            !self.files.is_empty() && self.files.len() <= 256,
            "select 1 to 256 files"
        );
        ensure!(
            self.folder || self.files.len() == 1,
            "multiple files require a folder"
        );
        let mut paths = BTreeSet::new();
        let mut total = 0u64;
        for file in &self.files {
            valid_path(&file.path)?;
            ensure!(
                self.folder || !file.path.contains('/'),
                "single file has nested path"
            );
            ensure!(paths.insert(file.path.clone()), "duplicate path");
            total = total.checked_add(file.size).context("size overflow")?;
            ensure!(total <= MAX_BYTES, "upload exceeds 1 GiB");
        }
        for path in &paths {
            let mut parent = path.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(
                    !paths.contains(prefix),
                    "file conflicts with a parent folder"
                );
                parent = prefix;
            }
        }
        Ok(total)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Session {
    id: String,
    actor: String,
    updated_at: u64,
    manifest: Manifest,
}
#[derive(Debug, Deserialize)]
pub(crate) struct ChunkQuery {
    session: String,
    file: usize,
    offset: u64,
}
#[derive(Debug, Deserialize)]
pub(crate) struct SessionQuery {
    session: String,
}

fn authority(app: &App, headers: &HeaderMap) -> Result<String> {
    let upload = app.uploads.as_ref().context("uploads are disabled")?;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    ensure!(
        bool::from(bearer.as_bytes().ct_eq(upload.token.as_bytes())),
        "upload credential required"
    );
    ensure!(
        headers.get(header::ORIGIN).and_then(|h| h.to_str().ok())
            == Some(app.config.public_url.trim_end_matches('/')),
        "upload origin denied"
    );
    let actor = headers
        .get("x-library-actor")
        .and_then(|h| h.to_str().ok())
        .context("authenticated actor required")?;
    ensure!(
        actor.starts_with("oidc:")
            && actor.len() <= 512
            && actor.is_ascii()
            && !actor.chars().any(char::is_control),
        "invalid actor"
    );
    Ok(actor.into())
}
fn lock(config: &catalog::Config) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.state_dir.join("upload.lock"))?;
    file.try_lock()
        .context("another upload operation is active")?;
    Ok(file)
}
fn session(config: &catalog::Config, id: &str, actor: &str) -> Result<Session> {
    let session: Option<Session> = catalog::load(&config.state_dir.join("upload-session.json"))?;
    let session = session.context("upload session not found")?;
    ensure!(
        session.id == id && session.actor == actor,
        "upload session not found"
    );
    ensure!(
        catalog::now().saturating_sub(session.updated_at) <= 3600,
        "upload session expired"
    );
    Ok(session)
}
fn error(error: anyhow::Error) -> Response {
    eprintln!("IPFS upload failed: {error:#}");
    failure(StatusCode::CONFLICT, &error.to_string())
}
pub(crate) fn pending_form(config: &catalog::Config, actor: &str) -> Result<String> {
    let session: Option<Session> = catalog::load(&config.state_dir.join("upload-session.json"))?;
    Ok(
        match session
            .filter(|s| s.actor == actor && catalog::now().saturating_sub(s.updated_at) <= 3600)
        {
            Some(s) => format!(
                "<section class=details><h2>Unfinished upload</h2><p>{}</p><form method=post action='/api/upload/cancel?session={}'><button>Cancel saved upload</button></form><p>Previously pinned data is kept.</p></section>",
                crate::library::escape(&s.manifest.name),
                s.id
            ),
            None => String::new(),
        },
    )
}

pub(crate) async fn begin(
    State(app): State<App>,
    headers: HeaderMap,
    Json(manifest): Json<Manifest>,
) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(a) => a,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let total = manifest.validate()?;
        let _lock = lock(&app.config)?;
        let old: Option<Session> =
            catalog::load(&app.config.state_dir.join("upload-session.json"))?;
        if let Some(old) = old.filter(|s| catalog::now().saturating_sub(s.updated_at) <= 3600) {
            ensure!(
                old.actor == actor
                    && serde_json::to_value(&old.manifest)? == serde_json::to_value(&manifest)?,
                "another upload is active; finish or cancel it"
            );
            return Ok(json!({"session":old.id,"chunk_bytes":CHUNK,"bytes":total}));
        }
        let uploads = app.uploads.as_ref().context("uploads disabled")?;
        let receipts: BTreeMap<String, Value> =
            catalog::load(&app.config.state_dir.join("uploads.json"))?;
        ensure!(
            receipts.len() < catalog::MAX_ENTRIES,
            "upload catalog is full"
        );
        let used: u64 = receipts
            .values()
            .map(|r| r["bytes"].as_u64().unwrap_or(0))
            .sum();
        ensure!(
            used.saturating_add(total) <= QUOTA,
            "8 GiB library upload allowance exhausted"
        );
        ensure!(
            fs2::available_space(&uploads.config.storage_path)? >= RESERVE + 2 * total,
            "insufficient storage; keep 2 GiB free plus staging and imported data"
        );
        let pending = app.config.state_dir.join("upload-pending");
        if pending.exists() {
            fs::remove_dir_all(&pending)?;
        }
        fs::create_dir(&pending)?;
        for i in 0..manifest.files.len() {
            fs::File::create(pending.join(i.to_string()))?;
        }
        let mut random = [0u8; 24];
        getrandom::fill(&mut random)
            .map_err(|e| anyhow::anyhow!("session randomness failed: {e}"))?;
        let id: String = random.iter().map(|b| format!("{b:02x}")).collect();
        catalog::save(
            &app.config.state_dir.join("upload-session.json"),
            &Some(Session {
                id: id.clone(),
                actor,
                updated_at: catalog::now(),
                manifest,
            }),
        )?;
        Ok(json!({"session":id,"chunk_bytes":CHUNK,"bytes":total}))
    })
    .await;
    match result {
        Ok(Ok(value)) => protected_response(Json(value).into_response()),
        Ok(Err(e)) => error(e),
        Err(e) => error(e.into()),
    }
}

fn write_chunk(
    config: &catalog::Config,
    actor: &str,
    query: &ChunkQuery,
    bytes: &[u8],
) -> Result<u64> {
    let _lock = lock(config)?;
    let mut session = session(config, &query.session, actor)?;
    let spec = session
        .manifest
        .files
        .get(query.file)
        .context("invalid file number")?;
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= CHUNK
            && query
                .offset
                .checked_add(bytes.len() as u64)
                .is_some_and(|end| end <= spec.size),
        "chunk exceeds declared size"
    );
    let mut file = fs::OpenOptions::new().read(true).write(true).open(
        config
            .state_dir
            .join("upload-pending")
            .join(query.file.to_string()),
    )?;
    let current = file.metadata()?.len();
    let end = query.offset + bytes.len() as u64;
    if query.offset == current {
        file.seek(SeekFrom::End(0))?;
        file.write_all(bytes)?;
        file.sync_data()?;
    } else {
        ensure!(end <= current, "chunk offset does not match stored bytes");
        let mut stored = vec![0; bytes.len()];
        file.seek(SeekFrom::Start(query.offset))?;
        file.read_exact(&mut stored)?;
        ensure!(stored == bytes, "retried chunk does not match stored data");
    }
    session.updated_at = catalog::now();
    catalog::save(
        &config.state_dir.join("upload-session.json"),
        &Some(session),
    )?;
    Ok(end)
}
pub(crate) async fn chunk(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<ChunkQuery>,
    bytes: Bytes,
) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(a) => a,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    match tokio::task::spawn_blocking(move || write_chunk(&app.config, &actor, &query, &bytes))
        .await
    {
        Ok(Ok(offset)) => protected_response(Json(json!({"offset":offset})).into_response()),
        Ok(Err(e)) => error(e),
        Err(e) => error(e.into()),
    }
}
pub(crate) async fn status(State(app): State<App>, headers: HeaderMap) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(a) => a,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    match catalog::load::<Option<Session>>(&app.config.state_dir.join("upload-session.json")) {
        Ok(Some(s)) if s.actor == actor && catalog::now().saturating_sub(s.updated_at) <= 3600 => {
            protected_response(Json(json!({"session":s.id,"name":s.manifest.name})).into_response())
        }
        Ok(_) => protected_response(Json(json!({"session":null})).into_response()),
        Err(e) => error(e),
    }
}
pub(crate) async fn cancel(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<SessionQuery>,
) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(a) => a,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    let result = (|| -> Result<()> {
        let _lock = lock(&app.config)?;
        session(&app.config, &query.session, &actor)?;
        catalog::save(
            &app.config.state_dir.join("upload-session.json"),
            &Option::<Session>::None,
        )?;
        fs::remove_dir_all(app.config.state_dir.join("upload-pending"))?;
        Ok(())
    })();
    match result {
        Ok(()) => protected_response(Json(json!({"cancelled":true})).into_response()),
        Err(e) => error(e),
    }
}

pub(crate) async fn commit(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<SessionQuery>,
) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(a) => a,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    match import(&app, &query.session, &actor).await {
        Ok(value) => protected_response(Json(value).into_response()),
        Err(e) => error(e),
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NativeQuery {
    folder: bool,
}
pub(crate) async fn native(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<NativeQuery>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let actor = match authority(&app, &headers) {
        Ok(actor) => actor,
        Err(_) => return failure(StatusCode::FORBIDDEN, "Upload permission required"),
    };
    let result = async {
        let guard = lock(&app.config)?;
        let old: Option<Session> = catalog::load(&app.config.state_dir.join("upload-session.json"))?;
        ensure!(old.is_none_or(|s|catalog::now().saturating_sub(s.updated_at)>3600),"An unfinished upload exists. Finish or cancel it before starting another.");
        let upload = app.uploads.as_ref().context("uploads disabled")?;
        ensure!(fs2::available_space(&upload.config.storage_path)? >= RESERVE + 2 * FORM_LIMIT as u64,"insufficient storage for a browser upload");
        let receipts: BTreeMap<String,Value> = catalog::load(&app.config.state_dir.join("uploads.json"))?;
        let used:u64 = receipts.values().map(|r|r["bytes"].as_u64().unwrap_or(0)).sum();
        let pending = app.config.state_dir.join("upload-pending");
        if pending.exists(){fs::remove_dir_all(&pending)?;}
        fs::create_dir(&pending)?;
        let mut files = Vec::new();
        let mut name = String::new();
        let mut root = None::<String>;
        let mut total = 0u64;
        while let Some(mut field) = multipart.next_field().await? {
            if field.name()==Some("name") {
                let mut value = Vec::new();
                while let Some(chunk)=field.chunk().await? {ensure!(value.len()+chunk.len()<=240,"upload name too long");value.extend_from_slice(&chunk);}
                name=String::from_utf8(value)?.trim().into();
                continue;
            }
            ensure!(field.name()==Some("files"),"unexpected upload field");
            let path=field.file_name().context("file has no filename")?.to_string();
            if path.is_empty(){continue;}
            valid_path(&path)?;
            let path=if query.folder {
                let (folder,path)=path.split_once('/').context("This browser did not send folder paths. Select files instead, or use Chrome for folder selection.")?;
                ensure!(root.as_ref().is_none_or(|old|old==folder),"files have different root folders");
                root=Some(folder.into());
                path.into()
            }else{path};
            ensure!(files.len()<256,"upload exceeds 256 files");
            let mut file=tokio::fs::File::create(pending.join(files.len().to_string())).await?;
            let mut size=0u64;
            while let Some(chunk)=field.chunk().await? {
                total+=chunk.len() as u64;
                ensure!(total<=FORM_LIMIT as u64 && used.saturating_add(total)<=QUOTA,"upload exceeds browser or storage allowance");
                tokio::io::AsyncWriteExt::write_all(&mut file,&chunk).await?;
                size+=chunk.len() as u64;
            }
            file.sync_all().await?;
            files.push(File{path,size});
        }
        ensure!(!files.is_empty(),"select at least one file");
        let folder=query.folder||files.len()>1;
        if name.is_empty(){name=root.unwrap_or_else(||if files.len()==1{files[0].path.clone()}else{"Uploaded files".into()});}
        let manifest=Manifest{name,folder,files};
        manifest.validate()?;
        let mut random=[0u8;24];
        getrandom::fill(&mut random).map_err(|e|anyhow::anyhow!("session randomness failed: {e}"))?;
        let id:String=random.iter().map(|b|format!("{b:02x}")).collect();
        catalog::save(&app.config.state_dir.join("upload-session.json"),&Some(Session{id:id.clone(),actor:actor.clone(),updated_at:catalog::now(),manifest}))?;
        drop(guard);
        import(&app,&id,&actor).await
    }.await;
    match result {
        Ok(value) => {
            let mut reply = protected_response(StatusCode::SEE_OTHER.into_response());
            reply.headers_mut().insert(
                header::LOCATION,
                format!("/files/{}", value["cid"].as_str().unwrap())
                    .parse()
                    .unwrap(),
            );
            reply
        }
        Err(e) => error(e),
    }
}

async fn small_json(mut reply: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 4096,
            "Kubo verification reply too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
async fn import(app: &App, id: &str, actor: &str) -> Result<Value> {
    let _lock = lock(&app.config)?;
    let session = session(&app.config, id, actor)?;
    let total = session.manifest.validate()?;
    let upload = app.uploads.as_ref().context("uploads disabled")?;
    let node = app
        .config
        .nodes
        .iter()
        .find(|node| node.name == upload.config.node)
        .context("upload node missing")?;
    let mut form = reqwest::multipart::Form::new();
    let mut order: Vec<_> = session.manifest.files.iter().enumerate().collect();
    order.sort_by_key(|(_, f)| &f.path);
    for (i, spec) in order {
        let file = tokio::fs::File::open(
            app.config
                .state_dir
                .join("upload-pending")
                .join(i.to_string()),
        )
        .await?;
        ensure!(
            file.metadata().await?.len() == spec.size,
            "upload is incomplete"
        );
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let part = reqwest::multipart::Part::stream_with_length(body, spec.size)
            .file_name(spec.path.clone())
            .mime_str("application/octet-stream")?;
        form = form.part("file", part);
    }
    // Kubo owns UnixFS layout and pinning. No IPFS data format is reimplemented.
    let mut reply = app
        .http
        .post(format!("{}/api/v0/add", node.rpc))
        .query(&[
            ("cid-version", "1"),
            ("recursive", "true"),
            (
                "wrap-with-directory",
                if session.manifest.folder {
                    "true"
                } else {
                    "false"
                },
            ),
            ("pin", "true"),
            ("pin-name", session.manifest.name.as_str()),
            ("progress", "false"),
        ])
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let mut receipt = Vec::new();
    while let Some(chunk) = reply.chunk().await? {
        ensure!(
            receipt.len() + chunk.len() <= 256 * 1024,
            "Kubo import receipt too large"
        );
        receipt.extend_from_slice(&chunk);
    }
    let last = std::str::from_utf8(&receipt)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .last()
        .context("missing import receipt")?;
    let value: Value = serde_json::from_str(last)?;
    let cid = value["Hash"]
        .as_str()
        .context("Kubo did not return an imported CID")?;
    catalog::validate_cid(cid)?;
    let stat = small_json(
        app.http
            .post(format!("{}/api/v0/files/stat", node.rpc))
            .query(&[("arg", format!("/ipfs/{cid}"))])
            .send()
            .await?
            .error_for_status()?,
    )
    .await?;
    ensure!(
        stat["Type"]
            == if session.manifest.folder {
                "directory"
            } else {
                "file"
            },
        "imported root has unexpected type"
    );
    ensure!(
        session.manifest.folder || stat["Size"].as_u64() == Some(total),
        "imported file size does not match upload"
    );
    let pins = small_json(
        app.http
            .post(format!("{}/api/v0/pin/ls", node.rpc))
            .query(&[("arg", cid), ("type", "recursive")])
            .send()
            .await?
            .error_for_status()?,
    )
    .await?;
    ensure!(
        pins["Keys"][cid]["Type"] == "recursive",
        "Kubo did not confirm the uploaded root pin"
    );
    let mut receipts: BTreeMap<String, Value> =
        catalog::load(&app.config.state_dir.join("uploads.json"))?;
    let receipt = json!({"cid":cid,"name":session.manifest.name,"bytes":total,"files":session.manifest.files.len(),"paths":session.manifest.files.iter().map(|file|&file.path).collect::<Vec<_>>(),"folder":session.manifest.folder,"node":node.name,"actor":actor,"created_at":catalog::now()});
    receipts.insert(cid.into(), receipt.clone());
    catalog::save(&app.config.state_dir.join("uploads.json"), &receipts)?;
    // Serialize with the scanner. Pin name remains recoverable even if this write fails.
    let config = app.config.clone();
    let cid = cid.to_owned();
    let node_name = node.name.clone();
    let manifest = session.manifest.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let refresh_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(config.state_dir.join("refresh.lock"))?;
        refresh_lock.lock()?;
        let mut snapshot: catalog::Snapshot = catalog::load(&config.snapshot())?;
        let entry = snapshot.entries.entry(cid.clone()).or_default();
        entry.cid = cid;
        if !entry.nodes.contains(&node_name) {
            entry.nodes.push(node_name);
        }
        entry.names.insert(0, manifest.name);
        entry.name_source = "pin_name".into();
        entry.kind = if manifest.folder { "directory" } else { "file" }.into();
        if manifest.folder {
            entry.text = manifest
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(24_000)
                .collect();
        }
        entry.size = Some(total);
        entry.inspected = false;
        entry.text_checked = false;
        catalog::save(&config.snapshot(), &snapshot)
    })
    .await??;
    catalog::save(
        &app.config.state_dir.join("upload-session.json"),
        &Option::<Session>::None,
    )?;
    fs::remove_dir_all(app.config.state_dir.join("upload-pending"))?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_paths_and_conflicting_or_oversized_manifests() {
        for path in ["../x", "/file", "a//b", "a/./b", "a\\b", "a/%2f", "a\nfile"] {
            assert!(valid_path(path).is_err(), "{path}");
        }
        assert!(valid_path("parts/Vehicle model.glb").is_ok());
        let mut manifest = Manifest {
            name: "Collection".into(),
            folder: true,
            files: vec![
                File {
                    path: "a".into(),
                    size: 3,
                },
                File {
                    path: "a/b".into(),
                    size: 2,
                },
            ],
        };
        assert!(manifest.validate().is_err());
        manifest.files = vec![File {
            path: "a".into(),
            size: MAX_BYTES + 1,
        }];
        assert!(manifest.validate().is_err());
    }
    #[test]
    fn chunks_are_owned_ordered_size_bounded_and_replay_checked() {
        let dir = tempfile::tempdir().unwrap();
        let config = catalog::Config {
            listen: "127.0.0.1:1".parse().unwrap(),
            state_dir: dir.path().into(),
            access_token_file: dir.path().join("token"),
            nodes: vec![],
            gateway: "https://content.example".into(),
            public_url: "https://library.example".into(),
            uploads: None,
        };
        fs::create_dir(dir.path().join("upload-pending")).unwrap();
        fs::File::create(dir.path().join("upload-pending/0")).unwrap();
        catalog::save(
            &dir.path().join("upload-session.json"),
            &Some(Session {
                id: "abc".into(),
                actor: "owner".into(),
                updated_at: catalog::now(),
                manifest: Manifest {
                    name: "Test".into(),
                    folder: false,
                    files: vec![File {
                        path: "test.txt".into(),
                        size: 4,
                    }],
                },
            }),
        )
        .unwrap();
        let query = ChunkQuery {
            session: "abc".into(),
            file: 0,
            offset: 0,
        };
        assert!(write_chunk(&config, "other", &query, b"ab").is_err());
        assert_eq!(write_chunk(&config, "owner", &query, b"ab").unwrap(), 2);
        assert_eq!(write_chunk(&config, "owner", &query, b"ab").unwrap(), 2);
        assert!(write_chunk(&config, "owner", &query, b"xx").is_err());
        assert!(
            write_chunk(
                &config,
                "owner",
                &ChunkQuery {
                    session: "abc".into(),
                    file: 0,
                    offset: 3
                },
                b"xx"
            )
            .is_err()
        );
        assert_eq!(
            write_chunk(
                &config,
                "owner",
                &ChunkQuery {
                    session: "abc".into(),
                    file: 0,
                    offset: 2
                },
                b"cd"
            )
            .unwrap(),
            4
        );
        assert_eq!(
            fs::read(dir.path().join("upload-pending/0")).unwrap(),
            b"abcd"
        );
    }
}
