import { CHANGE_WAIT_SECS } from "./generated/constants.ts";
// The commander HTTP API, typed over the shapes generated from
// claude-commander-protocol (src/generated). This is the one place that knows
// about the bearer header and what a 401 means; nothing else calls fetch.
//
// Never hand-write a wire shape here: if a response has no generated type,
// the fix is a `ts` derive in protocol, not an interface in this file.

import type {
  AddProjectRequest,
  AgentStatesSnapshot,
  ApiErrorBody,
  ApplyOutcome,
  ConfigPatch,
  ConfigView,
  CreatedId,
  CreateOptions,
  CreateSessionOpts,
  EditSession,
  EditSessionOutcome,
  NewComment,
  PlanSessionEdit,
  ProjectId,
  ReviewedToggle,
  ReviewSnapshot,
  ScanResponse,
  SessionEditorInput,
  SessionEditorModel,
  SessionEditorPlan,
  SessionId,
  Snapshot,
  ToggleReviewed,
} from "./generated/index.ts";

/** Where the current token came from — decides what a rejection says. */
export type TokenSource = "none" | "stored" | "hash" | "submitted";

/**
 * The browser's copy of the server's bearer token.
 *
 * Only a token the user just typed into the connect screen is reported as
 * "rejected". A stored or linked token that has gone stale (a rotated server
 * token, a reload) simply lands the user on the connect screen, which has
 * nothing to say until they submit something.
 *
 * After a rejection, `rejected` holds until a new token is set: the page stops
 * polling (every poll would only 401 again) and the connect screen keeps its
 * message instead of a late 401 re-opening it blank.
 */
export class Auth {
  token: string | null = null;
  source: TokenSource = "none";
  rejected = false;

  set(token: string | null, source: TokenSource): void {
    this.token = token || null;
    this.source = this.token ? source : "none";
    this.rejected = false;
  }

  /**
   * Forget the token after a 401 for a request sent with `sent` (default: the
   * current token). Returns the message the connect screen shows — or `null`
   * when `sent` is no longer the current token: that 401 is about a token the
   * user has already replaced (a poll in flight across a submit), and acting
   * on it would throw away the new one.
   */
  reject(sent: string | null = this.token): Rejection | null {
    if (sent !== this.token) return null;
    const rejection: Rejection = {
      message: this.source === "submitted" ? "That token was rejected." : undefined,
      token: this.token,
      source: this.source,
    };
    this.token = null;
    this.source = "none";
    this.rejected = true;
    return rejection;
  }

  headers(): Record<string, string> {
    return this.token ? { Authorization: `Bearer ${this.token}` } : {};
  }
}

/** A token the server refused, and what to tell the user about it. */
export interface Rejection {
  /** The connect screen's message, if the rejection deserves one. */
  message: string | undefined;
  token: string | null;
  source: TokenSource;
}

/** The server answered 401: the token is missing or wrong. */
export class Unauthorized extends Error {
  constructor() {
    super("unauthorized");
    this.name = "Unauthorized";
  }
}

/**
 * Any other non-2xx answer, with a user-facing message. `status` 0 means no
 * answer at all: the request timed out.
 */
export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** The user-facing message from a non-2xx body (`ApiErrorBody`), else `HTTP <status>`. */
export function errorMessage(status: number, body: unknown): string {
  const detail = (body as Partial<ApiErrorBody> | null)?.error;
  if (detail && typeof detail.message === "string" && detail.message) return detail.message;
  return `HTTP ${status}`;
}

type Method = "GET" | "POST" | "PATCH" | "PUT" | "DELETE";

/**
 * How long a read may take before it counts as no answer. Without a bound, one
 * fetch stuck on a half-open connection (a laptop waking from sleep) never
 * settles, the poll awaiting it never finishes, and the poller -- which only
 * schedules the next run after the current one -- stops for good.
 */
