"use strict";
const $ = (id) => document.getElementById(id);
let booting = true;
const startupEvents = [];
const state = {
  ready: false,
  snapshotRevision: 0,
  create: true,
  connected: false,
  peer: "",
  own: "",
  position: "",
  generation: 0,
  files: new Map(),
  lastOwn: null,
  lastDay: "",
  connecting: false,
  timeout: null,
};
const api = window.__TAURI__;
const touchInput = window.matchMedia("(pointer: coarse)");
function fitViewport() {
  const height = window.visualViewport?.height || window.innerHeight;
  document.documentElement.style.setProperty("--viewport-height", `${height}px`);
  if (document.activeElement === $("message")) bottom();
}
window.visualViewport?.addEventListener("resize", fitViewport);
window.addEventListener("resize", fitViewport);
fitViewport();
const call = (action, args = {}) =>
  api.core.invoke("dispatch", { request: { action, ...args } });
const show = (id, visible = true) => {
  $(id).hidden = !visible;
};

function tell(text) {
  $("banner-text").textContent = String(text);
  show("banner");
}
function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  const use = document.createElementNS(svg.namespaceURI, "use");
  use.setAttribute("href", `#${name}-icon`);
  svg.setAttribute("aria-hidden", "true");
  svg.append(use);
  return svg;
}
function avatar(element, peer) {
  const tail = peer.slice(-8);
  let tone = 0;
  for (const character of tail) tone += character.charCodeAt(0);
  element.classList.toggle("brandmark", !tail);
  if (tail) element.textContent = tail.slice(-2).toUpperCase();
  else {
    const logo = document.createElement("img");
    logo.src = "logo.png";
    logo.alt = "";
    element.replaceChildren(logo);
  }
  element.dataset.tone = String(tone % 4);
}
function setConnection(connected, label) {
  state.connected = connected;
  $("host").disabled = connected;
  $("join").disabled = connected;
  $("app").classList.toggle("is-connected", connected);
  $("connection-label").textContent = label;
  $("message").disabled = !connected;
  $("attach").disabled = !connected;
  $("send").disabled = !connected;
}
function authView(create) {
  state.ready = false;
  state.create = create;
  state.generation += 1;
  clearTimeout(state.timeout);
  state.connecting = false;
  show("startup", false);
  show("app", false);
  show("auth");
  show("confirm-field", create);
  show("auth-error", false);
  $("auth-title").textContent = create ? "欢迎，给自己配一把钥匙" : "欢迎回来";
  $("auth-subtitle").textContent = create
    ? "用口令和钥匙文件保护你的身份与聊天记录。"
    : "输入口令，打开你的加密聊天。";
  $("authenticate").textContent = create ? "创建并开始" : "解锁聊天";
  $("passphrase").autocomplete = create ? "new-password" : "current-password";
  $("passphrase").minLength = create ? 6 : 0;
  $("confirm-passphrase").required = create;
  $("pick-keyfile").textContent = create ? "保存到…" : "选择…";
  $("auth-switch").textContent = create
    ? "已有钥匙库？直接解锁"
    : "第一次使用？创建钥匙库";
  $("passphrase").value = "";
  $("confirm-passphrase").value = "";
  $("passphrase").focus();
}
function home() {
  show("home-button", false);
  show("drop-hint", false);
  show("resume-chat", state.connected || Boolean($("messages").children.length));
  show("welcome");
  show("connect-panel", false);
  show("conversation", false);
  show("composer", false);
}
function chat() {
  show("home-button");
  show("welcome", false);
  show("connect-panel", false);
  show("conversation");
  show("composer");
  show("empty-chat", !$("messages").children.length);
}
function enter(initial) {
  show("startup", false);
  state.ready = true;
  state.generation += 1;
  state.own = initial.my_id;
  state.snapshotRevision = initial.revision || 0;
  state.position = initial.position;
  $("my-id").textContent = `我的身份 · ${state.own.slice(-8)}`;
  $("detail-own").textContent = state.own;
  $("detail-network").textContent = state.position;
  $("network-hint").textContent = state.position;
  $("host").className =
    initial.position_kind === "campus" ? "primary" : "secondary";
  $("join").className =
    initial.position_kind === "campus" ? "secondary" : "primary";
  show("auth", false);
  show("app");
  show("banner", false);
  state.peer = initial.peer_id || "";
  avatar($("peer-avatar"), state.peer);
  $("peer-title").textContent = state.peer ? `对方 · ${state.peer.slice(-8)}` : "Spark Chat";
  $("detail-peer").textContent = state.peer || "尚未连接";
  setConnection(Boolean(initial.connected), initial.connected ? "已连接 · 加密通道" : "开始一段加密聊天");
  replaceHistory(initial.history || []);
  for (const file of initial.files || []) {
    addFile(file, file.outgoing);
    if (file.complete) finishFile(file.id, file.outgoing);
    else if (file.failed) failFile(file, file.outgoing);
    else if (file.accepted) acceptFile(file.id);
  }
  if (initial.connected || $("messages").children.length) {
    chat();
    bottom();
  } else home();
}
function clearChat() {
  state.peer = "";
  state.own = "";
  state.files.clear();
  state.lastDay = "";
  state.lastOwn = null;
  $("messages").replaceChildren();
  $("message").value = "";
  $("message").style.height = "";
  $("join-code").value = "";
  $("listen-code").value = "";
  $("detail-own").textContent = "";
  $("detail-peer").textContent = "尚未连接";
  $("peer-title").textContent = "Spark Chat";
  $("my-id").textContent = "我的身份";
  avatar($("peer-avatar"), "");
  setConnection(false, "已锁定");
  if ($("connection-details").open) $("connection-details").close();
  if ($("lock-confirm").open) $("lock-confirm").close();
  $("menu").open = false;
}
function nearBottom() {
  const stream = $("conversation");
  return stream.scrollHeight - stream.clientHeight - stream.scrollTop < 90;
}
function bottom() {
  const stream = $("conversation");
  stream.scrollTop = stream.scrollHeight;
}
function appendDay(ts) {
  const date = new Date(ts);
  const day = date.toLocaleDateString("zh-CN");
  if (day !== state.lastDay) {
    const row = document.createElement("div");
    row.className = "day";
    const span = document.createElement("span");
    span.textContent =
      day === new Date().toLocaleDateString("zh-CN") ? "今天" : day;
    row.append(span);
    $("messages").append(row);
    state.lastDay = day;
    state.lastOwn = null;
  }
  return date;
}
function messageRow(own, ts) {
  const date = appendDay(ts);
  const row = document.createElement("div");
  row.className = `message-row${own ? " own" : ""}${own !== state.lastOwn ? " group-start" : ""}`;
  const badge = document.createElement("div");
  if (state.lastOwn !== own) {
    badge.className = "avatar message-avatar";
    if (own) badge.textContent = "我";
    else avatar(badge, state.peer);
  } else badge.className = "avatar-slot";
  badge.setAttribute("aria-hidden", "true");
  if (!own) row.append(badge);
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  const time = document.createElement("time");
  time.dateTime = date.toISOString();
  time.textContent = date.toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  row.append(bubble);
  if (own) row.append(badge);
  $("messages").append(row);
  state.lastOwn = own;
  show("empty-chat", false);
  return { row, bubble, time };
}
function addText(message, scroll = true) {
  const stuck = nearBottom();
  const { bubble, time } = messageRow(message.own, message.ts);
  const p = document.createElement("p");
  p.textContent = message.text;
  if (!message.text.includes("\n") && message.text.length < 24)
    bubble.classList.add("short-bubble");
  bubble.append(p, time);
  if (scroll && (stuck || message.own)) bottom();
}
function replaceHistory(messages) {
  $("messages").replaceChildren();
  state.lastDay = "";
  state.lastOwn = null;
  const entries = [
    ...messages.map((message) => ({ ts: message.ts, message })),
    ...state.files.values(),
  ].sort((a, b) => a.ts - b.ts);
  for (const entry of entries) {
    if (entry.message) addText(entry.message, false);
    else {
      appendDay(entry.ts);
      $("messages").append(entry.row);
      state.lastOwn = entry.outgoing;
    }
  }
  show("empty-chat", !entries.length);
  bottom();
}
function bytes(size) {
  return size >= 1048576
    ? `${(size / 1048576).toFixed(1)} MB`
    : size >= 1024
      ? `${Math.round(size / 1024)} KB`
      : `${size} B`;
}
function addFile(data, outgoing) {
  const key = `${outgoing ? "out" : "in"}:${data.id}`;
  if (state.files.has(key)) return;
  const stuck = nearBottom();
  const ts = Date.now();
  const { row, bubble, time } = messageRow(outgoing, ts);
  const content = document.createElement("div");
  content.className = "file-content";
  const symbol = document.createElement("div");
  symbol.className = "file-symbol";
  symbol.append(icon("file"));
  const detail = document.createElement("div");
  const title = document.createElement("div");
  title.className = "file-title";
  title.textContent = data.name;
  const meta = document.createElement("div");
  meta.className = "file-meta";
  const track = document.createElement("div");
  track.className = "file-progress";
  const progress = document.createElement("span");
  track.append(progress);
  detail.append(title, meta, track);
  content.append(symbol, detail);
  const save = document.createElement("button");
  save.type = "button";
  save.className = "file-save";
  save.append(icon("download"), "接收并保存…");
  save.hidden = outgoing;
  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "file-cancel";
  cancel.textContent = "取消接收";
  cancel.hidden = outgoing;
  cancel.addEventListener("click", async () => {
    cancel.disabled = true;
    try { await call("cancel_file", { id: String(data.id) }); }
    catch (e) { tell(`取消未完成：${e}`); }
    finally { cancel.disabled = false; }
  });
  save.addEventListener("click", async () => {
    const generation = state.generation;
    save.disabled = true;
    try {
      const item = state.files.get(key);
      if (item?.complete) {
        await call("open_file", { id: String(data.id) });
        return;
      }
      const path = await call("save_file", { id: String(data.id) });
      if (state.generation !== generation) return;
      if (path) {
        const current = state.files.get(key);
        if (current && !current.complete && !current.failed) acceptFile(data.id);
      }
    } catch (e) {
      if (state.generation === generation)
        tell(state.files.get(key)?.complete ? `打开未完成：${e}` : `接收未开始：${e}。可以重新选择保存位置。`);
    } finally {
      save.disabled = false;
    }
  });
  bubble.append(content, save, cancel, time);
  state.files.set(key, {
    ...data,
    ts,
    row,
    outgoing,
    meta,
    track,
    progress,
    save,
    cancel,
    complete: false,
    accepted: false,
    saved: false,
  });
  meta.textContent = `${bytes(data.size)} · ${outgoing ? "正在准备文件，随后等待对方接收" : "等待你选择保存位置"}`;
  if (!outgoing) { chat(); bottom(); }
  if (stuck || outgoing) bottom();
}
function acceptFile(id) {
  const item = state.files.get(`in:${id}`);
  if (!item || item.complete || item.failed) return;
  item.accepted = true;
  item.save.hidden = true;
  item.meta.textContent = `${bytes(item.size)} · 正在接收`;
}
function failFile(data, outgoing) {
  const item = state.files.get(`${outgoing ? "out" : "in"}:${data.id}`);
  if (!item) return;
  item.failed = data.reason || data.failed || "传输中断";
  item.track.hidden = true;
  item.save.hidden = true;
  item.cancel.hidden = true;
  item.meta.textContent = item.failed;
}
function fileProgress(data, outgoing) {
  const item = state.files.get(`${outgoing ? "out" : "in"}:${data.id}`);
  if (!item || item.complete || item.failed) return;
  const fraction = data.total ? Math.min(1, data.done / data.total) : 0;
  item.progress.style.width = `${fraction * 100}%`;
  item.meta.textContent = `${bytes(data.done)} / ${bytes(data.total)} · ${outgoing ? (data.done === 0 ? "等待对方接收" : "发送中") : "接收中"}`;
}
function finishFile(id, outgoing) {
  const item = state.files.get(`${outgoing ? "out" : "in"}:${id}`);
  if (!item) return;
  item.complete = true;
  item.track.hidden = true;
  item.saved = !outgoing;
  item.cancel.hidden = true;
  item.meta.textContent = `${bytes(item.size)} · ${outgoing ? "对方已收到并保存" : "已保存"}`;
  item.save.textContent = "打开文件";
  item.save.hidden = outgoing;
}
function eventReceived(event) {
  if (booting) {
    if (startupEvents.length < 256) startupEvents.push(event);
    return;
  }
  if (!state.ready || (event.payload.revision != null && event.payload.revision <= state.snapshotRevision)) return;
  const data = event.payload.data;
  switch (event.payload.type) {
    case "NetPosition":
      state.position = data;
      $("network-hint").textContent = data;
      $("detail-network").textContent = data;
      break;
    case "NetPositionKind":
      state.position = data.text;
      $("network-hint").textContent = data.text;
      $("detail-network").textContent = data.text;
      break;
    case "ListenCode":
      $("listen-code").value = data.code;
      $("copy-code").disabled = false;
      $("connect-caption").textContent = "把连接码发给好友，等对方粘贴加入。";
      break;
    case "Connected":
      if (state.peer && state.peer !== data) {
        state.generation += 1;
        state.files.clear();
        $("messages").replaceChildren();
        $("message").value = "";
        $("message").style.height = "";
      }
      state.peer = data;
      avatar($("peer-avatar"), data);
      $("peer-title").textContent = `对方 · ${data.slice(-8)}`;
      $("detail-peer").textContent = data;
      setConnection(false, "通道已连接，正在打开加密记录…");
      break;
    case "History":
      replaceHistory(data);
      break;
    case "SessionReady":
      state.peer = data;
      avatar($("peer-avatar"), data);
      $("peer-title").textContent = `对方 · ${data.slice(-8)}`;
      $("detail-peer").textContent = data;
      clearTimeout(state.timeout);
      state.connecting = false;
      $("connect").disabled = false;
      show("reconnect", false);
      setConnection(true, "已连接 · 加密通道");
      show("banner", false);
      chat();
      bottom();
      $("message").focus();
      break;
    case "Disconnected":
      for (const item of state.files.values()) {
        if (!item.complete) {
          failFile({ id: item.id, reason: "传输中断，请重新发送" }, item.outgoing);
        }
      }
      clearTimeout(state.timeout);
      state.connecting = false;
      $("connect").disabled = false;
      show("reconnect");
      setConnection(false, "连接已断开");
      tell(`连接断开了：${data}。聊天记录仍在，重新创建或加入后继续。`);
      if ($("conversation").hidden) home();
      break;
    case "Msg":
      addText({ own: false, ts: Date.now(), text: data });
      break;
    case "FileIncoming":
      addFile({ ...data, size: data.size }, false);
      break;
    case "FileOutgoing":
      addFile(data, true);
      break;
    case "FileProgress":
      fileProgress(data, false);
      break;
    case "FileSendProgress":
      fileProgress(data, true);
      break;
    case "FileAccepted":
      acceptFile(data.id);
      break;
    case "FileFailed":
      failFile(data, data.outgoing);
      break;
    case "FileReady":
      finishFile(data.id, false);
      break;
    case "FileAcked":
      finishFile(data.id, true);
      break;
    case "Error":
      tell(data);
      state.connecting = false;
      $("connect").disabled = false;
      clearTimeout(state.timeout);
      $("join-status").textContent = "连接未完成，请检查后重试。";
      break;
  }
}

