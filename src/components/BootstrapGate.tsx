// First-run gate. Walks the user through:
//   1. Ollama present? -> install if not (streamed progress)
//   2. probe hardware  -> show specs + recommended model tier + override
//   3. pull models     -> streamed per-model download progress
//   4. warm resident model
// then calls onReady(plan) and hands control to the workbench.

import { useEffect, useMemo, useState } from "react";
import {
  assessModel,
  ensureModels,
  ensureOllamaInstalled,
  isOllamaInstalled,
  probeSystem,
  setModelPlan,
  warmModel,
} from "../lib/pipeline";
import { log as logger } from "../lib/log";
import type { LlmAssessment, ModelPlan, SystemProbe } from "../types";

type Phase =
  | "checking-ollama"
  | "installing-ollama"
  | "probing"
  | "choose"
  | "pulling"
  | "warming"
  | "error";

// Resident-LLM options offered in the override dropdown, smallest first. Mirrors
// LLM_CATALOG in src-tauri/src/bootstrap/models.rs. Deliberately no "thinking"
// model (e.g. qwen3): they return an empty body under this app's structured-output
// mode. The backend's assess_model call is what warns if a pick is too big.
const LLM_CHOICES = [
  "qwen2.5:0.5b-instruct",
  "qwen2.5:1.5b-instruct",
  "qwen2.5:3b-instruct",
  "llama3.2:3b",
  "phi4-mini",
  "qwen2.5:7b-instruct",
];

