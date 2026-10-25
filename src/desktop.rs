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
    },
    PickKeyfile {
        create: bool,
    },
    SendFile,
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

#[tauri::command]
async fn dispatch(
    handle: tauri::AppHandle,
    state: tauri::State<'_, Backend>,
    request: Request,
) -> Result<Value, String> {
    let backend = state.inner().clone();
    // 原生对话框、口令派生与磁盘写入都不占用 WebView 的界面线程。
    tauri::async_runtime::spawn_blocking(move || -> Result<Value, String> {
        #[cfg(target_os = "android")]
        if let Some(result) = android::dispatch(&handle, &backend, &request) {
            return result;
        }
        #[cfg(not(target_os = "android"))]
        let _ = handle;
        match request {
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
            Request::SendFile => {
                let Some(path) = rfd::FileDialog::new().pick_file() else {
                    return Ok(Value::Null);
                };
                backend
                    .lock()
                    .map_err(|_| "聊天服务暂时不可用")?
                    .send_path(&path)?;
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
            Request::Text { text } => app.text(&text).map(|v| json!(v)),
            _ => unreachable!("原生对话框分支已返回"),
        }
    })
    .await
    .map_err(|_| "操作未完成，请重试".to_owned())?
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
            if let Some(state) = handle.try_state::<Backend>() {
                if let Ok(mut app) = state.lock() {
                    app.lock();
                }
            }
        }
    });
    Ok(())
}
