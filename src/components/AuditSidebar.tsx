// Collapsible audit panel: per-step timings, the active model context, and the
// resolved on-disk locations. This is the "enterprise explainability" surface.

import { useEffect, useState } from "react";
import { auditPaths } from "../lib/pipeline";
import type { AuditPaths, ModelPlan, StepEvent } from "../types";

interface ToolTiming {
  tool: string;
  ok: boolean;
  ms: number;
}

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
              </ul>
            ) : (
              <p className="muted">unavailable</p>
            )}
          </section>
        </div>
      )}
    </aside>
  );
}
