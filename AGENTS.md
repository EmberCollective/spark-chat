# 星星之火（SparkNet）项目根规则 —— ~/opt/spark-chat

校园网内去中心化加密通信网络。终局形态：E2EE 通信、无总站、内容打不死（人人可托管密文）、持钥准入、P2P 作为路由备胎。当前载体是 spark-chat 桌面客户端、Android 预览版 + gated-web mTLS 门户。

## 项目组成

| 路径 | 角色 |
|---|---|
| `~/opt/spark-chat/` | **主产品**：Rust 桌面 / Android P2P 加密聊天（Tauri 2 + HTML/CSS + libp2p） |
| `~/opt/../.zcode/workspace/draft-*/gated-web/` | mTLS 持钥准入网站（Caddy + 自建 CA） |
| 同工作区 `p2p-lab/ dh-lab/` | 教学实验区（手写 DH、攻击复现），**永不进产品** |
| 详细交接状态 | `.agents/docs/交接.md`（先读它再动手） |

## 硬性边界（违反即返工，没有例外）

1. **禁止手搓密码学**：加密、密钥交换、协议设计一律用成熟库（libp2p/Noise/TLS/rustls/openssl/cryptography 库）。手写实现只允许存在于 `dh-lab/` 等教学目录，永远不允许进入 `spark-chat/` 或任何产品路径。
2. **UI 只参考不发明**：交互与布局对标成熟产品（桌面参考 Telegram Desktop，手机参考 Telegram Android，Signal 为辅）；所有新界面先指明参考来源。中文显示是硬要求（先确认系统 CJK 字体与中文实显，不引入远程字体）。手机验证单列布局、40px 聊天头像（自己显示“我”）、48px 触控区、长文本、软键盘和安全区；手机回车换行。验证 Activity / 页面重建后恢复聊天与待保存附件入口，不得把重建等同于系统杀进程后的恢复。
3. **密钥永不入 git**：任何私钥（`*.key`、keyfile、`friend.p12`、`分发口令.txt`、vault 数据目录、seed）禁止提交。**新仓库第一步是写 `.gitignore`**，提交前必须检查 `git status` 无密钥文件。密钥分发给成员走双通道（文件与口令不同路）。
4. **未用的机制显式关死**：打洞/STUN/relay 等当前不启用的能力，必须在代码里留注释指明开关位置（例：net.rs 中 STUN 配置行），不做黑盒默认。
5. **计划先行**：非平凡改动先出计划审批（plan mode）；关键分歧用选择题问用户。
6. **教学风格**：面向用户解释用白话+类比+实测，图表用 Mermaid；术语堆砌会被打回。
7. **钥匙文件按完整流程验收**：解锁选择已有文件，不走另存为。系统选择器返回 URI 或创建同名文件不代表已写入成功；首建须验证文件非空且可重新解锁，失败时不得声称已完成。空文件、错误文件与授权失效分别定位，不让用户反复创建同名副本；不擅删测试或用户文件。Android 的 content URI 由原生层读取，钥匙内容不得进入网页或日志。
8. **交付结论分层**：分别记录构建、最终安装包实际运行、跨端通信和用户验收；绑定产物哈希与对应源码。模拟器通过不能代表实体手机通过，独立测试节点通过不能代表用户桌面 GUI 已验收。签名材料、测试身份及测试口令不进入提交或安装包；未验证项与已知风险须写入交付记录。
9. **运行状态现场核对**：模拟器、服务、转发与连接码都是临时状态；操作或告知用户前重新核对，交接记录注明核查时间与失效条件。重启或锁定后重新确认连接码及转发。测试结束保留用户要求继续测试的环境，不擅自关闭用户客户端；不能承诺测试进程永久运行。

## spark-chat 架构契约（改代码前必读）

```
protocol.rs   唯一共享类型源（线上帧 + 连接码），签名不得擅改
vault.rs      密钥金字塔：口令⊕keyfile 解锁身份私钥 → ECDH 派生会话 DEK → AEAD 密文落盘
net.rs        libp2p request-response + 后台线程，与 UI 只走 mpsc（NetCmd/NetEvent）
transfer.rs   接收目标与未完成文件清理；只写用户选择的位置，校验成功才提交
netpos.rs     网络位置检测（骨干直连/宿舍路由后/离线）→ 连接方向引导
ui.rs         AppHooks trait 与可序列化 UI 事件；保留原有方法签名
web/          本地 HTML/CSS/JavaScript 界面，系统 CJK 字体，无远程页面或字体
desktop.rs    Tauri 原生窗口与窄 IPC 桥；密钥留在 Rust，不送到页面
android.rs    src/android.rs：Android 原生文件授权与读写桥（由 desktop.rs 引入）；选择/查看文件短时保活，返回或最多5分钟停止
lib.rs/main.rs 库与移动端入口 / 桌面入口；app.rs 为共享应用逻辑
gen/android/  Android 维护源码；禁止用 android init 覆盖自定义 Activity、插件、Manifest、Gradle
```

构建与坑见 `.agents/docs/交接.md`。
