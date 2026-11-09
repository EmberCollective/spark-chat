/* Real frontend, disposable deterministic backend. Native/network tests are separate. */
(() => {
  let listener;
  let active = "peer-alice";
  const peers = { "peer-alice": [], "peer-bob": [] };
  const calls = [];
  const png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJ1sAAAAASUVORK5CYII=";
  const initial = () => ({ vault_created:true, my_id:"self", peer_id:active, active_session:active,
    position:"测试网络", position_kind:"campus", connected:true, revision:0, history:peers[active], files:[], images:[],
    sessions:Object.keys(peers).map(id=>({id,connected:true,history:peers[id],files:[],images:[]})) });
  const forum = {joined:true,admin:false,name:"测试论坛",own_public:"public-self",peers:2,pending:0,disputed:0,members:[],topics:[
    {id:"root",title:"<img src=x onerror=alert(1)>",author:"author",reply_to:null,hidden:false,heads:["v2","v3"],versions:[
      {id:"v1",author:"author",body:"原文",body_hash:"h1",attachments:[]},
      {id:"v2",author:"bob",body:"修改甲",body_hash:"h2",attachments:[]},
      {id:"v3",author:"carol",body:"修改乙",body_hash:"h3",attachments:[]},
    ]}]};
  window.__TAURI__ = {event:{listen:async(name,fn)=>{if(name==="spark-event")listener=fn;}},core:{invoke:async(_, {request})=>{
    calls.push(request);
    switch(request.action) {
      case "initial": return initial();
      case "select_session": active=request.session_id; return initial();
      case "text": {const message={own:true,ts:Date.now(),text:request.text}; peers[request.session_id].push(message); return message;}
      case "prepare_image": return {mime:"image/png",data:png};
      case "image": return {id:"image-1",own:true,ts:Date.now(),mime:"image/png"};
      case "image_preview": return png;
      case "forum": return {state:forum};
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
      $("forum-open").click();await tick();
      check(!$("forum-panel").hidden&&$("workspace").hidden,"论坛独立入口");
      check(!document.querySelector("#forum-topics img"),"论坛标题不能注入HTML");
      document.querySelector(".forum-topic").click();
      check($("forum-detail").textContent.includes("修改甲")&&$("forum-detail").textContent.includes("修改乙"),"并发版本完整展示");
      check(!$("forum-detail").textContent.includes("私聊"),"论坛不提供私聊入口");
      $("forum-close").click();$("lock").click();await tick();
      check($("forum-panel").hidden&&$("messages").children.length===0&&$("image-draft-preview").getAttribute("src")===null,"锁定清除会话图片和论坛视图");
      document.title=`UI checks: ${results.length} passed`;
    } catch(error) {results.push(`FAIL ${error.message}`);document.title="UI checks: FAILED";}
    const output=document.createElement("pre");output.id="check-results";output.textContent=results.join("\n");document.body.append(output);
  });
})();
