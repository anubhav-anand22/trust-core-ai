// Thin wrappers around the Tauri command layer. Every backend call the UI makes
// goes through here so component code stays declarative.

import { invoke, Channel } from "@tauri-apps/api/core";
import type {
  AuditPaths,
  ModelPlan,
  ModelProgress,
  StepEvent,
  SystemProbe,
  UploadedFile,
} from "../types";

/** Is the `ollama` binary on PATH? */
export const isOllamaInstalled = () => invoke<boolean>("is_ollama_installed");

/** Install Ollama if missing, streaming progress lines. */
export function ensureOllamaInstalled(onLine: (line: string) => void) {
  const channel = new Channel<string>();
  channel.onmessage = onLine;
  return invoke<boolean>("ensure_ollama_installed", { onProgress: channel });
}

/** Detected RAM/GPU plus the recommended model tier. */
export const probeSystem = () => invoke<SystemProbe>("probe_system");

/** Pull the given models, streaming per-model download progress. */
export function ensureModels(
  models: string[],
  onProgress: (p: ModelProgress) => void,
) {
  const channel = new Channel<ModelProgress>();
  channel.onmessage = onProgress;
  return invoke<void>("ensure_models", { models, onProgress: channel });
}

/** Persist the confirmed model plan on the backend. */
export const setModelPlan = (plan: ModelPlan) =>
  invoke<void>("set_model_plan", { plan });

/** Warm the resident model (idempotent). */
export const warmModel = () => invoke<void>("warm_model");

/** Fold the session's context into encrypted long-term memory. */
export const endSession = (sessionId: string) =>
  invoke<void>("end_session", { sessionId });

/** Absolute on-disk paths for the audit sidebar. */
export const auditPaths = () => invoke<AuditPaths>("audit_paths");

/**
 * Run one turn. Streams `StepEvent`s to `onEvent`; resolves when the backend
 * command returns (i.e. the turn is finished or errored).
 */
export function submitTurn(
  args: { prompt: string; sessionId: string; files: UploadedFile[] },
  onEvent: (e: StepEvent) => void,
) {
  const channel = new Channel<StepEvent>();
  channel.onmessage = onEvent;
  return invoke<void>("submit_turn", {
    prompt: args.prompt,
    sessionId: args.sessionId,
    files: args.files,
    onEvent: channel,
  });
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
