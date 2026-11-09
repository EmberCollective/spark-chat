//! 本地 WebView 只接触显示数据；身份私钥和会话密钥始终留在 Rust。
use crate::app::App;
use crate::ui::{AppHooks, UiEvent};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tauri::{Emitter, Manager};

#[cfg(target_os = "android")]
#[path = "android.rs"]
mod android;

#[derive(Clone, serde::Serialize)]
struct RevisionedEvent<'a> {
    revision: u64,
    #[serde(flatten)]
    event: &'a UiEvent,
}

type Backend = Arc<Mutex<App>>;
type ForumBackend = Arc<Mutex<Option<crate::forum::hub::ForumHub>>>;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Initial,
    Authenticate {
        passphrase: String,
        keyfile: String,
        create: bool,
    },
    Lock,
    Listen,
    Connect {
        code: String,
    },
    Text {
        text: String,
        session_id: Option<String>,
    },
    SelectSession {
        session_id: String,
    },
    PrepareImage {
        data: String,
    },
    Image {
        session_id: String,
        data: String,
    },
    ImagePreview {
        session_id: String,
        id: String,
    },
    SaveImage {
        session_id: String,
        id: String,
    },
    Forum {
        command: Value,
    },
    ForumAttach {
        community_id: String,
    },
    ForumSave {
        community_id: String,
        hash: String,
        name: String,
        size: u64,
    },
    ForumPreview {
        community_id: String,
        hash: String,
    },
    PickKeyfile {
        create: bool,
    },
    SendFile {
        session_id: Option<String>,
    },
    OpenLink {
        url: String,
    },
    OpenFile {
        id: String,
    },
    CancelFile {
        id: String,
    },
    SaveFile {
        id: String,
    },
}

fn ensure_community(forum: &Option<crate::forum::hub::ForumHub>, id: &str) -> Result<(), String> {
    if forum.as_ref().ok_or("论坛尚未启动")?.current_id() != id {
        return Err("社区已切换，请重新操作".into());
    }
    Ok(())
}

fn validated_link(value: &str) -> Result<tauri::Url, String> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("链接格式无效".into());
    }
    let url = tauri::Url::parse(value).map_err(|_| "链接格式无效")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("仅支持不含登录凭据的 HTTP/HTTPS 链接".into());
    }
    Ok(url)
}

