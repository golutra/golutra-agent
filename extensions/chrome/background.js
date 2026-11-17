import { pageOperation } from './page.js';
import { frameAccess } from './frames.js';
const HOST = 'com.golutra.browser';
let port = null;
let selected = null;
let attached = false;
let queue = Promise.resolve();
let operationBinding = null;
let operationRevision = 0;
let connecting = null;
let pendingPort = null;
let connectionEpoch = 0;
let uiRevision = 0;
const expectedDetach = new Set();
let settings = {autoTasks:true, autoConnect:true};
let connectionError = '';
const RECONNECT_ALARM = 'golutra-reconnect';
const ready = Promise.all([chrome.storage.session.get('selected'), chrome.storage.local.get('settings')]).then(([session, local]) => {
  selected = session.selected || null;
  // 安装即启用新建任务页；保留用户明确停止/断开的 false，升级不能覆盖用户决定。
  settings = {autoTasks:local.settings?.autoTasks !== false, autoConnect:local.settings?.autoConnect !== false};
});
const save = () => chrome.storage.session.set({ selected });
const saveSettings = () => chrome.storage.local.set({settings});
const failure = (code, message) => Object.assign(new Error(message), { code });
const frames = frameAccess(command);

function checkUrl(url) {
  if (typeof url !== 'string') throw failure('TAB_NOT_READY', '无法读取当前网页地址，请等待加载完成后重新点击扩展授权');
  const value = new URL(url);
  if (!['http:', 'https:'].includes(value.protocol)) throw failure('UNSUPPORTED_URL', '仅支持 http/https 网页');
  return value;
}

async function detach() {
  if (selected && attached) {
    attached = false;
    const tabId = selected.tabId;
    frames.clear();
    expectedDetach.add(tabId);
    await chrome.debugger.detach({tabId}).catch(() => { expectedDetach.delete(tabId); });
  }
}

async function stop(revokeAutomatic = true) {
  uiRevision++;
  // 用户停止持久撤销自动任务授权；断线只冻结原任务，不能隐式恢复旧副作用。
  if (revokeAutomatic) settings.autoTasks = false;
  if (selected) {
    selected.approved = false;
    selected.phase = 'waiting_for_user';
  }
  // 同步捕获待释放的连接；持久化等待期间的新授权不能被旧 stop 拆掉。
  await Promise.all([save(), detach(), saveSettings()]);
}

async function connect() {
  if (port) return;
  if (connecting) return connecting;
  const epoch = connectionEpoch;
  const candidate = chrome.runtime.connectNative(HOST);
  pendingPort = candidate;
  connecting = new Promise((resolve, reject) => {
    let closed = false;
    const timer = setTimeout(() => { closed = true; candidate.disconnect(); reject(Error('本机连接超时，请重新连接')); }, 15000);
    candidate.onMessage.addListener(message => {
      if (closed) return;
      if (message.type === 'ready') {
        if (epoch !== connectionEpoch) {clearTimeout(timer); closed = true; candidate.disconnect(); reject(Error('连接已取消')); return;}
        if (message.version !== 1) { clearTimeout(timer); closed = true; candidate.disconnect(); reject(Error('本机连接协议不匹配，请重新安装')); return; }
        clearTimeout(timer); port = candidate; connectionError = ''; void chrome.alarms.clear(RECONNECT_ALARM); resolve(); return;
      }
      const revision = uiRevision;
      queue = queue.catch(() => {}).then(async () => {
        if (candidate !== port) return;
        const response = revision !== uiRevision
          ? {id:message?.id,ok:false,code:'USER_IN_CONTROL',message:'授权已改变，未执行排队的操作'}
          : await handle(message, revision);
        try { candidate.postMessage(response); } catch { /* 宿主已断开，禁止重放。 */ }
      });
    });
    candidate.onDisconnect.addListener(() => {
      closed = true;
      clearTimeout(timer);
      if (port === candidate) port = null;
      if (port === null) void stop(false);
      connectionError = chrome.runtime.lastError?.message || '本机连接已断开，请检查本机连接程序';
      if (settings.autoConnect) void chrome.alarms.create(RECONNECT_ALARM, {delayInMinutes:1});
      reject(Error(chrome.runtime.lastError?.message || '本机连接已断开。先运行 golutra-agent browser chrome install'));
    });
    candidate.postMessage({type:'hello',version:1});
  });
  try { await connecting; } finally { connecting = null; if (pendingPort === candidate) pendingPort = null; }
}

