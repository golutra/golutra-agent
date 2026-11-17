// 使用真实后台模块验证授权生命周期；不把此夹具当作浏览器实机验收。
import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';

async function fixture(local = {}, session = {}, handshake = true) {
  const event = () => ({listeners:[], addListener(fn){this.listeners.push(fn);}, emit(...args){for(const fn of this.listeners) fn(...args);}});
  const area = data => ({get:async key=>({[key]:structuredClone(data[key])}),set:async value=>Object.assign(data,structuredClone(value))});
  const id = 'eeefgmejgclnckofpckhcponhikcpdlc';
  const tabs = new Map([[1,{id:1,url:'https://private.example.test/',title:'Private'}]]);
  const calls = []; const ports = []; const alarms = new Set(); const replies = new Map();
  const api = {
    storage:{local:area(local),session:area(session)},
    alarms:{onAlarm:event(),create:async name=>alarms.add(name),clear:async name=>alarms.delete(name)},
    runtime:{id,onMessage:event(),onStartup:event(),getURL:name=>`chrome-extension://${id}/${name}`,
      connectNative(){
        const port = {onMessage:event(),onDisconnect:event(),disconnect(){this.onDisconnect.emit();},postMessage(value){
          if(value.type==='hello') {if(handshake) queueMicrotask(()=>this.onMessage.emit({type:'ready',version:1}));}
          else {replies.get(value.id)?.(value);replies.delete(value.id);}
        }};
        ports.push(port);return port;
      }},
    tabs:{onRemoved:event(),get:async id=>{if(!tabs.has(id))throw Error('No tab');return tabs.get(id);},query:async()=>[tabs.get(1)],
      create:async options=>{const tab={id:tabs.size+1,title:'Task',...options};tabs.set(tab.id,tab);return tab;}},
    debugger:{onDetach:event(),onEvent:event(),attach:async source=>calls.push(['attach',source.tabId]),detach:async()=>{},
      sendCommand:async(source,method)=>{calls.push([method,source.tabId]);return {result:{value:{observed:true}}};}},
  };
  globalThis.chrome = api;
  await import(`./background.js?test=${randomUUID()}`);
  const ui = action=>new Promise(resolve=>api.runtime.onMessage.listeners[0]({action},{id,url:api.runtime.getURL('popup.html')},resolve));
  // 等待模块初始化，不依赖固定浏览器时序。
  await ui('status');
  const request = (tool,args={},owner='a',binding=null,id=randomUUID())=>new Promise(resolve=>{
    replies.set(id,resolve);ports.at(-1).onMessage.emit({id,tool,args,session:owner,binding});
  });
  return {api,local,session,tabs,calls,ports,alarms,ui,request};
}

test('fresh installation runs new task pages automatically without taking private tabs or clicking setup',async()=>{
  const f=await fixture();
  assert.equal(f.ports.length,1);
  const status=await f.request('browser_status');
  assert.equal(status.ok,true);assert.equal(status.result.approved,false);assert.equal(status.result.can_open_task,true);
  assert.equal(f.tabs.size,1);
  const callId=randomUUID();
  const opened=await f.request('browser_run',{action:'open',url:'https://example.test/one'},'a',null,callId);
  assert.equal(opened.ok,true);assert.equal(opened.tab_id,2);assert.equal(f.tabs.get(1).url,'https://private.example.test/');
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/one'},'a',null,callId)).code,'DUPLICATE_CALL');
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/two'},'b')).code,'TAB_BUSY');
  assert.equal(f.tabs.size,2);
  assert.equal((await f.request('browser_run',{action:'snapshot'},'a',opened.binding)).ok,true);
  assert.ok(f.calls.every(([,tab])=>tab===2));
  assert.equal((await f.request('browser_finish',{},'a',opened.binding)).ok,true);
  assert.equal(f.tabs.size,2);
  const second=await f.request('browser_run',{action:'open',url:'https://example.test/two'},'b');
  assert.equal(second.ok,true);assert.equal(second.tab_id,3);
  await f.ui('stop');
  assert.equal(f.local.settings.autoTasks,false);
  assert.equal((await f.request('browser_run',{action:'snapshot',resume_after_user:true},'b',second.binding)).code,'USER_IN_CONTROL');
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/'},'c')).code,'AUTO_TASKS_DISABLED');
  await f.ui('enable-auto');
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/'},'c')).ok,true);
  assert.equal((await f.request('browser_run',{action:'snapshot'},'b',second.binding)).code,'BINDING_CHANGED');
  await f.ui('disconnect');
});

