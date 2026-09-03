// Shared types mirroring the Rust command layer (src-tauri) and the
// `workbench-core` StepEvent contract. Keep field names in sync with the
// #[derive(Serialize)] structs on the Rust side.

export interface HardwareInfo {
  total_ram_gb: number;
  gpu_vendor: string;
  gpu_name: string;
  vram_gb: number | null;
  cuda_available: boolean;
}

export interface ModelPlan {
  llm: string;
  vision: string;
  embed: string;
  tier_label: string;
}

export interface SystemProbe {
  hardware: HardwareInfo;
  recommended: ModelPlan;
}

export interface ModelProgress {
  model: string;
  message: string;
  percentage: number | null;
}

export interface AuditPaths {
  data_dir: string;
  uploads: string;
  lancedb: string;
  session_context: string;
  persistent_memory: string;
}

// One attachment, base64-encoded, as `submit_turn` expects it.
export interface UploadedFile {
  name: string;
  content_base64: string;
}

// Discriminated union matching workbench_core::events::StepEvent
// (#[serde(tag = "stage", rename_all = "snake_case")]).
export type StepEvent =
  | { stage: "idle" }
  | { stage: "parsing_context"; attempt: number }
  | { stage: "validating_plan"; attempt: number }
  | { stage: "awaiting_user"; errors: string[]; plan_json: string }
  | { stage: "executing_tool"; tool: string; index: number; total: number }
  | { stage: "tool_finished"; tool: string; ok: boolean; elapsed_ms: number }
  | { stage: "quality_check"; passed: boolean }
  | { stage: "synthesizing" }
  | { stage: "done"; report_json: string }
  | { stage: "error"; message: string };

export type StepStage = StepEvent["stage"];

// workbench_core::engine::schemas::FinalReport
export interface FinalReport {
  summary: string;
  findings: string[];
  citations: string[];
  safety_notes: string[];
  degraded: boolean;
}

// workbench_core::engine::schemas::Plan (for the HITL editor)
export interface TaskStep {
  id: string;
  task: string;
  args: unknown;
  depends_on: string[];
}
export interface Plan {
  steps: TaskStep[];
}
