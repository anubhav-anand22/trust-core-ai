// Thin wrappers around the Tauri command layer. Every backend call the UI makes
// goes through here so component code stays declarative.

import { invoke, Channel } from "@tauri-apps/api/core";
import { log, timed, type Fields } from "./log";
import type {
  AuditPaths,
  LlmAssessment,
  ModelPlan,
  ModelProgress,
  SessionContext,
  SessionMeta,
  StepEvent,
  SystemProbe,
  UploadedFile,
} from "../types";

/** Is Ollama available (server responding, on PATH, or in a known location)? */
export const isOllamaInstalled = () =>
  timed("is_ollama_installed", undefined, () =>
    invoke<boolean>("is_ollama_installed"),
  );

/** Install Ollama if missing, streaming progress lines. */
export function ensureOllamaInstalled(onLine: (line: string) => void) {
  const channel = new Channel<string>();
  channel.onmessage = (line) => {
    log.info("ollama install", { line });
    onLine(line);
  };
  return timed("ensure_ollama_installed", undefined, () =>
    invoke<boolean>("ensure_ollama_installed", { onProgress: channel }),
  );
}

/** Detected RAM/GPU/cores plus the recommended model tier. */
export const probeSystem = () =>
  timed("probe_system", undefined, async () => {
    const probe = await invoke<SystemProbe>("probe_system");
    // Logged in full because a wrong tier recommendation is the difference
    // between a usable app and multi-minute steps.
    log.info("hardware probed", {
      ram_gb: probe.hardware.total_ram_gb,
      cpu_cores: probe.hardware.cpu_cores,
      gpu: probe.hardware.gpu_name,
      vram_gb: probe.hardware.vram_gb,
      cuda: probe.hardware.cuda_available,
      recommended_llm: probe.recommended.llm,
      tier: probe.recommended.tier_label,
    });
    return probe;
  });

/** Pull the given models, streaming per-model download progress. */
export function ensureModels(
  models: string[],
  onProgress: (p: ModelProgress) => void,
) {
  const channel = new Channel<ModelProgress>();
  channel.onmessage = onProgress;
  return timed("ensure_models", { models }, () =>
    invoke<void>("ensure_models", { models, onProgress: channel }),
  );
}

/** Persist the confirmed model plan on the backend. */
export const setModelPlan = (plan: ModelPlan) =>
  timed("set_model_plan", { llm: plan.llm, tier: plan.tier_label }, () =>
    invoke<void>("set_model_plan", { plan }),
  );

/** Warm the resident model (idempotent). */
export const warmModel = () =>
  timed("warm_model", undefined, () => invoke<void>("warm_model"));

/** Risk assessment for running `model` on this host — drives the override warning. */
export const assessModel = (model: string) =>
  timed("assess_model", { model }, () =>
    invoke<LlmAssessment>("assess_model", { model }),
  );

/** Ask the backend to stop the turn in progress. Safe to call when none is. */
export const cancelTurn = () => {
  log.info("user requested turn cancellation");
  return invoke<void>("cancel_turn").catch((e) =>
    log.error("cancel_turn failed", { error: String(e) }),
  );
};

/** Fold the session's context into encrypted long-term memory. */
export const endSession = (sessionId: string) =>
  timed("end_session", { sessionId }, () =>
    invoke<void>("end_session", { sessionId }),
  );

/** Past chat sessions, newest first, for the sidebar. */
export const listSessions = () =>
  timed("list_sessions", undefined, () =>
    invoke<SessionMeta[]>("list_sessions"),
  );

/** Full transcript of one session, to redraw it. */
export const loadSession = (sessionId: string) =>
  timed("load_session", { sessionId }, () =>
    invoke<SessionContext>("load_session", { sessionId }),
  );

/** Delete a session file (the long-term digest is kept). */
export const deleteSession = (sessionId: string) =>
  timed("delete_session", { sessionId }, () =>
    invoke<void>("delete_session", { sessionId }),
  );