test('upgrade preserves explicit stop instead of enabling the new default again',async()=>{
  const f=await fixture({settings:{autoTasks:false,autoConnect:true}});
  assert.equal((await f.request('browser_status')).result.can_open_task,false);
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/'})).code,'AUTO_TASKS_DISABLED');
  assert.equal(f.tabs.size,1);
  await f.ui('disconnect');
});

test('stopping during asynchronous tab creation leaves no controlled target and does not replay',async()=>{
  const f=await fixture({settings:{autoTasks:true}});
  let release, reached;const creating=new Promise(r=>{reached=r;});
  const create=f.api.tabs.create;
  f.api.tabs.create=async options=>{reached();await new Promise(r=>{release=r;});return create(options);};
  const pending=f.request('browser_run',{action:'open',url:'https://example.test/'});
  await creating;await f.ui('stop');release();
  assert.equal((await pending).code,'USER_IN_CONTROL');
  assert.equal(f.session.selected,null);assert.equal(f.calls.length,0);assert.equal(f.tabs.size,2);
  await f.ui('disconnect');
});

test('connection loss freezes active task; explicit disconnect survives worker restart and disables retries',async()=>{
  const f=await fixture({settings:{autoTasks:true}});
  const opened=await f.request('browser_run',{action:'open',url:'https://example.test/'});
  f.ports[0].disconnect();await f.ui('status');
  assert.equal(f.alarms.has('golutra-reconnect'),true);
  f.api.alarms.onAlarm.emit({name:'golutra-reconnect'});
  await f.ui('connect');
  assert.equal(f.ports.length,2);
  assert.equal((await f.request('browser_run',{action:'snapshot'},'a',opened.binding)).code,'USER_IN_CONTROL');
  assert.equal((await f.request('browser_run',{action:'open',url:'https://example.test/'},'b')).code,'TAB_BUSY');
  await f.ui('disconnect');
  assert.equal(f.local.settings.autoConnect,false);assert.equal(f.local.settings.autoTasks,false);assert.equal(f.alarms.size,0);
  const restarted=await fixture(f.local,f.session);
  assert.equal(restarted.ports.length,0);
  restarted.api.alarms.onAlarm.emit({name:'golutra-reconnect'});await restarted.ui('status');
  assert.equal(restarted.ports.length,0);
});

test('startup retains automatic-task consent but never creates a page on status or accepts non-web URLs',async()=>{
  const f=await fixture({settings:{autoTasks:true}});
  assert.equal((await f.request('browser_status')).result.can_open_task,true);
  assert.equal(f.tabs.size,1);
  for(const url of ['file:///etc/hosts','chrome://settings','javascript:alert(1)']) {
    assert.equal((await f.request('browser_run',{action:'open',url})).code,'UNSUPPORTED_URL');
  }
  assert.equal(f.tabs.size,1);assert.equal(f.calls.length,0);
  await f.ui('disconnect');
});

test('disconnect invalidates a pending handshake and ignores late ready messages',async()=>{
  const f=await fixture({}, {}, false);
  await f.ui('disconnect');
  f.ports[0].onMessage.emit({type:'ready',version:1});
  assert.equal((await f.ui('status')).connected,false);
  f.ports[0].onMessage.emit({type:'ready',version:1});
  assert.equal((await f.ui('status')).connected,false);
  assert.equal(f.alarms.size,0);
  assert.equal(f.local.settings.autoConnect,false);
});

test('late tab lookup cannot reconnect or authorize after explicit disconnect',async()=>{
  const f=await fixture();
  let release;
  f.api.tabs.query=()=>new Promise(resolve=>{release=resolve;});
  const pending=f.ui('authorize');
  await f.ui('status');
  await f.ui('disconnect');
  release([f.tabs.get(1)]);
  assert.equal((await pending).ok,false);
  assert.equal(f.local.settings.autoConnect,false);
  assert.equal(f.ports.length,1);
  assert.equal(f.session.selected,null);
});

test('closing a finished page retains automatic consent; closing an active page stops control',async()=>{
  const f=await fixture({settings:{autoTasks:true}});
  const first=await f.request('browser_run',{action:'open',url:'https://example.test/'});
  await f.request('browser_finish',{},'a',first.binding);
  f.tabs.delete(first.tab_id);f.api.tabs.onRemoved.emit(first.tab_id);
  assert.equal((await f.request('browser_status')).result.can_open_task,true);
  const next=await f.request('browser_run',{action:'open',url:'https://example.test/'});
  f.tabs.delete(next.tab_id);f.api.tabs.onRemoved.emit(next.tab_id);
  const status=await f.request('browser_status',{},'a',next.binding);
  assert.equal(status.ok,true);assert.equal(status.result.closed,true);
  assert.equal(status.result.approved,false);assert.equal(status.result.can_open_task,false);
  assert.equal(f.local.settings.autoTasks,false);
  await f.ui('disconnect');
});
