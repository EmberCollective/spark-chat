//! 安卓系统文档授权只在原生层使用，不把文件内容或密钥送入页面。
use super::{Backend, Request};
use crate::vault::{Vault, VaultError};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use tauri::{
    plugin::{Builder, PluginHandle, TauriPlugin},
    Manager, Wry,
};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::{FilePath, FsExt, OpenOptions};

struct Documents(PluginHandle<Wry>);
#[derive(Deserialize)]
struct DocumentName {
    name: String,
}
#[derive(Deserialize)]
struct DocumentSpace {
    bytes: u64,
}

pub fn metadata_plugin() -> TauriPlugin<Wry> {
    Builder::new("spark-documents")
        .setup(|app, api| {
            app.manage(Documents(
                api.register_android_plugin("cn.sparknet.chat", "DocumentInfo")?,
            ));
            Ok(())
        })
        .build()
}

struct FileTask(tauri::AppHandle);
impl FileTask {
    fn begin(handle: &tauri::AppHandle) -> Result<Self, String> {
        let _: Value = handle
            .state::<Documents>()
            .0
            .run_mobile_plugin("beginPicker", json!({}))
            .map_err(|e| e.to_string())?;
        Ok(Self(handle.clone()))
    }
}
impl Drop for FileTask {
    fn drop(&mut self) {
        let _: Result<Value, _> = self
            .0
            .state::<Documents>()
            .0
            .run_mobile_plugin("endPicker", json!({}));
    }
}
fn document_uri(value: &str) -> Result<FilePath, String> {
    let url = tauri::Url::parse(value).map_err(|_| "请重新选择钥匙文件")?;
    if url.scheme() != "content" {
        return Err("请使用系统文件选择器选择文件".into());
    }
    Ok(FilePath::Url(url))
}
fn write_document(handle: &tauri::AppHandle, path: FilePath, data: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    let mut file = handle.fs().open(path, options)?;
    file.write_all(data)?;
    file.flush()
}

pub fn dispatch(
    handle: &tauri::AppHandle,
    backend: &Backend,
    request: &Request,
) -> Option<Result<Value, String>> {
    match request {
        Request::PickKeyfile { create } => Some(Ok({
            let dialog = handle.dialog().file().set_file_name("spark.keyfile");
            let path = if *create {
                dialog.blocking_save_file()
            } else {
                dialog.blocking_pick_file()
            };
            json!(path.map(|p| p.to_string()))
        })),
        Request::Authenticate {
            passphrase,
            keyfile,
            create,
        } => Some((|| {
            let path = document_uri(keyfile)?;
            let mut app = backend.lock().map_err(|_| "聊天服务暂时不可用")?;
            app.authenticate_with(passphrase, keyfile, *create, |dir| {
                if *create {
                    Vault::create_with_writer(passphrase, dir, |content| {
                        write_document(handle, path, content.as_bytes()).map_err(VaultError::Io)
                    })
                    .map(|v| v.0)
                } else {
                    Vault::unlock_with_reader(passphrase, dir, || {
                        let mut opts = OpenOptions::new();
                        opts.read(true);
                        let mut text = String::new();
                        handle
                            .fs()
                            .open(path, opts)?
                            .take(4097)
                            .read_to_string(&mut text)?;
                        if text.len() > 4096 {
                            return Err(VaultError::MissingKeyfile);
                        }
                        Ok(text)
                    })
                }
                .map_err(|e| format!("{e}。若文件授权已失效，请重新选择钥匙文件。"))
            })
            .map(|v| json!(v))
        })()),
        Request::SendFile => Some((|| {
            let _task = FileTask::begin(handle)?;
            let Some(path) = handle.dialog().file().blocking_pick_file() else {
                return Ok(Value::Null);
            };
            let info: DocumentName = handle
                .state::<Documents>()
                .0
                .run_mobile_plugin("metadata", json!({"uri": path.to_string()}))
                .map_err(|e| format!("无法读取文件名称：{e}"))?;
            let name = std::path::Path::new(&info.name)
                .file_name()
                .and_then(|v| v.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("文件")
                .to_owned();
            let mut options = OpenOptions::new();
            options.read(true);
            let file = handle.fs().open(path, options).map_err(|e| e.to_string())?;
            backend
                .lock()
                .map_err(|_| "聊天服务暂时不可用")?
                .send_opened(name, file)?;
            Ok(Value::Null)
        })()),
        Request::SaveFile { id } => Some((|| {
            let _task = FileTask::begin(handle)?;
            let id: u64 = id.parse().map_err(|_| "文件编号无效")?;
            let name = backend
                .lock()
                .map_err(|_| "聊天服务暂时不可用")?
                .received_name(id)
                .ok_or("文件已不在内存，请让对方重新发送")?;
            let name = std::path::Path::new(&name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("收到的文件");
            let Some(path) = handle
                .dialog()
                .file()
                .set_file_name(name)
                .blocking_save_file()
            else {
                return Ok(Value::Null);
            };
            let location = path.to_string();
            let mut options = OpenOptions::new();
            options.write(true).truncate(true);
            let file = handle
                .fs()
                .open(path.clone(), options)
                .map_err(|e| e.to_string())?;
            let app_handle = handle.clone();
            let cleanup_uri = location.clone();
            let target = crate::transfer::ReceiveTarget::document(file, move || {
                let result: Result<Value, _> = app_handle
                    .state::<Documents>()
                    .0
                    .run_mobile_plugin("remove", json!({"uri": cleanup_uri}));
                if result.is_err() {
                    let _ = tauri::Emitter::emit(
                        &app_handle,
                        "spark-event",
                        crate::ui::UiEvent::Error(
                            "未完成的文件未能删除，请在所选目录手动删除".into(),
                        ),
                    );
                }
            });
            let size = backend
                .lock()
                .map_err(|_| "聊天服务暂时不可用")?
                .check_receive_location(id, &location)?;
            let info: Result<DocumentSpace, _> = handle
                .state::<Documents>()
                .0
                .run_mobile_plugin("space", json!({"uri": location}));
            if !info
                .as_ref()
                .is_ok_and(|s| s.bytes >= size.saturating_add(16 * 1024 * 1024))
            {
                return Err("保存位置空间不足或无法确认可用空间，请换一个位置".into());
            }
            backend
                .lock()
                .map_err(|_| "聊天服务暂时不可用")?
                .accept_received(id, target, location)?;
            Ok(json!(path.to_string()))
        })()),
        Request::OpenFile { id } => Some((|| {
            let id = id.parse().map_err(|_| "文件编号无效")?;
            let uri = backend
                .lock()
                .map_err(|_| "聊天服务暂时不可用")?
                .saved_location(id)?;
            let value: Value = handle
                .state::<Documents>()
                .0
                .run_mobile_plugin("open", json!({"uri": uri}))
                .map_err(|e| format!("无法打开文件：{e}"))?;
            Ok(value)
        })()),
        _ => None,
    }
}
