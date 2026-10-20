use std::{cell::Cell, rc::Rc};

use js_sys::{Function, Promise, Reflect, Uint8Array};
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    HtmlButtonElement, HtmlProgressElement, RequestCredentials, RequestInit, RequestRedirect,
    Response,
};

use crate::{
    browser::{DirectClient, DirectConnection, DirectPreference},
    framing::RangeFrame,
};

#[derive(Clone, Deserialize)]
struct Provider {
    endpoint: iroh::EndpointAddr,
    protocol: String,
    node: String,
}
#[derive(Clone, Deserialize)]
struct Options {
    relay_url: String,
    #[serde(default)]
    stun_urls: Vec<String>,
    provider: Provider,
}
#[derive(Deserialize)]
struct Session {
    session: String,
    expires_at: u64,
    size: u64,
    max_range_bytes: u64,
    provider: Provider,
    relay_url: String,
}

fn document() -> web_sys::Document {
    web_sys::window().unwrap().document().unwrap()
}
fn status(text: &str) {
    document()
        .get_element_by_id("status")
        .unwrap()
        .set_text_content(Some(text));
}
fn details(value: &Value) {
    document()
        .get_element_by_id("details")
        .unwrap()
        .set_text_content(Some(&serde_json::to_string_pretty(value).unwrap()));
}
fn button(id: &str) -> HtmlButtonElement {
    document()
        .get_element_by_id(id)
        .unwrap()
        .dyn_into()
        .unwrap()
}
fn error(error: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&error.to_string())
}

pub fn start() {
    spawn_local(async {
        if let Err(e) = initialize().await {
            status(&message(&e));
        }
    });
}

async fn initialize() -> Result<(), JsValue> {
    let query =
        web_sys::UrlSearchParams::new_with_str(&web_sys::window().unwrap().location().search()?)?;
    let cid = query
        .get("cid")
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(|| error("Choose a file from the library."))?;
    let path = query.get("path").unwrap_or_default();
    let name = query
        .get("name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.rsplit('/')
                .next()
                .filter(|p| !p.is_empty())
                .unwrap_or("download.bin")
                .to_owned()
        });
    document()
        .get_element_by_id("filename")
        .unwrap()
        .set_text_content(Some(&name));
    let fallback = document().get_element_by_id("fallback").unwrap();
    fallback.set_attribute(
        "href",
        &format!(
            "https://library-content.fpl.dev/ipfs/{}?path={}&download=true",
            js_sys::encode_uri_component(&cid),
            js_sys::encode_uri_component(&path)
        ),
    )?;
    fallback.remove_attribute("hidden")?;
    let options: Options =
        serde_json::from_value(api("/api/download/options", None).await?).map_err(error)?;
    let picker_supported = Reflect::get(
        web_sys::window().unwrap().as_ref(),
        &"showSaveFilePicker".into(),
    )?
    .is_function();
    #[cfg(feature = "browser-fixture")]
    let fixture = query.get("fixture").as_deref() == Some("opfs");
    #[cfg(not(feature = "browser-fixture"))]
    let fixture = false;
    if !picker_supported && !fixture {
        status("This browser supports HTTPS downloads. Use the link below.");
        return Ok(());
    }
    status(&format!(
        "Ready. {} is available. Choose where to save the file.",
        options.provider.node
    ));
    let cancel = Rc::new(Cell::new(false));
    let cancel_flag = cancel.clone();
    let cancel_handler = Closure::<dyn FnMut()>::new(move || {
        cancel_flag.set(true);
        status("Cancelling…");
    });
    button("cancel").set_onclick(Some(cancel_handler.as_ref().unchecked_ref()));
    cancel_handler.forget();
    let handler = Closure::<dyn FnMut()>::new(move || {
        // The picker starts synchronously inside the trusted button event.
        let file = if fixture {
            fixture_file()
        } else {
            pick_file(&name)
        };
        let Ok(file) = file else {
            status("The save dialog is unavailable. Use the HTTPS download link.");
            return;
        };
        cancel.set(false);
        button("save").set_disabled(true);
        button("cancel").set_disabled(false);
        let (cid, path, cancel, options) =
            (cid.clone(), path.clone(), cancel.clone(), options.clone());
        spawn_local(async move {
            let result = save(file, &cid, &path, options, cancel).await;
            button("save").set_disabled(false);
            button("cancel").set_disabled(true);
            if let Err(e) = result {
                status(&format!("{}. You can retry or use HTTPS.", message(&e)));
            }
        });
    });
    button("save").set_onclick(Some(handler.as_ref().unchecked_ref()));
    handler.forget();
    button("save").set_disabled(false);
    Ok(())
}

