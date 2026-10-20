//! Browse only descendants of a cataloged root. No arbitrary gateway or RPC path.
use crate::{
    catalog::{self, Entry},
    library::{App, bytes, escape, page, protected_response},
};
use anyhow::{Context, Result, ensure};
use axum::response::{Html, IntoResponse, Response};
use serde_json::Value;
use std::fmt::Write as _;

async fn rpc(app: &App, node: &catalog::Node, command: &str, arg: &str) -> Result<Value> {
    ensure!(
        matches!(command, "files/stat" | "ls"),
        "invalid browse command"
    );
    let mut reply = app
        .http
        .post(format!("{}/api/v0/{command}", node.rpc))
        .query(&[("arg", arg)])
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = reply.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 1024 * 1024,
            "folder reply exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn ipfs_path(cid: &str, path: &str) -> Result<String> {
    catalog::validate_cid(cid)?;
    if path.is_empty() {
        return Ok(format!("/ipfs/{cid}"));
    }
    crate::uploads::valid_path(path)?;
    Ok(format!("/ipfs/{cid}/{path}"))
}
pub async fn resolve(app: &App, root: &Entry, path: &str) -> Result<Entry> {
    let arg = ipfs_path(&root.cid, path)?;
    ensure!(root.kind == "directory", "root is not a folder");
    for node in app
        .config
        .nodes
        .iter()
        .filter(|node| root.nodes.contains(&node.name))
    {
        if let Ok(Ok(stat)) = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            rpc(app, node, "files/stat", &arg),
        )
        .await
        {
            let kind = stat["Type"].as_str().context("missing entry type")?;
            ensure!(
                matches!(kind, "file" | "directory"),
                "unsupported folder entry"
            );
            catalog::validate_cid(stat["Hash"].as_str().context("missing folder entry CID")?)?;
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            let mime = match name
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str()
            {
                "pdf" => "application/pdf",
                "txt" | "md" => "text/plain",
                "json" => "application/json",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "glb" => "model/gltf-binary",
                "zip" => "application/zip",
                _ => "application/octet-stream",
            };
            return Ok(Entry {
                cid: root.cid.clone(),
                names: vec![name],
                name_source: "directory_entry".into(),
                kind: kind.into(),
                mime: mime.into(),
                size: stat["Size"].as_u64(),
                nodes: vec![node.name.clone()],
                inspected: true,
                text_checked: true,
                ..Entry::default()
            });
        }
    }
    anyhow::bail!("folder entry is unavailable")
}
fn link(cid: &str, path: &str) -> String {
    let mut url = reqwest::Url::parse("https://library.invalid/").unwrap();
    url.set_path(&format!("/files/{cid}"));
    if !path.is_empty() {
        url.query_pairs_mut().append_pair("path", path);
    }
    format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
}
pub async fn browse(app: &App, entry: &Entry, path: &str) -> Result<Response> {
    let arg = ipfs_path(&entry.cid, path)?;
    let mut listing = None;
    for node in app
        .config
        .nodes
        .iter()
        .filter(|node| entry.nodes.contains(&node.name))
    {
        if let Ok(Ok(value)) = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            rpc(app, node, "ls", &arg),
        )
        .await
        {
            listing = Some(value);
            break;
        }
    }
    let listing = listing.context("folder is unavailable")?;
    let links = listing["Objects"][0]["Links"]
        .as_array()
        .context("invalid folder listing")?;
    ensure!(links.len() <= 1024, "folder exceeds 1024 entries");
    let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
    let mut body = format!(
        "<header><a class=brand href='/'>IPFS <span>Library</span></a></header><p><a href='/'>All files</a> / <a href='{}'>Parent folder</a></p><section class=intro><p class=eyebrow>FOLDER</p><h1>{}</h1><p>{}</p></section><section class=files>",
        escape(&link(&entry.cid, parent)),
        escape(&entry.display_name()),
        escape(if path.is_empty() {
            "Files and folders stored together under one content address."
        } else {
            path
        })
    );
    for child in links {
        let name = child["Name"].as_str().context("entry has no name")?;
        let relative = if path.is_empty() {
            name.into()
        } else {
            format!("{path}/{name}")
        };
        crate::uploads::valid_path(&relative)?;
        write!(body,"<article><div class=fileicon>{}</div><div class=fileinfo><h2><a href='{}'>{}</a></h2><p>{}</p></div></article>",if child["Type"]==1{"DIR"}else{"FILE"},escape(&link(&entry.cid,&relative)),escape(name),bytes(child["Size"].as_u64())).unwrap();
    }
    if links.is_empty() {
        body.push_str("<p>This folder is empty.</p>");
    }
    body.push_str("</section>");
    Ok(protected_response(
        Html(page(&entry.display_name(), &body)).into_response(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_catalog_root_relative_paths_can_be_resolved() {
        let cid = "bafkreid4xhlzm7ppdjvigsyevb7grissmrmx4kx3wgd6m4of7mifopcnc4";
        assert_eq!(
            ipfs_path(cid, "parts/model.glb").unwrap(),
            format!("/ipfs/{cid}/parts/model.glb")
        );
        assert!(ipfs_path(cid, "../other").is_err());
        assert!(ipfs_path("http://localhost/", "x").is_err());
        assert!(link(cid, "a b/file.txt").contains("path=a+b%2Ffile.txt"));
    }
}
