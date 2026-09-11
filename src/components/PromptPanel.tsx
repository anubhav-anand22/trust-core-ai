// Prompt textarea + multi-file drop zone (audio / pdf / image), plus the
// fast/deep read toggle.

import { useEffect, useRef, useState } from "react";
import { cancelTurn, type TurnMode } from "../lib/pipeline";

// Kept in sync with `FileKind::from_extension` in
// crates/workbench-core/src/engine/schemas.rs. A type not listed here is still
// accepted by the drop zone but the backend will warn and skip it.
const ACCEPT =
  ".pdf,.png,.jpg,.jpeg,.tif,.tiff,.bmp,.webp,.wav,.mp3,.m4a,.flac,.ogg,.aac,.wma";

const ACCEPT_EXT = new Set(
  ACCEPT.split(",").map((e) => e.trim().toLowerCase()),
);

function extOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot >= 0 ? name.slice(dot).toLowerCase() : "";
}

export function PromptPanel({
  busy,
  onSubmit,
  resetToken,
}: {
  busy: boolean;
  onSubmit: (prompt: string, files: File[], mode: TurnMode) => void;
  /** Bumped by `App` each time a turn actually delivers a report. */
  resetToken: number;
}) {
  const [prompt, setPrompt] = useState("");
  const [files, setFiles] = useState<File[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [deep, setDeep] = useState(false);
  const [rejected, setRejected] = useState<string[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);

  // Empty the box once a turn has delivered its answer, so a follow-up starts
  // from a clean prompt and no stale attachments.
  //
  // Driven by a counter from the parent rather than by `busy` going false,
  // because a turn that FAILED or parked in the HITL modal must keep what the
  // user typed — clearing there would throw away their prompt and files at the
  // exact moment they need to retry with them. `App` bumps this only on a
  // committed report.
  //
  // The `deep` toggle deliberately survives: it reads as a session preference,
  // not per-question input.
  useEffect(() => {
    if (resetToken === 0) return; // first mount, nothing delivered yet
    setPrompt("");
    setFiles([]);
    setRejected([]);
    // The hidden <input type="file"> keeps its own value. Without this, picking
    // the SAME file again fires no `change` event and the attachment silently
    // never comes back.
    if (inputRef.current) inputRef.current.value = "";
  }, [resetToken]);

  function addFiles(list: FileList | null) {
    if (!list) return;
    const incoming = Array.from(list);
    // The drop zone bypasses the picker's `accept` filter, so screen it here.
    const ok = incoming.filter((f) => ACCEPT_EXT.has(extOf(f.name)));
    const bad = incoming.filter((f) => !ACCEPT_EXT.has(extOf(f.name)));
    if (bad.length) setRejected(bad.map((f) => f.name));
    setFiles((prev) => {
      const names = new Set(prev.map((f) => f.name));
      return [...prev, ...ok.filter((f) => !names.has(f.name))];
    });
  }

  function submit() {
    if (!prompt.trim() || busy) return;
    setStopping(false);
    onSubmit(prompt.trim(), files, deep ? "deep" : "fast");
  }

  return (
    <div className="prompt-panel">
      <textarea
        className="prompt-input"
        placeholder="Describe the task, e.g. “Assess corrosion risk on pump P-101 from these attachments,” or “What credit-card fee applies to a ₹1000 payment per this rate card?”"
        value={prompt}
        disabled={busy}
        onChange={(e) => setPrompt(e.currentTarget.value)}
        onKeyDown={(e) => {
          if ((e.metaKey || e.ctrlKey) && e.key === "Enter") submit();
        }}
      />

      <div
        className={`dropzone${dragOver ? " dropzone-over" : ""}`}
        onDragOver={(e) => {
          e.preventDefault();
          setDragOver(true);
        }}
        onDragLeave={() => setDragOver(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragOver(false);
          addFiles(e.dataTransfer.files);
        }}
        onClick={() => inputRef.current?.click()}
      >
        {files.length === 0 ? (
          <span>Drop PDF / image / audio files here, or click to choose</span>
        ) : (
          <ul className="file-list">
            {files.map((f) => (
              <li key={f.name}>
                <span>{f.name}</span>
                <button
                  type="button"
                  className="file-remove"
                  onClick={(e) => {
                    e.stopPropagation();
                    setFiles((prev) => prev.filter((x) => x.name !== f.name));
                  }}
                >
                  ✕
                </button>
              </li>
            ))}
          </ul>
        )}
        <input
          ref={inputRef}
          type="file"
          multiple
          accept={ACCEPT}
          hidden
          onChange={(e) => addFiles(e.currentTarget.files)}
        />
      </div>

      {rejected.length > 0 && (
        <p className="muted small reject-note">
          Skipped (unsupported type): {rejected.join(", ")}
          <button
            type="button"
            className="link-btn"
            onClick={() => setRejected([])}
          >
            dismiss
          </button>
        </p>
      )}

      <label className="deep-toggle" title="Read the whole document in passes instead of retrieving the relevant part. Slower, thorough — use for a long document where a specific figure must not be missed.">
        <input
          type="checkbox"
          checked={deep}
          disabled={busy}
          onChange={(e) => setDeep(e.currentTarget.checked)}
        />
        Deep read (whole document, slower)
      </label>

      <div className="run-row">
        <button
          type="button"
          className="run-btn"
          disabled={busy || !prompt.trim()}
          onClick={submit}
        >
          {busy ? "Running…" : "Run"}
        </button>
        {busy && (
          <button
            type="button"
            className="stop-btn"
            onClick={() => {
              setStopping(true);
              cancelTurn();
            }}
            disabled={stopping}
          >
            {stopping ? "Stopping…" : "Stop"}
          </button>
        )}
      </div>
    </div>
  );
}
