// First screen: who is this for? Five card questions, every one skippable.
//
// The answers never go through a model: the backend maps them to concrete needs
// with a fixed table (src-tauri/src/bootstrap/profile.rs), because at this point
// no model is installed yet. That same table decides, e.g., whether a vision
// model gets downloaded at all.

import { useState } from "react";
import { saveProfile } from "../lib/pipeline";
import { log } from "../lib/log";
import type { UserProfile } from "../types";

type Option = { key: string; label: string; hint?: string };

// Keys must match the tables in profile.rs.
const INDUSTRIES: Option[] = [
  { key: "energy", label: "Manufacturing, oil & gas, energy" },
  { key: "construction", label: "Construction & engineering" },
  { key: "finance", label: "Finance & banking" },
  { key: "legal", label: "Legal & compliance" },
  { key: "healthcare", label: "Healthcare" },
  { key: "logistics", label: "Logistics & supply chain" },
  { key: "it", label: "IT & software" },
  { key: "government", label: "Government / public sector" },
  { key: "education", label: "Education" },
  { key: "general", label: "General / other" },
];

const WORK: Option[] = [
  { key: "reports", label: "Reports & management summaries" },
  { key: "analysis", label: "Document & data analysis" },
  { key: "inspection", label: "Field inspection" },
  { key: "planning", label: "Planning & operations" },
  { key: "compliance", label: "Compliance & audit" },
  { key: "writing", label: "Writing & communication" },
  { key: "coding", label: "Coding" },
  { key: "design", label: "Design review" },
];

const INPUTS: Option[] = [
  { key: "documents", label: "PDFs & documents" },
  { key: "photos", label: "Photos", hint: "adds a vision model" },
  { key: "voice", label: "Voice notes" },
  { key: "code", label: "Code" },
];

const PRIORITY: Option[] = [
  { key: "fast", label: "Fastest answers", hint: "smaller model, snappier" },
  { key: "balanced", label: "Balanced" },
  { key: "quality", label: "Best quality", hint: "bigger model, slower on a weak machine" },
];

const LANGUAGE: Option[] = [
  { key: "english", label: "English" },
  { key: "hinglish", label: "Hindi + English" },
  { key: "multilingual", label: "Other / multilingual" },
];

const EMPTY: UserProfile = { industry: "", work: [], inputs: [], priority: "", language: "" };

export function ProfileGate({
  initial,
  onDone,
}: {
  initial: UserProfile | null;
  onDone: (p: UserProfile) => void;
}) {
  const [p, setP] = useState<UserProfile>(initial ?? EMPTY);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

  const pick = (field: "industry" | "priority" | "language", key: string) =>
    setP((prev) => ({ ...prev, [field]: prev[field] === key ? "" : key }));
  const toggle = (field: "work" | "inputs", key: string) =>
    setP((prev) => ({
      ...prev,
      [field]: prev[field].includes(key)
        ? prev[field].filter((k) => k !== key)
        : [...prev[field], key],
    }));

  async function finish() {
    setSaving(true);
    setError("");
    try {
      await saveProfile(p);
      log.info("profile confirmed", { industry: p.industry, priority: p.priority });
      onDone(p);
    } catch (e) {
      setError(String(e));
      setSaving(false);
    }
  }

  const single = (field: "industry" | "priority" | "language", opts: Option[]) => (
    <div className="profile-cards">
      {opts.map((o) => (
        <button
          key={o.key}
          type="button"
          className={`profile-card${p[field] === o.key ? " profile-card-on" : ""}`}
          onClick={() => pick(field, o.key)}
        >
          <span>{o.label}</span>
          {o.hint && <span className="profile-hint">{o.hint}</span>}
        </button>
      ))}
      <button
        type="button"
        className={`profile-card profile-unsure${p[field] === "" ? " profile-card-on" : ""}`}
        onClick={() => setP((prev) => ({ ...prev, [field]: "" }))}
      >
        Not sure
      </button>
    </div>
  );

  const multi = (field: "work" | "inputs", opts: Option[]) => (
    <div className="profile-cards">
      {opts.map((o) => (
        <button
          key={o.key}
          type="button"
          className={`profile-card${p[field].includes(o.key) ? " profile-card-on" : ""}`}
          onClick={() => toggle(field, o.key)}
        >
          <span>{o.label}</span>
          {o.hint && <span className="profile-hint">{o.hint}</span>}
        </button>
      ))}
      <button
        type="button"
        className={`profile-card profile-unsure${p[field].length === 0 ? " profile-card-on" : ""}`}
        onClick={() => setP((prev) => ({ ...prev, [field]: [] }))}
      >
        Not sure
      </button>
    </div>
  );

  return (
    <div className="bootstrap profile">
      <h1>TrustCore AI</h1>
      <p className="bootstrap-sub">
        Tell us how you'll use it, and we'll pick the smartest model your machine can run
        smoothly. Every question is optional.
      </p>

      <section className="profile-q">
        <h3>Which industry?</h3>
        {single("industry", INDUSTRIES)}
      </section>

      <section className="profile-q">
        <h3>What do you mostly do? <span className="muted small">pick any</span></h3>
        {multi("work", WORK)}
      </section>

      <section className="profile-q">
        <h3>What will you give it? <span className="muted small">pick any</span></h3>
        {multi("inputs", INPUTS)}
        {p.inputs.length > 0 && !p.inputs.includes("photos") && !p.work.includes("inspection") && (
          <p className="muted small">No photos → no vision model to download (saves ~1.7 GB).</p>
        )}
      </section>

      <section className="profile-q">
        <h3>What matters more?</h3>
        {single("priority", PRIORITY)}
      </section>

      <section className="profile-q">
        <h3>Language</h3>
        {single("language", LANGUAGE)}
      </section>

      {error && <p className="err">{error}</p>}
      <button className="run-btn" onClick={finish} disabled={saving}>
        {saving ? "Saving…" : "Continue to hardware check"}
      </button>
    </div>
  );
}
