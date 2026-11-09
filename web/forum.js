"use strict";
// Discourse 的主题列表与顺序回复；所有用户文字只作为文本展示。
(() => {
  const panel = document.getElementById("forum-panel");
  const status = document.getElementById("forum-status");
  const list = document.getElementById("forum-topics");
  const detail = document.getElementById("forum-detail");
  const editor = document.getElementById("forum-editor");
  let snapshot = null;
  let selected = null;
  let parents = [];
  let busy = false;
  let epoch = 0;
  let rendered = "";
  const requested = new Set();
  let attachments = [];
  let draftTimer;
  let draftSave = Promise.resolve();
  const $f = id => document.getElementById(id);
  const command = cmd => window.__TAURI__.core.invoke("dispatch", {
    request: { action: "forum", command: cmd },
  });
  function note(value) { status.textContent = String(value); }
  function node(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = text;
    if (className) element.className = className;
    return element;
  }
  function button(text, action) {
    const element = node("button", text, "secondary");
    element.type = "button";
    element.addEventListener("click", action);
    return element;
  }
  async function run(cmd) {
    if (busy) return;
    busy = true;
    const generation = epoch;
    panel.setAttribute("aria-busy", "true");
    try {
      const result = await command(cmd);
      if (generation !== epoch) return;
      if (result?.state) render(result.state);
      else await refresh();
      if (result?.code) { $f("forum-code").value = result.code; $f("forum-code-box").hidden = false; }
      note(result?.message || result?.state?.status || "操作已完成");
      return result;
    } catch (error) { note(error); }
    finally { busy = false; panel.removeAttribute("aria-busy"); }
  }
  async function refresh() {
    const generation = epoch;
    try {
      const result = await command({ kind: "state" });
      if (generation !== epoch) return;
      render(result.state || result);
      if (result.code) { $f("forum-code").value = result.code; $f("forum-code-box").hidden = false; }
    }
    catch (error) { note(error); }
  }
  function render(data) {
    const serialized = JSON.stringify(data);
    if (serialized === rendered) return;
    rendered = serialized;
    snapshot = data;
    $f("forum-own-public").value = data.own_public || "";
    $f("forum-setup").hidden = Boolean(data.joined);
    $f("forum-main").hidden = !data.joined;
    $f("forum-admin").hidden = !data.admin;
    $f("forum-resume-draft").hidden = !(data.draft?.body || data.draft?.title || data.draft?.attachments?.length);
    $f("forum-name").textContent = data.name || "校园论坛";
    $f("forum-network").textContent = data.joined
      ? `${data.peers || 0} 个连接 · ${data.pending || 0} 条记录待补齐 · ${data.disputed || 0} 条争议记录`
      : "先创建论坛，或导入管理员提供的入网材料。";
    renderTopics();
    if (selected) renderDetail();
    const members = $f("forum-members");
    members.replaceChildren();
    for (const member of data.members || []) {
      const row = node("div", undefined, "forum-member");
      row.append(node("code", member.id), node("span", member.active ? "有效" : "已吊销"));
      if (data.admin && member.active && member.id !== data.own_public) {
        row.append(button("吊销资格", () => {
          if (confirm(`吊销 ${member.id} 的论坛资格？未被检查点确认的修改会进入争议状态。此操作由你的管理身份签署。`)) run({ kind: "revoke", member: member.id });
        }));
      }
      members.append(row);
    }
  }
  function renderTopics() {
    const query = $f("forum-search").value.trim().toLowerCase();
    list.replaceChildren();
    const topics = (snapshot?.topics || []).filter(t => !t.reply_to && (!t.hidden || snapshot.admin) &&
      (!query || t.title.toLowerCase().includes(query)));
    for (const topic of topics) {
      const row = node("button", undefined, "forum-topic");
      row.type = "button";
      row.append(node("strong", `${topic.hidden ? "[已隐藏] " : ""}${topic.title || "未命名主题"}`));
      row.append(node("span", `${topic.author.slice(-8)} · ${topic.heads.length > 1 ? `${topic.heads.length} 个并发版本` : "签名已验证"}`, "subtle"));
      row.addEventListener("click", () => { selected = topic.id; renderDetail(); });
      list.append(row);
    }
    if (!topics.length) list.append(node("p", query ? "没有匹配的已同步主题。" : "还没有主题，发布第一条内容吧。", "subtle"));
  }
  function contentView(topic) {
    const article = node("article", undefined, "forum-post");
    article.append(node("h3", topic.title || "回复"));
    article.append(node("p", `创建者 ${topic.author.slice(-8)} · ${topic.id.slice(0, 12)}`, "subtle"));
    if (topic.heads.length > 1) article.append(node("p", "有并发修改，以下版本均已保留。可编辑并提交一个合并版本。", "forum-conflict"));
    const history = node("details");
    history.append(node("summary", "查看署名修改历史"));
    for (const version of topic.versions || []) {
      const section = node("section", undefined, "forum-version");
      section.append(node("p", `修改者 ${version.author.slice(-8)} · ${version.id.slice(0, 12)}`, "subtle"));
      if (version.body != null) section.append(node("p", version.body, "forum-body"));
      else section.append(button("查找并获取正文", () => run({ kind: "fetch", hash: version.body_hash })));
      for (const attachment of version.attachments || []) {
        section.append(button(`${attachment.name} · ${(attachment.size / 1e6).toFixed(2)} MB · ${attachment.ready ? "保存文件" : "查找副本"}`, async () => {
          if (!attachment.ready) { await run({ kind: "fetch", hash: attachment.hash }); return; }
          try {
            const saved = await window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_save",hash:attachment.hash,name:attachment.name,size:attachment.size}});
            if (saved) note("文件已保存到选择的位置。");
          } catch(error) { note(error); }
        }));
        if (attachment.ready && attachment.size <= 5_000_000 && /\.(png|jpe?g|webp)$/i.test(attachment.name)) {
          const image = node("img"); image.alt = attachment.name; image.className = "chat-photo";
          const generation=epoch;
          window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_preview",hash:attachment.hash}}).then(result=>{
            if(generation===epoch && ["image/png","image/jpeg","image/webp"].includes(result.mime)) image.src=`data:${result.mime};base64,${result.data}`;
          }).catch(()=>{image.alt="图片无法预览，可作为文件保存";});
          section.append(image);
        }
      }
      if (topic.heads.includes(version.id)) article.append(section);
      else history.append(section);
    }
    if (history.children.length > 1) article.append(history);
    article.append(button(topic.heads.length > 1 ? "编辑并解决分支" : "署名编辑", () => openEditor(topic)));
    return article;
  }
  function renderDetail() {
    detail.replaceChildren();
    const topic = snapshot?.topics?.find(t => t.id === selected);
    if (!topic) return;
    detail.append(button("返回主题列表", () => { selected = null; detail.replaceChildren(); list.hidden = false; }));
    list.hidden = true;
    if (topic.hidden) { detail.append(node("p", "该主题已由管理员隐藏。")); if(snapshot.admin) detail.append(button("恢复显示",()=>run({kind:"moderate",target:selected,hidden:false}))); return; }
    detail.append(contentView(topic));
    for (const version of topic.versions || []) {
      if (topic.heads.includes(version.id) && version.body == null && !requested.has(version.body_hash)) {
        requested.add(version.body_hash);
        command({ kind: "fetch", hash: version.body_hash }).catch(note);
      }
    }
    for (const reply of snapshot.topics.filter(t => t.reply_to === selected && !t.hidden)) detail.append(contentView(reply));
    detail.append(button("回复", () => openEditor(null, selected)));
    if (snapshot.admin) detail.append(button("隐藏主题", () => run({ kind: "moderate", target: selected, hidden: true })));
  }
  function openEditor(topic = null, replyTo = null) {
    const current = topic?.versions?.find(version => topic.heads.includes(version.id));
    if (topic && current?.body == null) { note("请先获取当前正文，再提交修改。"); return; }
    editor.hidden = false;
    editor.dataset.target = topic?.id || "";
    editor.dataset.replyTo = replyTo || "";
    parents = topic?.heads || [];
    $f("forum-title").value = topic?.title || "";
    $f("forum-body").value = current?.body || "";
    attachments = [...(current?.attachments || [])];
    renderAttachments();
    $f("forum-submit").textContent = topic ? "签署并提交修改" : "签署并发布";
    $f("forum-body").focus();
  }
  $f("forum-open").addEventListener("click", () => {
    panel.hidden = false;
    $f("workspace").hidden = true;
    $f("composer").hidden = true;
    refresh();
  });
  $f("forum-close").addEventListener("click", () => { panel.hidden = true; $f("workspace").hidden = false; window.dispatchEvent(new Event("spark-forum-close")); });
  $f("forum-refresh").addEventListener("click", () => run({ kind: "sync" }));
  $f("forum-search").addEventListener("input", renderTopics);
  $f("forum-new").addEventListener("click", () => openEditor());
  $f("forum-cancel").addEventListener("click", () => { editor.hidden = true; });
  function renderAttachments() {
    const container=$f("forum-attachments"); container.replaceChildren();
    attachments.forEach((attachment,index)=>{
      const row=node("div",undefined,"forum-member");
      row.append(node("span",`${attachment.name} · ${(attachment.size/1e6).toFixed(2)} MB`));
      row.append(button("移除",()=>{attachments.splice(index,1);renderAttachments();scheduleDraft();}));container.append(row);
    });
  }
  $f("forum-attach").addEventListener("click", async()=>{
    if(attachments.length>=5){note("每次发布最多 5 个附件。");return;}
    const generation=epoch; $f("forum-attach").disabled=true;
    try{
      const result=await window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_attach"}});
      if(generation!==epoch||!result)return;
      const attachment=result.attachment;
      if(attachments.some(a=>a.hash===attachment.hash)){note("这个附件已经添加。");return;}
      if(attachments.reduce((sum,a)=>sum+a.size,0)+attachment.size>30_000_000){note("附件合计不能超过 30 MB。");return;}
      attachments.push(attachment);renderAttachments();scheduleDraft();
    }catch(error){note(error);}finally{$f("forum-attach").disabled=false;}
  });
  $f("forum-create").addEventListener("click", () => run({ kind: "create", name: $f("forum-create-name").value.trim() }));
  $f("forum-join").addEventListener("click", () => run({ kind: "join", code: $f("forum-join-code").value.trim() }));
  $f("forum-listen").addEventListener("click", () => run({ kind: "listen" }));
  $f("forum-connect").addEventListener("click", () => run({ kind: "connect", code: $f("forum-peer-code").value.trim() }));
  $f("forum-admit").addEventListener("click", () => run({ kind: "admit", member: $f("forum-member-key").value.trim() }));
  $f("forum-checkpoint").addEventListener("click", async()=>{
    const records=[...new Set((snapshot?.topics||[]).flatMap(t=>t.versions.filter(v=>!v.disputed).map(v=>v.id)))].sort();
    if(!records.length){note("没有可确认的已同步历史。");return;}
    for(let i=0;i<records.length;i+=512){if(!await run({kind:"checkpoint",records:records.slice(i,i+512)}))break;}
  });
  function draftPayload() { return {title:$f("forum-title").value,body:$f("forum-body").value,target:editor.dataset.target||null,reply_to:editor.dataset.replyTo||null,parents:[...parents],attachments:attachments.map(({name,size,hash})=>({name,size,hash}))}; }
  function scheduleDraft() {
    clearTimeout(draftTimer);
    const generation=epoch;
    draftTimer=setTimeout(()=>{
      const draft=draftPayload();
      draftSave=draftSave.catch(()=>{}).then(async()=>{
        if(generation!==epoch)return;
        await command({kind:"draft",...draft});
        if(generation===epoch)note("草稿已加密保存在本机。");
      }).catch(error=>{if(generation===epoch)note(`草稿未保存：${error}`);});
    },500);
  }
  editor.addEventListener("input",scheduleDraft);
  $f("forum-resume-draft").addEventListener("click",()=>{
    const draft=snapshot?.draft;if(!draft)return;
    editor.hidden=false;editor.dataset.target=draft.target||"";editor.dataset.replyTo=draft.reply_to||"";
    parents=[...(draft.parents||[])];attachments=[...(draft.attachments||[])];
    $f("forum-title").value=draft.title||"";$f("forum-body").value=draft.body||"";renderAttachments();$f("forum-body").focus();
  });
  editor.addEventListener("submit", async event => {
    event.preventDefault();
    clearTimeout(draftTimer);await draftSave;
    const result = await run({ kind: "publish", title: $f("forum-title").value.trim(), body: $f("forum-body").value,
      target: editor.dataset.target || null, reply_to: editor.dataset.replyTo || null, parents,
      attachments:attachments.map(({name,size,hash})=>({name,size,hash})) });
    if (result) editor.hidden = true;
  });
  window.addEventListener("spark-locked", () => {
    epoch += 1; rendered = ""; requested.clear();
    clearTimeout(draftTimer);
    attachments=[];renderAttachments();
    snapshot = null; selected = null; parents = [];
    panel.hidden = true; detail.replaceChildren(); list.replaceChildren();
    for (const input of panel.querySelectorAll("input,textarea")) input.value = "";
    $f("workspace").hidden = false;
  });
  let refreshing = false;
  setInterval(async () => {
    if (panel.hidden || busy || refreshing || document.hidden) return;
    refreshing = true;
    try { await refresh(); } finally { refreshing = false; }
  }, 3000);
})();
