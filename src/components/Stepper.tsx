// Horizontal execution stepper, advanced by StepEvents from the pipeline.
//
//   Idle -> Parsing Context -> Validating Plan -> Executing <tool> -> Synthesizing

import type { StepEvent, StepStage } from "../types";

const STAGES: { key: StepStage | "executing_tool"; label: string }[] = [
  { key: "idle", label: "Idle" },
  { key: "parsing_context", label: "Parsing Context" },
  { key: "validating_plan", label: "Validating Plan" },
  { key: "executing_tool", label: "Executing Tools" },
  { key: "synthesizing", label: "Synthesizing" },
  { key: "done", label: "Done" },
];

// Where each incoming event sits on the 0..STAGES.length-1 track.
function positionFor(stage: StepStage): number {
  switch (stage) {
    case "idle":
      return 0;
    case "parsing_context":
      return 1;
    case "validating_plan":
    case "awaiting_user":
      return 2;
    case "executing_tool":
    case "tool_finished":
    case "quality_check":
      return 3;
    case "synthesizing":
      return 4;
    case "done":
      return 5;
    case "error":
      return -1;
    case "warning":
      // A warning is an aside, not a stage — the caller filters these out before
      // asking for a position, so this is only here for exhaustiveness.
      return 0;
  }
}

export function Stepper({ events }: { events: StepEvent[] }) {
  // Warnings interleave with real stage events; they must not rewind the track.
  const staged = events.filter((e) => e.stage !== "warning");
  const last = staged[staged.length - 1];
  const stage = last?.stage ?? "idle";
  const pos = positionFor(stage);
  const errored = stage === "error";
  const awaiting = stage === "awaiting_user";

  const currentTool =
    [...staged].reverse().find((e) => e.stage === "executing_tool")?.tool ??
    null;

  return (
    <div className="stepper" role="list" aria-label="Execution progress">
      {STAGES.map((s, i) => {
        const state =
          errored && i > 0
            ? "error"
            : i < pos
              ? "done"
              : i === pos
                ? awaiting
                  ? "waiting"
                  : "active"
                : "pending";
        const label =
          s.key === "executing_tool" && currentTool && i === pos
            ? `Executing: ${currentTool}`
            : s.label;
        return (
          <div key={s.key} className={`step step-${state}`} role="listitem">
            <span className="step-dot">{state === "done" ? "✓" : i}</span>
            <span className="step-label">{label}</span>
          </div>
        );
      })}
      {errored && (
        <div className="step step-error" role="listitem">
          <span className="step-dot">!</span>
          <span className="step-label">
            Error: {last?.stage === "error" ? last.message : ""}
          </span>
        </div>
      )}
    </div>
  );
}