fn pick_file(name: &str) -> Result<Promise, JsValue> {
    let window = web_sys::window().unwrap();
    let options = js_sys::Object::new();
    Reflect::set(&options, &"suggestedName".into(), &name.into())?;
    let picker: Function =
        Reflect::get(window.as_ref(), &"showSaveFilePicker".into())?.dyn_into()?;
    picker.call1(window.as_ref(), &options)?.dyn_into()
}

#[cfg(feature = "browser-fixture")]
fn fixture_file() -> Result<Promise, JsValue> {
    // This sink is compiled only into a controlled browser qualification bundle.
    Ok(wasm_bindgen_futures::future_to_promise(async {
        let navigator = web_sys::window().unwrap().navigator();
        let storage = Reflect::get(navigator.as_ref(), &"storage".into())?;
        let directory = invoke(&storage, "getDirectory", &[]).await?;
        let options = js_sys::Object::new();
        Reflect::set(&options, &"create".into(), &JsValue::TRUE)?;
        invoke(
            &directory,
            "getFileHandle",
            &["ipfs-direct-qualification.bin".into(), options.into()],
        )
        .await
    }))
}
#[cfg(not(feature = "browser-fixture"))]
fn fixture_file() -> Result<Promise, JsValue> {
    Err(error("browser fixture is disabled"))
}

async fn save(
    file: Promise,
    cid: &str,
    path: &str,
    options: Options,
    cancel: Rc<Cell<bool>>,
) -> Result<(), JsValue> {
    let handle = JsFuture::from(file).await?;
    let sink = invoke(&handle, "createWritable", &[]).await?;
    let client =
        match DirectClient::spawn(options.relay_url.clone(), options.stun_urls.clone()).await {
            Ok(client) => client,
            Err(error) => {
                let _ = invoke(&sink, "abort", &[]).await;
                return Err(error);
            }
        };
    let result = transfer(&client, &sink, cid, path, &options, &cancel).await;
    if result.is_ok() {
        invoke(&sink, "close", &[]).await?;
        status("File saved. Connection details are available below.");
        document()
            .get_element_by_id("status")
            .unwrap()
            .set_attribute("data-state", "saved")?;
    } else {
        let _ = invoke(&sink, "abort", &[]).await;
    }
    let _ = client.close().await;
    result
}

async fn session(client: &DirectClient, cid: &str, path: &str) -> Result<Session, JsValue> {
    serde_json::from_value(
        api(
            "/api/download/session",
            Some(json!({
        "endpoint_id":client.endpoint_id(),"cid":cid,"path":path})),
        )
        .await?,
    )
    .map_err(error)
}

async fn transfer(
    client: &DirectClient,
    sink: &JsValue,
    cid: &str,
    path: &str,
    options: &Options,
    cancel: &Cell<bool>,
) -> Result<(), JsValue> {
    let mut grant = session(client, cid, path).await?;
    if grant.provider.endpoint.id != options.provider.endpoint.id
        || grant.provider.protocol != options.provider.protocol
        || grant.relay_url != options.relay_url
    {
        return Err(error("The serving endpoint changed. Reload this page."));
    }
    let mut connection = client
        .dial(
            &grant.provider.endpoint,
            grant.provider.protocol.as_bytes(),
            DirectPreference::PreferWebRtc,
        )
        .await?;
    let result = ranges(client, &mut connection, sink, cid, path, &mut grant, cancel).await;
    let _ = connection.close().await;
    result
}

