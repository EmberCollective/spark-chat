# 星星之火 · Spark Chat

校园网内的点对点加密聊天客户端。界面采用 Telegram 浅色聊天布局，使用用户提供的黑色火焰 Logo；Rust 负责网络、身份与加密存储，Tauri 2 使用系统 WebView 显示本地 HTML/CSS。

当前版本：**0.1.1 预览版**。本轮更新见 [CHANGELOG.md](CHANGELOG.md)。

## 使用

Windows x64 预览安装包为 `delivery/Spark-Chat-0.1.1-Windows-x64-Setup-preview.exe`，双击后按向导安装；默认安装到当前用户目录，缺失 WebView2 时需联网补装。包已构建并检查完整性，但未做 Windows 实机运行验收，也未签名。详细范围见 `delivery/0.1.1验收记录.txt`。

Mac 预览包 `delivery/Spark-Chat-0.1.1-macOS-preview.zip` 解压后打开 `Spark Chat.app`。第一次使用时设置口令并另存钥匙文件；以后用这两项解锁。请单独备份钥匙文件，遗失身份密钥后无法恢复原来的记录。

一人点“创建聊天”并分享连接码，另一人点“加入聊天”粘贴。宿舍路由器后的用户通常应加入校园骨干网用户创建的聊天。当前只支持一位对方，不提供打洞或中继。

连接后可以发文字、选择或拖入一个文件。收到文件先点“接收并保存…”选择位置，逐块写入并校验成功后可点“打开文件”，由系统应用打开。可取消接收；断线或失败会清理未完成内容，需重新发送。桌面端已有文件在接收成功前保持原样。

聊天页左上角可以返回主页，再从“返回聊天记录”继续，身份与当前连接保持解锁；已有连接时不重复创建或加入。Android 系统返回键也先回主页。锁定会断开网络、取消未完成的传输并清理会话显示；已经保存的文件仍在所选位置。

## 范围

- 界面、脚本、图标均随应用打包；没有远程网页、在线字体或前端框架。
- 身份私钥和会话密钥保留在 Rust；页面只得到显示所需的公开身份、消息和状态。
- 联系人头像由公开身份末位生成字母与配色，不是身份核验凭据。Noise 提供通道加密；原来的短码不再被界面称为安全核对码。
- 保留原有 vault 目录及加密文件格式。旧版用身份末 8 位保存的记录，在公钥一致时复制到完整身份路径，原件保留。
- 文字上限 1 MiB；文件按 256 KiB 分块读写，取消原 64 MiB 硬限制，一次发送一份、最多两份等待或接收任务。接收前确认大小并选择保存位置，检查可用空间并留出 16 MiB；写入失败或取消时清理未完成内容。选择位置约有 5 分钟，传输停滞约 2 分钟后超时；失败需重发，不支持断点续传。Android 目前要求文档提供方能报告本地可用空间，不支持无法检查空间的云端目的地。
- 聊天双方建议同时升级。新版可读取旧分组连接码，但未保证与旧客户端的所有文件传输场景互通。
- 当前交付为 Mac 本地预览，仅作本地签名，未经 Apple 公证。Windows、Linux 与旧版 macOS 尚未进行实际设备验收。
- Windows 目标为 Windows 10/11，需 Microsoft WebView2；安装配置会在缺失时引导安装。macOS 配置最低 10.15；这不是对所有旧版系统均已测试的声明。

## 开发与验证

需要 Rust 和对应平台的构建工具。前端没有 npm 依赖或构建步骤。

```sh
cargo check --all-targets
cargo test --all-targets
cargo run
npm exec --yes --package @tauri-apps/cli -- tauri build --debug --bundles app
```

在本机 macOS 交叉构建 Windows x64 NSIS 安装包，需要 LLVM、LLD、NSIS、Windows Rust target 和 cargo-xwin。该项目直接使用 Tauri 的 VC runtime 链接设置，不要额外强制完整静态 CRT。

