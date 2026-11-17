// 历史原生桥接兼容夹具；当前 Chrome 产品后端验收使用 ../opencli/live-test.mjs。
// 独立 Chrome for Testing 验收夹具。只控制本脚本创建的 Profile、标签页与下载目录。
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
const binary=process.env.GOLUTRA_AGENT_CHROME_TEST_BINARY;
if(!binary)throw Error('Set GOLUTRA_AGENT_CHROME_TEST_BINARY to Chrome for Testing (extension loading enabled)');
const root=await fs.mkdtemp(path.join(os.tmpdir(),'golutra-chrome-live-'));
console.log('Chrome fixture:',root);
const extension=path.resolve('extensions/chrome');
const id='eeefgmejgclnckofpckhcponhikcpdlc';
const connectionDir=path.join(root,'bridge');
const nativeExecutable=path.join(root,'golutra-browser-host');
await fs.copyFile(path.resolve('target/debug/examples/chrome_fixture_host'),nativeExecutable);
await fs.chmod(nativeExecutable,0o700);
// 指定 user-data-dir 后 Native Messaging 随之使用隔离目录，不修改用户已有注册。
if(process.platform!=='darwin')throw Error('This bootstrap currently targets macOS; use the Rust test with a manually authorized Chrome tab on other platforms');
const nativeDir=path.join(root,'profile/NativeMessagingHosts');
await fs.mkdir(nativeDir,{recursive:true});
const manifest=path.join(nativeDir,'com.golutra.browser.json');
const original=await fs.readFile(manifest).catch(e=>{if(e.code!=='ENOENT')throw e;return null;});
await fs.writeFile(manifest,JSON.stringify({name:'com.golutra.browser',description:'Golutra local acceptance',path:nativeExecutable,type:'stdio',allowed_origins:[`chrome-extension://${id}/`]}),{mode:0o600});
const server=http.createServer((request,response)=>{
  if(request.url==='/popup'){response.writeHead(200,{'content-type':'text/html'});response.end('<title>Golutra popup continuation</title><p id="continued">Popup continuation</p>');return;}
  if(request.url==='/cross-frame'){response.writeHead(200,{'content-type':'text/html;charset=utf-8'});response.end('<label>跨域<input id="cross-name"></label><button id="cross-save" onclick="document.querySelector(\'#cross-result\').textContent=document.querySelector(\'#cross-name\').value">保存</button><output id="cross-result"></output>');return;}
  if(request.url==='/frame'){response.writeHead(200,{'content-type':'text/html'});response.end('<button id="frame-button" onclick="this.textContent=\'Frame done\'">Frame</button>');return;}
  if(request.url==='/download'){response.writeHead(200,{'content-type':'text/plain','content-disposition':'attachment; filename="golutra-chrome-receipt.txt"'});response.end('golutra-chrome-download-ok');return;}
  response.writeHead(200,{'content-type':'text/html;charset=utf-8'});
  response.end('<!doctype html><title>Golutra Chrome acceptance</title><label>Name <input id="name"></label><button id="save" onclick="document.querySelector(\'#result\').textContent=document.querySelector(\'#name\').value">Save</button><output id="result"></output><input id="upload" type="file"><button id="popup" onclick="window.open(\'/popup\')">Open popup</button><div style="height:2000px"></div><iframe src="/frame"></iframe><iframe src="http://localhost:'+server.address().port+'/cross-frame"></iframe>');
});
server.listen(0,'127.0.0.1');await once(server,'listening');
const url=`http://127.0.0.1:${server.address().port}/`;
const browser=spawn(binary,[`--user-data-dir=${root}/profile`,`--load-extension=${extension}`,`--disable-extensions-except=${extension}`,'--site-per-process','--headless=new','--remote-debugging-port=0','--no-first-run','--no-default-browser-check','--disable-background-networking','--enable-logging=stderr','--vmodule=native_message*=2','about:blank'],{env:{...process.env,GOLUTRA_AGENT_CHROME_TEST_CONNECTION_DIR:connectionDir},stdio:['ignore','ignore','pipe']});
let stderr='';browser.stderr.on('data',chunk=>{stderr=(stderr+chunk).slice(-12000);});
let socket;
const waitFor=async(fn,timeout=15000)=>{const end=Date.now()+timeout;while(Date.now()<end){const result=await fn();if(result)return result;await new Promise(r=>setTimeout(r,100));}throw Error('condition timed out');};
try{
  const portFile=await waitFor(()=>fs.readFile(path.join(root,'profile/DevToolsActivePort'),'utf8').catch(()=>null));
  const [port,wsPath]=portFile.trim().split('\n');
  socket=new WebSocket(`ws://127.0.0.1:${port}${wsPath}`);await once(socket,'open');
  let next=0;const pending=new Map();
  socket.addEventListener('message',event=>{const message=JSON.parse(event.data);const item=pending.get(message.id);if(item){pending.delete(message.id);message.error?item.reject(Error(JSON.stringify(message.error))):item.resolve(message.result);}});
  const send=(method,params={},sessionId)=>new Promise((resolve,reject)=>{const id=++next;const timer=setTimeout(()=>{pending.delete(id);reject(Error(`CDP timeout: ${method}`));},25000);pending.set(id,{resolve:r=>{clearTimeout(timer);resolve(r);},reject:e=>{clearTimeout(timer);reject(e);}});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));});
  await send('Browser.setDownloadBehavior',{behavior:'allow',downloadPath:path.join(root,'downloads')});
  const page=await send('Target.createTarget',{url});
  const popup=await send('Target.createTarget',{url:`chrome-extension://${id}/popup.html`});
  const {sessionId}=await send('Target.attachToTarget',{targetId:popup.targetId,flatten:true});
  await waitFor(async()=>{const r=await send('Runtime.evaluate',{expression:'typeof show === "function" && document.querySelector("#status")?.textContent !== "正在检查连接…"',returnByValue:true},sessionId);return r.result?.value===true;});
  await send('Target.activateTarget',{targetId:page.targetId});
  // 全新 Profile 不点击任何开启/授权按钮，验证安装后的默认行为。
  await waitFor(async()=>{const r=await send('Runtime.evaluate',{expression:'show("status").then(() => document.querySelector("#status").textContent)',awaitPromise:true,returnByValue:true},sessionId);return r.result?.value?.startsWith('已连接');});
  const defaults=await send('Runtime.evaluate',{expression:'document.querySelector("#enable-auto").hidden',returnByValue:true},sessionId);
  if(defaults.result?.value!==true)throw Error('Fresh install must not require enabling automation');
  const handoff=path.join(root,'popup-handoff');
  const test=spawn('cargo',['test','-p','golutra-agent-tools','--test','chrome_browser_live','--','--ignored','--nocapture'],{stdio:'inherit',env:{...process.env,GOLUTRA_AGENT_CHROME_LIVE_URL:url,GOLUTRA_AGENT_CHROME_POPUP_HANDOFF:handoff,GOLUTRA_AGENT_CHROME_TEST_CONNECTION_DIR:connectionDir,GOLUTRA_AGENT_CHROME_TEST_AUTOMATIC:'1'}});
  const exited=once(test,'exit');
  try {
    await waitFor(async()=>{if(test.exitCode!==null)throw Error('Rust test exited before popup handoff');return fs.readFile(handoff).catch(()=>null);},60000);
    const child=await waitFor(async()=>{const {targetInfos}=await send('Target.getTargets');return targetInfos.find(t=>t.type==='page' && t.url===`${url}popup`);});
    await send('Target.activateTarget',{targetId:child.targetId});
    const continued=await send('Runtime.evaluate',{expression:'show("continue").then(() => document.querySelector("#status").textContent)',awaitPromise:true,returnByValue:true},sessionId);
    if(!continued.result?.value?.includes('ready'))throw Error(`Popup continuation failed: ${JSON.stringify(continued)}`);
    await fs.writeFile(handoff+'.done','authorized');
    const [code]=await exited;if(code!==0)throw Error(`Rust acceptance exited ${code}`);
  } finally {if(test.exitCode===null){test.kill('SIGTERM');await exited;}}
  // 在同一测试目标上覆盖 UI 停止与重新授权，而非直接更改扩展存储状态。
  await send('Runtime.evaluate',{expression:'document.querySelector("#authorize").click()'},sessionId);
  await waitFor(async()=>{const r=await send('Runtime.evaluate',{expression:'document.querySelector("#status").textContent',returnByValue:true},sessionId);return r.result?.value?.includes('ready');});
  await send('Runtime.evaluate',{expression:'document.querySelector("#stop").click()'},sessionId);
  await waitFor(async()=>{const r=await send('Runtime.evaluate',{expression:'document.querySelector("#status").textContent',returnByValue:true},sessionId);return r.result?.value?.includes('waiting_for_user');});
  await send('Runtime.evaluate',{expression:'document.querySelector("#disconnect").click()'},sessionId);
  const status=await send('Runtime.evaluate',{expression:'show("status").then(() => document.querySelector("#status").textContent)',awaitPromise:true,returnByValue:true},sessionId);
  if(!status.result?.value?.startsWith('未连接'))throw Error('Disconnect failed');
  const {targetInfos}=await send('Target.getTargets');
  if(!targetInfos.some(t=>t.targetId===page.targetId && t.url===url))throw Error('Pre-existing page was changed');
  console.log('PASS Chrome extension popup: zero setup clicks, automatic connection and new task tabs, private tab preserved, stop, disconnect');
}catch(error){await fs.writeFile(path.join(root,'browser-stderr.log'),stderr);console.error(stderr);throw error;}
finally{socket?.close();browser.kill('SIGTERM');await Promise.race([once(browser,'exit'),new Promise(r=>setTimeout(r,3000))]);server.close();if(original)await fs.writeFile(manifest,original);else await fs.unlink(manifest);}