/** Rename a session (sidebar title). */
export const renameSession = (sessionId: string, title: string) =>
  timed("rename_session", { sessionId }, () =>
    invoke<void>("rename_session", { sessionId, title }),
  );

/** Absolute on-disk paths for the audit sidebar. */
export const auditPaths = () => invoke<AuditPaths>("audit_paths");

/** Open the log directory in the OS file manager. Returns the path. */
export const openLogDir = () => invoke<string>("open_log_dir");

/**
 * Run one turn. Streams `StepEvent`s to `onEvent`; resolves when the backend
 * command returns (i.e. the turn is finished or errored).
 */
/** "fast" = retrieve then answer; "deep" = read the whole document in passes. */
export type TurnMode = "fast" | "deep";

export function submitTurn(
  args: {
    prompt: string;
    sessionId: string;
    files: UploadedFile[];
    mode?: TurnMode;
  },
  onEvent: (e: StepEvent) => void,
) {
  const channel = new Channel<StepEvent>();
  channel.onmessage = logStepEvent(onEvent);
  return timed(
    "submit_turn",
    {
      sessionId: args.sessionId,
      prompt_chars: args.prompt.length,
      files: args.files.map((f) => f.name),
      mode: args.mode ?? "fast",
    },
    () =>
      invoke<void>("submit_turn", {
        prompt: args.prompt,
        sessionId: args.sessionId,
        files: args.files,
        mode: args.mode ?? "fast",
        onEvent: channel,
      }),
  );
}

/**
 * Wrap a StepEvent handler so every pipeline event is logged as it arrives.
 *
 * These are the timeline of a turn as the UI saw it. Logging them here (rather
 * than in each component) means the record is identical no matter which screen
 * is mounted, and it survives the component unmounting mid-turn.
 */
function logStepEvent(inner: (e: StepEvent) => void) {
  return (e: StepEvent) => {
    switch (e.stage) {
      case "error":
        log.error("pipeline event: error", { message: e.message });
        break;
      case "awaiting_user":
        log.warn("pipeline event: awaiting_user (HITL)", { errors: e.errors });
        break;
      case "warning":
        log.warn("pipeline event: warning", {
          message: e.message,
          detail: e.detail ?? undefined,
        });
        break;
      case "tool_finished":
        log[e.ok ? "info" : "error"]("pipeline event: tool_finished", {
          tool: e.tool,
          ok: e.ok,
          elapsed_ms: e.elapsed_ms,
        });
        break;
      case "quality_check":
        log[e.passed ? "info" : "warn"]("pipeline event: quality_check", {
          passed: e.passed,
        });
        break;
      default:
        log.debug(`pipeline event: ${e.stage}`, e as unknown as Fields);
    }
    inner(e);
  };
}

/**
 * Resume a parked turn with the plan the user reviewed in the HITL modal.
 * `force: true` skips re-validation ("run anyway"); otherwise a still-invalid
 * plan re-emits an `awaiting_user` event. Streams `StepEvent`s like `submitTurn`.
 */
export function resumeTurn(
  args: {
    prompt: string;
    sessionId: string;
    files: UploadedFile[];
    planJson: string;
    force: boolean;
    mode?: TurnMode;
  },
  onEvent: (e: StepEvent) => void,
) {
  const channel = new Channel<StepEvent>();
  channel.onmessage = logStepEvent(onEvent);
  return timed(
    "resume_turn",
    { sessionId: args.sessionId, force: args.force },
    () =>
      invoke<void>("resume_turn", {
        prompt: args.prompt,
        sessionId: args.sessionId,
        files: args.files,
        planJson: args.planJson,
        force: args.force,
        mode: args.mode ?? "fast",
        onEvent: channel,
      }),
  );
}

/** Read a File into the base64 shape `submit_turn` expects. */
export function fileToUpload(file: File): Promise<UploadedFile> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error);
    reader.onload = () => {
      // reader.result is a data URL: "data:<mime>;base64,<payload>"
      const result = String(reader.result);
      const comma = result.indexOf(",");
      resolve({ name: file.name, content_base64: result.slice(comma + 1) });
    };
    reader.readAsDataURL(file);
  });
}