async function autoConnect() {
  await ready;
  if (!settings.autoConnect) return;
  try { await connect(); } catch (error) {
    connectionError = error.message;
    if (settings.autoConnect) await chrome.alarms.create(RECONNECT_ALARM, {delayInMinutes:1});
  }
}

chrome.alarms.onAlarm.addListener(alarm => {if (alarm.name === RECONNECT_ALARM) void autoConnect();});
chrome.runtime.onStartup.addListener(() => {void autoConnect();});
// 仅建立本机通信，不授予任何现有页面访问权；失败按分钟退避，禁用后不重连。
void autoConnect();

async function command(method, params = {}, sessionId) {
  assertControl();
  const tabId = selected.tabId;
  const tab = await chrome.tabs.get(tabId);
  assertControl();
  checkUrl(tab.url);
  if (!attached) {
    await chrome.debugger.attach({tabId}, '1.3');
    // attach 返回前用户可能已经停止；不能让刚建立的调试连接绕过停止。
    try { assertControl(); } catch (error) {
      expectedDetach.add(tabId);
      await chrome.debugger.detach({tabId}).catch(() => expectedDetach.delete(tabId));
      throw error;
    }
    attached = true;
  }
  assertControl();
  if (sessionId && !frames.has(sessionId)) throw Error('Frame session changed; observe frames again');
  const result = await chrome.debugger.sendCommand({tabId,...(sessionId ? {sessionId} : {})}, method, params);
  assertControl();
  return result;
}

function assertControl() {
  assertBinding();
  if (!selected?.approved || selected.phase === 'waiting_for_user') throw failure('USER_IN_CONTROL', '用户已停止控制，请重新授权');
}

function assertBinding() {
  if ((selected?.binding || null) !== operationBinding) throw failure('BINDING_CHANGED', '操作期间授权目标发生改变，已停止');
  if (operationRevision !== uiRevision) throw failure('USER_IN_CONTROL', '用户已停止或更改控制，请先检查结果并重新授权');
}

async function evaluate(expression, frameId) {
  const context = frameId ? await frames.context(frameId) : {};
  const result = await command('Runtime.evaluate', {expression, returnByValue:true, awaitPromise:true, timeout:10000, userGesture:true,
    ...(context.contextId ? {contextId:context.contextId} : {})}, context.sessionId);
  if (result.exceptionDetails) throw Error(result.exceptionDetails.text + ': ' + (result.exceptionDetails.exception?.description || ''));
  return result.result?.value;
}

async function page(action, args = {}) {
  return evaluate(`(${pageOperation.toString()})(${JSON.stringify(action)},${JSON.stringify(args)})`, args.frame_id);
}

