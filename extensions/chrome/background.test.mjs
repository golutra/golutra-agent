import test from 'node:test';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';

test('selected tab authorization, exclusive session, handoff, stop, rebind and duplicate request guards',async()=>{
  const event=()=>({listeners:[],addListener(fn){this.listeners.push(fn);},emit(...args){for(const fn of this.listeners)fn(...args);}});
  const storage={};const runtimeEvent=event();const detachEvent=event();
  const id='eeefgmejgclnckofpckhcponhikcpdlc';
  let active=1;let count=0;let reply;let native;
  globalThis.chrome={
    storage:{local:{get:async()=>({}),set:async()=>{}},session:{get:async key=>({[key]:structuredClone(storage[key])}),set:async value=>Object.assign(storage,structuredClone(value))}},
    alarms:{onAlarm:event(),create:async()=>{},clear:async()=>{}},
    runtime:{id,getURL:name=>`chrome-extension://${id}/${name}`,onMessage:runtimeEvent,onStartup:event(),
      connectNative(){native={onMessage:event(),onDisconnect:event(),disconnect(){this.onDisconnect.emit();},postMessage(value){
        if(value.type==='hello')queueMicrotask(()=>this.onMessage.emit({type:'ready',version:1}));else reply(value);
      }};return native;}},
    tabs:{query:async()=>[{id:active,url:`https://example.test/${active}`}],get:async tabId=>({id:tabId,title:'Fixture',url:`https://example.test/${tabId}`}),onRemoved:event()},
    debugger:{attach:async()=>{},detach:async source=>detachEvent.emit(source),onDetach:detachEvent,onEvent:event(),
      sendCommand:async(source,method)=>{count++;assert.equal(source.tabId,active);return method==='Runtime.evaluate'?{result:{value:{observed:true}}}:{};}},
  };
  await import('./background.js');
  const ui=action=>new Promise(resolve=>runtimeEvent.listeners[0]({action},{id,url:`chrome-extension://${id}/popup.html`},resolve));
  const request=async(tool,args={},session='a',binding=null,requestId=randomUUID())=>{
    const result=new Promise(resolve=>{reply=resolve;});
    native.onMessage.emit({id:requestId,tool,args,session,binding});return result;
  };
  assert.equal((await ui('authorize')).connected,true);
  const first=await request('browser_run',{action:'snapshot'});
  assert.equal(first.ok,true);
  const before=count;
  assert.equal((await request('browser_run',{action:'snapshot'},'b')).code,'TAB_BUSY');
  assert.equal(count,before);
  assert.equal((await request('browser_handoff')).ok,true);
  assert.equal((await request('browser_run',{action:'snapshot'})).code,'USER_IN_CONTROL');
  assert.equal((await request('browser_run',{action:'snapshot',resume_after_user:true})).ok,true);
  await ui('stop');
  assert.equal((await request('browser_run',{action:'snapshot',resume_after_user:true})).code,'USER_IN_CONTROL');
  await ui('authorize');
  assert.equal((await request('browser_run',{action:'snapshot'})).binding,first.binding);
  const callId=randomUUID();
  assert.equal((await request('browser_run',{action:'snapshot'},'a',first.binding,callId)).ok,true);
  assert.equal((await request('browser_run',{action:'snapshot'},'a',first.binding,callId)).code,'DUPLICATE_CALL');
  active=2;await ui('authorize');
  assert.equal((await request('browser_run',{action:'snapshot'},'a',first.binding)).code,'BINDING_CHANGED');
  assert.equal((await request('browser_run',{action:'snapshot'},'b')).ok,true);
  assert.equal((await request('browser_finish',{},'b')).phase,'finished');
  assert.equal((await request('browser_run',{action:'snapshot'},'b')).code,'USER_IN_CONTROL');
  // stop 在异步 attach 尚未返回时发生：不得再下发 Runtime.evaluate。
  await ui('authorize');
  let releaseAttach; let reachedAttach;
  const attaching=new Promise(resolve=>{reachedAttach=resolve;});
  chrome.debugger.attach=async()=>{reachedAttach();await new Promise(resolve=>{releaseAttach=resolve;});};
  const countBeforeAttach=count;
  const interrupted=request('browser_run',{action:'snapshot'},'b');
  await attaching;
  await ui('stop');
  releaseAttach();
  assert.equal((await interrupted).code,'USER_IN_CONTROL');
  assert.equal(count,countBeforeAttach);
  chrome.debugger.attach=async()=>{};

  // 切换目标不能把旧请求的失败写进新授权，也不能把 mouseReleased 发给新页面。
  await ui('authorize');
  let releasePress;let reachedPress;
  const pressing=new Promise(resolve=>{reachedPress=resolve;});
  const originalCommand=chrome.debugger.sendCommand;
  chrome.debugger.sendCommand=async(source,method,params)=>{
    if(method==='Input.dispatchMouseEvent' && params.type==='mousePressed'){
      reachedPress();await new Promise(resolve=>{releasePress=resolve;});return {};
    }
    return originalCommand(source,method,params);
  };
  const clicking=request('browser_run',{action:'click',selector:'#button'},'b');
  await pressing;
  active=3;await ui('authorize');
  releasePress();
  assert.equal((await clicking).code,'BINDING_CHANGED');
  assert.equal(storage.selected.phase,'ready');
  chrome.debugger.sendCommand=originalCommand;
  assert.equal((await request('browser_run',{action:'snapshot'},'c')).ok,true);

  // 用户明确授权直接子弹窗后，原 session/binding 可继续；其他 tab 不能借此接管。
  const parentBinding=storage.selected.binding;
  await request('browser_handoff',{},'c');
  active=4;
  assert.equal((await ui('continue')).ok,false);
  chrome.tabs.query=async()=>[{id:active,openerTabId:3,url:'https://example.test/popup'}];
  assert.equal((await ui('continue')).ok,true);
  assert.equal(storage.selected.binding,parentBinding);
  assert.equal((await request('browser_run',{action:'snapshot'},'c',parentBinding)).ok,true);
  await request('browser_handoff',{},'c',parentBinding);
  active=3;
  chrome.tabs.query=async()=>[{id:active,url:'https://example.test/3'}];
  assert.equal((await ui('continue')).ok,true);
  assert.equal((await request('browser_run',{action:'snapshot'},'c',parentBinding)).ok,true);
  // 回执日志落盘期间的停止也必须阻止尚未 dispatch 的请求。
  const originalSet=chrome.storage.session.set;
  let releaseJournal;let reachedJournal;
  const journaling=new Promise(resolve=>{reachedJournal=resolve;});
  chrome.storage.session.set=async value=>{
    if(value.dispatched){reachedJournal();await new Promise(resolve=>{releaseJournal=resolve;});}
    return originalSet(value);
  };
  const beforeJournal=count;
  const journalRequest=request('browser_run',{action:'snapshot'},'c',parentBinding);
  await journaling;
  await ui('stop');
  releaseJournal();
  assert.equal((await journalRequest).code,'USER_IN_CONTROL');
  assert.equal(count,beforeJournal);
  chrome.storage.session.set=originalSet;
  await ui('disconnect');
});