```sh
PATH=/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/lld/bin:$PATH \
XWIN_CACHE_DIR="$PWD/target/xwin-cache" \
npm exec --yes --package @tauri-apps/cli -- tauri build \
  --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis
```

`tests/net_e2e.rs` 使用真实 libp2p 双节点，覆盖连续有序文字、双向分块文件、空文件和空闲后继续聊天。`tests/ui-check.js` 在浏览器内使用隔离的后端替身检查界面；它不会被打包到客户端。

Debug 构建可用 `SPARK_TEST_DATA_DIR` 指向一次性测试目录；发行优化构建忽略此变量。不要使用真实钥匙库做自动界面测试。

原 egui 源码基线保存在本机 `.agents/backups/egui-baseline-20261025.tar.gz`。已按用户授权建立本地 Git 仓库，以 `v0.1.1` 记录本轮源码；未配置远端或推送，未运行原生 GitHub Code Review/Security Review。

参考：[Telegram 主题](https://telegram.org/blog/chat-themes-interactive-emoji-read-receipts)、[Tauri 进程模型](https://tauri.app/concept/process-model/)、[各平台前置要求](https://v2.tauri.app/start/prerequisites/)。


## Android 手机预览版

安装包：`delivery/Spark-Chat-0.1.1-Android-arm64-preview.apk`。

界面参考 Telegram Android：单列对话、40px 聊天头像（自己显示“我”）、48px 触控区，支持长消息和文件名换行。原生窗口让出状态栏、挖孔和软键盘区域；手机回车换行，点击发送按钮发送。

- 目标：Android 8.0+、ARM64；依赖系统 Android WebView，无远程页面或字体。
- 安卓使用系统文件选择器创建/选择钥匙文件、选择发送附件、保存收到的文件。钥匙文件内容不进入网页，文件发送直接使用已授权句柄，不额外缓存明文。请备份钥匙文件；重新启动后若文件授权失效，重新选择同一个文件。
- 选择或查看文件时使用最多 5 分钟的短时前台任务，避免系统在后台收回连接；返回聊天后自动停止。普通聊天仍需保持前台，尚未实现后台常驻、推送或穿透中继。接入可互通的校园网/Wi-Fi 后，创建或加入聊天。
- 本地私有数据放在 Android 应用数据目录；关闭系统应用备份。卸载会清除本机加密库，不能只靠钥匙文件恢复聊天记录。
- APK、实际验收结论、哈希与源码指纹见 `delivery/Android验收记录.txt`；未测的平台或设备不可视为通过。旧 vault 风险仍见 `.agents/docs/审查-20261025.txt`。

构建环境：JDK 21、Android SDK 36、NDK 29、Rust `aarch64-linux-android`。原生项目 `gen/android` 是维护中的源码；不要用 `android init` 覆盖 `MainActivity.kt`、`DocumentInfo.kt`、Manifest 或 Gradle 配置。

```bash
npm ci
ANDROID_HOME="$HOME/Library/Android/sdk" \
NDK_HOME="$HOME/Library/Android/sdk/ndk/29.0.14206865" \
npm run tauri -- android build --apk --target aarch64
```

release APK 需用 Android 官方 `zipalign`/`apksigner` 签名后才能侧载。签名密钥与密码必须保存在项目以外，不进入安装包或 Git。本机预览签名保存在 `~/.android/spark-chat-preview.jks`，仅供预览更新沿用；它不是商店发布身份。

本轮文件流程修复的三个预览包、源码指纹与实际验收边界见 `delivery/文件流程修复验收记录.txt`。双方建议同时使用本轮版本；旧版接收端仍会拒绝超过 64 MiB 的文件。历史记录的加密格式未变；离线历史的独立界面入口本轮未新增。

0.1.1 的产物版本、哈希与本次构建检查见 `delivery/0.1.1验收记录.txt`；此前文件流程完整验收仅对应原清单中的0.1.0修复候选，不自动视为新包全部重测通过。