#[tauri::command]
async fn dispatch(
    handle: tauri::AppHandle,
    state: tauri::State<'_, Backend>,
    forum_state: tauri::State<'_, ForumBackend>,
    request: Request,
) -> Result<Value, String> {
    let backend = state.inner().clone();
    let forum_backend = forum_state.inner().clone();
    // 原生对话框、口令派生与磁盘写入都不占用 WebView 的界面线程。
    tauri::async_runtime::spawn_blocking(move || -> Result<Value, String> {
        if let Request::Forum { mut command } = request {
            use zeroize::Zeroize;
            let app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
            let (mut seed, directory) = app.forum_material()?;
            let mut forum = forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?;
            if forum.is_none() {
                let mut storage_key = [0u8; 32];
                hkdf::Hkdf::<sha2::Sha256>::new(None, &seed)
                    .expand(b"SparkNet/forum/local-storage/v1", &mut storage_key).map_err(|_| "无法初始化论坛存储")?;
                let started = crate::forum::hub::ForumHub::start(seed, storage_key, directory.join("forum"));
                seed.zeroize();
                storage_key.zeroize();
                *forum = Some(started.map_err(|e| e.to_string())?);
            }
            seed.zeroize();
            drop(app);
            let object = command.as_object_mut().ok_or("论坛操作无效")?;
            if let Some(kind) = object.remove("kind") { object.insert("action".into(), kind); }
            let expected = object.remove("community_id");
            let expected = match &expected { Some(value) => Some(value.as_str().ok_or("社区标识格式无效")?), None => None };
            let unscoped = matches!(object.get("action").and_then(Value::as_str), Some("state" | "community_list"));
            forum.as_ref().ok_or("论坛尚未启动")?.check_scope(expected, unscoped).map_err(|e|e.to_string())?;
            let command = serde_json::from_value(command).map_err(|_| "论坛操作格式无效")?;
            return forum.as_ref().ok_or("论坛尚未启动")?.handle(command).map_err(|e| e.to_string());
        }
        if matches!(&request, Request::Lock) {
            // Use the same lock order as forum startup; no forum operation outlives lock completion.
            let mut app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
            forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?.take();
            app.lock();
            return Ok(Value::Null);
        }
        #[cfg(target_os = "android")]
        if let Some(result) = android::dispatch(&handle, &backend, &forum_backend, &request) {
            return result;
        }
        #[cfg(not(target_os = "android"))]
        let _ = handle;
        match request {
            Request::ForumPreview { ref hash, ref community_id } => {
                use base64::Engine;
                let app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
                app.forum_material()?;
                let forum = forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?;
                ensure_community(&forum, community_id)?;
                drop(app);
                let result = forum.as_ref().ok_or("论坛尚未启动")?.read_preview(hash.clone()).map_err(|e| e.to_string())?;
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(result["bytes"].as_str().ok_or("预览数据无效")?).map_err(|_| "预览数据无效")?;
                let image = crate::images::validate_and_normalize(&bytes).map_err(|e| e.to_string())?;
                return Ok(json!({"mime":image.mime,"data":base64::engine::general_purpose::STANDARD.encode(image.bytes)}));
            }
            #[cfg(not(target_os = "android"))]
            Request::ForumAttach { ref community_id } => {
                ensure_community(&*forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?, community_id)?;
                let Some(path) = rfd::FileDialog::new().pick_file() else { return Ok(Value::Null); };
                let app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
                app.forum_material()?;
                let forum = forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?;
                ensure_community(&forum, community_id)?;
                drop(app);
                let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                let name = path.file_name().and_then(|s|s.to_str()).unwrap_or("附件").to_owned();
                return forum.as_ref().ok_or("请先加入论坛")?.attach(name,file).map_err(|e|e.to_string());
            }
            #[cfg(not(target_os = "android"))]
            Request::ForumSave { ref hash, ref name, size, ref community_id } => {
                if size > 20_000_000 { return Err("附件超过大小限制".into()); }
                let name = std::path::Path::new(name).file_name().and_then(|s|s.to_str()).unwrap_or("附件");
                ensure_community(&*forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?, community_id)?;
                let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() else { return Ok(Value::Null); };
                let app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
                app.forum_material()?;
                let forum = forum_backend.lock().map_err(|_| "论坛服务暂时不可用")?;
                ensure_community(&forum, community_id)?;
                drop(app);
                let target = crate::transfer::ReceiveTarget::path(&path,size).map_err(|e|e.to_string())?;
                forum.as_ref().ok_or("请先加入论坛")?.export(hash.clone(),target.file.try_clone().map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
                target.commit().map_err(|e|e.to_string())?;
                return Ok(json!(path.to_string_lossy()));
            }
            #[cfg(not(target_os = "android"))]
            Request::PickKeyfile { create } => {
                let dialog = rfd::FileDialog::new().set_file_name("spark.keyfile");
                let path = if create {
                    dialog.save_file()
                } else {
                    dialog.pick_file()
                };
                return Ok(json!(path.map(|p| p.to_string_lossy().into_owned())));
            }
            #[cfg(not(target_os = "android"))]
            Request::SaveImage { ref session_id, ref id } => {
                use std::io::Write;
                let extension = {
                    let app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
                    let bytes = image_bytes(&app, session_id, id)?;
                    if image::guess_format(&bytes).ok() == Some(image::ImageFormat::Png) { "png" } else { "jpg" }
                };
                let Some(path) = rfd::FileDialog::new().set_file_name(format!("聊天图片.{extension}")).save_file() else { return Ok(Value::Null); };
                let bytes = image_bytes(&*backend.lock().map_err(|_| "聊天服务暂时不可用")?, session_id, id)?;
                let mut target = crate::transfer::ReceiveTarget::path(&path, bytes.len() as u64).map_err(|e| e.to_string())?;
                target.file.write_all(&bytes).map_err(|e| e.to_string())?;
                target.commit().map_err(|e| e.to_string())?;
                return Ok(json!(path.to_string_lossy()));
            }
            #[cfg(not(target_os = "android"))]
            Request::SendFile { ref session_id } => {
                let target = session_id.clone().unwrap_or_else(|| {
                    backend.lock().map(|app| app.initial_state().peer_id).unwrap_or_default()
                });
                let Some(path) = rfd::FileDialog::new().pick_file() else {
                    return Ok(Value::Null);
                };
                backend
                    .lock()
                    .map_err(|_| "聊天服务暂时不可用")?
                    .send_path_to(&target, &path)?;
                return Ok(Value::Null);
            }
            #[cfg(not(target_os = "android"))]
            Request::SaveFile { id } => {
                let id: u64 = id.parse().map_err(|_| "文件编号无效")?;
                let name = backend
                    .lock()
                    .map_err(|_| "聊天服务暂时不可用")?
                    .received_name(id)
                    .ok_or("文件已不在内存，请让对方重新发送")?;
                let name = std::path::Path::new(&name)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("收到的文件")
                    .to_owned();
                let dialog = rfd::FileDialog::new().set_file_name(&name);
                let Some(path) = dialog.save_file() else {
                    return Ok(Value::Null);
                };
                backend
                    .lock()
                    .map_err(|_| "聊天服务暂时不可用")?
                    .save_received(id, &path)?;
                return Ok(json!(path.to_string_lossy()));
            }
            #[cfg(not(target_os = "android"))]
            Request::OpenLink { url } => {
                let url = validated_link(&url)?;
                #[cfg(target_os = "macos")]
                let mut command = std::process::Command::new("/usr/bin/open");
                #[cfg(target_os = "windows")]
                let mut command = std::process::Command::new("explorer.exe");
                #[cfg(target_os = "linux")]
                let mut command = std::process::Command::new("xdg-open");
                if !command.arg(url.as_str()).status().map_err(|e| e.to_string())?.success() {
                    return Err("系统未能打开浏览器".into());
                }
                return Ok(Value::Null);
            }
            #[cfg(not(target_os = "android"))]
            Request::OpenFile { id } => {
                let id = id.parse().map_err(|_| "文件编号无效")?;
                let location = backend
                    .lock()
                    .map_err(|_| "聊天服务暂时不可用")?
                    .saved_location(id)?;
                let path = std::fs::canonicalize(&location)
                    .map_err(|_| "文件已移动或删除，请在保存目录查找")?;
                if !path.is_file() {
                    return Err("保存位置已不是文件".into());
                }
                #[cfg(target_os = "macos")]
                let mut command = std::process::Command::new("/usr/bin/open");
                #[cfg(target_os = "windows")]
                let mut command = std::process::Command::new("explorer.exe");
                #[cfg(target_os = "linux")]
                let mut command = std::process::Command::new("xdg-open");
                #[cfg(target_os = "macos")]
                command.arg("--");
                let status = command
                    .arg(path)
                    .status()
                    .map_err(|e| format!("无法打开文件：{e}"))?;
                if !status.success() {
                    return Err("系统未能打开文件，请确认已安装对应应用".into());
                }
                return Ok(Value::Null);
            }
            _ => {}
        }
        let mut app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
        match request {
            Request::Initial => Ok(json!(app.initial_state())),
            Request::Authenticate {
                passphrase,
                keyfile,
                create,
            } => app
                .authenticate(&passphrase, &keyfile, create)
                .map(|v| json!(v)),
            Request::Lock => {
                app.lock();
                Ok(Value::Null)
            }
            Request::CancelFile { id } => {
                app.cancel_received(id.parse().map_err(|_| "文件编号无效")?)?;
                Ok(Value::Null)
            }
            Request::Listen => {
                app.listen()?;
                Ok(Value::Null)
            }
            Request::Connect { code } => {
                app.connect(&code)?;
                Ok(Value::Null)
            }
            Request::Text { text, session_id } => match session_id {
                Some(id) => app.text_to(&id, &text).map(|v| json!(v)),
                None => app.text(&text).map(|v| json!(v)),
            },
            Request::SelectSession { session_id } => app.select_session(&session_id).map(|v| json!(v)),
            Request::PrepareImage { data } => {
                use base64::Engine;
                if app.initial_state().my_id.is_empty() { return Err("请先解锁".into()); }
                if data.len() > 6_666_668 { return Err("图片超过 5 MB".into()); }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "图片数据无效")?;
                let image = crate::images::validate_and_normalize(&bytes).map_err(|e| e.to_string())?;
                Ok(json!({"mime": image.mime, "data": base64::engine::general_purpose::STANDARD.encode(image.bytes)}))
            }
            Request::Image { session_id, data } => {
                use base64::Engine;
                if data.len() > 6_666_668 { return Err("图片超过 5 MB".into()); }
                let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "图片数据无效")?;
                app.image_to(&session_id, &bytes).map(|v| json!(v))
            }
            Request::ImagePreview { session_id, id } => app.image_preview(&session_id, &id).map(|v| json!(v)),
            _ => unreachable!("原生对话框分支已返回"),
        }
    })
    .await
    .map_err(|_| "操作未完成，请重试".to_owned())?
}

