import { useState } from "react";
import type { FinalReport, MemoryFact } from "../types";

const sameFact = (a: MemoryFact, b: MemoryFact) =>
  a.key.toLowerCase() === b.key.toLowerCase() && a.value === b.value;

export function ReportView({
  report,
  remembered = [],
  onRemember,
}: {
  report: FinalReport;
  /** Already-confirmed facts, so a proposal that is already stored isn't offered again. */
  remembered?: MemoryFact[];
  /** Omit to hide proposals (nothing to confirm into). */
  onRemember?: (fact: MemoryFact) => void;
}) {
  // Dismissals are per-view only: a dismissed proposal stores nothing, so there
  // is nothing to persist — it just stops asking on this screen.
  const [dismissed, setDismissed] = useState<MemoryFact[]>([]);
  const proposals = (report.memory_proposals ?? []).filter(
    (p) =>
      !remembered.some((r) => sameFact(r, p)) && !dismissed.some((d) => sameFact(d, p)),
  );

  return (
    <div className="report">
      <div className="report-head">
        <h2>Inspection Report</h2>
        {report.degraded && (
          <span className="badge badge-warn" title="A tool failed or a quality check tripped">
            degraded
          </span>
        )}
      </div>

      <p className="report-summary">{report.summary}</p>

      {report.findings.length > 0 && (
        <section>
          <h3>Findings</h3>
          <ul>
            {report.findings.map((f, i) => (
              <li key={i}>{f}</li>
            ))}
          </ul>
        </section>
      )}

      {report.safety_notes.length > 0 && (
        <section className="report-safety">
          <h3>Safety notes</h3>
          <ul>
            {report.safety_notes.map((s, i) => (
              <li key={i}>{s}</li>
            ))}
          </ul>
        </section>
      )}

      {report.citations.length > 0 && (
        <section>
          <h3>Sources</h3>
          <ul className="report-cites">
            {report.citations.map((c, i) => (
              <li key={i}>{c}</li>
            ))}
          </ul>
        </section>
      )}

      {onRemember && proposals.length > 0 && (
        <section className="report-memory">
          <h3>Remember across chats?</h3>
          <p className="muted small">
            Nothing is stored unless you confirm it. Confirmed facts are encrypted
            on this machine and given to future chats as background.
          </p>
          <ul>
            {proposals.map((p) => (
              <li key={`${p.key}=${p.value}`} className="memory-proposal">
                <span>
                  <b>{p.key}</b>: {p.value}
                </span>
                <button type="button" onClick={() => onRemember(p)}>
                  Remember
                </button>
                <button
                  type="button"
                  className="secondary"
                  onClick={() => setDismissed((d) => [...d, p])}
                >
                  Dismiss
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