export function BootstrapGate({ onReady }: { onReady: (plan: ModelPlan) => void }) {
  const [phase, setPhase] = useState<Phase>("checking-ollama");
  const [log, setLog] = useState<string[]>([]);
  const [probe, setProbe] = useState<SystemProbe | null>(null);
  const [llm, setLlm] = useState<string>("");
  const [pullLines, setPullLines] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>("");
  // Risk check for the currently-selected model against this host. Refreshed
  // whenever the dropdown changes; drives the warning strip and blocks an
  // impossible pick.
  const [assessment, setAssessment] = useState<LlmAssessment | null>(null);

  const push = (line: string) => setLog((l) => [...l.slice(-40), line]);

  // Step 1 + 2: ollama, then probe.
  useEffect(() => {
    (async () => {
      try {
        logger.info("bootstrap: checking for Ollama");
        if (!(await isOllamaInstalled())) {
          // Worth a warning: on a machine where Ollama *is* installed, reaching
          // this branch means detection failed and the user is about to sit
          // through a pointless reinstall.
          logger.warn("bootstrap: Ollama not detected; running the installer");
          setPhase("installing-ollama");
          await ensureOllamaInstalled(push);
        }
        setPhase("probing");
        const p = await probeSystem();
        setProbe(p);
        setLlm(p.recommended.llm);
        setAssessment(p.assessment);
        setPhase("choose");
        logger.info("bootstrap: awaiting model confirmation", {
          recommended: p.recommended.llm,
          tier: p.recommended.tier_label,
        });
      } catch (e) {
        logger.error("bootstrap failed", { error: String(e) });
        setError(String(e));
        setPhase("error");
      }
    })();
  }, []);

  const chosenPlan: ModelPlan | null = useMemo(() => {
    if (!probe) return null;
    return { ...probe.recommended, llm };
  }, [probe, llm]);

  // Ask the backend how risky the current pick is on this machine.
  useEffect(() => {
    if (!probe || !llm) return;
    let live = true;
    assessModel(llm)
      .then((a) => {
        if (!live) return;
        setAssessment(a);
        if (a.severity !== "ok") {
          logger.warn("bootstrap: model assessment", {
            model: a.severity,
            severity: a.severity,
            warnings: a.warnings,
          });
        }
      })
      .catch((e) => logger.error("assess_model failed", { error: String(e) }));
    return () => {
      live = false;
    };
  }, [probe, llm]);

  const blocked = assessment?.severity === "blocked";

  async function confirm() {
    if (!chosenPlan) return;
    // Whether the user accepted the recommendation matters: overriding to a
    // larger model on a weak host is the known cause of unusable turn times.
    const overrode = probe != null && chosenPlan.llm !== probe.recommended.llm;
    logger.info("bootstrap: user confirmed model plan", {
      llm: chosenPlan.llm,
      recommended: probe?.recommended.llm,
      overrode_recommendation: overrode,
    });
    if (overrode) {
      logger.warn("bootstrap: recommendation overridden", {
        chosen: chosenPlan.llm,
        recommended: probe?.recommended.llm,
        cpu_cores: probe?.hardware.cpu_cores,
        cuda: probe?.hardware.cuda_available,
      });
    }
    try {
      setPhase("pulling");
      await setModelPlan(chosenPlan);
      await ensureModels(
        [chosenPlan.llm, chosenPlan.vision, chosenPlan.embed],
        (p) =>
          setPullLines((prev) => ({
            ...prev,
            [p.model]:
              p.percentage != null ? `${p.message} (${p.percentage}%)` : p.message,
          })),
      );
      setPhase("warming");
      await warmModel();
      logger.info("bootstrap: complete, handing over to the workbench", {
        llm: chosenPlan.llm,
      });
      onReady(chosenPlan);
    } catch (e) {
      logger.error("bootstrap: model setup failed", { error: String(e) });
      setError(String(e));
      setPhase("error");
    }
  }

  return (
    <div className="bootstrap">
      <h1>Sovereign AI Workbench</h1>
      <p className="bootstrap-sub">Offline setup</p>

      {phase === "checking-ollama" && <p>Checking for Ollama…</p>}

      {phase === "installing-ollama" && (
        <>
          <p>Installing Ollama…</p>
          <pre className="bootstrap-log">{log.join("\n")}</pre>
        </>
      )}

      {phase === "probing" && <p>Detecting hardware…</p>}

      {(phase === "choose" || phase === "pulling" || phase === "warming") && probe && (
        <div className="bootstrap-card">
          <h3>Detected hardware</h3>
          <ul className="hw-list">
            <li>
              <span>System RAM</span>
              <b>
                {probe.hardware.total_ram_gb.toFixed(1)} GB
                <span className="muted small">
                  {" "}
                  · {probe.hardware.available_ram_gb.toFixed(1)} GB free
                </span>
              </b>
            </li>
            <li>
              <span>CPU cores</span>
              <b>
                {probe.hardware.physical_cores} physical
                {probe.hardware.cpu_cores !== probe.hardware.physical_cores
                  ? ` · ${probe.hardware.cpu_cores} logical`
                  : ""}
              </b>
            </li>
            <li>
              <span>GPU</span>
              <b>
                {probe.hardware.gpu_name}
                {probe.hardware.vram_gb != null
                  ? ` · ${probe.hardware.vram_gb.toFixed(1)} GB`
                  : ""}
              </b>
            </li>
            <li>
              <span>CUDA</span>
              <b>{probe.hardware.cuda_available ? "yes" : "no"}</b>
            </li>
          </ul>

          <h3>Model tier</h3>
          <p className="muted">{probe.recommended.tier_label}</p>
          <label className="field">
            Resident LLM
            <select
              value={llm}
              disabled={phase !== "choose"}
              onChange={(e) => setLlm(e.currentTarget.value)}
            >
              {LLM_CHOICES.map((m) => (
                <option key={m} value={m}>
                  {m}
                  {m === probe.recommended.llm ? "  (recommended)" : ""}
                </option>
              ))}
            </select>
          </label>
          <p className="muted small">
            vision: {probe.recommended.vision} · embeddings: {probe.recommended.embed}
            {assessment != null
              ? ` · ~${assessment.approx_download_gb.toFixed(1)} GB download`
              : ""}
          </p>

          {assessment != null && assessment.warnings.length > 0 && (
            <ul
              className={`assess assess-${assessment.severity}`}
              aria-label="Model suitability"
            >
              {assessment.warnings.map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          )}

          {phase === "choose" && (
            <button
              className="run-btn"
              onClick={confirm}
              disabled={blocked}
              title={
                blocked
                  ? "This model will not fit on this machine — pick a smaller one."
                  : undefined
              }
            >
              {blocked ? "Model too large for this machine" : "Download & start"}
            </button>
          )}

          {(phase === "pulling" || phase === "warming") && (
            <div className="pull-status">
              {Object.entries(pullLines).map(([model, line]) => (
                <div key={model} className="pull-row">
                  <b>{model}</b>
                  <span>{line}</span>
                </div>
              ))}
              {phase === "warming" && <p>Warming resident model…</p>}
            </div>
          )}
        </div>
      )}

      {phase === "error" && (
        <div className="bootstrap-card">
          <h3 className="err">Setup failed</h3>
          <pre className="bootstrap-log">{error}</pre>
          <button className="run-btn" onClick={() => location.reload()}>
            Retry
          </button>
        </div>
      )}
    </div>
  );
}