fn image_bytes(app: &App, peer: &str, id: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(app.image_preview(peer, id)?)
        .map_err(|_| "图片缓存数据无效".into())
}

pub fn run() -> anyhow::Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let builder = tauri::Builder::default();
    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(android::metadata_plugin());
    let app = builder
        .invoke_handler(tauri::generate_handler![dispatch])
        // 主 WebviewWindow 的拖放由运行时合成为 WindowEvent；不是子 WebviewEvent。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::DragDrop(event) = event {
                match event {
                    tauri::DragDropEvent::Enter { .. } | tauri::DragDropEvent::Over { .. } => {
                        let _ = window.emit("spark-drag", true);
                    }
                    tauri::DragDropEvent::Leave => {
                        let _ = window.emit("spark-drag", false);
                    }
                    tauri::DragDropEvent::Drop { paths, .. } => {
                        let _ = window.emit("spark-drag", false);
                        let result = if paths.len() != 1 {
                            Err("请一次拖入一个文件".to_owned())
                        } else if let Some(state) = window.try_state::<Backend>() {
                            state
                                .lock()
                                .map_err(|_| "聊天服务暂时不可用".to_owned())
                                .and_then(|mut app| app.send_path(&paths[0]))
                        } else {
                            Err("请先解锁并连接好友".into())
                        };
                        if let Err(e) = result {
                            let _ = window.emit("spark-event", UiEvent::Error(e));
                        }
                    }
                    _ => {}
                }
            }
        })
        .setup(move |app| {
            #[cfg(target_os = "android")]
            let backend = App::with_dir(app.path().app_data_dir()?);
            #[cfg(not(target_os = "android"))]
            let backend = App::new();
            app.manage(Arc::new(Mutex::new(backend)) as Backend);
            app.manage(Arc::new(Mutex::new(None)) as ForumBackend);
            let state = app.state::<Backend>().inner().clone();
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("spark-ui-events".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(mut app) = state.lock() {
                            let events = app.poll_events();
                            let revision = app.event_revision();
                            for event in events {
                                let _ = handle.emit_to(
                                    "main",
                                    "spark-event",
                                    RevisionedEvent {
                                        revision,
                                        event: &event,
                                    },
                                );
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })?;
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run(move |handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            stopped.store(true, Ordering::Relaxed);
            if let Some(state) = handle.try_state::<ForumBackend>() {
                if let Ok(mut forum) = state.lock() {
                    forum.take();
                }
            }
            if let Some(state) = handle.try_state::<Backend>() {
                if let Ok(mut app) = state.lock() {
                    app.lock();
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod link_tests {
    #[test]
    fn external_links_only_allow_web_addresses() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:secret@example.com",
            "https://example.com/\n",
        ] {
            assert!(super::validated_link(bad).is_err());
        }
        assert!(super::validated_link("https://example.com/路径?a=1").is_ok());
    }
}
