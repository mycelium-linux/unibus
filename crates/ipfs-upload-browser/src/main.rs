use js_sys::Uint8Array;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AbortController, Blob, Event, File, HtmlButtonElement, HtmlInputElement, HtmlProgressElement,
    Request, RequestCredentials, RequestInit, RequestRedirect, Response,
};

type Result<T> = std::result::Result<T, String>;
#[derive(Default)]
struct Ui {
    busy: Cell<bool>,
    paused: Cell<bool>,
    session: RefCell<Option<String>>,
    abort: RefCell<Option<AbortController>>,
    limits: RefCell<Value>,
}
fn element(id: &str) -> web_sys::Element {
    web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(id)
        .unwrap()
}
fn input(id: &str) -> HtmlInputElement {
    element(id).unchecked_into()
}
fn button(id: &str, disabled: bool) {
    element(id)
        .unchecked_into::<HtmlButtonElement>()
        .set_disabled(disabled);
}
fn message(text: &str) {
    element("status").set_text_content(Some(text));
}
fn progress(value: f64) {
    element("progress")
        .unchecked_into::<HtmlProgressElement>()
        .set_value(value);
}
fn js_error(value: JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| "Network request failed. Sign in again if needed, then resume.".into())
}
async fn api(path: &str, body: JsValue, content_type: &str, ui: Option<&Ui>) -> Result<Value> {
    let options = RequestInit::new();
    options.set_method("POST");
    options.set_credentials(RequestCredentials::SameOrigin);
    options.set_redirect(RequestRedirect::Error);
    options.set_body(&body);
    if let Some(ui) = ui {
        let control = AbortController::new().map_err(js_error)?;
        options.set_signal(Some(&control.signal()));
        *ui.abort.borrow_mut() = Some(control);
    }
    let request = Request::new_with_str_and_init(path, &options).map_err(js_error)?;
    request
        .headers()
        .set("Content-Type", content_type)
        .map_err(js_error)?;
    let result = JsFuture::from(web_sys::window().unwrap().fetch_with_request(&request)).await;
    if let Some(ui) = ui {
        ui.abort.borrow_mut().take();
    }
    let response: Response = result.map_err(js_error)?.dyn_into().map_err(js_error)?;
    let text = JsFuture::from(response.text().map_err(js_error)?)
        .await
        .map_err(js_error)?
        .as_string()
        .ok_or("Invalid server response")?;
    if !response.ok() {
        return Err(if response.status() == 401 || response.status() == 403 {
            "Upload access is unavailable. Sign in again and check your upload permission.".into()
        } else {
            text.chars().take(300).collect()
        });
    }
    serde_json::from_str(&text).map_err(|_| "Invalid server response".into())
}
async fn json_api(path: &str, value: &Value, ui: Option<&Ui>) -> Result<Value> {
    api(
        path,
        JsValue::from_str(&value.to_string()),
        "application/json",
        ui,
    )
    .await
}
async fn status() -> Result<Value> {
    json_api("/api/upload/status", &json!({}), None).await
}
fn selected() -> (Vec<File>, bool) {
    let folder = input("folder").files().filter(|f| f.length() > 0);
    let is_folder = folder.is_some();
    let list = folder.or_else(|| input("files").files());
    (
        list.map(|list| (0..list.length()).filter_map(|i| list.get(i)).collect())
            .unwrap_or_default(),
        is_folder,
    )
}
fn check_pause(ui: &Ui) -> Result<()> {
    if ui.paused.get() {
        Err("Upload paused. Select the same files and resume when ready.".into())
    } else {
        Ok(())
    }
}
async fn hash_file(file: &File, chunk: u64, ui: &Ui, done: u64, total: u64) -> Result<String> {
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    let size = file.size() as u64;
    while offset < size {
        check_pause(ui)?;
        let end = (offset + chunk).min(size);
        let blob = file
            .slice_with_f64_and_f64(offset as f64, end as f64)
            .map_err(js_error)?;
        let buffer = JsFuture::from(blob.array_buffer())
            .await
            .map_err(js_error)?;
        hash.update(Uint8Array::new(&buffer).to_vec());
        offset = end;
        progress(if total == 0 {
            100.0
        } else {
            (done + offset) as f64 / total as f64 * 100.0
        });
        message(&format!("Checking files · {}", file.name()));
        gloo_timers::future::TimeoutFuture::new(0).await;
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn complete(value: &Value) -> Result<()> {
    let cid = value["cid"].as_str().ok_or("Missing saved file address")?;
    if cid.len() > 128 || !cid.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Invalid saved file address".into());
    }
    let document = web_sys::window().unwrap().document().unwrap();
    let link = document.create_element("a").map_err(js_error)?;
    link.set_attribute("href", &format!("/files/{cid}"))
        .map_err(js_error)?;
    link.set_text_content(Some("Open your uploaded files →"));
    let result = element("result");
    result.set_text_content(None);
    result.append_child(&link).map_err(js_error)?;
    message("Upload saved. Your files are available in the library.");
    progress(100.0);
    Ok(())
}
async fn poll_save(ui: &Ui) -> Result<()> {
    button("pause", true);
    button("cancel", true);
    message("Checking and saving to IPFS… You can return to this page while saving continues.");
    loop {
        let current = status().await?;
        match current["phase"].as_str().unwrap_or("") {
            "complete" => {
                ui.session.borrow_mut().take();
                return complete(&current["receipt"]);
            }
            "failed" | "interrupted" => {
                return Err(current["error"]
                    .as_str()
                    .unwrap_or("Saving failed. Resume or cancel the saved upload.")
                    .into());
            }
            "saving" => gloo_timers::future::TimeoutFuture::new(1500).await,
            _ => {
                return Err(
                    "Saving status changed. Reload and select the same files to resume.".into(),
                );
            }
        }
    }
}
async fn upload(ui: &Ui) -> Result<()> {
    let current = status().await?;
    if current["phase"] == "saving" {
        return poll_save(ui).await;
    }
    if current["phase"] == "complete"
        && ui.session.borrow().as_deref() == current["session"].as_str()
    {
        ui.session.borrow_mut().take();
        return complete(&current["receipt"]);
    }
    let (files, is_folder) = selected();
    if files.is_empty() {
        return Err("Choose files or a folder first.".into());
    }
    let limits = ui.limits.borrow().clone();
    let max_bytes = limits["max_bytes"]
        .as_u64()
        .ok_or("Upload limits unavailable")?;
    let chunk = limits["chunk_bytes"]
        .as_u64()
        .ok_or("Part size unavailable")?;
    let total = files
        .iter()
        .try_fold(0u64, |sum, file| sum.checked_add(file.size() as u64))
        .ok_or("Files are too large")?;
    if total > max_bytes || files.len() > limits["max_files"].as_u64().unwrap_or(0) as usize {
        return Err("Choose files within the displayed size and file-count limits.".into());
    }
    let first_path = js_sys::Reflect::get(&files[0], &"webkitRelativePath".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    let default_name = if is_folder {
        first_path.split('/').next().unwrap_or("Folder").to_string()
    } else if files.len() == 1 {
        files[0].name()
    } else {
        "Uploaded files".into()
    };
    let name = input("name").value();
    let name = if name.trim().is_empty() {
        default_name
    } else {
        name.trim().into()
    };
    let mut specs = Vec::new();
    let mut checked = 0u64;
    for file in &files {
        let path = if is_folder {
            let relative = js_sys::Reflect::get(file, &"webkitRelativePath".into())
                .map_err(js_error)?
                .as_string()
                .ok_or("Folder paths unavailable")?;
            relative
                .split_once('/')
                .ok_or("Folder paths unavailable")?
                .1
                .to_string()
        } else {
            file.name()
        };
        let digest = hash_file(file, chunk, ui, checked, total).await?;
        checked += file.size() as u64;
        specs.push(json!({"path":path,"size":file.size() as u64,"sha256":digest}));
    }
    check_pause(ui)?;
    let manifest = json!({"name":name,"folder":is_folder||files.len()>1,"files":specs});
    message("Preparing upload or resuming saved parts…");
    let admitted = json_api("/api/upload/begin", &manifest, Some(ui)).await?;
    let id = admitted["session"]
        .as_str()
        .ok_or("Missing upload session")?
        .to_string();
    *ui.session.borrow_mut() = Some(id.clone());
    button("cancel", false);
    let snapshot = status().await?;
    if snapshot["phase"] == "saving" {
        return poll_save(ui).await;
    }
    let offsets = snapshot["offsets"]
        .as_array()
        .ok_or("Missing saved part offsets")?;
    let mut sent = offsets.iter().filter_map(Value::as_u64).sum::<u64>();
    for (index, file) in files.iter().enumerate() {
        let mut offset = offsets
            .get(index)
            .and_then(Value::as_u64)
            .ok_or("Missing file offset")?;
        let size = file.size() as u64;
        while offset < size {
            check_pause(ui)?;
            let end = (offset + chunk).min(size);
            let blob: Blob = file
                .slice_with_f64_and_f64(offset as f64, end as f64)
                .map_err(js_error)?;
            let path = format!("/api/upload/chunk?session={id}&file={index}&offset={offset}");
            let mut last_error = None;
            for attempt in 0..3 {
                match api(
                    &path,
                    blob.clone().into(),
                    "application/octet-stream",
                    Some(ui),
                )
                .await
                {
                    Ok(_) => {
                        last_error = None;
                        break;
                    }
                    Err(error) => {
                        check_pause(ui)?;
                        last_error = Some(error);
                        gloo_timers::future::TimeoutFuture::new(500 * (attempt + 1)).await;
                    }
                }
            }
            if let Some(error) = last_error {
                let stored = status().await?;
                let actual = stored["offsets"][index].as_u64().unwrap_or(offset);
                if actual < end {
                    return Err(error);
                }
            }
            sent += end - offset;
            offset = end;
            progress(if total == 0 {
                100.0
            } else {
                sent as f64 / total as f64 * 100.0
            });
            message(&format!(
                "{:.1}% · {} · {:.1} / {:.1} MiB",
                if total == 0 {
                    100.0
                } else {
                    sent as f64 / total as f64 * 100.0
                },
                specs[index]["path"].as_str().unwrap_or("File"),
                sent as f64 / 1048576.0,
                total as f64 / 1048576.0
            ));
        }
    }
    check_pause(ui)?;
    button("pause", true);
    button("cancel", true);
    let saving = json_api(
        &format!("/api/upload/commit?session={id}&background=true"),
        &json!({}),
        None,
    )
    .await;
    if let Err(error) = saving {
        if !matches!(
            status().await?["phase"].as_str(),
            Some("saving" | "complete")
        ) {
            return Err(error);
        }
    }
    poll_save(ui).await
}

fn main() {
    let ui = Rc::new(Ui::default());
    for (id, other) in [("files", "folder"), ("folder", "files")] {
        let clear = Closure::<dyn FnMut(Event)>::new(move |_| input(other).set_value(""));
        element(id)
            .add_event_listener_with_callback("change", clear.as_ref().unchecked_ref())
            .unwrap();
        clear.forget();
    }
    let state = ui.clone();
    let submit = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        if state.busy.replace(true) {
            return;
        }
        state.paused.set(false);
        button("start", true);
        button("pause", false);
        input("files").set_disabled(true);
        input("folder").set_disabled(true);
        element("result").set_text_content(None);
        let state = state.clone();
        spawn_local(async move {
            if let Err(error) = upload(&state).await {
                message(&error);
            }
            state.busy.set(false);
            button("start", false);
            button("pause", true);
            button("cancel", state.session.borrow().is_none());
            input("files").set_disabled(false);
            input("folder").set_disabled(false);
        });
    });
    element("upload-form")
        .add_event_listener_with_callback("submit", submit.as_ref().unchecked_ref())
        .unwrap();
    submit.forget();
    let state = ui.clone();
    let pause = Closure::<dyn FnMut(Event)>::new(move |_| {
        state.paused.set(true);
        if let Some(control) = state.abort.borrow_mut().take() {
            control.abort();
        }
        message("Pausing… Saved parts are kept for resume.");
    });
    element("pause")
        .add_event_listener_with_callback("click", pause.as_ref().unchecked_ref())
        .unwrap();
    pause.forget();
    let state = ui.clone();
    let cancel = Closure::<dyn FnMut(Event)>::new(move |_| {
        let state = state.clone();
        state.paused.set(true);
        if let Some(control) = state.abort.borrow_mut().take() {
            control.abort();
        }
        let id = state.session.borrow().clone();
        spawn_local(async move {
            if let Some(id) = id {
                match json_api(
                    &format!("/api/upload/cancel?session={id}"),
                    &json!({}),
                    None,
                )
                .await
                {
                    Ok(_) => {
                        state.session.borrow_mut().take();
                        message("Saved upload cancelled. Previously pinned files are kept.");
                        button("cancel", true);
                    }
                    Err(error) => message(&error),
                }
            }
        });
    });
    element("cancel")
        .add_event_listener_with_callback("click", cancel.as_ref().unchecked_ref())
        .unwrap();
    cancel.forget();
    spawn_local(async move {
        match status().await {
            Ok(value) => {
                *ui.limits.borrow_mut() = value["limits"].clone();
                element("limits").set_text_content(Some(&format!(
                    "Up to {:.0} GiB and {} files per upload. Transfers use {} MiB parts.",
                    value["limits"]["max_bytes"].as_u64().unwrap_or(0) as f64 / 1073741824.0,
                    value["limits"]["max_files"],
                    value["limits"]["chunk_bytes"].as_u64().unwrap_or(0) / 1048576
                )));
                *ui.session.borrow_mut() = value["session"].as_str().map(str::to_string);
                button("start", false);
                button("cancel", ui.session.borrow().is_none());
                if value["phase"] == "saving" {
                    ui.busy.set(true);
                    button("start", true);
                    if let Err(e) = poll_save(&ui).await {
                        message(&e);
                    }
                    ui.busy.set(false);
                    button("start", false);
                } else if value["phase"] == "complete" {
                    ui.session.borrow_mut().take();
                    button("cancel", true);
                    if let Err(e) = complete(&value["receipt"]) {
                        message(&e);
                    }
                } else {
                    message(if ui.session.borrow().is_some() {
                        "An unfinished upload is saved. Select the same files to resume, or cancel it."
                    } else {
                        "Choose files or a folder to start."
                    });
                }
            }
            Err(error) => message(&error),
        }
    });
}
