// 在临时副本中隔离 OpenCLI 固定端口，复用上游真实扩展；不修改用户 HOME 或现有 daemon。
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {createInterface} from 'node:readline';
const {GOLUTRA_AGENT_CHROME_TEST_BINARY:chromeBinary,GOLUTRA_AGENT_OPENCLI_PACKAGE:packageRoot,GOLUTRA_AGENT_OPENCLI_EXTENSION:extensionRoot}=process.env;
const backgroundAcceptance=process.env.GOLUTRA_AGENT_OPENCLI_BACKGROUND_ACCEPTANCE==='1';
if(!chromeBinary||!packageRoot||!extensionRoot)throw Error('Set GOLUTRA_AGENT_CHROME_TEST_BINARY, GOLUTRA_AGENT_OPENCLI_PACKAGE and GOLUTRA_AGENT_OPENCLI_EXTENSION');
const root=await fs.mkdtemp(path.join(os.tmpdir(),'golutra-opencli-live-'));
console.log('OpenCLI fixture:',root);
const app=path.join(root,'app');const extension=path.join(root,'extension');
await fs.cp(packageRoot,app,{recursive:true});
await fs.symlink(path.resolve(packageRoot,'../..'),path.join(app,'node_modules'),'dir');
await fs.cp(extensionRoot,extension,{recursive:true});
const reserve=http.createServer();reserve.listen(0,'127.0.0.1');await once(reserve,'listening');const port=reserve.address().port;await new Promise(r=>reserve.close(r));
for(const file of [path.join(app,'dist/src/constants.js'),path.join(extension,'dist/background.js')]) {
  const source=await fs.readFile(file,'utf8');if(!source.includes('19825'))throw Error('Fixture port marker missing');
  await fs.writeFile(file,source.replaceAll('19825',String(port)));
}
const env={...process.env,OPENCLI_CONFIG_DIR:path.join(root,'config'),OPENCLI_CACHE_DIR:path.join(root,'cache'),OPENCLI_NO_UPDATE_CHECK:'1',OPENCLI_DAEMON_PORT:'',OPENCLI_PROFILE:''};
const waitFor=async fn=>{const end=Date.now()+30000;while(Date.now()<end){if(await fn())return;await new Promise(r=>setTimeout(r,100));}throw Error('Fixture readiness timeout');};
const server=http.createServer((_req,res)=>{res.setHeader('Content-Type','text/html;charset=utf-8');res.end('<!doctype html><title>Golutra OpenCLI</title><label>姓名<input id="name"></label><button id="save" onclick="document.querySelector(\'#result\').textContent=document.querySelector(\'#name\').value">保存</button><button>备用</button><output id="result"></output>');});
server.listen(0,'127.0.0.1');await once(server,'listening');const url=`http://127.0.0.1:${server.address().port}/`;
const children=[];let errors='';
const start=(program,args,options={})=>{const child=spawn(program,args,{env,stdio:['ignore','pipe','pipe'],...options});children.push(child);child.stdout?.on('data',b=>process.stdout.write(b));child.stderr?.on('data',b=>{errors=(errors+b).slice(-12000);});return child;};
try {
  start(process.execPath,[path.join(app,'dist/src/daemon.js')]);
  await waitFor(()=>fetch(`http://127.0.0.1:${port}/ping`).then(r=>r.ok).catch(()=>false));
  start(chromeBinary,[`--user-data-dir=${root}/profile`,`--load-extension=${extension}`,`--disable-extensions-except=${extension}`,'--headless=new','--remote-debugging-port=0','--no-first-run','--no-default-browser-check','--disable-background-networking','about:blank']);
  await waitFor(()=>fetch(`http://127.0.0.1:${port}/status`,{headers:{'X-OpenCLI':'1'}}).then(r=>r.json()).then(s=>s.extensionConnected).catch(()=>false));
  const testArgs=backgroundAcceptance
    ? ['test','-p','golutra-agent-tools','--test','background_browser_live','chrome_background_browser_live','--','--ignored','--nocapture','--test-threads=1']
    : ['test','-p','golutra-agent-tools','--test','opencli_browser_live','--','--ignored','--nocapture'];
  const testEnv={...env,GOLUTRA_AGENT_OPENCLI_LIVE_EXECUTABLE:path.join(app,'dist/src/main.js'),GOLUTRA_AGENT_OPENCLI_LIVE_URL:url};
  // The isolated extension gets its own identity; never bind it to a user's Profile.
  delete testEnv.GOLUTRA_AGENT_OPENCLI_LIVE_PROFILE;
  const test=start('cargo',testArgs,{env:testEnv});
  let output='';test.stdout.on('data',b=>{output=(output+b).slice(-128*1024);});
  const [code]=await once(test,'exit');
  if(backgroundAcceptance) {
    const manifest=output.match(/^Finish manifest after inspection: (.+)$/m)?.[1]?.trim();
    if(!manifest)throw Error(`Background acceptance produced no finish manifest (exit ${code})`);
    await fs.writeFile(path.join(root,'background-manifest.txt'),manifest+'\n');
    console.log(`Background acceptance exit ${code}; inspect ${manifest}. Browser remains alive. Send finish on stdin after inspection.`);
    const lines=createInterface({input:process.stdin,terminal:false});
    let confirmed=false;
    try {for await(const line of lines) {if(line.trim()==='finish'){confirmed=true;break;}console.log('Send finish after inspecting artifacts.');}}
    finally {lines.close();}
    if(!confirmed)throw Error('stdin closed before the inspection/finish handoff');
    const finish=start('cargo',['test','-p','golutra-agent-tools','--test','background_browser_live','background_browser_live_finish','--','--ignored','--nocapture','--test-threads=1'],{env:{...testEnv,GOLUTRA_AGENT_BACKGROUND_LIVE_MANIFEST:manifest}});
    const [finishCode]=await once(finish,'exit');if(finishCode!==0)throw Error(`Background task finish exited ${finishCode}`);
  }
  if(code!==0)throw Error(`Rust acceptance exited ${code}`);
  console.log('PASS isolated OpenCLI acceptance');
} catch(error) {console.error(errors);throw error;}
finally {
  for(const child of children.reverse()) {if(child.exitCode!==null)continue;child.kill('SIGTERM');await Promise.race([once(child,'exit'),new Promise(r=>setTimeout(r,3000))]);if(child.exitCode===null)child.kill('SIGKILL');}
  server.close();await fs.writeFile(path.join(root,'stderr.log'),errors);
}
