// Collapsible audit panel: per-step timings, the active model context, and the
// resolved on-disk locations. This is the "enterprise explainability" surface.

import { useEffect, useState } from "react";
import { auditPaths, openLogDir } from "../lib/pipeline";
import { log } from "../lib/log";
import type { AuditPaths, ModelPlan, StepEvent } from "../types";

interface ToolTiming {
  tool: string;
  ok: boolean;
  ms: number;
}

// A static trail through the pipeline, independent of any turn actually
// running — the live `Stepper` shows the same stages while a turn is in
// flight, but it is gone the moment the answer commits (see App.tsx). This is
// the one place a first-time viewer can read "what just happened" — or is
// about to — without having run anything, and without reading the README.
// Mirrors `run_turn`'s doc comment in crates/workbench-core/src/pipeline.rs.
const HOW_IT_WORKS: { label: string; tag?: string; detail: string }[] = [
  {
    label: "Parse intent",
    detail:
      "A small resident model reads the prompt and the attached files and extracts what is being asked and what kind each file is.",
  },
  {
    label: "Validate plan",
    tag: "Rust, deterministic",
    detail:
      "The model proposes a plan; a hand-written Rust validator checks it against fixed rules — not by asking the model to grade its own work. An invalid plan retries automatically, or is handed to you to edit.",
  },
  {
    label: "Execute tools",
    detail:
      "Each step runs one tool in turn — PDF/OCR/audio parsing, SOP retrieval, image analysis — against only the file it names.",
  },
  {
    label: "Quality check",
    detail:
      "Rule-based checks run before synthesis, independent of the model, and mark the report degraded if something looks wrong.",
  },
  {
    label: "Synthesize",
    detail: "A final pass turns the tool outputs into one report, with citations.",
  },
];

export function AuditSidebar({
  events,
  plan,
}: {
  events: StepEvent[];
  plan: ModelPlan | null;
}) {
  const [open, setOpen] = useState(true);
  const [paths, setPaths] = useState<AuditPaths | null>(null);

  useEffect(() => {
    auditPaths().then(setPaths).catch(() => setPaths(null));
  }, []);

  const timings: ToolTiming[] = events
    .filter((e): e is Extract<StepEvent, { stage: "tool_finished" }> => e.stage === "tool_finished")
    .map((e) => ({ tool: e.tool, ok: e.ok, ms: e.elapsed_ms }));

  const totalMs = timings.reduce((a, t) => a + t.ms, 0);

  return (
    <aside className={`audit${open ? "" : " audit-collapsed"}`}>
      <button className="audit-toggle" onClick={() => setOpen((o) => !o)}>
        {open ? "▸ Hide audit" : "◂ Audit"}
      </button>

      {open && (
        <div className="audit-body">
          <section>
            <h4>How it works</h4>
            <ol className="hiw-list">
              {HOW_IT_WORKS.map((s, i) => (
                <li key={s.label} title={s.detail}>
                  <span className="hiw-num">{i + 1}</span>
                  <span className="hiw-label">{s.label}</span>
                  {s.tag && <span className="hiw-tag">{s.tag}</span>}
                </li>
              ))}
            </ol>
            <p className="muted small">
              The plan is checked by code before any tool runs — not by asking the
              model to grade its own output.
            </p>
          </section>

          <section>
            <h4>Active models</h4>
            {plan ? (
              <dl className="audit-dl">
                <dt>resident LLM</dt>
                <dd>{plan.llm}</dd>
                <dt>vision</dt>
                <dd>{plan.vision}</dd>
                <dt>embeddings</dt>
                <dd>{plan.embed}</dd>
                <dt>tier</dt>
                <dd>{plan.tier_label}</dd>
              </dl>
            ) : (
              <p className="muted">not selected</p>
            )}
          </section>

          <section>
            <h4>Step timings</h4>
            {timings.length === 0 ? (
              <p className="muted">no tools run yet</p>
            ) : (
              <table className="audit-table">
                <tbody>
                  {timings.map((t, i) => (
                    <tr key={i} className={t.ok ? "" : "row-fail"}>
                      <td>{t.tool}</td>
                      <td className="num">{t.ms} ms</td>
                    </tr>
                  ))}
                  <tr className="audit-total">
                    <td>total</td>
                    <td className="num">{totalMs} ms</td>
                  </tr>
                </tbody>
              </table>
            )}
          </section>

          <section>
            <h4>Local storage</h4>
            {paths ? (
              <ul className="audit-paths">
                <li title={paths.data_dir}>
                  <span>data</span>
                  <code>{paths.data_dir}</code>
                </li>
                <li title={paths.uploads}>
                  <span>uploads</span>
                  <code>{paths.uploads}</code>
                </li>
                <li title={paths.lancedb}>
                  <span>kb index</span>
                  <code>{paths.lancedb}</code>
                </li>
                <li title={paths.session_context}>
                  <span>session</span>
                  <code>{paths.session_context}</code>
                </li>
                <li title={paths.persistent_memory}>
                  <span>memory (enc)</span>
                  <code>{paths.persistent_memory}</code>
                </li>
                <li title={paths.logs}>
                  <span>logs</span>
                  <code>{paths.logs}</code>
                </li>
              </ul>
            ) : (
              <p className="muted">unavailable</p>
            )}
          </section>

          <section>
            <h4>Diagnostics</h4>
            <p className="muted small">
              Every UI action and pipeline step is written to a daily log file.
              Attach it to a bug report.
            </p>
            <button
              className="secondary"
              onClick={() => {
                log.info("user opened the log directory");
                openLogDir().catch((e) =>
                  log.error("could not open log directory", { error: String(e) }),
                );
              }}
            >
              Open log folder
            </button>
          </section>
        </div>
      )}
    </aside>
  );
}
