/* Real frontend, disposable deterministic backend. Native/network tests are separate. */
(() => {
  let listener;
  let active = "peer-alice";
  const peers = { "peer-alice": [], "peer-bob": [] };
  const calls = [];
  const images = { "peer-alice": [], "peer-bob": [] };
  let pendingImage = null;
  let delayImage = false;
  let nextImage = 0;
  let failInitial = false;
  const files = { "peer-alice": [], "peer-bob": [] };
  const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJ1sAAAAASUVORK5CYII=";
  const initial = () => ({ vault_created:true, my_id:"self", peer_id:active, active_session:active,
    position:"测试网络", position_kind:"campus", connected:true, revision:0, history:peers[active], files:files[active], images:images[active],
    sessions:Object.keys(peers).map(id=>({id,connected:true,history:peers[id],files:files[id],images:images[id]})) });
  const forum = {joined:true,admin:true,categories:{general:"综合",study:"学习"},name:"测试论坛",own_public:"public-self",peers:2,pending:0,disputed:0,members:[],topics:[
    {id:"root",title:"<img src=x onerror=alert(1)>",author:"author",reply_to:null,hidden:false,heads:["v2","v3"],versions:[
      {id:"v1",author:"author",body:"原文",body_hash:"h1",attachments:[]},
      {id:"v2",author:"bob",body:"修改甲",body_hash:"h2",attachments:[]},
      {id:"v3",author:"carol",body:"修改乙",body_hash:"h3",attachments:[]},
    ]}]};
  const firstTopic = forum.topics[0];
  Object.assign(firstTopic,{locked:false,category:"general"});
  const topics = [firstTopic, ...["second", "third"].map(id => ({id,title:`主题 ${id}`,author:"author",reply_to:null,hidden:false,heads:[id],versions:[{id,author:"author",body:`${id} https://example.com/resource`,body_hash:id,attachments:[]}]}))];
  const replies = [1,2,3].map(n=>({id:`reply-${n}`,title:"",author:"author",reply_to:"root",hidden:false,heads:[`reply-${n}`],versions:[{id:`reply-${n}`,author:"author",body:`分页回复 ${n}`,body_hash:`reply-${n}`,attachments:[]}]}));
  Object.assign(forum, {total:topics.length,page_offset:0,page_size:2,focused_topic:null,reply_offset:0,reply_total:0,topics:topics.slice(0,2)});
  window.__TAURI__ = {event:{listen:async(name,fn)=>{if(name==="spark-event")listener=fn;}},core:{invoke:async(_, {request})=>{
    calls.push(request);
    switch(request.action) {
      case "initial": if(failInitial)throw new Error("测试状态读取失败");return initial();
      case "select_session": active=request.session_id; return initial();
      case "text": {const message={own:true,ts:Date.now(),text:request.text}; peers[request.session_id].push(message); return message;}
      case "prepare_image": return {mime:"image/png",data:png};
      case "image": {
        if(delayImage) await new Promise(resolve=>{pendingImage=resolve;});
        const result={id:`image-${++nextImage}`,own:true,ts:Date.now(),mime:"image/png"};
        images[request.session_id].push(result);
        return result;
      }
      case "image_preview": return png;
      case "forum": {
        const command=request.command;
        if(command.kind==="lock") topics.find(t=>t.id===command.target).locked=command.locked;
        if(command.kind==="categorize") topics.find(t=>t.id===command.target).category=command.category;
        if(command.kind==="search") return {search:{query:command.query,offset:command.offset,total:1,results:[{topic_id:"third",title:"主题 third",excerpt:"跨页命中"}]}};
        if(command.kind==="page") Object.assign(forum,{page_offset:command.offset,focused_topic:null,reply_offset:0,reply_total:0,topics:topics.slice(command.offset,command.offset+2)});
        if(command.kind==="topic") Object.assign(forum,{focused_topic:command.id,reply_offset:command.offset,reply_total:command.id==="root"?replies.length:0,topics:[topics.find(t=>t.id===command.id),...(command.id==="root"?replies.slice(command.offset,command.offset+2):[])]});
        return {state:structuredClone(forum)};
      }
      default:return null;
    }
  }}};
  const tick=()=>new Promise(resolve=>setTimeout(resolve,30));
  document.addEventListener("DOMContentLoaded", async()=>{
    const results=[];
    const check=(value,message)=>{if(!value)throw new Error(message);results.push(`PASS ${message}`);};
    const $=id=>document.getElementById(id);
    try {
      await tick();
      check($("session-picker").options.length===2,"同时展示两个聊天会话");
      check(document.documentElement.scrollWidth<=window.innerWidth,"多会话导航不撑破手机屏幕");
      $("message").value="Alice的草稿";$("message").dispatchEvent(new Event("input"));
      $("session-picker").value="peer-bob";$("session-picker").dispatchEvent(new Event("change"));await tick();
      check($("message").value==="","会话草稿独立");
      peers["peer-alice"].push({own:false,ts:Date.now(),text:"Alice后台消息"});
      listener({payload:{type:"SessionEvent",data:{session_id:"peer-alice",event:{type:"Msg",data:"Alice后台消息"}}}});
      check(!$("messages").textContent.includes("Alice后台消息"),"后台会话事件不串入当前聊天");
      $("message").value="发给Bob";$("send").click();await tick();
      check(calls.find(c=>c.action==="text").session_id==="peer-bob","发送携带明确会话目标");
      $("session-picker").value="peer-alice";$("session-picker").dispatchEvent(new Event("change"));await tick();
      check($("message").value==="Alice的草稿"&&$("messages").textContent.includes("Alice后台消息"),"切回会话恢复草稿与后台消息");
      const clipboard=new DataTransfer();clipboard.items.add(new File([Uint8Array.from(atob(png),c=>c.charCodeAt(0))],"截图.png",{type:"image/png"}));
      $("message").dispatchEvent(new ClipboardEvent("paste",{clipboardData:clipboard,bubbles:true,cancelable:true}));await tick();
      check(!$("image-draft").hidden&&!calls.some(c=>c.action==="image"),"粘贴仅准备预览，不自动上传");
      $("image-draft-cancel").click();check($("image-draft").hidden,"取消图片不发送");
      $("message").dispatchEvent(new ClipboardEvent("paste",{clipboardData:clipboard,bubbles:true,cancelable:true}));await tick();
      $("message").value="";$("send").click();await tick();
      check(calls.find(c=>c.action==="image").session_id==="peer-alice"&&document.querySelector(".chat-photo"),"图片独立发送并直接显示");
      $("message").dispatchEvent(new ClipboardEvent("paste",{clipboardData:clipboard,bubbles:true,cancelable:true}));await tick();
      $("message").value="随图片发给Alice";$("message").dispatchEvent(new Event("input"));
      delayImage=true;$("send").click();await tick();
      check(typeof pendingImage==="function","图片发送可以模拟等待");
      $("session-picker").value="peer-bob";$("session-picker").dispatchEvent(new Event("change"));await tick();
      pendingImage();delayImage=false;await tick();
      check(calls.filter(c=>c.action==="image").at(-1).session_id==="peer-alice"&&calls.filter(c=>c.action==="text").at(-1).session_id==="peer-alice","等待图片时切换会话不改变图片或附带文字的收件人");
      check(!$("messages").textContent.includes("随图片发给Alice"),"后台发送完成不污染当前会话");
      $("session-picker").value="peer-alice";$("session-picker").dispatchEvent(new Event("change"));await tick();
      check($("message").value===""&&$("image-draft").hidden,"后台发送完成清理原会话草稿，避免重发");
      $("forum-open").click();await tick();
      check(!$("forum-panel").hidden&&$("workspace").hidden,"论坛独立入口");
      check(!document.querySelector("#forum-topics img"),"论坛标题不能注入HTML");
      const pageButton=(container,label)=>[...container.querySelectorAll("button")].find(button=>button.textContent===label);
      check($("forum-topics").querySelectorAll(".forum-topic").length===2,"主题列表只展示当前页");
      pageButton($("forum-topics"),"下一页").click();await tick();
      check($("forum-topics").textContent.includes("主题 third")&&!$("forum-topics").textContent.includes("主题 second"),"主题下一页获取对应分页");
      pageButton($("forum-topics"),"上一页").click();await tick();
      document.querySelector(".forum-topic").click();await tick();
      check($("forum-detail").textContent.includes("修改甲")&&$("forum-detail").textContent.includes("修改乙"),"并发版本完整展示");
      check(!$("forum-detail").textContent.includes("私聊"),"论坛不提供私聊入口");
      const categorySelect=$("forum-detail").querySelector('select[aria-label="设置主题版块"]');
      categorySelect.value="study";categorySelect.dispatchEvent(new Event("change"));await tick();
      check(calls.some(c=>c.action==="forum"&&c.command.kind==="categorize"&&c.command.target==="root"&&c.command.category==="study"),"管理员分类操作携带主题与目标版块");
      pageButton($("forum-detail"),"锁定主题").click();await tick();
      check(calls.some(c=>c.action==="forum"&&c.command.kind==="lock"&&c.command.locked===true)&&$("forum-detail").textContent.includes("管理员已锁定"),"管理员可锁定主题并展示锁定状态");
      check(![...$("forum-detail").querySelectorAll("button")].some(b=>b.textContent.includes("署名编辑")||b.textContent==="回复"),"锁定主题隐藏署名编辑和回复入口");
      pageButton($("forum-detail"),"解除主题锁定").click();await tick();

      check($("forum-detail").textContent.includes("分页回复 1")&&!$("forum-detail").textContent.includes("分页回复 3"),"主题详情按页展示回复");
      pageButton($("forum-detail"),"下一页").click();await tick();
      check($("forum-detail").textContent.includes("分页回复 3")&&!$("forum-detail").textContent.includes("分页回复 1"),"回复翻页保留主题并加载下一页");
      pageButton($("forum-detail"),"返回主题列表").click();await tick();
      check(!$("forum-topics").hidden&&$("forum-detail").children.length===0,"返回主题列表清除详情视图");
      $("forum-search").value="third";$("forum-search").dispatchEvent(new Event("input"));
      await new Promise(resolve=>setTimeout(resolve,320));
      check(calls.some(c=>c.action==="forum"&&c.command.kind==="search"&&c.command.query==="third")&&$("forum-topics").textContent.includes("跨页命中"),"搜索向后端查询当前页之外的主题");
      document.querySelector(".forum-topic").click();await tick();
      check(calls.filter(c=>c.action==="forum"&&c.command.kind==="topic").at(-1).command.id==="third"&&$("forum-detail").textContent.includes("https://example.com/resource"),"跨页搜索结果加载对应主题详情");
      check(!calls.some(c=>c.action==="open_link"),"展示外链不会自动打开或请求外部网站");
      $("forum-detail").querySelector("a").click();await tick();
      check(calls.filter(c=>c.action==="open_link").length===1&&calls.find(c=>c.action==="open_link").url==="https://example.com/resource","外链仅在点击时交给原生浏览器");
      $("forum-close").click();
      files["peer-bob"]=[{id:"pending",name:"后台文件",outgoing:false,complete:false,accepted:false,failed:""}];
      $("lock").click();await tick();
      check($("lock-confirm").open&&!calls.some(c=>c.action==="lock"),"后台会话待保存附件也必须确认锁定");
      $("cancel-lock").click();files["peer-bob"]=[];
      failInitial=true;$("lock").click();await tick();
      check(!calls.some(c=>c.action==="lock")&&$("banner-text").textContent.includes("尚未锁定"),"读取附件状态失败时不直接锁定");
      failInitial=false;$("lock").click();await tick();
      check($("forum-panel").hidden&&$("messages").children.length===0&&$("image-draft-preview").getAttribute("src")===null,"锁定清除会话图片和论坛视图");
      document.title=`UI checks: ${results.length} passed`;
    } catch(error) {results.push(`FAIL ${error.message}`);document.title="UI checks: FAILED";}
    const output=document.createElement("pre");output.id="check-results";output.textContent=results.join("\n");document.body.append(output);
  });
})();
