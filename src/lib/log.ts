// Front-end logging that lands in the same file as the Rust logs.
//
// Two destinations on every call:
//   1. the browser console (devtools, while developing);
//   2. the Rust `ui_log` command, which writes into the app's rotating log file.
//
// The point of (2) is that a bug report needs one file showing the click, the
// command it invoked, and the pipeline's reaction, in order — not a console the
// user has to know how to open and screenshot.
//
// Never let logging break the app: every call is fire-and-forget and swallows
// its own errors.

import { invoke } from "@tauri-apps/api/core";

type Level = "debug" | "info" | "warn" | "error";

/** Extra structured context. Kept free-form so callers can attach anything. */
export type Fields = Record<string, unknown> | undefined;

function emit(level: Level, message: string, fields?: Fields) {
  const prefix = `[${level}] ${message}`;
  if (level === "error") console.error(prefix, fields ?? "");
  else if (level === "warn") console.warn(prefix, fields ?? "");
  else console.log(prefix, fields ?? "");

  // Fire-and-forget. A logging failure must never surface to the user.
  invoke("ui_log", { level, message, fields: fields ?? null }).catch(() => {});
}

export const log = {
  debug: (message: string, fields?: Fields) => emit("debug", message, fields),
  info: (message: string, fields?: Fields) => emit("info", message, fields),
  warn: (message: string, fields?: Fields) => emit("warn", message, fields),
  error: (message: string, fields?: Fields) => emit("error", message, fields),
};

/**
 * Time an async operation, logging start, duration and outcome.
 *
 * Wraps every backend call so the log shows how long each command took — the
 * information you need when the app "feels slow" but you cannot say where the
 * time went.
 */
export async function timed<T>(
  name: string,
  fields: Fields,
  run: () => Promise<T>,
): Promise<T> {
  const started = performance.now();
  log.info(`${name}: start`, fields);
  try {
    const result = await run();
    log.info(`${name}: ok`, {
      ...fields,
      elapsed_ms: Math.round(performance.now() - started),
    });
    return result;
  } catch (e) {
    log.error(`${name}: failed`, {
      ...fields,
      elapsed_ms: Math.round(performance.now() - started),
      error: String(e),
    });
    throw e;
  }
}

/** Turn an unexpected `window` error or promise rejection into a log line. */
export function installGlobalErrorLogging() {
  window.addEventListener("error", (e) => {
    log.error("uncaught error", {
      message: e.message,
      source: e.filename,
      line: e.lineno,
    });
  });
  window.addEventListener("unhandledrejection", (e) => {
    log.error("unhandled promise rejection", { reason: String(e.reason) });
  });
}
