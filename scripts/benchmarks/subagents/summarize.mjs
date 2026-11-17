// Summarize saved attempts; missing browser acceptance stays explicit.
import fs from 'node:fs';
import path from 'node:path';
const root = path.resolve(process.argv[2]);
const results = JSON.parse(fs.readFileSync(path.join(root, 'results.json'), 'utf8'));
const sum = (items, key) => items.reduce((n, item) => n + (item[key] ?? 0), 0);
function distribution(values) {
  const sorted = values.filter(Number.isFinite).sort((a, b) => a - b), n = sorted.length;
  return n ? { count: n, min: sorted[0], median: (sorted[Math.floor((n - 1) / 2)] + sorted[Math.ceil((n - 1) / 2)]) / 2, max: sorted[n - 1], total: sorted.reduce((a, b) => a + b, 0) } : { count: 0, min: null, median: null, max: null, total: null };
}
const samples = results.map(m => {
  if (m.error) return { arm: m.meta?.arm, meta: m.meta, error: m.error };
  const usages = Object.values(m.usage), children = m.tasks.filter(t => t.name !== 'parent');
  const browserFile = path.join(root, `${m.meta.arm}-browser.json`);
  const browser = fs.existsSync(browserFile) ? JSON.parse(fs.readFileSync(browserFile, 'utf8')) : null;
  const visualFile = path.join(root, `${m.meta.arm}-visual.json`);
  const visual = fs.existsSync(visualFile) ? JSON.parse(fs.readFileSync(visualFile, 'utf8')) : null;
  return {
    arm: m.meta.arm, cap: m.meta.cap, wallSeconds: m.meta.wallSeconds, exitCode: m.meta.exitCode,
    interrupted: m.meta.interrupted, binarySha256: m.meta.binarySha256, source: m.source,
    contractSha256: m.meta.contractSha256, verifierSha256: m.meta.verifierSha256,
    childCount: m.childCount, childExecutions: m.childExecutions, peak: m.peak, failedTasks: m.failedTasks,
    firstChildSeconds: Math.min(...children.map(t => t.startSeconds)),
    childWindowSeconds: m.childWindowSeconds, childExecutionSeconds: m.childExecutionSeconds,
    meanActiveChildren: m.childWindowSeconds ? m.childExecutionSeconds / m.childWindowSeconds : null,
    admissionMs: distribution(children.map(t => t.admissionMs)),
    usage: { parent: m.usage.parent, children: Object.fromEntries(Object.entries(m.usage).filter(([key]) => key !== 'parent')),
      total: sum(usages, 'total'), input: sum(usages, 'input'), output: sum(usages, 'output'), cachedInput: sum(usages, 'cachedInput'),
      knownUsageRecords: sum(usages, 'requests'), incompleteRecords: sum(usages, 'incomplete'), missingRequests: m.providerRequestsWithoutUsage },
    providerRetries: m.providerRetries, toolErrors: m.failures.length,
    resourceWaitMs: distribution(m.resources.map(r => r.resourceWaitMs)),
    backgroundQueueMs: distribution(m.resources.map(r => r.backgroundQueueMs)),
    inbox: { total: m.inbox.total, delivered: m.inbox.delivered, deliveryLagMs: distribution(m.inbox.deliveryLagMs) },
    processes: m.processes, noToolParentResponses: m.parentResponses.filter(r => r.tools === 0).length,
    quality: { logic: { passed: m.verification.passed, failed: m.verification.failed, exitCode: m.verification.exitCode },
      browser: browser ? { completed: browser.completed, passed: browser.passed, failed: browser.failed, failures: browser.checks.filter(c => !c.pass) } : null,
      visual,
      fixtureUnchanged: m.fixtureUnchanged },
    tasks: children.map(t => ({ name: t.name, startSeconds: t.startSeconds, endSeconds: t.endSeconds, status: t.status, effort: t.effort, toolProfile: t.toolProfile }))
  };
});
const groups = [4, 8].map(cap => {
  const selected = samples.filter(s => s.cap === cap && !s.error);
  return { cap, samples: selected.length, wallSeconds: distribution(selected.map(s => s.wallSeconds)),
    tokens: distribution(selected.map(s => s.usage.total)),
    runtimeSucceeded: selected.filter(s => s.exitCode === 0 && s.failedTasks === 0).length,
    logicPassed: selected.filter(s => s.quality.logic.exitCode === 0).length,
    browserBehaviorPassed: selected.filter(s => s.quality.browser?.completed && s.quality.browser.failed === 0).length,
    visualPassed: selected.filter(s => s.quality.visual?.reviewed === true && s.quality.visual.passed === true).length,
    acceptancePassed: selected.filter(s => s.quality.logic.exitCode === 0 && s.quality.browser?.completed && s.quality.browser.failed === 0 && s.quality.visual?.reviewed === true && s.quality.visual.passed === true).length };
});
console.log(JSON.stringify({ evidenceRoot: root, order: samples.map(s => s.cap), samples, groups,
  limits: ['Two samples per setting; no stable P95 or optimal-cap claim.', 'Generation and browser acceptance measured separately.', 'Automated browser behavior checks do not prove visual correctness; missing manual review stays null.', 'Tokens are usage, not a price estimate.', 'Original attempts and products retained; no winner selection.'] }, null, 2));