async fn ranges(
    client: &DirectClient,
    connection: &mut DirectConnection,
    sink: &JsValue,
    cid: &str,
    path: &str,
    grant: &mut Session,
    cancel: &Cell<bool>,
) -> Result<(), JsValue> {
    let size = grant.size;
    let mut offset = 0u64;
    let progress: HtmlProgressElement = document()
        .get_element_by_id("progress")
        .unwrap()
        .dyn_into()?;
    progress.set_max(size.max(1) as f64);
    while offset < size {
        if cancel.get() {
            return Err(error("Download cancelled"));
        }
        if (js_sys::Date::now() / 1000.0) as u64 + 30 >= grant.expires_at {
            let renewed = session(client, cid, path).await?;
            if renewed.size != size
                || renewed.provider.endpoint.id != grant.provider.endpoint.id
                || renewed.provider.protocol != grant.provider.protocol
            {
                return Err(error("The serving endpoint changed during download"));
            }
            *grant = renewed;
        }
        if grant.max_range_bytes == 0 || grant.max_range_bytes > 8 * 1024 * 1024 {
            return Err(error("The provider advertised an invalid range limit"));
        }
        let length = (size - offset).min(grant.max_range_bytes);
        let mut attempts = 0;
        let bytes = loop {
            let request = serde_json::to_vec(
                &json!({"session":grant.session,"offset":offset,"length":length}),
            )
            .map_err(error)?;
            match receive_range(connection, &request, size, offset, length, cancel).await {
                Ok(bytes) => break bytes,
                Err(failure) if attempts < 2 && !cancel.get() => {
                    attempts += 1;
                    status("Reconnecting. The last completed range is preserved…");
                    let _ = connection.close().await;
                    gloo_timers::future::TimeoutFuture::new(500).await;
                    let renewed = session(client, cid, path).await?;
                    if renewed.size != size
                        || renewed.provider.endpoint.id != grant.provider.endpoint.id
                        || renewed.provider.protocol != grant.provider.protocol
                    {
                        return Err(error("The serving endpoint changed during download"));
                    }
                    *grant = renewed;
                    *connection = client
                        .dial(
                            &grant.provider.endpoint,
                            grant.provider.protocol.as_bytes(),
                            DirectPreference::PreferWebRtc,
                        )
                        .await
                        .map_err(|_| failure)?;
                }
                Err(failure) => return Err(failure),
            }
        };
        // Buffer at most one range. A failed partial range never reaches disk.
        invoke(sink, "write", &[Uint8Array::from(bytes.as_slice()).into()]).await?;
        offset += length;
        progress.set_value(offset as f64);
        status(&format!(
            "Saving {:.1} of {:.1} MiB from {}…",
            offset as f64 / 1048576.0,
            size as f64 / 1048576.0,
            grant.provider.node
        ));
        if offset == length {
            details(&connection.diagnostics().await?);
        }
    }
    let diagnostics = connection.diagnostics().await?;
    let route = route_label(&diagnostics);
    details(
        &json!({"bytes":size,"node":grant.provider.node,"route":route,"diagnostics":diagnostics}),
    );
    status(&format!(
        "Committing {:.1} MiB from {} · {}",
        size as f64 / 1048576.0,
        grant.provider.node,
        route
    ));
    progress.set_value(size.max(1) as f64);
    Ok(())
}

async fn receive_range(
    connection: &DirectConnection,
    request: &[u8],
    size: u64,
    offset: u64,
    length: u64,
    cancel: &Cell<bool>,
) -> Result<Vec<u8>, JsValue> {
    let stream = connection.open_request(request).await?;
    let mut frame = RangeFrame::new(size, offset, length).map_err(error)?;
    let mut bytes = Vec::with_capacity(length as usize);
    while let Some(chunk) = stream.read_chunk().await? {
        if cancel.get() {
            return Err(error("Download cancelled"));
        }
        bytes.extend_from_slice(frame.push(&chunk).map_err(error)?);
    }
    frame.finish().map_err(error)?;
    Ok(bytes)
}

fn route_label(diagnostics: &Value) -> &'static str {
    if diagnostics["iroh"].as_array().is_some_and(|paths| {
        paths
            .iter()
            .any(|p| p["selected"] == true && p["custom"] == true)
    }) {
        if diagnostics["ice"].as_array().is_some_and(|pairs| {
            pairs.iter().any(|p| {
                p["pair"]["state"] == "succeeded"
                    && p["local"]["candidateType"]
                        .as_str()
                        .is_some_and(|v| v != "relay")
                    && p["remote"]["candidateType"]
                        .as_str()
                        .is_some_and(|v| v != "relay")
            })
        }) {
            "direct WebRTC"
        } else {
            "WebRTC · path details available"
        }
    } else {
        "Iroh relay"
    }
}

async fn api(path: &str, body: Option<Value>) -> Result<Value, JsValue> {
    let init = RequestInit::new();
    init.set_credentials(RequestCredentials::SameOrigin);
    init.set_redirect(RequestRedirect::Error);
    if let Some(body) = body {
        init.set_method("POST");
        init.set_body(&serde_json::to_string(&body).map_err(error)?.into());
        let headers = web_sys::Headers::new()?;
        headers.set("Content-Type", "application/json")?;
        init.set_headers(&headers);
    }
    let response: Response = JsFuture::from(
        web_sys::window()
            .unwrap()
            .fetch_with_str_and_init(path, &init),
    )
    .await?
    .dyn_into()?;
    let status = response.status();
    let text = JsFuture::from(response.text()?)
        .await?
        .as_string()
        .unwrap_or_default();
    if status != 200 {
        return Err(error(format!(
            "Library authorization failed ({status}). Sign in again."
        )));
    }
    if text.len() > 64 * 1024 {
        return Err(error("Library reply exceeds its limit"));
    }
    serde_json::from_str(&text).map_err(error)
}

async fn invoke(object: &JsValue, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let function: Function = Reflect::get(object, &name.into())?.dyn_into()?;
    let array = js_sys::Array::new();
    for arg in args {
        array.push(arg);
    }
    let promise: Promise = function.apply(object, &array)?.dyn_into()?;
    JsFuture::from(promise).await
}

fn message(value: &JsValue) -> String {
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}
