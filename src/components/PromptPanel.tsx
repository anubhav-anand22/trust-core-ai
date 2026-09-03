// Prompt textarea + multi-file drop zone (audio / pdf / image).

import { useRef, useState } from "react";

const ACCEPT = ".pdf,.png,.jpg,.jpeg,.tif,.tiff,.bmp,.webp,.wav,.mp3,.m4a,.flac,.ogg";

export function PromptPanel({
  busy,
  onSubmit,
}: {
  busy: boolean;
  onSubmit: (prompt: string, files: File[]) => void;
}) {
  const [prompt, setPrompt] = useState("");
  const [files, setFiles] = useState<File[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  function addFiles(list: FileList | null) {
    if (!list) return;
    setFiles((prev) => {
      const names = new Set(prev.map((f) => f.name));
      return [...prev, ...Array.from(list).filter((f) => !names.has(f.name))];
    });
  }

  function submit() {
    if (!prompt.trim() || busy) return;
    onSubmit(prompt.trim(), files);
  }

  return (
    <div className="prompt-panel">
      <textarea
        className="prompt-input"
        placeholder="Describe the inspection task, e.g. “Assess corrosion risk on pump P-101 from these attachments.”"
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

      <button className="run-btn" disabled={busy || !prompt.trim()} onClick={submit}>
        {busy ? "Running…" : "Run inspection"}
      </button>
    </div>
  );
}