export const READ_TIMEOUT_MS = 10_000;
/**
 * Mutations get far longer: creating a session can fetch the remote and add a
 * worktree first. The bound is only there so none can hang forever. So do the
 * reads that do real work server-side (`review`: a diff of the whole worktree,
 * slow on a big change or a cold cache), via `request`'s `timeoutMs`.
 */
export const WRITE_TIMEOUT_MS = 120_000;
export const HEAVY_READ_TIMEOUT_MS = WRITE_TIMEOUT_MS;

export interface ApiOptions {
  /** Called on the first 401 for the current token, after it has been forgotten. */
  onUnauthorized: (rejection: Rejection) => void;
  fetch?: typeof fetch;
  /** Overrides READ_TIMEOUT_MS (tests). */
  timeoutMs?: number;
  /** Overrides WRITE_TIMEOUT_MS and HEAVY_READ_TIMEOUT_MS (tests). */
  writeTimeoutMs?: number;
}

export class Api {
  readonly auth: Auth;
  private readonly onUnauthorized: (rejection: Rejection) => void;
  private readonly fetchImpl: typeof fetch;
  private readonly readTimeoutMs: number;
  private readonly writeTimeoutMs: number;

  constructor(auth: Auth, opts: ApiOptions) {
    this.auth = auth;
    this.onUnauthorized = opts.onUnauthorized;
    this.fetchImpl = opts.fetch ?? ((input, init) => fetch(input, init));
    this.readTimeoutMs = opts.timeoutMs ?? READ_TIMEOUT_MS;
    this.writeTimeoutMs = opts.writeTimeoutMs ?? WRITE_TIMEOUT_MS;
  }

  /**
   * The server refused the token — over HTTP (a 401) or on the attach socket
   * (`WS_ERR_AUTH`): forget it and hand over to the connect screen. `sent` is
   * the token that request carried; a refusal of a since-replaced one is
   * ignored (see `Auth.reject`).
   */
  unauthorized(sent: string | null = this.auth.token): void {
    // Report a rejection once: later 401s from requests already in flight
    // change nothing the user needs to see.
    const first = !this.auth.rejected;
    const outcome = this.auth.reject(sent);
    if (outcome && first) this.onUnauthorized(outcome);
  }

  /**
   * One request. Resolves to the parsed JSON body (`undefined` for an empty
   * one, e.g. a 204); rejects with `Unauthorized` or `ApiError` -- including
   * `ApiError(0, ...)` when no answer (headers and body) arrives in time.
   */
  async request<T>(
    method: Method,
    path: string,
    body?: unknown,
    timeout: "read" | "long" | "changes" = method === "GET" ? "read" : "long",
  ): Promise<T> {
    const sent = this.auth.token;
    const headers: Record<string, string> = { Accept: "application/json", ...this.auth.headers() };
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const ms =
      timeout === "read"
        ? this.readTimeoutMs
        : timeout === "changes"
          ? (CHANGE_WAIT_SECS + 10) * 1000
          : this.writeTimeoutMs;
    // A write that timed out may well still be running server-side (a create
    // fetching the remote): say so, or the natural retry makes a duplicate.
    const expiredMessage =
      method === "GET"
        ? `request timed out after ${seconds(ms)}`
        : `no response after ${seconds(ms)} — the server may still be working; refresh before retrying`;
    const { res, text } = await withTimeout(ms, expiredMessage, async (signal) => {
      init.signal = signal;
      const res = await this.fetchImpl(`/api${path}`, init);
      // A 401's body is irrelevant; don't wait on it.
      return { res, text: res.status === 401 ? "" : await res.text() };
    });
    if (res.status === 401) {
      this.unauthorized(sent);
      throw new Unauthorized();
    }
    let parsed: unknown;
    try {
      parsed = text ? JSON.parse(text) : undefined;
    } catch {
      parsed = undefined;
    }
    if (!res.ok) throw new ApiError(res.status, errorMessage(res.status, parsed));
    return parsed as T;
  }

  // ---- workspace -----------------------------------------------------------

  changes(since?: number): Promise<number> {
    return this.request(
      "GET",
      since === undefined ? "/changes" : `/changes?since=${since}`,
      undefined,
      "changes",
    );
  }

