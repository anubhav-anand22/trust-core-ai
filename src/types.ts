// Shared types mirroring the Rust command layer (src-tauri) and the
// `workbench-core` StepEvent contract. Keep field names in sync with the
// #[derive(Serialize)] structs on the Rust side.

export interface HardwareInfo {
  total_ram_gb: number;
  /** RAM free right now — what decides whether a model fits beside everything else. */
  available_ram_gb: number;
  /** Logical cores (hyperthreads). Shown for information. */
  cpu_cores: number;
  /** Physical cores — the number the thread budget and model tier are sized on. */
  physical_cores: number;
  gpu_vendor: string;
  gpu_name: string;
  vram_gb: number | null;
  cuda_available: boolean;
  /** The inference backend Ollama will use. `metal` = Apple Silicon unified memory. */
  backend: "cpu" | "cuda" | "rocm" | "metal";
  os: string;
  arch: string;
  /** Without AVX2, CPU inference is several times slower per core. */
  avx2: boolean;
  avx512: boolean;
}

// workbench_core::bootstrap::models::LlmAssessment — the override risk check.
export interface LlmAssessment {
  model: string;
  approx_ram_gb: number;
  approx_download_gb: number;
  /** Which of (resident, vision, embed) still need pulling. Empty = no download. */
  missing_models: string[];
  /** Vision model that goes with this choice: "" = none, or the model itself if multimodal. */
  vision: string;
  severity: "ok" | "caution" | "blocked";
  warnings: string[];
}

// src-tauri bootstrap::profile::UserProfile — answers from the first screen.
// Empty string / empty list = "Not sure".
export interface UserProfile {
  industry: string;
  work: string[];
  inputs: string[];
  priority: string;
  language: string;
}

export interface ModelPlan {
  llm: string;
  vision: string;
  embed: string;
  tier_label: string;
  /** Context window the resident model runs with, chosen with the tier. */
  num_ctx: number;
}

export interface CatalogEntry {
  name: string;
  /** Proven to drive this app's structured-JSON pipeline. Unverified = pickable, never recommended. */
  verified: boolean;
  multimodal: boolean;
  download_gb: number;
}

export interface SystemProbe {
  hardware: HardwareInfo;
  recommended: ModelPlan;
  assessment: LlmAssessment;
  /** Why this model, one plain-language line each. */
  reasons: string[];
  /** Next smaller model that fits (snappier), and next more capable one (slower). */
  faster: string | null;
  smarter: string | null;
  /** Dropdown options, straight from the backend catalogue. */
  catalog: CatalogEntry[];
  /** Local tags with Ollama's implicit `:latest` stripped — compare with `===`. */
  installed_models: string[];
  /** Linux: an old system-wide Ollama's models exist but this app can't see them. */
  legacy_system_models: boolean;
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
  logs: string;
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
  | { stage: "warning"; message: string; detail?: string | null }
  | { stage: "executing_tool"; tool: string; index: number; total: number }
  /** `device`: the mesh device that ran it, absent when it ran on this one. */
  | { stage: "tool_finished"; tool: string; ok: boolean; elapsed_ms: number; device?: string }
  | { stage: "quality_check"; passed: boolean }
  | { stage: "synthesizing" }
  | { stage: "done"; report_json: string }
  | { stage: "error"; message: string }
  /** Partial output of a prose model call. `text` is the FULL text so far (not a
   *  delta), so a dropped or repeated message can't garble the display. Never
   *  stored in the `events` list — see App.tsx `liveText`. */
  | { stage: "streaming"; tool: string; text: string };

export type StepStage = StepEvent["stage"];

// workbench_core::memory::session::Exchange — one turn in the transcript.
export interface Exchange {
  prompt: string;
  attachments: string[];
  report: FinalReport | null;
  ts: number;
}

// workbench_core::memory::SessionContext (subset the UI needs).
export interface SessionContext {
  session_id: string;
  title: string;
  created_at: number;
  updated_at: number;
  exchanges: Exchange[];
}

// workbench_core::memory::session::SessionMeta — one sidebar row.
export interface SessionMeta {
  session_id: string;
  title: string;
  updated_at: number;
  exchange_count: number;
}

// workbench_core::engine::schemas::FinalReport
// workbench_core::engine::schemas::MemoryFact — one confirmed (or proposed)
// cross-chat fact, e.g. { key: "plant", value: "MRPL Phase-3" }.
export interface MemoryFact {
  key: string;
  value: string;
}

export interface FinalReport {
  summary: string;
  findings: string[];
  citations: string[];
  safety_notes: string[];
  degraded: boolean;
  /** Facts the model suggests remembering across chats. Absent on reports saved
   *  before this field existed. Nothing is stored until the user confirms. */
  memory_proposals?: MemoryFact[];
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

// src-tauri/src/mesh.rs — the LAN mesh (docs/12-mesh.md)
export type MeshRole = "off" | "host" | "worker";

export interface MeshDevice {
  device_id: string;
  name: string;
  /** false = paired just now, waiting on the consent screen. */
  consented: boolean;
  busy: boolean;
  gpu: boolean;
  gpu_name: string;
  cores: number;
  ram_gb: number;
  llm_model: string | null;
  tok_s: number | null;
  link_mbps: number | null;
}

export interface MeshHostStatus {
  /** null once expired. */
  code: string | null;
  code_seconds_left: number;
  /** "192.168.1.20:52345", for networks where discovery finds nothing. */
  address: string | null;
  discoverable: boolean;
  tok_s: number | null;
  devices: MeshDevice[];
  delegator: string | null;
  pinned: string | null;
}

export interface MeshWorkerStatus {
  host_name: string;
  /** "idle" or the running job's task type, never its content. */
  activity: string;
  threads: number;
  max_threads: number;
  has_gpu: boolean;
  lend_gpu: boolean;
}

export interface MeshStatus {
  role: MeshRole;
  device_name: string;
  host: MeshHostStatus | null;
  worker: MeshWorkerStatus | null;
  known_hosts: { device_id: string; name: string }[];
}
