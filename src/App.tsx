import { useState } from "react";
import { invoke, Channel } from "@tauri-apps/api/core";
import "./App.css";

interface ModelProgress {
  model: string;
  message: string;
  percentage?: number | null;
}

function App() {
  const [prompt, setPrompt] = useState("");
  const [response, setResponse] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  async function generateResponse() {
    if (!prompt.trim()) return;

    setLoading(true);
    setError("");
    setResponse("");

    try {
      const res = await invoke<string>("generate_response", { prompt });
      setResponse(res);
    } catch (err: any) {
      setError(err.toString());
    } finally {
      setLoading(false);
    }
  }

  return (
    <main className="container">
      <h1>Local AI Assistant</h1>

      <div className="chat-container">
        <div className="output-box">
          {error ? (
            <div className="error">{error}</div>
          ) : (
            <div className="response">
              {response || "Type a prompt below to start..."}
            </div>
          )}
          {loading && <div className="loading">Generating response...</div>}
        </div>
      </div>

      <form
        className="input-row"
        onSubmit={(e) => {
          e.preventDefault();
          generateResponse();
        }}
      >
        <input
          id="prompt-input"
          value={prompt}
          onChange={(e) => setPrompt(e.currentTarget.value)}
          placeholder="Ask something to llama3.2:3b..."
          disabled={loading}
        />
        <button type="submit" disabled={loading || !prompt.trim()}>
          {loading ? "Thinking..." : "Send"}
        </button>
        <button
          type="button"
          onClick={async () => {
            console.log(await invoke<boolean>("is_ollama_installed"));
          }}
        >
          TEST is ollama installed
        </button>
        <button
          type="button"
          onClick={async () => {
            const onProgress = new Channel<ModelProgress>();
            onProgress.onmessage = (progress) => {
              console.log(progress);
            };

            try {
              setLoading(true);
              await invoke("ensure_required_models", { onProgress });
            } catch (err) {
              console.error("Failed downloading models:", err);
            } finally {
              setLoading(false);
            }
          }}
        >
          TEST ensure_required_models
        </button>
        <button
          type="button"
          onClick={async () => {
            const onProgress = new Channel<string>();
            onProgress.onmessage = (message) => {
              console.log(message);
            };

            try {
              setLoading(true);
              const success = await invoke<boolean>("ensure_ollama_installed", {
                onProgress,
              });
              console.log(success);
            } catch (err) {
              console.log(err);
            } finally {
              setLoading(false);
            }
          }}
        >
          TEST ensure_ollama_installed
        </button>
      </form>
    </main>
  );
}

export default App;