async function download(url) {
  assertControl();
  const source = await chrome.tabs.get(selected.tabId);
  assertControl();
  if (checkUrl(url).origin !== checkUrl(source.url).origin) throw Error('下载 URL 必须与授权页面同源');
  const id = await chrome.downloads.download({url, conflictAction:'uniquify', saveAs:false});
  for (let count = 0; count < 100; count++) {
    const [item] = await chrome.downloads.search({id});
    if (item?.state === 'complete') return {download_id:id, path:item.filename};
    if (item?.state === 'interrupted') throw Error(`Download interrupted: ${item.error}`);
    if (!selected?.approved || selected.binding !== operationBinding) throw failure('USER_IN_CONTROL', '已停止控制；下载可能继续，请检查下载管理器');
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  throw Error('下载仍在进行，请检查下载管理器；不要重复下载');
}

async function perform(args) {
  if (args.frame_id && !['snapshot','fill','click','evaluate'].includes(args.action)) throw Error('frame_id supports snapshot/fill/click/evaluate; other actions use the top-level tab');
  switch (args.action) {
    case 'frames': return frames.snapshot();
    case 'snapshot': return page('snapshot', args);
    case 'navigate': {
      checkUrl(args.url);
      const result = await command('Page.navigate', {url:args.url});
      if (result.errorText) throw Error(result.errorText);
      return {navigation_dispatched:true, url:args.url};
    }
    case 'fill': return page('fill', args);
    case 'click': {
      if (args.frame_id) return page('click', args);
      const point = await page('point', args);
      await command('Input.dispatchMouseEvent', {type:'mousePressed',button:'left',clickCount:1,...point});
      await command('Input.dispatchMouseEvent', {type:'mouseReleased',button:'left',clickCount:1,...point});
      return {clicked:true};
    }
    case 'press': {
      const codes = {Enter:13,Tab:9,Escape:27,Backspace:8,ArrowLeft:37,ArrowUp:38,ArrowRight:39,ArrowDown:40};
      if (args.key?.length === 1) return command('Input.insertText', {text:args.key});
      if (!codes[args.key]) throw Error('Unsupported key');
      await command('Input.dispatchKeyEvent', {type:'keyDown',key:args.key,windowsVirtualKeyCode:codes[args.key], ...(args.key === 'Enter' ? {text:'\r'} : {})});
      return command('Input.dispatchKeyEvent', {type:'keyUp',key:args.key,windowsVirtualKeyCode:codes[args.key]});
    }
    case 'scroll': return command('Input.dispatchMouseEvent', {type:'mouseWheel',x:100,y:100,deltaX:Number(args.x)||0,deltaY:Number(args.y)||0});
    case 'screenshot': return command('Page.captureScreenshot', {format:'png'});
    case 'evaluate': if (typeof args.expression !== 'string') throw Error('expression is required'); return evaluate(args.expression, args.frame_id);
    case 'upload': {
      if (!Array.isArray(args.files) || !args.files.length || args.files.some(path => typeof path !== 'string' || !/^(\/|[A-Za-z]:[\\/])/.test(path))) throw Error('Absolute file paths required');
      const {root} = await command('DOM.getDocument');
      const {nodeIds} = await command('DOM.querySelectorAll', {nodeId:root.nodeId,selector:args.selector});
      if (nodeIds.length !== 1) throw Error('Expected exactly one file input');
      return command('DOM.setFileInputFiles', {nodeId:nodeIds[0],files:args.files});
    }
    case 'download': return download(args.url);
    default: throw Error('Unsupported browser action');
  }
}

async function dispatch(request) {
  operationRevision = uiRevision;
  operationBinding = selected?.binding || null;
  if (request.tool === 'browser_run' && request.args?.action === 'open') return openTaskTab(request);
  if (request.tool === 'browser_status' && !selected) return {approved:false,automatic_tasks:settings.autoTasks,can_open_task:settings.autoTasks};
  if (!selected) throw failure('NO_AUTHORIZED_TAB', '请点击扩展，授权当前标签页');
  if (request.binding && request.binding !== selected.binding) throw failure('BINDING_CHANGED', '授权目标已改变；拒绝操作其他标签页，请开始新会话');
  if (selected.owner && selected.owner !== request.session) throw failure('TAB_BUSY', '此标签页属于另一个 Agent 会话，请先结束它');
  operationBinding = selected.binding;
  operationRevision = uiRevision;
  if (request.tool === 'browser_status') {
    const tab = await chrome.tabs.get(selected.tabId).catch(() => null);
    const children = await chrome.tabs.query({});
    return {title:tab?.title,url:tab?.url,closed:!tab,approved:!!tab && selected.approved,automatic_tasks:settings.autoTasks,can_open_task:settings.autoTasks && !selected.owner,
      popups:children.filter(child => tab && child.openerTabId === tab.id && /^https?:/.test(child.url || '')).map(child => ({tab_id:child.id,title:child.title,url:child.url})),
      next:'先交回控制，再由用户在关联页面扩展中选择「在此关联页面继续当前任务」，不会自动接管。'};
  }
  if (request.tool === 'browser_finish') {
    if (selected.owner !== request.session) throw failure('NO_ACTIVE_TASK', '当前会话没有绑定');
    await detach();
    if (operationRevision !== uiRevision || selected.binding !== operationBinding) throw failure('BINDING_CHANGED', '结束期间授权已改变');
    selected.owner = null; selected.phase = 'finished'; selected.approved = false; await save(); return {};
  }
  if (request.tool === 'browser_handoff') {
    if (selected.owner !== request.session) throw failure('NO_ACTIVE_TASK', '当前会话没有绑定');
    selected.phase = 'waiting_for_user'; await save(); assertBinding(); await detach(); return {};
  }
  if (request.tool !== 'browser_run') throw Error('Unknown browser tool');
  if (!selected.approved) throw failure('USER_IN_CONTROL', '用户已停止控制；必须在扩展中重新授权');
  if (selected.phase === 'waiting_for_user' && !request.args?.resume_after_user) throw failure('USER_IN_CONTROL', '等待用户确认交回控制权');
  if (selected.phase === 'outcome_unknown' && !['frames','snapshot','screenshot'].includes(request.args?.action)) throw Error('先观察页面确认上次操作结果');
  selected.owner = request.session;
  const observingFramesAfterFailure = selected.phase === 'outcome_unknown' && request.args?.action === 'frames';
  selected.phase = 'ready';
  await save();
  assertControl();
  const result = await perform(request.args || {});
  if (!selected.approved || selected.binding !== operationBinding) throw failure('USER_IN_CONTROL', '操作期间用户停止或切换了控制目标，请检查最终结果');
  if (observingFramesAfterFailure) {selected.phase = 'outcome_unknown'; await save();}
  return result;
}

async function openTaskTab(request) {
  checkUrl(request.args.url);
  if (!settings.autoTasks) throw failure('AUTO_TASKS_DISABLED', '自动任务已被停止；需要用户在扩展中选择「恢复自动运行」后才能新建任务页');
  if (request.binding || selected?.owner) throw failure('TAB_BUSY', '已有任务绑定，请先结束它；继续当前任务请使用 navigate');
  // 新建页面不继承对任意旧标签页的访问权，也不接受模型传入 tab_id。
  const revision = uiRevision;
  const tab = await chrome.tabs.create({url:request.args.url,active:true});
  if (revision !== uiRevision || !settings.autoTasks) throw failure('USER_IN_CONTROL', '创建期间用户停止或改变授权；页面可能已打开，未取得控制权');
  selected = {tabId:tab.id,binding:crypto.randomUUID(),owner:request.session,approved:true,phase:'ready',taskCreated:true};
  operationBinding = selected.binding;
  await save();
  assertControl();
  return {created:true,url:request.args.url};
}

async function handle(request, revision) {
  await ready;
  const id = request?.id;
  operationBinding = null;
  if (typeof id !== 'string' || typeof request?.session !== 'string') return {id,ok:false,code:'INVALID_REQUEST'};
  try {
    // 回执前先记录 ID；Native Host 断连或调用者取消都不能使同一次操作重复执行。
    const { dispatched = [] } = await chrome.storage.session.get('dispatched');
    if (dispatched.includes(id)) throw failure('DUPLICATE_CALL', 'Invocation already dispatched');
    await chrome.storage.session.set({dispatched:[...dispatched.slice(-255),id]});
    if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '等待期间授权已改变，未执行操作');
    const result = await dispatch(request);
    assertBinding();
    const response = {id,ok:true,binding:selected?.binding || null,tab_id:selected?.tabId || null,phase:selected?.phase || 'idle'};
    if (request.args?.action === 'screenshot') response.image = result.data;
    else response.result = result;
    if (JSON.stringify(response).length > 7 * 1024 * 1024) throw Error('Result too large');
    return response;
  } catch (error) {
    if (selected && selected.binding === operationBinding && uiRevision === operationRevision && selected.owner === request.session && selected.phase === 'ready' && request.tool !== 'browser_status') {selected.phase = 'outcome_unknown'; await save();}
    return {id,ok:false,code:error.code || 'CHROME_ACTION_FAILED',message:String(error.message).slice(0,4000)};
  }
}

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if (sender.id !== chrome.runtime.id || sender.url !== chrome.runtime.getURL('popup.html')) return false;
  (async () => {
    await ready;
    const revision = message.action === 'status' ? uiRevision : ++uiRevision;
    if (message.action === 'enable-auto' || message.action === 'connect') {
      settings.autoConnect = true;
      // 开关只允许新建任务页；被停止的旧目标仍必须逐页重新授权。
      if (message.action === 'enable-auto') {
        settings.autoTasks = true;
        if (selected && !selected.approved && selected.phase === 'waiting_for_user') {
          await detach();
          if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '授权设置已改变');
          selected = null; await save();
        }
      }
      await saveSettings();
      if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '连接设置已被用户更改');
      await connect();
    } else if (message.action === 'disable-auto') {
      await stop();
    }
    if (message.action === 'authorize' || message.action === 'continue') {
      const [tab] = await chrome.tabs.query({active:true,currentWindow:true});
      if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '连接授权已被用户取消');
      checkUrl(tab.url);
      const previous = selected;
      const allowedTabs = previous?.allowedTabs || (previous ? [previous.tabId] : []);
      if (message.action === 'continue' && (!previous?.owner || (!allowedTabs.includes(tab.id) && tab.openerTabId !== previous.tabId) || previous.phase !== 'waiting_for_user')) {
        throw failure('NOT_TASK_POPUP', '先让 Agent 交回控制，再选择当前页的直接子弹窗或本任务已授权的页面');
      }
      settings.autoConnect = true; await saveSettings();
      if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '连接授权已被用户取消');
      await connect();
      if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '连接授权已被用户取消');
      if (selected?.tabId !== tab.id) {
        await detach();
        if (revision !== uiRevision) throw failure('USER_IN_CONTROL', '连接授权已被用户取消');
        selected = message.action === 'continue'
          ? {...previous, tabId:tab.id, allowedTabs:[...new Set([...allowedTabs,tab.id])].slice(-32)}
          : {tabId:tab.id,binding:crypto.randomUUID(),owner:null};
      }
      selected.approved = true; selected.phase = 'ready'; await save();
    } else if (message.action === 'stop') await stop();
    else if (message.action === 'disconnect') {
      connectionEpoch++;
      settings.autoConnect = false;
      await stop(); await chrome.alarms.clear(RECONNECT_ALARM);
      const connection = port || pendingPort;
      connection?.disconnect(); port = null;
    }
    const tab = selected ? await chrome.tabs.get(selected.tabId).catch(() => null) : null;
    return {ok:true,connected:!!port,title:tab?.title,phase:selected?.phase,automatic_tasks:settings.autoTasks,auto_connect:settings.autoConnect,connection_error:connectionError};
  })().then(respond, error => respond({ok:false,error:error.message}));
  return true;
});
chrome.debugger.onDetach.addListener(source => {
  if (expectedDetach.delete(source.tabId)) return;
  if (attached && source.tabId === selected?.tabId) {attached = false; void stop();}
});
chrome.tabs.onRemoved.addListener(tabId => {
  if (tabId !== selected?.tabId) return;
  if (selected.owner) {void stop(); return;}
  // 关闭已经结束的任务页只是清理页面，不能撤销用户对后续新任务的许可。
  uiRevision++; attached = false; frames.clear(); selected = null; void save();
});
chrome.debugger.onEvent.addListener((source, method, params) => {
  if (source.tabId === selected?.tabId) frames.event(source, method, params);
});
