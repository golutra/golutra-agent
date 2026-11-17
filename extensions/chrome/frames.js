// 子 frame 调试会话只来自已授权标签页的自动附加事件，不接受模型指定任意 target。
export function frameAccess(command) {
  const sessions = new Map();
  const autoAttach = {autoAttach:true, waitForDebuggerOnStart:false, flatten:true, filter:[{type:'iframe',exclude:false},{exclude:true}]};
  function event(source, method, params) {
    if (method === 'Target.attachedToTarget' && params.targetInfo.type === 'iframe') {
      sessions.set(params.sessionId, params.targetInfo.targetId);
    } else if (method === 'Target.detachedFromTarget') sessions.delete(params.sessionId);
  }
  async function list() {
    await command('Target.setAutoAttach', autoAttach);
    const frames = new Map();
    function visit(tree, sessionId) {
      frames.set(tree.frame.id, {frame_id:tree.frame.id, parent_id:tree.frame.parentId || null,
        url:tree.frame.url, name:tree.frame.name || '', sessionId});
      for (const child of tree.childFrames || []) visit(child, sessionId);
    }
    visit((await command('Page.getFrameTree')).frameTree);
    // setAutoAttach 会发送已存在的子 target；嵌套 OOPIF 也逐级订阅。
    const visited = new Set();
    while (visited.size < sessions.size) {
      const id = [...sessions.keys()].find(id => !visited.has(id));
      if (!id) break;
      visited.add(id);
      if (visited.size > 64) throw Error('Too many child frame targets');
      await command('Target.setAutoAttach', autoAttach, id);
      visit((await command('Page.getFrameTree', {}, id)).frameTree, id);
    }
    return frames;
  }
  return {
    event, clear:() => sessions.clear(), has:id => sessions.has(id),
    async snapshot() { return [...(await list()).values()].map(({sessionId, ...frame}) => frame); },
    async context(frameId) {
      const frame = (await list()).get(frameId);
      if (!frame) throw Error('Frame missing or changed; run frames again');
      const {executionContextId} = await command('Page.createIsolatedWorld', {frameId,worldName:'golutra-dom'}, frame.sessionId);
      return {contextId:executionContextId, sessionId:frame.sessionId};
    },
  };
}
