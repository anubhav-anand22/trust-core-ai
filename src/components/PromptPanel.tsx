// Prompt card: textarea, Attach + drop anywhere on the card (audio / pdf /
// image), the fast/deep read toggle, the local-model chip and Run.

import { useEffect, useRef, useState } from "react";
import { cancelTurn, type TurnMode } from "../lib/pipeline";
import { PixelIcon } from "./PixelIcon";

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
  exampleText,
  exampleToken,
  lockedReason,
  modelLabel,
}: {
  busy: boolean;
  /** The resident model's tag, shown as the "Local model" chip. */
  modelLabel?: string;
  /** Set while this device serves as a mesh Worker: the panel is read-only and says why. */
  lockedReason?: string;
  onSubmit: (prompt: string, files: File[], mode: TurnMode) => void;
  /** Bumped by `App` each time a turn actually delivers a report. */
  resetToken: number;
  /** Text from the last-clicked "try one of these" card. */
  exampleText?: string;
  /** Bumped by `App` on every example click, even a repeat click of the same card. */
  exampleToken?: number;
}) {
  const [prompt, setPrompt] = useState("");
  const [files, setFiles] = useState<File[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [deep, setDeep] = useState(false);
  const [rejected, setRejected] = useState<string[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

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

  // A "try one of these" card was clicked: fill the box with its prompt and put
  // the cursor there, so the only thing left for a judge to do is attach the
  // named fixture and hit Run. Only fills the TEXT — the picker can't reach
  // into the repo to attach the file itself; there is no filesystem capability
  // on the frontend (see docs/11-conversations-and-memory.md).
  useEffect(() => {
    if (!exampleToken) return; // 0/undefined: no example has been clicked yet
    if (exampleText != null) setPrompt(exampleText);
    textareaRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- keyed on the
    // token so a repeat click of the SAME example still re-fills and refocuses.
  }, [exampleToken]);

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

  const locked = busy || !!lockedReason;

  function submit() {
    if (!prompt.trim() || locked) return;
    setStopping(false);
    onSubmit(prompt.trim(), files, deep ? "deep" : "fast");
  }

  return (
    <div
      className={`prompt-panel${dragOver ? " prompt-panel-over" : ""}`}
      // The whole card is the drop target now, not a separate dashed box.
      onDragOver={(e) => {
        e.preventDefault();
        setDragOver(true);
      }}
      onDragLeave={(e) => {
        // dragleave also fires when the pointer moves onto a child (the
        // textarea, a chip). Only a leave to outside the panel ends the hover.
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDragOver(false);
      }}
      onDrop={(e) => {
        e.preventDefault();
        setDragOver(false);
        addFiles(e.dataTransfer.files);
      }}
    >
      {lockedReason && <p className="prompt-locked">{lockedReason}</p>}
      <div className="prompt-card">
        <textarea
          ref={textareaRef}
          className="prompt-input"
          placeholder="Describe your task or attach a file, e.g. “Assess corrosion risk on pump P-101 from these attachments.”"
          value={prompt}
          disabled={locked}
          onChange={(e) => setPrompt(e.currentTarget.value)}
          onKeyDown={(e) => {
            if ((e.metaKey || e.ctrlKey) && e.key === "Enter") submit();
          }}
        />

        {files.length > 0 && (
          <ul className="file-list">
            {files.map((f) => (
              <li key={f.name}>
                <span title={f.name}>{f.name}</span>
                <button
                  type="button"
                  className="file-remove"
                  aria-label={`Remove ${f.name}`}
                  onClick={() => setFiles((prev) => prev.filter((x) => x.name !== f.name))}
                >
                  ✕
                </button>
              </li>
            ))}
          </ul>
        )}

        <div className="prompt-toolbar">
          <button
            type="button"
            className="tool-btn"
            title="PDF, image or audio. You can also drop files anywhere on this box."
            onClick={() => inputRef.current?.click()}
          >
            <PixelIcon name="clip" size={14} />
            Attach
          </button>
          <label className="deep-toggle" title="Read the whole document in passes instead of retrieving the relevant part. Slower, thorough — use for a long document where a specific figure must not be missed.">
            <input
              type="checkbox"
              checked={deep}
              disabled={locked}
              onChange={(e) => setDeep(e.currentTarget.checked)}
            />
            Deep read
          </label>

          <span className="toolbar-spacer" />

          {modelLabel && (
            <span className="model-chip" title="Runs on this machine; nothing leaves it.">
              <span className="model-chip-label">Local model</span>
              <span className="model-chip-name">{modelLabel}</span>
            </span>
          )}
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
          <button
            type="button"
            className="run-btn"
            disabled={locked || !prompt.trim()}
            onClick={submit}
          >
            <PixelIcon name="run" size={12} />
            {busy ? "Running…" : "Run"}
          </button>
        </div>
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
      <p className="prompt-hint">
        {dragOver ? "Drop to attach" : "Drop PDF / image / audio files on the box · Ctrl+Enter to run"}
      </p>
    </div>
  );
}
