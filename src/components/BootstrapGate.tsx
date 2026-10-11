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
  ensureToolModels,
  ensureOllamaInstalled,
  isOllamaInstalled,
  probeSystem,
  setModelPlan,
  warmModel,
} from "../lib/pipeline";
import { log as logger } from "../lib/log";
import type { LlmAssessment, ModelPlan, ModelProgress, SystemProbe, UserProfile } from "../types";

type Phase =
  | "checking-ollama"
  | "installing-ollama"
  | "probing"
  | "choose"
  | "pulling"
  | "warming"
  | "error";

/** One line summarising the profile, e.g. "energy · inspection, compliance · photos · balanced". */
function profileSummary(p: UserProfile): string {
  const parts = [
    p.industry || "any industry",
    p.work.length ? p.work.join(", ") : "",
    p.inputs.length ? p.inputs.join(", ") : "",
    p.priority || "balanced",
    p.language && p.language !== "english" ? p.language : "",
  ].filter(Boolean);
  return parts.join(" · ");
}

export function BootstrapGate({
  profile,
  onChangeProfile,
  onReady,
}: {
  profile: UserProfile;
  onChangeProfile: () => void;
  onReady: (plan: ModelPlan) => void;
}) {
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

  // The vision model follows the resident choice (a multimodal model reads images
  // itself; "no photos" means none), so it comes from the assessment of `llm`.
  const chosenPlan: ModelPlan | null = useMemo(() => {
    if (!probe) return null;
    const vision =
      assessment && assessment.model === llm ? assessment.vision : probe.recommended.vision;
    return { ...probe.recommended, llm, vision };
  }, [probe, llm, assessment]);

  // The backend catalogue, plus any chat model already on this machine that isn't
  // in it — someone's existing pull should be pickable without a download.
  // Specialist models (vision, embeddings) are not resident-LLM candidates.
  const installed = useMemo(() => new Set(probe?.installed_models ?? []), [probe]);
  const catalog = useMemo(() => probe?.catalog ?? [], [probe]);
  const choices = useMemo(() => {
    const names = catalog.map((c) => c.name);
    const extra = [...installed].filter(
      (m) =>
        !names.includes(m) &&
        !m.startsWith("moondream") &&
        !m.startsWith("minicpm-v") &&
        !m.includes("embed"),
    );
    return [...names, ...extra];
  }, [catalog, installed]);

  function choiceLabel(m: string): string {
    const entry = catalog.find((c) => c.name === m);
    const tags: string[] = [];
    if (probe && m === probe.recommended.llm) tags.push("recommended");
    if (installed.has(m)) tags.push("installed");
    if (!entry || !entry.verified) tags.push("untested");
    if (entry?.multimodal) tags.push("reads images");
    return tags.length ? `${m}  (${tags.join(", ")})` : m;
  }

  // Nothing to pull: say "Start", not "Download & start".
  const needsDownload = (assessment?.missing_models.length ?? 1) > 0;

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
      // vision is "" when the profile expects no photos, and equals llm when the
      // resident model is multimodal — pull each real model once.
      const needed = [...new Set([chosenPlan.llm, chosenPlan.vision, chosenPlan.embed])].filter(
        Boolean,
      );
      const onPull = (p: ModelProgress) =>
        setPullLines((prev) => ({
          ...prev,
          [p.model]: p.percentage != null ? `${p.message} (${p.percentage}%)` : p.message,
        }));
      await ensureModels(needed, onPull);
      // OCR / speech-to-text weights aren't Ollama models. Without them an
      // attached invoice photo went unread on a fresh machine. A failure here
      // only disables that tool, so it must never block startup.
      await ensureToolModels(onPull).catch((e) =>
        logger.warn("bootstrap: tool models not installed", { error: String(e) }),
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
      <h1>TrustCore AI</h1>
      <p className="bootstrap-sub">Sovereign AI Agentic Workbench · Offline setup</p>
      <p className="muted small profile-line">
        Profile: {profileSummary(profile)}{" "}
        <button
          type="button"
          className="link-btn"
          onClick={onChangeProfile}
          disabled={phase === "pulling" || phase === "warming"}
        >
          Change profile
        </button>
      </p>

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
              <span>Runs on</span>
              <b>
                {{ cpu: "CPU", cuda: "NVIDIA GPU (CUDA)", rocm: "AMD GPU (ROCm)", metal: "Apple GPU (Metal)" }[
                  probe.hardware.backend
                ]}
                <span className="muted small">
                  {" "}
                  · {probe.hardware.os}
                  {probe.hardware.backend === "cpu" && !probe.hardware.avx2 ? " · no AVX2" : ""}
                </span>
              </b>
            </li>
          </ul>

          <h3>Recommended for you</h3>
          <p className="muted">{probe.recommended.tier_label}</p>
          <ul className="reasons">
            {probe.reasons.map((r, i) => (
              <li key={i}>{r}</li>
            ))}
          </ul>
          {phase === "choose" && (probe.faster || probe.smarter) && (
            <div className="alternatives">
              {probe.faster && (
                <button type="button" className="secondary" onClick={() => setLlm(probe.faster!)}>
                  Faster: {probe.faster}
                </button>
              )}
              {probe.smarter && (
                <button type="button" className="secondary" onClick={() => setLlm(probe.smarter!)}>
                  Smarter (slower): {probe.smarter}
                </button>
              )}
              {llm !== probe.recommended.llm && (
                <button type="button" className="link-btn" onClick={() => setLlm(probe.recommended.llm)}>
                  back to recommended
                </button>
              )}
            </div>
          )}
          <label className="field">
            Resident LLM
            <select
              value={llm}
              disabled={phase !== "choose"}
              onChange={(e) => setLlm(e.currentTarget.value)}
            >
              {choices.map((m) => (
                <option key={m} value={m}>
                  {choiceLabel(m)}
                </option>
              ))}
            </select>
          </label>
          <p className="muted small">
            vision:{" "}
            {!chosenPlan?.vision
              ? "none (no photos in profile)"
              : chosenPlan.vision === llm
                ? "built into the resident model"
                : chosenPlan.vision}{" "}
            · embeddings: {probe.recommended.embed}
            {assessment != null
              ? needsDownload
                ? ` · ~${assessment.approx_download_gb.toFixed(1)} GB to download (${assessment.missing_models.join(", ")})`
                : " · all models already installed — nothing to download"
              : ""}
          </p>
          {probe.legacy_system_models && needsDownload && (
            <p className="muted small">
              Models from an earlier system-wide Ollama install were found, but this
              app runs its own per-user Ollama (no admin password) and cannot read
              them, so the ones listed above will be downloaded again.
            </p>
          )}

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
              {blocked
                ? "Model too large for this machine"
                : needsDownload
                  ? "Download & start"
                  : "Start"}
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
