// Sovereign AI Workbench — app shell.
// Bootstrap gate -> workbench (prompt + stepper + report + audit sidebar).

import { useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import { AuditSidebar } from "./components/AuditSidebar";
import { BootstrapGate } from "./components/BootstrapGate";
import { HitlModal } from "./components/HitlModal";
import { PromptPanel } from "./components/PromptPanel";
import { ReportView } from "./components/ReportView";
import { Stepper } from "./components/Stepper";
import { endSession, fileToUpload, submitTurn } from "./lib/pipeline";
import type { FinalReport, ModelPlan, StepEvent } from "./types";

function newSessionId(): string {
  return `s-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function App() {
  const [plan, setPlan] = useState<ModelPlan | null>(null);
  const [events, setEvents] = useState<StepEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<FinalReport | null>(null);
  const [hitl, setHitl] = useState<{ errors: string[]; planJson: string } | null>(null);
  const [fatal, setFatal] = useState<string | null>(null);
  const sessionId = useRef<string>(newSessionId());

  // Persist long-term memory when the window is closing/reloading.
  useEffect(() => {
    const onUnload = () => {
      endSession(sessionId.current).catch(() => {});
    };
    window.addEventListener("beforeunload", onUnload);
    return () => window.removeEventListener("beforeunload", onUnload);
  }, []);

  const activity = useMemo(
    () =>
      events
        .filter((e) => e.stage === "executing_tool" || e.stage === "tool_finished")
        .slice(-8),
    [events],
  );

  async function run(prompt: string, files: File[]) {
    setBusy(true);
    setEvents([]);
    setReport(null);
    setHitl(null);
    setFatal(null);
    try {
      const uploads = await Promise.all(files.map(fileToUpload));
      await submitTurn({ prompt, sessionId: sessionId.current, files: uploads }, (e) => {
        setEvents((prev) => [...prev, e]);
        if (e.stage === "done") {
          try {
            setReport(JSON.parse(e.report_json) as FinalReport);
          } catch {
            /* leave report null */
          }
        } else if (e.stage === "awaiting_user") {
          setHitl({ errors: e.errors, planJson: e.plan_json });
        } else if (e.stage === "error") {
          setFatal(e.message);
        }
      });
    } catch (e) {
      setFatal(String(e));
    } finally {
      setBusy(false);
    }
  }

  if (!plan) return <BootstrapGate onReady={setPlan} />;

  return (
    <div className="workbench">
      <header className="wb-header">
        <h1>Sovereign AI Workbench</h1>
        <span className="wb-tier">{plan.tier_label}</span>
        <button
          className="secondary wb-newsession"
          disabled={busy}
          onClick={async () => {
            await endSession(sessionId.current).catch(() => {});
            sessionId.current = newSessionId();
            setEvents([]);
            setReport(null);
            setHitl(null);
            setFatal(null);
          }}
        >
          End session &amp; new
        </button>
      </header>

      <main className="wb-main">
        <section className="wb-left">
          <PromptPanel busy={busy} onSubmit={run} />

          {events.length > 0 && <Stepper events={events} />}

          {activity.length > 0 && (
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

          {fatal && <div className="fatal">Error: {fatal}</div>}
          {report && <ReportView report={report} />}
        </section>

        <AuditSidebar events={events} plan={plan} />
      </main>

      {hitl && (
        <HitlModal
          errors={hitl.errors}
          planJson={hitl.planJson}
          onClose={() => setHitl(null)}
        />
      )}
    </div>
  );
}

export default App;
