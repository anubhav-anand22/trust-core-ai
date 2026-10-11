import { useEffect, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { SearchHit, SearchReply } from "./tinyfish.worker";
import "./SearchSidebar.css";

// Online web search powered by TinyFish (hackathon sponsor integration).
//
// Isolated on purpose: it shares no state, commands or data with TrustCore's
// offline pipeline, and the TinyFish SDK runs in its own Web Worker
// (tinyfish.worker.ts), so nothing here can affect the app's globals.

const KEY_STORAGE = "tinyfish_api_key";

/** Result URLs come from a third party: only ever hand web links to the OS. */
function isWebUrl(url: string): boolean {
  return /^https?:\/\//i.test(url);
}

function loadKey(): string {
  try {
    return localStorage.getItem(KEY_STORAGE) ?? "";
  } catch {
    return "";
  }
}

export function SearchSidebar() {
  const [open, setOpen] = useState(false);
  const [apiKey, setApiKey] = useState(loadKey);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SearchHit[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const worker = useRef<Worker | null>(null);
  const lastId = useRef(0);

  // One worker, started on first search and stopped when the sidebar unmounts.
  useEffect(() => () => worker.current?.terminate(), []);

  function getWorker(): Worker {
    if (!worker.current) {
      const w = new Worker(new URL("./tinyfish.worker.ts", import.meta.url), { type: "module" });
      w.onmessage = (e: MessageEvent<SearchReply>) => {
        if (e.data.id !== lastId.current) return; // a newer search superseded it
        setLoading(false);
        if (e.data.ok) {
          setResults(e.data.results);
          setError(null);
        } else {
          setResults([]);
          setError(e.data.error);
        }
      };
      worker.current = w;
    }
    return worker.current;
  }

  function saveKey(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const key = String(new FormData(e.currentTarget).get("apiKey") ?? "").trim();
    if (!key) return;
    try {
      localStorage.setItem(KEY_STORAGE, key);
    } catch {
      // Storage unavailable: the key still works for this session.
    }
    setApiKey(key);
  }

  function clearKey() {
    try {
      localStorage.removeItem(KEY_STORAGE);
    } catch {
      // nothing to clear
    }
    setApiKey("");
    setResults([]);
    setError(null);
  }

  function search(e: React.FormEvent) {
    e.preventDefault();
    const q = query.trim();
    if (!q || !apiKey) return;
    lastId.current += 1;
    setLoading(true);
    setError(null);
    getWorker().postMessage({ id: lastId.current, apiKey, query: q });
  }

  return (
    <aside className={`tf-sidebar${open ? "" : " tf-collapsed"}`} aria-label="TinyFish web search">
      <button className="tf-toggle" onClick={() => setOpen((o) => !o)}>
        {open ? "▸ Hide web search" : "◂ Web"}
      </button>

      {open && (
        <div className="tf-body">
          <div>
            <h4 className="tf-title">Web search</h4>
            <p className="tf-note">
              Online · powered by <strong>TinyFish</strong>. Separate from TrustCore's offline
              analysis: nothing you analyse is sent here.
            </p>
          </div>

          {!apiKey ? (
            <form className="tf-form" onSubmit={saveKey}>
              <input className="tf-input" type="password" name="apiKey" placeholder="TinyFish API key" required />
              <button type="submit" className="run-btn">Save key</button>
            </form>
          ) : (
            <>
              <form className="tf-form" onSubmit={search}>
                <input
                  className="tf-input"
                  type="text"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder="Search the web…"
                  required
                />
                <button type="submit" className="run-btn" disabled={loading}>
                  {loading ? "Searching…" : "Search"}
                </button>
              </form>
              <button className="tf-link-btn" onClick={clearKey}>Clear API key</button>

              {error && <div className="tf-error">{error}</div>}

              <ul className="tf-results">
                {results.map((r, i) => (
                  <li key={`${r.url}-${i}`} className="tf-result">
                    {isWebUrl(r.url) ? (
                      <button className="tf-result-title" onClick={() => openUrl(r.url)} title={r.url}>
                        {r.title || r.url}
                      </button>
                    ) : (
                      <span className="tf-result-title tf-result-plain">{r.title || r.url}</span>
                    )}
                    <p>{r.snippet}</p>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}
    </aside>
  );
}
