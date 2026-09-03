// Human-in-the-loop off-ramp. Shown when the planner exhausts its 2 retries.
// The user sees the validator's complaints and the last (invalid) plan, and can
// either edit the plan JSON and re-run, or dismiss.
//
// Phase 2 note: the backend `resume_turn` path is not wired yet, so "Re-run with
// this plan" currently just closes the modal and lets the user resubmit. The
// editor already produces the exact shape a future resume command will accept.

import { useState } from "react";

export function HitlModal({
  errors,
  planJson,
  onClose,
}: {
  errors: string[];
  planJson: string;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState(() => pretty(planJson));
  const [jsonError, setJsonError] = useState<string | null>(null);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>Plan needs your input</h2>
        <p className="modal-sub">
          The planner could not produce a valid plan after two automatic retries.
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
          value={draft}
          onChange={(e) => {
            setDraft(e.currentTarget.value);
            setJsonError(null);
          }}
        />
        {jsonError && <p className="hitl-json-error">{jsonError}</p>}

        <div className="modal-actions">
          <button
            className="secondary"
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
          <button
            onClick={() => {
              try {
                JSON.parse(draft);
                onClose();
              } catch (err) {
                setJsonError(`Invalid JSON: ${(err as Error).message}`);
              }
            }}
          >
            Looks right — close
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