$("auth-switch").addEventListener("click", () => authView(!state.create));
$("pick-keyfile").addEventListener("click", async () => {
  try {
    const path = await call("pick_keyfile", { create: state.create });
    if (path) $("keyfile").value = path;
  } catch (e) {
    $("auth-error").textContent = String(e);
    show("auth-error");
  }
});
$("auth-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  show("auth-error", false);
  if (state.create && $("passphrase").value !== $("confirm-passphrase").value) {
    $("auth-error").textContent = "两次口令不一致，请再核对一下。";
    show("auth-error");
    return;
  }
  $("authenticate").disabled = true;
  $("auth-switch").disabled = true;
  $("authenticate").textContent = "正在打开…";
  try {
    enter(
      await call("authenticate", {
        passphrase: $("passphrase").value,
        keyfile: $("keyfile").value,
        create: state.create,
      }),
    );
  } catch (e) {
    $("auth-error").textContent = String(e);
    show("auth-error");
  } finally {
    $("passphrase").value = "";
    $("confirm-passphrase").value = "";
    $("authenticate").disabled = false;
    $("auth-switch").disabled = false;
    $("authenticate").textContent = state.create ? "创建并开始" : "解锁聊天";
  }
});
async function lockChat() {
  $("lock").disabled = true;
  try {
    await call("lock");
    clearChat();
    authView(false);
  } catch (e) {
    tell(`锁定未完成：${e}`);
  } finally {
    $("lock").disabled = false;
  }
}
$("lock").addEventListener("click", () => {
  const pending = [...state.files.values()].some((file) =>
    !file.failed && (file.outgoing ? !file.complete : !file.saved),
  );
  if (pending) $("lock-confirm").showModal();
  else lockChat();
});
$("cancel-lock").addEventListener("click", () => $("lock-confirm").close());
$("confirm-lock").addEventListener("click", lockChat);
$("reconnect").addEventListener("click", home);
$("resume-chat").addEventListener("click", chat);
$("dismiss-banner").addEventListener("click", () => show("banner", false));
$("show-details").addEventListener("click", () => {
  $("menu").open = false;
  $("connection-details").showModal();
});
$("close-details").addEventListener("click", () =>
  $("connection-details").close(),
);
$("back-home").addEventListener("click", home);
$("home-button").addEventListener("click", home);
$("host").addEventListener("click", async () => {
  show("welcome", false);
  show("conversation", false);
  show("composer", false);
  show("connect-panel");
  show("hosting");
  show("join-form", false);
  $("connect-title").textContent = "等待好友加入";
  $("connect-caption").textContent = "正在准备连接码…";
  $("listen-code").value = "";
  $("copy-code").disabled = true;
  try {
    await call("listen");
  } catch (e) {
    tell(e);
  }
});
$("join").addEventListener("click", () => {
  show("welcome", false);
  show("conversation", false);
  show("composer", false);
  show("connect-panel");
  show("hosting", false);
  show("join-form");
  $("connect-title").textContent = "加入好友的聊天";
  $("connect-caption").textContent = "粘贴好友发来的连接码，即可直接连接。";
  $("join-code").focus();
});
$("join-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (state.connecting) return;
  state.connecting = true;
  $("connect").disabled = true;
  $("join-status").textContent = "正在联系对方，请稍候…";
  try {
    await call("connect", { code: $("join-code").value.trim() });
    state.timeout = setTimeout(() => {
      if (state.connecting) {
        $("join-status").textContent =
          "等待较久，请确认对方在线、网络可达。仍在尝试连接。";
        state.connecting = false;
        $("connect").disabled = false;
      }
    }, 20000);
  } catch (e) {
    state.connecting = false;
    $("connect").disabled = false;
    $("join-status").textContent = String(e);
  }
});
$("copy-code").addEventListener("click", async () => {
  const code = $("listen-code");
  try {
    if (navigator.clipboard?.writeText)
      await navigator.clipboard.writeText(code.value);
    else {
      code.focus();
      code.select();
      if (!document.execCommand("copy"))
        throw new Error("请手动复制选中的连接码");
    }
    $("copy-code").textContent = "已复制";
    setTimeout(() => {
      $("copy-code").textContent = "复制连接码";
    }, 1600);
  } catch (_) {
    code.focus();
    code.select();
    tell("请复制已选中的完整连接码。手机长按复制，电脑使用复制快捷键。");
  }
});
$("message").addEventListener("input", () => {
  $("message").style.height = "auto";
  $("message").style.height = `${Math.min(140, $("message").scrollHeight)}px`;
});
$("message").addEventListener("keydown", (event) => {
  if (
    !touchInput.matches &&
    event.key === "Enter" &&
    !event.shiftKey &&
    !event.isComposing &&
    event.keyCode !== 229
  ) {
    event.preventDefault();
    $("send").click();
  }
});
$("composer").addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = $("message").value.trim();
  if (!text || !state.connected || $("send").disabled) return;
  if (new TextEncoder().encode(text).length > 1048576) {
    tell("这条消息太长了，请拆成几条发送。");
    return;
  }
  const generation = state.generation;
  const original = $("message").value;
  $("send").disabled = true;
  try {
    const message = await call("text", { text });
    if (generation !== state.generation) return;
    addText(message);
    if ($("message").value === original) {
      $("message").value = "";
      $("message").style.height = "";
    }
    $("message").focus();
  } catch (e) {
    if (generation === state.generation) tell(`消息未发送：${e}`);
  } finally {
    $("send").disabled = !state.connected;
  }
});
$("attach").addEventListener("click", async () => {
  const generation = state.generation;
  $("attach").disabled = true;
  try {
    await call("send_file");
  } catch (e) {
    if (generation === state.generation) tell(`文件未发送：${e}`);
  } finally {
    $("attach").disabled = !state.connected;
  }
});

