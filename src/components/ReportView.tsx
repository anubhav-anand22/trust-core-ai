import type { FinalReport } from "../types";

export function ReportView({ report }: { report: FinalReport }) {
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
    </div>
  );
}
