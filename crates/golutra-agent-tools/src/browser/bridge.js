// 宿主在独立脚本作用域中注入参数；不依赖浏览器宿主继承 shell 的 cwd/env。
const fs = await import('node:fs');
const path = await import('node:path');
const config = __GOLUTRA_BROWSER_CONFIG__;
const state = config.state;
const receipt = { ok: false, images: [], code: null, message: null };
const save = (target, value) => {
  const temporary = target + '.tmp';
  const fd = fs.openSync(temporary, 'w', 0o600);
  try { fs.writeFileSync(fd, JSON.stringify(value)); fs.fsyncSync(fd); }
  finally { fs.closeSync(fd); }
  fs.renameSync(temporary, target);
};
const fail = (code, message) => { const error = new Error(message); error.code = code; throw error; };
// 只恢复本会话由成功 finish 确认保留的 Space；claim 后标签失效，必须重新 adopt。
const resumeFinished = async () => {
  const entry = (await listTaskSpaces()).find(s => s.id === state.space_id);
  if (!entry || entry.name !== state.space_name) fail('EGO_SPACE_MISSING', 'The retained goal space is missing or changed; do not replace it silently');
  if (entry.ownership === 'agentDelegatedToUser' && !config.resume_after_user) {
    fail('EGO_TASK_SPACE_USER_IN_CONTROL', 'The user has control; wait for confirmation');
  }
  let task;
  if (entry.ownership === 'user') {
    if (typeof claimTaskSpace !== 'function') fail('EGO_CAPABILITY_MISMATCH', 'Installed SDK cannot reclaim retained results');
    task = await claimTaskSpace(state.space_id);
  } else if (entry.ownership === 'agentDelegatedToUser') {
    task = await takeOverTaskSpace(state.space_id);
  } else if (entry.ownership === 'agent') {
    task = await taskSpace(state.space_id);
  } else {
    fail('EGO_TASK_SPACE_INACTIVE', 'Unknown ownership; do not claim this space');
  }
  let pages = await task.pages();
  for (const tab of await task.tabs()) {
    if (!tab.label && tab.openedBy === 'agent') pages.push(await task.adopt(tab.page));
  }
  if (!pages.length) fail('EGO_RETAINED_PAGE_MISSING', 'No retained agent page remains; do not adopt unrelated user tabs');
  state.default_page = pages[0].label;
  state.reclaim_pending = false;
  state.reclaimable = false;
  receipt.resumed_finished = true;
  receipt.pages = await Promise.all(pages.map(async p => ({ label: p.label, url: await p.url() })));
  save(config.state_path, state);
  return task;
};
try {
  if (typeof taskSpace !== 'function' || typeof listTaskSpaces !== 'function'
      || typeof globalThis.ego?.getBrowserVersion !== 'function') {
    fail('EGO_CAPABILITY_MISMATCH', 'Native Ego host and matching SDK are required');
  }
  receipt.browser_version = await ego.getBrowserVersion();
  if (config.action === 'status') {
    receipt.spaces = (await listTaskSpaces()).filter(s =>
      (state.space_id != null && s.id === state.space_id) ||
      (state.space_name && s.name === state.space_name));
    receipt.ok = true;
  } else {
    let task;
    if (state.space_id != null) {
      // 已绑定但被删除的 Space 必须报告错误，不能偷偷创建空白替代品。
      task = state.reclaim_pending ? await resumeFinished() : config.resume_after_user
        ? await takeOverTaskSpace(state.space_id)
        : await taskSpace(state.space_id);
    } else {
      const existing = (await listTaskSpaces()).find(s => s.name === state.space_name);
      if (existing) task = await taskSpace(existing.id);
      else if (config.may_create) task = await taskSpace(state.space_name);
      else fail('EGO_SPACE_MISSING', 'The bound task space is missing; inspect before starting a new goal');
    }
    state.space_id = task.spaceId;
    // 在第一项网页副作用之前保存 ID；进程中断后也可回到同一个 Space。
    state.phase = 'running';
    save(config.state_path, state);
    if (config.action === 'handoff') {
      await task.handOff();
      state.phase = 'waiting_for_user';
    } else if (config.action === 'finish') {
      const result = await task.finish({ keep: config.keep });
      // 原生回执决定页面是否保留；不能把 SDK 返回空值或协议不匹配解释成清理成功。
      if (!result || result.spaceId !== state.space_id || typeof result.closedSpace !== 'boolean'
          || !Array.isArray(result.keptManagedLabels) || !Array.isArray(result.closedManagedLabels)) {
        fail('EGO_FINISH_RECEIPT_INVALID', 'Task finish returned no valid receipt; inspect the browser before retrying');
      }
      receipt.completion = result;
      receipt.retention_policy = config.keep;
      receipt.page_retained = result.keptManagedLabels.length > 0 || result.preservedUnmanagedCount > 0;
      receipt.task_ended = true;
      state.reclaimable = !result.closedSpace && result.keptManagedLabels.length > 0;
      state.reclaim_pending = false;
      state.default_page = null;
      state.phase = 'finished';
    } else {
      const browser = Object.freeze({
        artifactPath(name) {
          if (typeof name !== 'string' || !name || path.basename(name) !== name || name === '.' || name === '..') {
            fail('INVALID_ARTIFACT_NAME', 'Use a filename without directory components');
          }
          return path.join(config.artifact_directory, name);
        },
        async screenshot(page) {
          if (receipt.images.length >= 2) fail('IMAGE_LIMIT', 'At most two screenshots per invocation');
          const destination = path.join(config.artifact_directory, `screenshot-${receipt.images.length + 1}.png`);
          await page.screenshot({ path: destination });
          receipt.images.push(destination);
          return destination;
        },
      });
      if (config.recovery === 'reload') {
        // 不知道用户脚本在哪个 Page 执行过 evaluate；必须重载所有受管文档才解除迟到副作用保护。
        const pages = await task.pages();
        if (!pages.length) fail('EGO_RECOVERY_NO_PAGES', 'No managed pages remain to reload');
        for (const p of pages) await p.reload();
        state.requires_reload = false;
        save(config.state_path, state);
      }
      const AsyncFunction = Object.getPrototypeOf(async function() {}).constructor;
      const execute = new AsyncFunction('task', 'page', 'browser', 'console', config.code);
      const pageLabel = config.page || state.default_page || 'p1';
      const result = await execute(task, task.page(pageLabel), browser, console);
      if (result !== undefined) console.log(result);
      receipt.page = pageLabel;
      state.phase = state.requires_reload ? 'outcome_unknown' : 'ready';
      receipt.requires_reload = state.requires_reload;
    }
    receipt.ok = true;
    save(config.state_path, state);
  }
} catch (error) {
  receipt.code = String(error?.code || 'EGO_EXECUTION_FAILED');
  receipt.message = String(error?.message || error).slice(0, 8192);
  receipt.executionStopped = error?.executionStopped;
  receipt.mayHaveLateEffects = error?.mayHaveLateEffects;
  receipt.stopped = error?.stopped;
  if (config.action !== 'status') {
    if (error?.mayHaveLateEffects === true || error?.executionStopped === false
        || (receipt.code === 'EGO_PAGE_EVALUATION_TIMED_OUT'
          && error?.executionStopped !== true && error?.stopped !== true)) {
      state.requires_reload = true;
    }
    // 已结束与暂时交给用户是不同状态；不能让 takeOver 恢复一个已结束的任务。
    state.phase = receipt.code === 'EGO_TASK_SPACE_USER_IN_CONTROL' ? 'waiting_for_user'
      : receipt.code === 'EGO_TASK_SPACE_INACTIVE' ? 'ended' : 'outcome_unknown';
    save(config.state_path, state);
  }
} finally {
  save(config.receipt_path, receipt);
}
