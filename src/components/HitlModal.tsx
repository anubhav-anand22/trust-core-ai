// Human-in-the-loop off-ramp. Shown when the planner exhausts its 2 retries.
// The user sees the validator's complaints and the last (invalid) plan, edits
// the plan JSON, and either re-runs it (the deterministic validator runs again)
// or forces it through as-is. "Dismiss" just closes the modal.

import { useState } from "react";

export function HitlModal({
  errors,
  planJson,
  busy,
  onResume,
  onClose,
}: {
  errors: string[];
  planJson: string;
  busy: boolean;
  onResume: (planJson: string, force: boolean) => void;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(() => pretty(planJson));
  const [jsonError, setJsonError] = useState<string | null>(null);

  // Parse-check the draft, then hand it to the resume path. Returns false (and
  // shows the parse error) if the textarea isn't valid JSON.
  function resume(force: boolean): void {
    try {
      JSON.parse(draft);
    } catch (err) {
      setJsonError(`Invalid JSON: ${(err as Error).message}`);
      return;
    }
    onResume(draft, force);
  }

  return (
    <div className="modal-backdrop" onClick={busy ? undefined : onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>Plan needs your input</h2>
        <p className="modal-sub">
          The planner could not produce a valid plan after two automatic retries.
          Edit the plan and re-run it, or force it through as-is.
        </p>

        <h3>What the validator rejected</h3>
        <ul className="hitl-errors">
          {errors.map((e, i) => (
            <li key={i}>{e}</li>
          ))}
        </ul>

        <h3>Proposed plan (editable)</h3>
        <textarea
          className="hitl-editor"
          spellCheck={false}
          disabled={busy}
          value={draft}
          onChange={(e) => {
            setDraft(e.currentTarget.value);
            setJsonError(null);
          }}
        />
        {jsonError && <p className="hitl-json-error">{jsonError}</p>}

        <div className="modal-actions">
          <button className="secondary" disabled={busy} onClick={onClose}>
            Dismiss
          </button>
          <button
            className="secondary"
            disabled={busy}
            onClick={() => {
              try {
                navigator.clipboard?.writeText(draft);
              } catch {
                /* clipboard unavailable */
              }
            }}
          >
            Copy plan
          </button>
          <button className="secondary" disabled={busy} onClick={() => resume(true)}>
            Run anyway
          </button>
          <button disabled={busy} onClick={() => resume(false)}>
            {busy ? "Running…" : "Re-run with this plan"}
          </button>
        </div>
      </div>
    </div>
  );
}

function pretty(json: string): string {
  try {
    return JSON.stringify(JSON.parse(json), null, 2);
  } catch {
    return json;
  }
}
