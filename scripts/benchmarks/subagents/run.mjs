// Opt-in live provider benchmark. Stores every attempt; never retries/selects a winner.
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { measure } from './metrics.mjs';

const [sourceBinary, outputRoot, profile, model] = process.argv.slice(2);
if (!sourceBinary || !outputRoot || !profile || !model) throw new Error('Usage: run.mjs DRIVER NEW_OUTPUT_DIRECTORY PROVIDER_PROFILE MODEL');
const root = path.resolve(outputRoot), fixtures = path.dirname(fileURLToPath(import.meta.url));
await fs.mkdir(root, { mode: 0o700 });
const binary = path.join(root, 'driver');
await fs.copyFile(sourceBinary, binary);
await fs.chmod(binary, 0o700);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const binarySha256 = digest(await fs.readFile(binary));
const contract = await fs.readFile(path.join(fixtures, 'CONTRACT.md'), 'utf8');
const verifier = await fs.readFile(path.join(fixtures, 'verify.mjs'));
await fs.writeFile(path.join(root, 'verify.mjs'), verifier, { flag: 'wx', mode: 0o600 });
const results = [];
for (const [index, cap] of [4, 8, 8, 4].entries()) {
  const arm = `run-${index + 1}-c${cap}`, workspace = path.join(root, arm), runDir = path.join(root, `${arm}-state`);
  await fs.mkdir(path.join(workspace, '.golutra-agent'), { recursive: true, mode: 0o700 });
  await fs.writeFile(path.join(workspace, 'CONTRACT.md'), contract, { flag: 'wx', mode: 0o600 });
  await fs.writeFile(path.join(workspace, '.golutra-agent/runtime.json'), JSON.stringify({ provider_profile: profile, model, reasoning_effort: 'low', subagent_max_concurrent: cap }), { flag: 'wx', mode: 0o600 });
  const prompt = `Implement the complete ORBIT BREAKER game in CONTRACT.md. Create exactly eight named child sessions matching its assignments, independent context, shared workspace, inherited model/effort. Each child must implement its owned files. Pass the contract path and module API; use the structured assignment for owned_paths/new_paths and local checks. Parent owns integration and the UI state contract. Keep source in this workspace; preserve CONTRACT.md, configuration and ../verify.mjs.\nConcurrency cap is ${cap}. Dispatch up to ${cap} children in concise responses, never exceeding the cap; fill freed slots until all eight assignments finish. Work on independent integration while children run. Do not respawn completed children. Inspect results, fix actual defects, run node ../verify.mjs ., and report honest verification limits. Do not launch persistent servers during this timed run. The caller independently measures browser acceptance afterwards, including mute labels/aria, pause/Resume, movement and narrow layouts. Successful logic checks alone do not establish browser acceptance.\n`;
  const promptPath = path.join(root, `${arm}-prompt.md`);
  await fs.writeFile(promptPath, prompt, { flag: 'wx', mode: 0o600 });
  const args = [workspace, runDir, promptPath, 'low', process.execPath, path.join(root, 'verify.mjs')];
  const meta = { arm, cap, profile, model, effort: 'low', binarySha256, contractSha256: digest(contract), verifierSha256: digest(verifier), promptSha256: digest(prompt), workspace, runDir, startedAt: new Date().toISOString() };
  const metaPath = path.join(root, `${arm}-meta.json`);
  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2), { flag: 'wx', mode: 0o600 });
  const stdout = await fs.open(path.join(root, `${arm}-stdout.jsonl`), 'wx', 0o600);
  const stderr = await fs.open(path.join(root, `${arm}-stderr.log`), 'wx', 0o600);
  console.log(JSON.stringify({ event: 'sample.started', arm, cap }));
  const start = performance.now();
  const child = spawn(binary, args, { stdio: ['ignore', stdout.fd, stderr.fd] });
  let interrupted = false;
  const stop = () => { interrupted = true; child.kill('SIGINT'); };
  process.once('SIGINT', stop);
  const deadline = setTimeout(stop, 1_290_000), force = setTimeout(() => child.kill('SIGKILL'), 1_320_000);
  const outcome = await new Promise(resolve => { child.once('error', error => resolve({ error: String(error) })); child.once('close', (exitCode, signal) => resolve({ exitCode, signal })); });
  clearTimeout(deadline); clearTimeout(force); process.off('SIGINT', stop);
  await stdout.close(); await stderr.close();
  Object.assign(meta, outcome, { interrupted, wallSeconds: (performance.now() - start) / 1000, finishedAt: new Date().toISOString() });
  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2), { mode: 0o600 });
  const verified = spawnSync(process.execPath, [path.join(root, 'verify.mjs'), workspace], { encoding: 'utf8', timeout: 60_000 });
  let checks; try { checks = JSON.parse(verified.stdout); } catch { checks = { error: verified.stderr, stdout: verified.stdout }; }
  let metrics, unchanged = false;
  try { metrics = measure(path.join(runDir, 'state/runtime.sqlite'), meta); }
  catch (error) { metrics = { meta, error: String(error) }; }
  try { unchanged = digest(await fs.readFile(path.join(workspace, 'CONTRACT.md'))) === meta.contractSha256 && digest(await fs.readFile(path.join(root, 'verify.mjs'))) === meta.verifierSha256; }
  catch (error) { meta.fixtureError = String(error); }
  const result = { ...metrics, verification: { ...checks, exitCode: verified.status, signal: verified.signal, error: verified.error?.message }, fixtureUnchanged: unchanged };
  await fs.writeFile(path.join(root, `${arm}-metrics.json`), JSON.stringify(result, null, 2), { flag: 'wx', mode: 0o600 });
  results.push(result);
  await fs.writeFile(path.join(root, 'results.json'), JSON.stringify(results, null, 2), { mode: 0o600 });
  console.log(JSON.stringify({ event: 'sample.finished', arm, seconds: meta.wallSeconds, exitCode: meta.exitCode, peak: metrics.peak, children: metrics.childCount, verification: verified.status, fixtureUnchanged: unchanged }));
  if (interrupted) break;
}
process.exitCode = results.length === 4 && results.every(r => r.meta.exitCode === 0 && r.verification.exitCode === 0 && r.fixtureUnchanged && r.childCount === 8 && r.failedTasks === 0 && r.peak <= r.meta.cap) ? 0 : 1;
