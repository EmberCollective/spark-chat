/* Browser self-check against the real frontend, with a local disposable backend stub. */
(() => {
  // Touch-mode fixture checks mobile Enter behavior without a real IME.
  if (new URLSearchParams(location.search).has("touch")) {
    const original = window.matchMedia.bind(window);
    window.matchMedia = query => query === "(pointer: coarse)" ? { matches: true } : original(query);
  }
  const resumeMode = new URLSearchParams(location.search).has("resume");
  let receive;
  let failText = false;
  let textCalls = 0;
  let saveCalls = 0;
  let openCalls = 0;
  let acceptedControls = false;
  const initial = {
    vault_created: true,
    keyfile: "test-only.keyfile",
    my_id: "test-self-12345678",
    position: "本地界面测试",
    position_kind: "campus",
    history: [],
    connected: false,
  };
  window.__TAURI__ = {
    event: {
      listen: async (name, listener) => {
        if (name === "spark-event") receive = listener;
      },
    },
    core: {
      invoke: async (_, { request }) => {
        switch (request.action) {
          case "initial":
            return resumeMode ? {
              ...initial, connected:true, peer_id:"test-peer-abcdefgh", revision:9,
              history:[{own:false,ts:Date.now(),text:"页面重建前的消息"}, ...Array.from({length:30}, (_,i)=>({own:false,ts:Date.now(),text:`恢复消息 ${i}`}))],
              files:[{id:"18446744073709551615",name:"已保存.txt",size:4,outgoing:false,complete:true}],
            } : {...initial,my_id:""};
          case "authenticate":
            return initial;
          case "text":
            textCalls += 1;
            if (failText) throw new Error("测试中的发送失败");
            return { own: true, ts: Date.now(), text: request.text };
          case "save_file":
            if (request.id !== "18446744073709551615")
              throw new Error("文件 ID 精度丢失");
            if (++saveCalls === 1) throw new Error("测试中的接收失败");
            receive({payload:{type:"FileAccepted",data:{id:request.id}}});
            acceptedControls = document.querySelector(".file-save").hidden && !document.querySelector(".file-cancel").hidden;
            receive({payload:{type:"FileReady",data:{id:request.id,name:"测试文件.txt"}}});
            return "selected-destination";
          case "open_file":
            if (request.id !== "18446744073709551615") throw new Error("文件 ID 精度丢失");
            openCalls++;
            return null;
          default:
            return null;
        }
      },
    },
  };
  const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
  const emit = (type, data) => receive({ payload: { type, data } });
  document.addEventListener("DOMContentLoaded", async () => {
    const result = document.createElement("pre");
    result.id = "check-results";
    const rows = [];
    const check = (condition, message) => {
      if (!condition) throw new Error(message);
      rows.push(`PASS ${message}`);
    };
    try {
      await tick();
      if (resumeMode) {
        check(document.getElementById("startup").hidden && document.getElementById("auth").hidden && !document.getElementById("app").hidden, "已解锁服务在页面重建后恢复聊天");
        check(document.getElementById("messages").textContent.includes("页面重建前的消息") && !document.querySelector(".file-save").hidden, "恢复历史及已保存文件打开入口");
        const savedRect = document.querySelector(".file-save").getBoundingClientRect();
        check(savedRect.height > 0 && savedRect.bottom <= document.getElementById("composer").getBoundingClientRect().top + 1, "恢复后滚动到最新消息并显示保存按钮");
        receive({payload:{type:"Msg",data:"stale-event",revision:8}});
        receive({payload:{type:"Msg",data:"new-event",revision:10}});
        check(!document.getElementById("messages").textContent.includes("stale-event") && document.getElementById("messages").textContent.includes("new-event"), "恢复时去除旧事件并接收新消息");
      } else {
      document.getElementById("passphrase").value = "test-passphrase";
      document.getElementById("authenticate").click();
      await tick();
      emit("Connected", "test-peer-abcdefgh");
      check(
        document.getElementById("send").disabled,
        "发送必须等到会话真正就绪",
      );
      const hostile = '<img src=x onerror="window.untrustedExecuted=true">';
      emit("History", [{ own: false, ts: Date.now(), text: hostile }]);
      emit("SessionReady", "test-peer-abcdefgh");
      check(
        !document.querySelector("#messages img") &&
          document.getElementById("messages").textContent.includes(hostile),
        "消息作为文字显示，不执行 HTML",
      );
      const input = document.getElementById("message");
      input.value = "中文输入法";
      input.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Enter",
          isComposing: true,
          bubbles: true,
          cancelable: true,
        }),
      );
      await tick();
      check(
        textCalls === 0 && input.value === "中文输入法",
        "输入法确认回车不发送",
      );
      HTMLFormElement.prototype.requestSubmit = undefined;
      input.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Enter",
          bubbles: true,
          cancelable: true,
        }),
      );
      await tick();
      if (matchMedia("(pointer: coarse)").matches) {
        check(textCalls === 0 && input.value === "中文输入法", "手机回车用于换行，不误发消息");
        document.getElementById("send").click();
        await tick();
      }
      check(
        textCalls === 1 && input.value === "",
        "发送按钮与旧 WebKit 均可正常发送",
      );
      const ownAvatar = document.querySelector(
        ".message-row.own .message-avatar",
      );
      const peerAvatar = document.querySelector(
        ".message-row:not(.own) .message-avatar",
      );
      check(
        ownAvatar.textContent === "我" &&
          getComputedStyle(ownAvatar).width === "40px" &&
          getComputedStyle(peerAvatar).width === "40px",
        "双方头像均为 40px，我方显示我",
      );
      failText = true;
      input.value = "失败时保留草稿";
      document.getElementById("send").click();
      await tick();
      check(input.value === "失败时保留草稿", "发送失败保留草稿");
      emit("History", []);
      document.getElementById("home-button").click();
      check(!document.getElementById("resume-chat").hidden && document.getElementById("host").disabled && document.getElementById("join").disabled, "空聊天返回主页后仍可返回，已有连接时不重复拨号");
      document.getElementById("resume-chat").click();
      const file = {
        id: "18446744073709551615",
        name: "测试文件.txt",
        size: 4,
      };
      emit("FileIncoming", file);
      document.getElementById("home-button").click();
      check(!document.getElementById("welcome").hidden && input.disabled === false, "返回主界面保留连接和解锁状态");
      document.getElementById("resume-chat").click();
      check(!document.getElementById("conversation").hidden, "主页可以返回当前聊天");
      check(window.sparkNavigateBack() && !document.getElementById("welcome").hidden && !window.sparkNavigateBack(), "系统返回先回主页，主页再交还系统处理");
      document.getElementById("resume-chat").click();
      document.querySelector(".file-save").click();
      await tick();
      check(
        !document.querySelector(".file-save").disabled &&
          !document.querySelector(".file-save").hidden,
        "选择保存位置失败可重试，文件 ID 保持精度",
      );
      document.querySelector(".file-save").click();
      await tick();
      check(acceptedControls, "接收中可以取消且不误报完成");
      check(!document.querySelector(".file-save").hidden && document.querySelector(".file-save").textContent === "打开文件" && document.querySelector(".file-cancel").hidden, "校验保存后显示打开文件");
      document.querySelector(".file-save").click();
      await tick();
      check(openCalls === 1, "打开文件只传已保存文件编号，调用原生打开");
      emit("Disconnected", "测试断线");
      check(
        !document.getElementById("reconnect").hidden && input.disabled,
        "断线保留记录并给出重连入口",
      );
      emit("Connected", "test-peer-abcdefgh");
      emit("History", []);
      emit("SessionReady", "test-peer-abcdefgh");
      check(
        document.querySelectorAll(".file-save").length === 1 &&
          !document.querySelector(".file-save").hidden,
        "重连后保留已保存文件的打开入口",
      );
      emit("FileOutgoing", { id: "8", name: "out.txt", size: 4 });
      emit("FileAcked", { id: "8" });
      emit("FileSendProgress", { id: "8", done: 4, total: 4 });
      check(
        document
          .querySelectorAll(".file-meta")[1]
          .textContent.includes("对方已收到"),
        "迟到的进度不能覆盖完成状态",
      );
      emit("Connected", "different-peer-87654321");
      emit("History", []);
      check(
        document.querySelectorAll(".file-save").length === 0,
        "切换对方身份不展示前一位的文件",
      );
      emit("FileIncoming", file);
      document.getElementById("lock").click();
      await tick();
      check(
        document.getElementById("lock-confirm").open &&
          document.getElementById("auth").hidden,
        "未完成附件时先提醒，再锁定",
      );
      document.getElementById("cancel-lock").click();
      check(
        !document.querySelector(".file-save").hidden,
        "取消锁定后仍可选择接收附件",
      );
      emit("SessionReady", "different-peer-87654321");
      emit("Msg", "长消息与链接：" + "https://example.invalid/" + "long-path".repeat(50));
      emit("FileIncoming", { id: "long-file", name: "超长中文文件名".repeat(20) + ".txt", size: 7 });
      await tick();
      check(document.documentElement.scrollWidth <= window.innerWidth, "长消息和文件名不会撑破屏幕");
      if (window.innerWidth <= 600) {
        const targets = ["lock", "attach", "send"].map(id => document.getElementById(id).getBoundingClientRect());
        check(targets.every(r => r.width >= 48 && r.height >= 48), "手机常用按钮触控区至少48px");
        const composer = document.getElementById("composer").getBoundingClientRect();
        check(composer.bottom <= window.innerHeight + 1 && composer.width <= window.innerWidth, "手机输入区完整位于可用屏幕");
      }
      document.getElementById("lock").click();
      document.getElementById("confirm-lock").click();
      await tick();
      check(
        document.getElementById("messages").children.length === 0 &&
          input.value === "" &&
          !document.getElementById("auth").hidden,
        "锁定清理界面记录与草稿",
      );
      }
      result.textContent = `${rows.join("\n")}\n${rows.length}/${rows.length} passed`;
      document.title = `UI checks: ${rows.length} passed`;
    } catch (error) {
      result.textContent = `${rows.join("\n")}\nFAIL ${error.message}`;
      document.title = "UI checks: FAILED";
    }
    result.style.cssText =
      "position:fixed;inset:10px;z-index:10000;background:white;color:#243c2c;padding:20px;overflow:auto;font-size:14px;line-height:1.8;white-space:pre-wrap";
    document.body.append(result);
  });
})();
