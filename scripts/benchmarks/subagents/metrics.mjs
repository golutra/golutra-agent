import fs from 'node:fs';
import { DatabaseSync } from 'node:sqlite';

export function measure(database, meta) {
  const db = new DatabaseSync(database, { readOnly: true });
  try {
    const events = db.prepare("SELECT event_json FROM runtime_events WHERE event_type != 'ProviderStreamed' ORDER BY sequence_no").all().map(r => JSON.parse(r.event_json));
    const registry = db.prepare('SELECT name,child_session_id FROM subagents').all();
    const inbox = db.prepare('SELECT result_id,session_id,created_at,delivered_at FROM background_results').all();
    const created = events.filter(e => e.event_type === 'task_created');
    const parent = created.find(e => !e.payload?.payload?._delegated_task);
    if (!parent) return { error: 'No parent execution recorded', meta };
    const epoch = Date.parse(parent.timestamp);
    const seconds = e => (Date.parse(e.timestamp) - epoch) / 1000;
    const name = e => registry.find(r => r.child_session_id === e.session_id)?.name || 'parent';
    const ended = new Set(['task_completed', 'task_aborted', 'task_interrupted', 'task_uncertain']);
    const tasks = created.map(e => {
      const end = events.find(t => t.task_id === e.task_id && ended.has(t.event_type));
      const start = events.find(t => t.event_type === 'tool_started' && t.payload.tool_call_id === e.payload.payload?._delegation_parent_tool_call_id);
      return { name: name(e), sessionId: e.session_id, taskId: e.task_id, startSeconds: seconds(e),
        endSeconds: end ? seconds(end) : null, status: end?.payload.status ?? 'running',
        admissionMs: start ? Date.parse(e.timestamp) - Date.parse(start.timestamp) : null,
        effort: e.payload.payload?.provider_generation_config?.reasoning_effort,
        toolProfile: e.payload.payload?.tool_profile, assignment: e.payload.payload?._delegation_assignment };
    });
    const children = tasks.filter(t => t.sessionId !== parent.session_id);
    const boundaries = children.flatMap(t => [[t.startSeconds, 1], [t.endSeconds ?? meta.wallSeconds, -1]])
      .sort((a, b) => a[0] - b[0] || a[1] - b[1]);
    let active = 0, peak = 0, union = 0, previous = boundaries[0]?.[0] ?? 0;
    for (const [at, delta] of boundaries) { if (active) union += at - previous; active += delta; peak = Math.max(peak, active); previous = at; }
    const usage = {}, failures = [], resources = [];
    for (const event of events) {
      const group = usage[name(event)] ??= { requests: 0, input: 0, output: 0, cachedInput: 0, total: 0, incomplete: 0, providerSeconds: 0 };
      if (event.event_type === 'token_usage_recorded') {
        const u = event.payload.record;
        group.requests++; group.input += u.input_tokens ?? 0; group.output += u.output_tokens ?? 0;
        group.cachedInput += u.cache_read_tokens ?? 0;
        group.total += u.provider_total_tokens ?? ((u.input_tokens ?? 0) + (u.output_tokens ?? 0));
        if (!u.usage_complete) group.incomplete++;
      }
      if (event.event_type === 'provider_completed') {
        const start = events.find(e => e.event_type === 'provider_started' && e.payload.provider_request_id === event.payload.provider_request_id);
        if (start) group.providerSeconds += seconds(event) - seconds(start);
      }
      if (event.event_type === 'tool_completed') {
        const envelope = event.payload.envelope;
        if (envelope.status !== 'ok') failures.push({ owner: name(event), tool: envelope.tool_name, summary: envelope.summary });
        const f = envelope.structured_facts;
        if (f.resource_wait_ms != null || f.background_queue_wait_ms != null) resources.push({ owner: name(event), tool: envelope.tool_name,
          resourceWaitMs: f.resource_wait_ms ?? null, backgroundQueueMs: f.background_queue_wait_ms ?? null, backgroundExecutionMs: f.background_execution_ms ?? null });
      }
    }
    const processes = [...new Map(events.filter(e => e.event_type === 'process_updated').map(e => [e.payload.process_id, e.payload])).values()];
    const own = events.filter(e => e.session_id === parent.session_id);
    const recordedUsage = events.filter(e => e.event_type === 'token_usage_recorded');
    const retries = [...new Map(events.filter(e => e.event_type === 'retry_scheduled' && e.payload.recovery?.phase === 'retrying')
      .map(e => [`${e.session_id}/${e.payload.provider_request_id}/${e.payload.recovery.attempt}`, e])).values()];
    return { meta, source: parent.payload.run_provenance?.build, tasks, childCount: registry.length, childExecutions: children.length, peak, childWindowSeconds: union,
      childExecutionSeconds: children.reduce((sum, c) => sum + ((c.endSeconds ?? c.startSeconds) - c.startSeconds), 0), usage, failures, resources,
      providerRetries: retries.length,
      providerRequestsWithoutUsage: events.filter(e => e.event_type === 'provider_started' && !recordedUsage.some(u => u.payload.record.request_event_id === e.payload.provider_request_id)).length,
      parentResponses: own.filter(e => e.event_type === 'provider_completed').map(e => ({ seconds: seconds(e), tools: e.payload.tool_call_count })),
      parentMessages: own.filter(e => e.event_type === 'assistant_message').map(e => ({ seconds: seconds(e), content: e.payload.content })),
      toolCatalogs: events.filter(e => e.event_type === 'context_snapshot_created').map(e => ({ owner: name(e), taskId: e.task_id, toolCount: Object.keys(e.payload.snapshot.tool_schema_digests).length })),
      inbox: { total: inbox.length, delivered: inbox.filter(r => r.delivered_at).length,
        deliveryLagMs: inbox.filter(r => r.delivered_at).map(r => Date.parse(r.delivered_at) - Date.parse(r.created_at)) },
      processes: { total: processes.length, terminal: processes.filter(p => p.terminal).length },
      failedTasks: tasks.filter(t => t.status !== 'completed').length };
  } finally { db.close(); }
}

if (process.argv[1]?.endsWith('/metrics.mjs')) {
  const meta = JSON.parse(fs.readFileSync(process.argv[3], 'utf8'));
  console.log(JSON.stringify(measure(process.argv[2], meta), null, 2));
}
