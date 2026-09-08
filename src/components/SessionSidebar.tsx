// Left rail: the list of past chat sessions, like the history pane in
// ChatGPT/Claude. Click one to reopen its full transcript.

import { useState } from "react";
import type { SessionMeta } from "../types";

function when(secs: number): string {
  if (!secs) return "";
  const d = new Date(secs * 1000);
  const today = new Date();
  const sameDay =
    d.getFullYear() === today.getFullYear() &&
    d.getMonth() === today.getMonth() &&
    d.getDate() === today.getDate();
  return sameDay
    ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleDateString([], { month: "short", day: "numeric" });
}

export function SessionSidebar({
  sessions,
  activeId,
  busy,
  onNew,
  onOpen,
  onRename,
  onDelete,
}: {
  sessions: SessionMeta[];
  activeId: string;
  busy: boolean;
  onNew: () => void;
  onOpen: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  function commitRename(id: string) {
    const t = draft.trim();
    if (t) onRename(id, t);
    setEditingId(null);
  }

  return (
    <aside className="session-rail">
      <button className="new-chat-btn" disabled={busy} onClick={onNew}>
        + New chat
      </button>

      <ul className="session-list">
        {sessions.length === 0 && (
          <li className="muted small session-empty">No past chats yet</li>
        )}
        {sessions.map((s) => {
          const active = s.session_id === activeId;
          return (
            <li
              key={s.session_id}
              className={`session-row${active ? " session-row-active" : ""}`}
            >
              {editingId === s.session_id ? (
                <input
                  className="session-rename"
                  autoFocus
                  value={draft}
                  onChange={(e) => setDraft(e.currentTarget.value)}
                  onBlur={() => commitRename(s.session_id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitRename(s.session_id);
                    if (e.key === "Escape") setEditingId(null);
                  }}
                />
              ) : (
                <button
                  className="session-open"
                  disabled={busy}
                  onClick={() => onOpen(s.session_id)}
                  title={s.title}
                >
                  <span className="session-title">{s.title}</span>
                  <span className="session-meta">
                    {s.exchange_count} turn{s.exchange_count === 1 ? "" : "s"} ·{" "}
                    {when(s.updated_at)}
                  </span>
                </button>
              )}

              <div className="session-actions">
                <button
                  title="Rename"
                  disabled={busy}
                  onClick={() => {
                    setEditingId(s.session_id);
                    setDraft(s.title);
                  }}
                >
                  ✎
                </button>
                <button
                  title="Delete"
                  disabled={busy}
                  onClick={() => {
                    if (confirm(`Delete chat "${s.title}"?`)) onDelete(s.session_id);
                  }}
                >
                  🗑
                </button>
              </div>
            </li>
          );
        })}
      </ul>
    </aside>
  );
}
