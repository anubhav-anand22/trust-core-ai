// Sovereign AI Workbench — app shell.
// Bootstrap gate -> chat (session rail + transcript + prompt + audit sidebar).

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import { AuditSidebar } from "./components/AuditSidebar";
import { BootstrapGate } from "./components/BootstrapGate";
import { HitlModal } from "./components/HitlModal";
import { PromptPanel } from "./components/PromptPanel";
import { ReportView } from "./components/ReportView";
import { SessionSidebar } from "./components/SessionSidebar";
import { Stepper } from "./components/Stepper";
import {
  deleteSession,
  endSession,
  fileToUpload,
  listSessions,
  loadSession,
  renameSession,
  resumeTurn,
  submitTurn,
  type TurnMode,
} from "./lib/pipeline";
import { installGlobalErrorLogging, log } from "./lib/log";
import type {
  Exchange,
  FinalReport,
  ModelPlan,
  SessionMeta,
  StepEvent,
  UploadedFile,
} from "./types";

const SESSION_KEY = "wb.sessionId";

function newSessionId(): string {
  return `s-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

/** Session id that survives a reload, so the chat is not orphaned. */
function initialSessionId(): string {
  try {
    const saved = localStorage.getItem(SESSION_KEY);
    if (saved) return saved;
  } catch {
    /* private window / storage disabled */
  }
  const fresh = newSessionId();
  try {
    localStorage.setItem(SESSION_KEY, fresh);
  } catch {
    /* ignore */
  }
  return fresh;
}

function App() {
  const [plan, setPlan] = useState<ModelPlan | null>(null);

  // --- session / transcript -------------------------------------------
  const [sessionId, setSessionId] = useState<string>(initialSessionId);
  const [sessions, setSessions] = useState<SessionMeta[]>([]);
  const [transcript, setTranscript] = useState<Exchange[]>([]);

  // --- live turn -----------------------------------------------------
  const [events, setEvents] = useState<StepEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<FinalReport | null>(null);
  const [livePrompt, setLivePrompt] = useState<string>("");
  // Set once a turn's result has moved into the transcript. `events` deliberately
  // survives that move — the audit sidebar's timings table is derived from it — so
  // this flag, not an empty `events`, is what retires the live transcript block.
  const [turnCommitted, setTurnCommitted] = useState(false);
  const [hitl, setHitl] = useState<{ errors: string[]; planJson: string } | null>(null);
  const [fatal, setFatal] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<{ message: string; detail?: string | null }[]>([]);

  const lastTurn = useRef<{
    prompt: string;
    uploads: UploadedFile[];
    mode: TurnMode;
  } | null>(null);

  // The report as it arrived on the channel. `commitTurn` reads it from here so the
  // transcript append never has to live inside a state updater: React treats updaters
  // as pure and may call them more than once, and StrictMode deliberately does — which
  // is how one turn used to land in the transcript twice.
  const liveReport = useRef<FinalReport | null>(null);

  // Authoritative in-flight flag. The `busy` state is set asynchronously, so two fast
  // activations (double-click Run, Ctrl+Enter twice) both read the stale `busy === false`
  // and fire two real `submit_turn` invokes — two pipeline runs, two exchanges on disk.
  const inFlight = useRef(false);

  const persistId = useCallback((id: string) => {
    try {
      localStorage.setItem(SESSION_KEY, id);
    } catch {
      /* ignore */
    }
  }, []);

  const refreshSessions = useCallback(() => {
    listSessions()
      .then(setSessions)
      .catch((e) => log.error("list_sessions failed", { error: String(e) }));
  }, []);

  // First mount: error logging, session list, and the current session's history.
  useEffect(() => {
    installGlobalErrorLogging();
    log.info("app started", { sessionId });
    refreshSessions();
    loadSession(sessionId)
      .then((s) => setTranscript(s.exchanges ?? []))
      .catch(() => setTranscript([])); // brand-new session: nothing on disk yet
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Persist long-term memory when the window is closing/reloading.
  useEffect(() => {
    const onUnload = () => {
      log.info("window closing; persisting long-term memory");
      endSession(sessionId).catch(() => {});
    };
    window.addEventListener("beforeunload", onUnload);
    return () => window.removeEventListener("beforeunload", onUnload);
  }, [sessionId]);

  const activity = useMemo(
    () =>
      events
        .filter((e) => e.stage === "executing_tool" || e.stage === "tool_finished")
        .slice(-8),
    [events],
  );

  function clearLive() {
    setEvents([]);
    setReport(null);
    setHitl(null);
    setFatal(null);
    setWarnings([]);
    setLivePrompt("");
    setTurnCommitted(false);
    liveReport.current = null;
  }

  // Fold a StepEvent stream into UI state. Shared by the initial turn and a resume.
  function onEvent(e: StepEvent) {
    setEvents((prev) => [...prev, e]);
    if (e.stage === "done") {
      try {
        const parsed = JSON.parse(e.report_json) as FinalReport;
        // The ref is what `commitTurn` reads; the state is what the live pane renders
        // while the turn is still on screen.
        liveReport.current = parsed;
        setReport(parsed);
      } catch {
        /* leave report null */
      }
    } else if (e.stage === "awaiting_user") {
      setHitl({ errors: e.errors, planJson: e.plan_json });
    } else if (e.stage === "error") {
      setFatal(e.message);
    } else if (e.stage === "warning") {
      setWarnings((prev) => [...prev, { message: e.message, detail: e.detail }]);
    }
  }

  // When a turn ends, MOVE its result into the transcript and refresh the rail.
  //
  // "Move", not "copy": the live pane renders the same report at the bottom of the
  // transcript, and it used to stay mounted after the turn finished (`showLive` is still
  // true because `events` is non-empty), so the finished report was drawn twice.
  //
  // Two things this must NOT do:
  //  - call `clearLive()` — it also clears `hitl`, and this runs after *every* turn
  //    including one the planner parked, which would close the HITL modal on open;
  //  - clear `warnings`/`fatal` — they are not duplicated anywhere, and the user should
  //    still see "Skipped x.mov" after the report lands.
  function commitTurn(prompt: string, attachments: string[]) {
    const finished = liveReport.current;
    liveReport.current = null;

    if (finished) {
      setTranscript((prev) => [
        ...prev,
        { prompt, attachments, report: finished, ts: Math.floor(Date.now() / 1000) },
      ]);
      setReport(null);
      setLivePrompt("");
      setTurnCommitted(true);
    }

    refreshSessions();
  }

  async function run(prompt: string, files: File[], mode: TurnMode) {
    if (inFlight.current) return;
    inFlight.current = true;
    log.info("user submitted a turn", { prompt_chars: prompt.length, mode });
    setBusy(true);
    clearLive();
    setLivePrompt(prompt);
    try {
      const uploads = await Promise.all(files.map(fileToUpload));
      lastTurn.current = { prompt, uploads, mode };
      await submitTurn({ prompt, sessionId, files: uploads, mode }, onEvent);
      commitTurn(
        prompt,
        files.map((f) => f.name),
      );
    } catch (e) {
      log.error("turn failed", { error: String(e) });
      setFatal(String(e));
    } finally {
      setBusy(false);
      inFlight.current = false;
    }
  }

  async function resume(planJson: string, force: boolean) {
    if (inFlight.current) return;
    log.info("user resumed a parked turn", { force });
    const ctx = lastTurn.current;
    if (!ctx) {
      setFatal("nothing to resume — submit a prompt first");
      return;
    }
    inFlight.current = true;
    setBusy(true);
    setEvents([]);
    setReport(null);
    setHitl(null);
    setFatal(null);
    setWarnings([]);
    setTurnCommitted(false);
    liveReport.current = null;
    try {
      await resumeTurn(
        { prompt: ctx.prompt, sessionId, files: ctx.uploads, planJson, force, mode: ctx.mode },
        onEvent,
      );
      commitTurn(ctx.prompt, []);
    } catch (e) {
      setFatal(String(e));
    } finally {
      setBusy(false);
      inFlight.current = false;
    }
  }

  async function switchTo(id: string) {
    if (busy || id === sessionId) return;
    await endSession(sessionId).catch(() => {});
    setSessionId(id);
    persistId(id);
    clearLive();
    lastTurn.current = null;
    try {
      const s = await loadSession(id);
      setTranscript(s.exchanges ?? []);
    } catch {
      setTranscript([]);
    }
  }

  async function newChat() {
    if (busy) return;
    await endSession(sessionId).catch(() => {});
    const id = newSessionId();
    setSessionId(id);
    persistId(id);
    setTranscript([]);
    clearLive();
    lastTurn.current = null;
    refreshSessions();
  }

  async function doRename(id: string, title: string) {
    await renameSession(id, title).catch((e) =>
      log.error("rename_session failed", { error: String(e) }),
    );
    refreshSessions();
  }

  async function doDelete(id: string) {
    await deleteSession(id).catch((e) =>
      log.error("delete_session failed", { error: String(e) }),
    );
    if (id === sessionId) await newChat();
    else refreshSessions();
  }

  if (!plan) return <BootstrapGate onReady={setPlan} />;

  // Warnings and a fatal error outlive the commit: they are not duplicated in the
  // transcript, so they stay on screen under the finished exchange.
  const showLive =
    busy || (!turnCommitted && events.length > 0) || !!fatal || warnings.length > 0;

  return (
    <div className="workbench">
      <header className="wb-header">
        <h1>Sovereign AI Workbench</h1>
        <span className="wb-tier">{plan.tier_label}</span>
      </header>

      <div className="wb-body">
        <SessionSidebar
          sessions={sessions}
          activeId={sessionId}
          busy={busy}
          onNew={newChat}
          onOpen={switchTo}
          onRename={doRename}
          onDelete={doDelete}
        />

        <main className="wb-main">
          <section className="wb-left">
            <div className="transcript">
              {transcript.length === 0 && !showLive && (
                <p className="muted transcript-empty">
                  Ask a question, attach a document, and the analysis appears here.
                  Follow-up questions in the same chat keep their context.
                </p>
              )}

              {transcript.map((x, i) => (
                <div key={i} className="exchange">
                  <div className="bubble bubble-user">
                    {x.prompt}
                    {x.attachments.length > 0 && (
                      <div className="bubble-files">
                        {x.attachments.map((a) => (
                          <span key={a} className="chip">
                            {a}
                          </span>
                        ))}
                      </div>
                    )}
                  </div>
                  {x.report && <ReportView report={x.report} />}
                </div>
              ))}

              {showLive && (
                <div className="exchange">
                  {livePrompt && <div className="bubble bubble-user">{livePrompt}</div>}
                  {!turnCommitted && events.length > 0 && <Stepper events={events} />}
                  {!turnCommitted && activity.length > 0 && (
                    <ul className="activity">
                      {activity.map((e, i) => (
                        <li key={i}>
                          {e.stage === "executing_tool"
                            ? `▶ ${e.tool} (${e.index + 1}/${e.total})`
                            : `${(e as any).ok ? "✓" : "✕"} ${(e as any).tool} · ${(e as any).elapsed_ms} ms`}
                        </li>
                      ))}
                    </ul>
                  )}
                  {warnings.length > 0 && (
                    <ul className="warnings" aria-label="Pipeline warnings">
                      {warnings.map((w, i) => (
                        <li key={i} title={w.detail ?? undefined}>
                          <span className="warn-icon" aria-hidden>
                            &#9888;
                          </span>
                          <span>{w.message}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                  {fatal && <div className="fatal">Error: {fatal}</div>}
                  {report && <ReportView report={report} />}
                </div>
              )}
            </div>

            <PromptPanel busy={busy} onSubmit={run} />
          </section>

          <AuditSidebar events={events} plan={plan} />
        </main>
      </div>

      {hitl && (
        <HitlModal
          errors={hitl.errors}
          planJson={hitl.planJson}
          busy={busy}
          onResume={resume}
          onClose={() => setHitl(null)}
        />
      )}
    </div>
  );
}

export default App;
