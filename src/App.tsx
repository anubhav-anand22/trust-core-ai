import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

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
            <div className="response">{response || "Type a prompt below to start..."}</div>
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
      </form>
    </main>
  );
}

export default App;
