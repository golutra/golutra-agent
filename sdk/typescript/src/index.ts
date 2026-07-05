export type SessionCommandKind =
  | "create"
  | "prompt"
  | "approve"
  | "deny"
  | "pause"
  | "resume"
  | "abort"
  | "compact"
  | "verify"
  | "replay"
  | "export";

export interface Actor {
  kind: "user" | "api" | "tui" | "cli" | "sdk" | "web" | "ide" | "runtime";
  id: string;
}

export interface SessionCommand {
  command_id: string;
  session_id?: string;
  kind: SessionCommandKind;
  idempotency_key: string;
  actor: Actor;
  payload: unknown;
  timestamp: string;
}

export interface RuntimeQuery {
  query_id: string;
  session_id: string;
  task_id?: string;
  kind: "session_state" | "task_state" | "user_projection" | "debug_projection" | "replay_cursor";
  requester: Actor["kind"];
  cursor?: number;
  timestamp: string;
}

export interface CommandAck {
  command_id: string;
  accepted: boolean;
  reason?: string;
}

export class GolutraClient {
  constructor(private readonly baseUrl: string) {}

  async sendCommand(command: SessionCommand): Promise<CommandAck> {
    const response = await fetch(new URL("/commands", this.baseUrl), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(command),
    });
    if (!response.ok) {
      throw new Error(`Golutra command failed: ${response.status}`);
    }
    return (await response.json()) as CommandAck;
  }

  async query<T = unknown>(query: RuntimeQuery): Promise<T> {
    const response = await fetch(new URL("/queries", this.baseUrl), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(query),
    });
    if (!response.ok) {
      throw new Error(`Golutra query failed: ${response.status}`);
    }
    return (await response.json()) as T;
  }
}