  workspace = () => this.request<Snapshot>("GET", "/workspace");
  agentStates = () => this.request<AgentStatesSnapshot>("GET", "/agent-states");
  createOptions = () => this.request<CreateOptions>("GET", "/create-options");

  // ---- sessions --------------------------------------------------------------

  createSession = (opts: CreateSessionOpts) =>
    this.request<CreatedId<SessionId>>("POST", "/sessions", opts);
  sessionEditorModel = (input: SessionEditorInput) =>
    this.request<SessionEditorModel>("POST", "/session-editor/model", input);
  planSessionEdit = (input: PlanSessionEdit) =>
    this.request<SessionEditorPlan>("POST", "/session-editor/plan", input);
  editSession = (id: SessionId, edit: EditSession) =>
    this.request<EditSessionOutcome>("PATCH", `/sessions/${id}`, { op: "edit", ...edit });
  restartSession = (id: SessionId) => this.request<unknown>("POST", `/sessions/${id}/restart`);
  killSession = (id: SessionId) => this.request<unknown>("POST", `/sessions/${id}/kill`);
  deleteSession = (id: SessionId) => this.request<unknown>("DELETE", `/sessions/${id}`);

  // ---- projects --------------------------------------------------------------

  addProject = (req: AddProjectRequest) =>
    this.request<CreatedId<ProjectId>>("POST", "/projects", req);
  scanProjects = (req: AddProjectRequest) =>
    this.request<ScanResponse>("POST", "/projects/scan", req);
  removeProject = (id: ProjectId) => this.request<unknown>("DELETE", `/projects/${id}`);

  // ---- config ----------------------------------------------------------------

  /**
   * `GET /config` serves the whole (redacted) host config; `ConfigView` is the
   * part of it the page reads, pinned against the real response server-side.
   */
  config = () => this.request<ConfigView>("GET", "/config");
  patchConfig = (patch: ConfigPatch) => this.request<unknown>("PATCH", "/config", patch);

  // ---- review ----------------------------------------------------------------

  review = (id: SessionId) =>
    this.request<ReviewSnapshot>("GET", `/sessions/${id}/review`, undefined, "long");
  addComment = (id: SessionId, c: NewComment) =>
    this.request<CreatedId<string>>("POST", `/sessions/${id}/comments`, c);
  deleteComment = (id: SessionId, cid: string) =>
    this.request<unknown>("DELETE", `/sessions/${id}/comments/${cid}`);
  toggleReviewed = (id: SessionId, req: ToggleReviewed) =>
    this.request<ReviewedToggle>("POST", `/sessions/${id}/files/reviewed`, req);
  applyComments = (id: SessionId) =>
    this.request<ApplyOutcome>("POST", `/sessions/${id}/comments/apply`);
}

function seconds(ms: number): string {
  return `${Math.round(ms / 1000)}s`;
}

/**
 * Run `work` with an abort signal, failing with `ApiError(0, ...)` after `ms`.
 * The race (rather than trusting the signal alone) bounds a fetch or body that
 * ignores its signal too; the signal still tears the real request down.
 */
async function withTimeout<T>(
  ms: number,
  expiredMessage: string,
  work: (signal: AbortSignal) => Promise<T>,
): Promise<T> {
  const ctl = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      ctl.abort();
      reject(new ApiError(0, expiredMessage));
    }, ms);
  });
  try {
    return await Promise.race([work(ctl.signal), expired]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Run a user-initiated mutation: `{ value }` on success, `null` on failure. A
 * failure is reported through `report` (an alert by default) — except a 401,
 * which the connect screen already handles.
 */
export async function act<T>(
  p: Promise<T>,
  report: (message: string) => void = (m) => alert(m),
): Promise<{ value: T } | null> {
  try {
    return { value: await p };
  } catch (e) {
    if (!(e instanceof Unauthorized)) {
      report(`Action failed: ${e instanceof Error ? e.message : String(e)}`);
    }
    return null;
  }
}