window.sparkNavigateBack = () => {
  for (const id of ["connection-details", "lock-confirm"]) {
    if ($(id).open) { $(id).close(); return true; }
  }
  if ($("menu").open) { $("menu").open = false; return true; }
  if (state.ready && $("welcome").hidden) { home(); return true; }
  return false;
};

document.addEventListener("dragover", event => event.preventDefault());
document.addEventListener("drop", event => event.preventDefault());

(async () => {
  if (!api) {
    $("startup").textContent = "请从 Spark Chat 客户端打开此界面。";
    return;
  }
  try {
    await api.event.listen("spark-drag", (event) => {
      const active = Boolean(event.payload) && state.ready;
      $("drop-hint").textContent = state.connected ? "松开发送文件" : "请先连接好友再发送文件";
      show("drop-hint", active);
    });
    await api.event.listen("spark-event", eventReceived);
    const initial = await call("initial");
    $("keyfile").value = initial.keyfile;
    if (initial.mobile) {
      $("keyfile").readOnly = true;
      $("keyfile").placeholder = "点右侧按钮选择钥匙文件";
      document.querySelector(".field-hint").textContent = "钥匙文件由系统文件管理器保管；请另行备份，重新打开后可能需要再次选择。";
    }
    if (initial.my_id) enter(initial);
    else authView(!initial.vault_created);
    booting = false;
    for (const event of startupEvents.splice(0)) eventReceived(event);
  } catch (e) {
    $("startup").textContent =
      `未能打开聊天服务：${e}。请关闭后重新打开客户端。`;
  }
})();
