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
  let pageOffset = 0;
  let parents = [];
  let busy = false;
  let epoch = 0;
  let rendered = "";
  const requested = new Set();
  let attachments = [];
  let draftTimer;
  let searchTimer;
  let searchResult = null;
  let searchGeneration = 0;
  let draftSave = Promise.resolve();
  let page = "home";
  let editorReturn = "home";
  const $f = id => document.getElementById(id);
  const command = cmd => window.__TAURI__.core.invoke("dispatch", {
    request: { action: "forum", command: { ...cmd, ...(snapshot?.current_community && !["state","community_list"].includes(cmd.kind) ? {community_id:snapshot.current_community} : {}) } },
  });
  function note(value) { status.textContent = String(value); status.hidden = !value; }
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
  function showPage(next) {
    if (next === "admin" && !snapshot?.admin) next = "home";
    page = next;
    for (const view of panel.querySelectorAll('[id^="forum-view-"]')) view.hidden = view.id !== `forum-view-${next}`;
    for (const nav of panel.querySelectorAll('[data-forum-page]')) nav.classList.toggle("active", nav.dataset.forumPage === next);
    const picker = $f("community-picker"); if (picker) picker.open = false;
    panel.scrollTop = 0;
  }
  function displayName(id) { return snapshot?.nicknames?.[id] || id?.slice(-8) || "未知成员"; }
  function goHome() { return run({kind:"page",offset:pageOffset}); }
  panel.addEventListener("click", event => {
    if(event.target.closest("#forum-profile-open")){
      if($f("forum-profile-dialog")){$f("forum-nickname").value=snapshot?.nickname||"";$f("forum-profile-dialog").showModal();}
    }
    if(event.target.closest("#forum-profile-cancel"))$f("forum-profile-dialog").close();
    const navigation = event.target.closest("[data-forum-page]");
    if (navigation) {
      const target = navigation.dataset.forumPage;
      if (target === "home") goHome();
      else if (target === "editor") openEditor();
      else showPage(target);
    }
    const tab = event.target.closest("[data-forum-admin-tab]");
    if (tab) {
      for (const item of panel.querySelectorAll("[data-forum-admin-tab]")) item.classList.toggle("active", item === tab);
      for (const name of ["members","boards","reports"]) {
        const view = $f(`forum-admin-${name}`); if (view) view.hidden = name !== tab.dataset.forumAdminTab;
      }
    }
  });
  document.addEventListener("click", event => { const picker=$f("community-picker"); if(picker&&!picker.contains(event.target))picker.open=false; });
  document.addEventListener("keydown", event => { if(event.key==="Escape"){const picker=$f("community-picker");if(picker)picker.open=false;} });
  async function preserveEditor() {
    clearTimeout(draftTimer);
    await draftSave.catch(()=>{});
    if(!editor.hidden) await command({kind:"draft",...draftPayload()});
  }
  async function createOrJoin(cmd) {
    if(busy)return;
    try {
      await preserveEditor(); epoch++; rendered="";
      const result=await run(cmd);
      if(result){attachments=[];parents=[];editor.hidden=true;showPage("home");}
    } catch(error){note(error);}
  }
  async function selectCommunity(id) {
    if (busy) return;
    if(id === snapshot?.current_community){await goHome();return;}
    await preserveEditor();
    epoch++; rendered="";
    const result=await run({kind:"community_select",id});
    if(result){attachments=[];parents=[];editor.hidden=true;showPage("home");}
  }
  async function run(cmd) {
    if (busy) return;
    busy = true;
    const generation = epoch;
    panel.setAttribute("aria-busy", "true");
    try {
      const result = await command(cmd);
      if (generation !== epoch) return;
      if (cmd.kind === "topic") page = "thread";
      if (cmd.kind === "page") page = "home";
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
    const changedCommunity = snapshot?.current_community && data.current_community !== snapshot.current_community;
    if(changedCommunity){selected=null;requested.clear();searchResult=null;page="home";$f("forum-search").value="";}
    snapshot = data;
    pageOffset = data.page_offset || 0;
    if (Object.hasOwn(data, "focused_topic")) selected = data.focused_topic;
    list.hidden = Boolean(selected);
    if (!selected) detail.replaceChildren();
    $f("forum-own-public").value = data.own_public || "";
    $f("forum-setup").hidden = false;
    $f("forum-main").hidden = !data.joined;
    $f("forum-admin").hidden = !data.admin;
    $f("forum-resume-draft").hidden = false;
    $f("forum-name").textContent = data.name || "社区网络";
    for(const id of ["community-current","forum-current-community","forum-community-name"]) if($f(id))$f(id).textContent=data.name||"选择社区";
    if($f("community-role"))$f("community-role").textContent=data.joined?(data.admin?"你创建的社区":"已加入 · 成员"):"尚未加入社区";
    for(const control of panel.querySelectorAll('[data-forum-page="admin"]'))control.hidden=!data.admin;
    if(!data.joined) page="communities";
    showPage(page);
    if($f("forum-profile-open")){const nickname=data.nickname||"我";$f("forum-profile-open").textContent=Array.from(nickname)[0];$f("forum-profile-open").title=nickname;}
    for(const containerId of ["forum-community-list","forum-owned-communities"]){
      const container=$f(containerId);if(!container)continue;container.replaceChildren();
      for(const community of data.communities || (data.joined?[{id:data.current_community,name:data.name,admin:data.admin}]:[])){
        const entry=button("",()=>selectCommunity(community.id).catch(note));entry.className=containerId==="forum-owned-communities"?"member-row":"community-option";
        const icon=node("span",Array.from(community.name||"社区")[0],"community-icon");const copy=node("span");copy.append(node("strong",community.name),node("small",community.admin?"管理员":"成员"));entry.append(icon,copy);
        if(community.id===data.current_community)entry.append(node("span","✓","community-check"));container.append(entry);
      }
    }
    const sidebar=$f("forum-side-categories");
    if(sidebar){sidebar.replaceChildren();for(const [id,title] of Object.entries(data.categories||{})){const item=button(title,()=>{ $f("forum-category-filter").value=id; showPage("home"); renderTopics(); });item.prepend(node("i",undefined,"dot"));sidebar.append(item);}}

    if($f("forum-header-status"))$f("forum-header-status").textContent=data.peers?"已连接":"未连接";
    $f("forum-network").textContent = data.joined
      ? `${data.peers || 0} 个连接 · ${data.pending || 0} 条记录待补齐 · ${data.disputed || 0} 条争议记录`
      : "先创建论坛，或导入管理员提供的入网材料。";
    if (data.governance_conflict) $f("forum-network").append(" · 管理记录发生分支，授权判断已暂停，请联系管理员。");
    const ownMember = data.members?.find(member => member.id === data.own_public);
    if (ownMember?.status === "expired") $f("forum-network").append(" · 你的成员资格已到期，请联系管理员续期。");
    if (ownMember?.status === "revoked") $f("forum-network").append(" · 你的成员资格已被管理员吊销。");
    const filter = $f("forum-category-filter"), selectedCategory = filter.value;
    filter.replaceChildren(); const all = node("option", "全部版块"); all.value = ""; filter.append(all);
    for (const [id,title] of Object.entries(data.categories || {})) { const option = node("option",title); option.value=id;filter.append(option); }
    filter.value = selectedCategory;
    const downloads = $f("forum-downloads"); downloads.replaceChildren();
    for (const task of data.downloads || []) {
      const row = node("div", undefined, "forum-member");
      row.append(node("span", `附件 ${task.hash.slice(0,8)}：${(task.received/1e6).toFixed(2)} / ${(task.size/1e6).toFixed(2)} MB`), button("取消下载", () => run({kind:"cancel_fetch",hash:task.hash}))); downloads.append(row);
    }
    const reports = $f("forum-reports"); reports.replaceChildren();
    for (const report of data.reports || []) {
      const item = node("div"); item.append(node("p", `${report.author.slice(-8)}：${report.reason}`));
      item.append(button("查看被举报内容", () => run({kind:"topic",id:report.topic_id || report.target,offset:0}))); reports.append(item);
    }
    renderTopics();
    if (selected && !$f("forum-search").value.trim()) renderDetail();
    const members = $f("forum-members");
    members.replaceChildren();
    for (const member of data.members || []) {
      const row = node("div", undefined, "forum-member");
      row.classList.add("member-row");
      const memberName=displayName(member.id), identity=node("div");
      identity.append(node("strong",memberName),node("small",`身份尾号 ${member.id.slice(-8)}`));
      const proof=node("details");proof.append(node("summary","查看身份"),node("code",member.id));identity.append(proof);
      row.append(node("span",Array.from(memberName)[0],"avatar"),identity, node("span", ({active:"有效",expired:"已到期",revoked:"已吊销"}[member.status] || (member.active ? "有效" : "不可用"))));
      if (data.admin && member.active && member.id !== data.own_public) {
        row.append(button("吊销资格", () => {
          if (confirm(`吊销 ${member.id} 的论坛资格？未被检查点确认的修改会进入争议状态。此操作由你的管理身份签署。`)) run({ kind: "revoke", member: member.id });
        }));
      }
      if (member.inviter) row.append(node("small", `邀请者 ${displayName(member.inviter)}`));
      members.append(row);
    }
  }
  function renderTopics() {
    const query = $f("forum-search").value.trim().toLowerCase();
    list.replaceChildren();
    if (query && searchResult) {
      list.hidden = false; detail.replaceChildren();
      for (const result of searchResult.results) {
        const row = node("button", undefined, "forum-topic"); row.type = "button";
        row.append(node("strong", result.title), node("span", result.excerpt || "正文未缓存", "subtle"));
        row.addEventListener("click", async () => {
          $f("forum-search").value = ""; searchResult = null; selected = result.topic_id;
          await run({kind:"topic",id:result.topic_id,offset:0}); renderDetail();
        }); list.append(row);
      }
      if (!searchResult.results.length) list.append(node("p", "已同步标题与本机正文中没有匹配内容。", "subtle"));
      paginate(list, searchResult.offset, searchResult.total, offset => search(offset));
      return;
    }
    const topics = (snapshot?.topics || []).filter(t => !t.reply_to && (!t.hidden || snapshot.admin) &&
      (!$f("forum-category-filter").value || t.category === $f("forum-category-filter").value) &&
      (!query || t.title.toLowerCase().includes(query) || t.versions.some(v => v.body?.toLowerCase().includes(query))));
    for (const topic of topics) {
      const row = node("button", undefined, "forum-topic");
      row.type = "button";
      row.classList.add("topic");
      const main=node("div"); main.append(node("div",`${topic.hidden?"[已隐藏] ":""}${topic.title||"未命名主题"}`,"topic-title"));
      const meta=node("div",undefined,"meta");meta.append(node("span",snapshot.categories?.[topic.category]||"综合","badge"),node("span",displayName(topic.author)));main.append(meta);
      row.append(main,node("div",String(topic.heads.length),"number"),node("div",topic.heads.length>1?"分支待合并":"已同步","number last-activity"));
      row.addEventListener("click", async () => {
        selected = topic.id;
        await run({ kind: "topic", id: topic.id, offset: 0 });
        renderDetail();
      });
      list.append(row);
    }
    paginate(list, pageOffset, snapshot?.total || 0, offset => run({ kind: "page", offset }));
    if (!topics.length) list.append(node("p", query ? "没有匹配的已同步主题。" : "还没有主题，发布第一条内容吧。", "subtle"));
  }
  function paginate(container, offset, total, action) {
    const size = snapshot?.page_size || 20;
    if (total <= size) return;
    const controls = node("nav", undefined, "forum-member");
    controls.setAttribute("aria-label", "分页浏览");
    if (offset > 0) controls.append(button("上一页", () => action(Math.max(0, offset - size))));
    controls.append(node("span", `${offset + 1}–${Math.min(offset + size, total)} / ${total}`));
    if (offset + size < total) controls.append(button("下一页", () => action(offset + size)));
    container.append(controls);
  }
  function bodyView(text) {
    const paragraph = node("p", undefined, "forum-body");
    for (const part of text.split(/(https?:\/\/[^\s<>]+)/g)) {
      if (!/^https?:\/\//.test(part)) { paragraph.append(document.createTextNode(part)); continue; }
      const link = node("a", part); link.href = part;
      link.addEventListener("click", event => {
        event.preventDefault();
        window.__TAURI__.core.invoke("dispatch", {request:{action:"open_link",url:part}}).catch(note);
      });
      paragraph.append(link);
    }
    return paragraph;
  }
  function contentView(topic) {
    const communityId = snapshot?.current_community;
    const locked = topic.locked || Boolean(topic.reply_to && snapshot.topics.find(t => t.id === topic.reply_to)?.locked);
    const article = node("article", undefined, "post forum-post");
    const avatar=node("span",Array.from(displayName(topic.author))[0],"avatar");
    const content=node("div",undefined,"post-content"); article.append(avatar,content);
    const head=node("div",undefined,"post-head"); head.append(node("strong",displayName(topic.author)),node("span","签名已验证","badge")); content.append(head);


    if (topic.heads.length > 1) {
      content.append(node("p", topic.merged_body != null ? "不重叠的修改已自动组合；原始署名版本仍保留。" : "有并发修改，以下版本均已保留。可编辑并提交一个合并版本。", "forum-conflict"));
      if (topic.merged_body != null) content.append(bodyView(topic.merged_body));
    }
    const menu=node("details"); menu.className="post-menu";menu.append(node("summary","•••"));head.append(menu);
    const history = node("details");
    history.append(node("summary", "查看署名修改历史"));
    for (const version of topic.versions || []) {
      const section = node("section", undefined, "forum-version");
      section.append(node("p", `署名 ${displayName(version.author)} · ${version.id.slice(0, 12)}`, "meta"));
      if (version.body_copies != null) section.append(node("p", version.body_copies > 1 ? `正文 ${version.body_copies} 份当前在线副本（含本机）` : version.body != null ? "正文仅本机保存，等待其他节点确认" : "正文尚未缓存", "subtle"));
      if (version.body != null) section.append(bodyView(version.body));
      else section.append(button("查找并获取正文", () => run({ kind: "fetch", hash: version.body_hash })));
      for (const attachment of version.attachments || []) {
        section.append(button(`${attachment.name} · ${(attachment.size / 1e6).toFixed(2)} MB · ${attachment.ready ? "保存文件" : "查找副本"}${attachment.copies != null ? ` · ${attachment.copies} 份在线副本` : ""}`, async () => {
          if (!attachment.ready) { await run({ kind: "fetch", hash: attachment.hash }); return; }
          try {
            const saved = await window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_save",community_id:communityId,hash:attachment.hash,name:attachment.name,size:attachment.size}});
            if (saved) note("文件已保存到选择的位置。");
          } catch(error) { note(error); }
        }));
        if (attachment.ready && attachment.size <= 5_000_000 && /\.(png|jpe?g|webp)$/i.test(attachment.name)) {
          const image = node("img"); image.alt = attachment.name; image.className = "chat-photo";
          const generation=epoch;
          window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_preview",community_id:communityId,hash:attachment.hash}}).then(result=>{
            if(generation===epoch && ["image/png","image/jpeg","image/webp"].includes(result.mime)) image.src=`data:${result.mime};base64,${result.data}`;
          }).catch(()=>{image.alt="图片无法预览，可作为文件保存";});
          section.append(image);
        }
      }
      if (topic.heads.includes(version.id)) content.append(section);
      else history.append(section);
    }
    if (history.children.length > 1) menu.append(history);
    const versions = topic.versions || [];
    if (versions.length > 1) {
      const comparison = node("details"); comparison.append(node("summary", "对照两个版本"));
      const columns = node("div", undefined, "forum-compare");
      [0, versions.length - 1].forEach(initial => {
        const column = node("section"); const picker = node("select");
        picker.setAttribute("aria-label", "选择对照版本");
        versions.forEach((version, index) => { const option = node("option", `${version.author.slice(-8)} · ${version.id.slice(0,8)}`); option.value = index; picker.append(option); });
        const body = node("div"); picker.value = initial;
        const show = () => { const version = versions[Number(picker.value)]; body.replaceChildren(version.body == null ? button("获取此版本正文", () => run({kind:"fetch",hash:version.body_hash})) : bodyView(version.body)); };
        picker.addEventListener("change", show); show(); column.append(picker, body); columns.append(column);
      });
      comparison.append(columns); menu.append(comparison);
    }
    if (locked) content.append(node("p", "管理员已锁定此主题；未确认的并发变更保留为争议。", "forum-conflict"));
    else menu.append(button(topic.heads.length > 1 ? "编辑并确认合并" : "署名编辑", () => openEditor(topic)));
    menu.append(button("举报", () => { const reason = prompt("举报原因（将作为署名记录保存，管理员可处理）"); if(reason?.trim()) run({kind:"report",target:topic.id,reason:reason.trim()}); }));
    return article;
  }
  function renderDetail() {
    detail.replaceChildren();
    const topic = snapshot?.topics?.find(t => t.id === selected);
    if (!topic) return;
    const crumb=node("div",undefined,"crumb"); crumb.append(button("返回主题列表",goHome),document.createTextNode(` / ${snapshot.categories?.[topic.category]||"综合"}`)); detail.append(crumb,node("h1",topic.title||"未命名主题"));
    detail.append(node("div",`${snapshot.reply_total||0} 条回复 · ${snapshot.categories?.[topic.category]||"综合"}`,"meta"));
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
    paginate(detail, snapshot.reply_offset || 0, snapshot.reply_total || 0, offset => run({ kind: "topic", id: selected, offset }));
    if (!topic.locked) {
      detail.append(button("回复", () => openEditor(null, selected)));
      detail.append(button("引用回复", () => { openEditor(null, selected); const current=topic.versions.find(v=>topic.heads.includes(v.id)); $f("forum-body").value=(current?.body || "").split("\n").map(line=>"> "+line).join("\n")+"\n\n"; scheduleDraft(); }));
    }
    if (snapshot.admin) {
      const moderation=node("details");moderation.append(node("summary","管理此主题"));detail.append(moderation);
      moderation.append(button("隐藏主题", () => run({ kind: "moderate", target: selected, hidden: true })));
      moderation.append(button(topic.locked ? "解除主题锁定" : "锁定主题", () => run({kind:"lock",target:selected,locked:!topic.locked})));
      const category = node("select"); category.setAttribute("aria-label", "设置主题版块");
      for(const [id,title] of Object.entries(snapshot.categories || {})) {const option=node("option",title);option.value=id;category.append(option);}
      category.value=topic.category || ""; category.addEventListener("change",()=>run({kind:"categorize",target:selected,category:category.value}));moderation.append(category);
    }
  }
  function openEditor(topic = null, replyTo = null) {
    const current = topic?.versions?.find(version => topic.heads.includes(version.id));
    if (topic && current?.body == null) { note("请先获取当前正文，再提交修改。"); return; }
    editorReturn = selected ? "thread" : "home";
    showPage("editor");
    editor.hidden = false;
    editor.dataset.target = topic?.id || "";
    editor.dataset.replyTo = replyTo || "";
    parents = topic?.heads || [];
    $f("forum-title").value = topic?.title || "";
    $f("forum-body").value = topic?.merged_body ?? current?.body ?? "";
    attachments = [...(current?.attachments || [])];
    renderAttachments();
    $f("forum-submit").textContent = topic ? "签署并提交修改" : "签署并发布";
    $f("forum-body").focus();
  }
  $f("forum-open").addEventListener("click", () => {
    panel.hidden = false; document.body.classList.add("forum-mode");
    $f("workspace").hidden = true;
    $f("composer").hidden = true;
    showPage(page);
    refresh();
  });
  $f("forum-close").addEventListener("click", () => { panel.hidden = true; document.body.classList.remove("forum-mode"); $f("workspace").hidden = false; window.dispatchEvent(new Event("spark-forum-close")); });
  $f("forum-refresh").addEventListener("click", () => run({ kind: "sync" }));
  async function search(offset = 0) {
    const query = $f("forum-search").value.trim();
    const generation = ++searchGeneration, lockedGeneration = epoch;
    if (!query) { searchResult = null; renderTopics(); return; }
    try {
      const result = await command({kind:"search",query,offset});
      if (generation !== searchGeneration || lockedGeneration !== epoch) return;
      searchResult = result.search; renderTopics();
    } catch(error) { if (generation === searchGeneration) note(error); }
  }
  $f("forum-search").addEventListener("input", () => {
    clearTimeout(searchTimer); searchGeneration++;
    searchTimer = setTimeout(() => search(), 250);
  });
  $f("forum-category-filter").addEventListener("change", renderTopics);
  $f("forum-category-save").addEventListener("click", () => run({kind:"category",id:$f("forum-category-id").value.trim(),title:$f("forum-category-title").value.trim()}));
  $f("forum-new").addEventListener("click", () => openEditor());
  $f("forum-cancel").addEventListener("click", () => { clearTimeout(draftTimer); const draft=draftPayload(); draftSave=draftSave.catch(()=>{}).then(()=>command({kind:"draft",...draft})).catch(note); editor.hidden = true; showPage(editorReturn); });
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
    const generation=epoch, communityId=snapshot?.current_community; $f("forum-attach").disabled=true;
    try{
      const result=await window.__TAURI__.core.invoke("dispatch",{request:{action:"forum_attach",community_id:communityId}});
      if(generation!==epoch||communityId!==snapshot?.current_community||!result)return;
      const attachment=result.attachment;
      if(attachments.some(a=>a.hash===attachment.hash)){note("这个附件已经添加。");return;}
      if(attachments.reduce((sum,a)=>sum+a.size,0)+attachment.size>30_000_000){note("附件合计不能超过 30 MB。");return;}
      attachments.push(attachment);renderAttachments();scheduleDraft();
    }catch(error){note(error);}finally{$f("forum-attach").disabled=false;}
  });
  $f("forum-create").addEventListener("click", () => createOrJoin({ kind: "create", name: $f("forum-create-name").value.trim() }));
  $f("forum-join").addEventListener("click", () => createOrJoin({ kind: "join", code: $f("forum-join-code").value.trim() }));
  $f("forum-listen").addEventListener("click", () => run({ kind: "listen" }));
  $f("forum-connect").addEventListener("click", () => run({ kind: "connect", code: $f("forum-peer-code").value.trim() }));
  $f("forum-admit").addEventListener("click", () => run({ kind: "admit", member: $f("forum-member-key").value.trim(), inviter: $f("forum-inviter").value.trim() || null }));
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
  panel.addEventListener("submit",async event=>{
    if(event.target.id!=="forum-profile-form")return;
    event.preventDefault();const nickname=$f("forum-nickname").value.trim();
    if(!nickname)return;
    if(await run({kind:"nickname",nickname}))$f("forum-profile-dialog").close();
  });
  editor.addEventListener("input",scheduleDraft);
  $f("forum-resume-draft").addEventListener("click",()=>{
    const draft=snapshot?.draft;if(!draft || !(draft.body || draft.title || draft.attachments?.length)){openEditor();note("还没有保存的草稿，可以从这里开始写。");return;}
    editorReturn=selected?"thread":"home";showPage("editor");
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
    if (result) { editor.hidden = true; showPage(editorReturn); }
  });
  window.addEventListener("spark-locked", () => {
    epoch += 1; rendered = ""; requested.clear();
    clearTimeout(draftTimer); clearTimeout(searchTimer); searchGeneration++; searchResult = null;
    attachments=[];renderAttachments();
    snapshot = null; selected = null; parents = []; page="home";
    panel.hidden = true; document.body.classList.remove("forum-mode"); detail.replaceChildren(); list.replaceChildren();
    if($f("forum-profile-dialog")?.open)$f("forum-profile-dialog").close();
    for(const id of ["forum-community-list","forum-owned-communities","forum-side-categories","forum-members","forum-reports","forum-downloads"]) $f(id)?.replaceChildren();
    $f("forum-category-filter").replaceChildren();
    for(const id of ["community-current","community-role","forum-name","forum-network","forum-header-status"]) if($f(id))$f(id).textContent="";
    if($f("forum-profile-open")){$f("forum-profile-open").textContent="我";$f("forum-profile-open").removeAttribute("title");}
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
