// First-run gate. Walks the user through:
//   1. Ollama present? -> install if not (streamed progress)
//   2. probe hardware  -> show specs + recommended model tier + override
//   3. pull models     -> streamed per-model download progress
//   4. warm resident model
// then calls onReady(plan) and hands control to the workbench.

import { useEffect, useMemo, useState } from "react";
import {
  ensureModels,
  ensureOllamaInstalled,
  isOllamaInstalled,
  probeSystem,
  setModelPlan,
  warmModel,
} from "../lib/pipeline";
import type { ModelPlan, SystemProbe } from "../types";

type Phase =
  | "checking-ollama"
  | "installing-ollama"
  | "probing"
  | "choose"
  | "pulling"
  | "warming"
  | "error";

// Resident-LLM options offered in the override dropdown.
const LLM_CHOICES = [
  "qwen2.5:7b-instruct",
  "phi4-mini",
  "qwen2.5:3b-instruct",
  "qwen2.5:1.5b-instruct",
];

export function BootstrapGate({ onReady }: { onReady: (plan: ModelPlan) => void }) {
  const [phase, setPhase] = useState<Phase>("checking-ollama");
  const [log, setLog] = useState<string[]>([]);
  const [probe, setProbe] = useState<SystemProbe | null>(null);
  const [llm, setLlm] = useState<string>("");
  const [pullLines, setPullLines] = useState<Record<string, string>>({});
  const [error, setError] = useState<string>("");

  const push = (line: string) => setLog((l) => [...l.slice(-40), line]);

  // Step 1 + 2: ollama, then probe.
  useEffect(() => {
    (async () => {
      try {
        if (!(await isOllamaInstalled())) {
          setPhase("installing-ollama");
          await ensureOllamaInstalled(push);
        }
        setPhase("probing");
        const p = await probeSystem();
        setProbe(p);
        setLlm(p.recommended.llm);
        setPhase("choose");
      } catch (e) {
        setError(String(e));
        setPhase("error");
      }
    })();
  }, []);

  const chosenPlan: ModelPlan | null = useMemo(() => {
    if (!probe) return null;
    return { ...probe.recommended, llm };
  }, [probe, llm]);

  async function confirm() {
    if (!chosenPlan) return;
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
      onReady(chosenPlan);
    } catch (e) {
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
              <b>{probe.hardware.total_ram_gb.toFixed(1)} GB</b>
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
          </p>

          {phase === "choose" && (
            <button className="run-btn" onClick={confirm}>
              Download &amp; start
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
