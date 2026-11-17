import fs from 'node:fs/promises';
import path from 'node:path';

// Called through the official ego-browser Node runtime with an already observed page.
// All game interaction uses browser keyboard/pointer APIs. Evaluation only observes.
export async function checkGame(page, outputRoot) {
  const arm = new URL(await page.url()).pathname.split('/').filter(Boolean).at(-1);
  if (!/^run-[1-4]-c[48]$/.test(arm)) throw new Error('Unexpected benchmark page');
  const checks = [], record = (name, pass, evidence) => checks.push({ name, pass, evidence });
  const snap = () => page.evaluate(() => window.__orbitSnapshot());
  const buttons = () => page.evaluate(() => [...document.querySelectorAll('[data-action]')].map(b => {
    const r = b.getBoundingClientRect();
    return { action: b.dataset.action, ordinal: [...document.querySelectorAll(`[data-action="${b.dataset.action}"]`)].indexOf(b), text: b.textContent.trim(), pressed: b.getAttribute('aria-pressed'), visible: !!(r.width && r.height), left: r.left, top: r.top, right: r.right, bottom: r.bottom };
  }));
  const clickAction = async (action, label) => {
    const target = (await buttons()).find(b => b.action === action && b.visible);
    if (!target) throw new Error(`No visible ${action} control`);
    await page.click(`[data-action="${action}"] >> nth=${target.ordinal}`, { label });
  };
  const mute = async () => (await buttons()).find(b => b.action === 'mute' && b.visible);
  let executionError;
  try {
  await page.waitForFunction(() => typeof window.__orbitSnapshot === 'function');
  record('title', (await snap()).mode === 'title');
  await page.screenshot({ path: path.join(outputRoot, `${arm}-title.png`) });
  const beforeMute = await mute();
  if (beforeMute) {
    await clickAction('mute', '检查静音反馈');
    const muted = await mute();
    record('mute label and aria', muted?.text === 'Unmute' && muted?.pressed === 'true', muted);
    await clickAction('mute', '恢复声音状态');
    const unmuted = await mute();
    record('unmute label and aria', unmuted?.text === 'Mute' && unmuted?.pressed === 'false', unmuted);
  } else record('mute is available on title', false);
  await clickAction('start', '开始游戏验收');
  await page.waitForFunction(() => window.__orbitSnapshot().mode === 'playing');
  const initial = await snap();
  await page.keyboard.down('ArrowRight');
  try { await page.waitForFunction(x => window.__orbitSnapshot().player.x > x + 60, initial.player.x, { timeout: 3000 }); }
  finally { await page.keyboard.up('ArrowRight'); }
  record('keyboard movement', (await snap()).player.x > initial.player.x + 60);
  await page.waitForFunction(() => window.__orbitSnapshot().enemies.length > 0, undefined, { timeout: 5000 });
  const aim = await page.evaluate(() => {
    const e = window.__orbitSnapshot().enemies[0], c = document.querySelector('canvas'), r = c.getBoundingClientRect();
    return { x: r.left + e.x * r.width / c.width, y: r.top + e.y * r.height / c.height };
  });
  await page.mouse.move(aim.x, aim.y, { label: '瞄准来袭敌人' });
  await page.keyboard.down('Space');
  try {
    await page.waitForFunction(() => window.__orbitSnapshot().bullets.length > 0, undefined, { timeout: 3000 });
    record('keyboard firing', true);
    await page.waitForFunction(() => window.__orbitSnapshot().kills > 0, undefined, { timeout: 12000 });
  } finally { await page.keyboard.up('Space'); }
  const combat = await snap();
  record('real kill and score', combat.kills > 0 && combat.score >= 10, { kills: combat.kills, score: combat.score });
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => window.__orbitSnapshot().mode === 'paused');
  const paused = await snap();
  await page.evaluate(() => new Promise(resolve => { let frames = 0; const tick = () => ++frames >= 8 ? resolve(true) : requestAnimationFrame(tick); requestAnimationFrame(tick); }));
  record('pause freezes simulation', (await snap()).elapsed === paused.elapsed);
  record('resume button label', (await buttons()).some(b => b.action === 'pause' && b.visible && b.text === 'Resume'));
  await clickAction('mute', '检查暂停时静音');
  record('paused mute feedback', (await mute())?.pressed === 'true', await mute());
  await page.cdp('Emulation.setDeviceMetricsOverride', { width: 420, height: 840, deviceScaleFactor: 1, mobile: false });
  const visible = (await buttons()).filter(b => b.visible);
  record('narrow controls fit', visible.length > 0 && visible.every(b => b.left >= 0 && b.top >= 0 && b.right <= 420 && b.bottom <= 840), visible);
  await page.screenshot({ path: path.join(outputRoot, `${arm}-narrow-paused.png`) });
  await clickAction('pause', '继续游戏运行');
  await page.waitForFunction(t => window.__orbitSnapshot().elapsed > t, paused.elapsed, { timeout: 3000 });
  record('resume', true);
  await page.keyboard.press('p');
  await page.waitForFunction(() => window.__orbitSnapshot().mode === 'paused');
  record('P pauses', (await snap()).mode === 'paused');
  await page.cdp('Emulation.clearDeviceMetricsOverride');
  await page.keyboard.press('p');
  await page.waitForFunction(() => window.__orbitSnapshot().mode === 'gameover', undefined, { timeout: 45000 });
  record('natural game over', (await snap()).player.hp <= 0);
  await page.screenshot({ path: path.join(outputRoot, `${arm}-gameover.png`) });
  await clickAction('restart', '检查重开重置');
  const restarted = await snap();
  record('restart resets run', restarted.mode === 'playing' && restarted.player.hp === 3 && restarted.score === 0 && restarted.kills === 0 && restarted.elapsed < 2, restarted);
  } catch (error) {
    executionError = String(error);
    record('browser sequence completed', false, executionError);
    await page.screenshot({ path: path.join(outputRoot, `${arm}-failure.png`) }).catch(() => {});
  } finally {
    await page.keyboard.up('ArrowRight').catch(() => {});
    await page.keyboard.up('Space').catch(() => {});
    await page.cdp('Emulation.clearDeviceMetricsOverride').catch(() => {});
  }
  const errors = await page.evaluate(() => window.__qaErrors || []);
  record('no observed page errors', errors.length === 0, errors);
  const result = { arm, completed: !executionError, error: executionError, passed: checks.filter(c => c.pass).length, failed: checks.filter(c => !c.pass).length, checks };
  await fs.writeFile(path.join(outputRoot, `${arm}-browser.json`), JSON.stringify(result, null, 2), { flag: 'wx', mode: 0o600 });
  return result;
}
