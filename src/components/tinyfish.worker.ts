// TinyFish web search, run in a Web Worker so it can never touch TrustCore.
//
// Sponsor integration, deliberately outside the offline pipeline. Nothing from
// a TrustCore turn (prompt, attachments, report) is sent here; it only sees
// what the user types into the search sidebar.
//
// Why a worker: the SDK always sends a `User-Agent` header, and TinyFish's CORS
// allow-list doesn't include it, so the browser's preflight fails (verified
// with an OPTIONS request: 400 with user-agent, 204 without). The SDK calls the
// global `fetch` and offers no way to pass a different one, so the header has to
// be stripped by wrapping `fetch`. A worker has its own globals, so the wrapper
// below exists only in this worker and the app's `fetch` is untouched.

import { TinyFish } from "@tiny-fish/sdk";

export type SearchRequest = { id: number; apiKey: string; query: string };
export type SearchHit = { title: string; url: string; snippet: string };
export type SearchReply =
  | { id: number; ok: true; results: SearchHit[] }
  | { id: number; ok: false; error: string };

const workerFetch = self.fetch.bind(self);
self.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
  const headers = new Headers(init?.headers);
  headers.delete("user-agent");
  return workerFetch(input, { ...init, headers });
};

self.onmessage = async (e: MessageEvent<SearchRequest>) => {
  const { id, apiKey, query } = e.data;
  let reply: SearchReply;
  try {
    const client = new TinyFish({ apiKey, maxRetries: 1, timeout: 30_000 });
    const res = await client.search.query({ query, location: "US", language: "en" });
    reply = {
      id,
      ok: true,
      results: res.results.map((r) => ({ title: r.title, url: r.url, snippet: r.snippet })),
    };
  } catch (err) {
    reply = { id, ok: false, error: err instanceof Error ? err.message : String(err) };
  }
  self.postMessage(reply);
};
