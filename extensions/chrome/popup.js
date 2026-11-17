let sequence = 0;
async function show(action) {
  const current = ++sequence;
  const output = document.querySelector('#status');
  try {
    const result = await chrome.runtime.sendMessage({ action });
    if (current !== sequence) return;
    if (!result.ok) throw Error(result.error);
    output.textContent = `${result.connected ? '已连接' : '未连接'}\n${result.automatic_tasks && result.auto_connect ? '自动运行已开启' : '自动运行已停止'}\n${result.title || '等待 Agent 创建任务页'}\n${result.phase || ''}${!result.connected && result.connection_error ? '\n'+result.connection_error : ''}`;
    document.getElementById('enable-auto').hidden = !!result.automatic_tasks && !!result.auto_connect;
  } catch (error) { if (current === sequence) output.textContent = error.message; }
}
for (const action of ['enable-auto', 'authorize', 'continue', 'stop', 'disconnect']) {
  document.getElementById(action).addEventListener('click', () => show(action));
}
show('status');
