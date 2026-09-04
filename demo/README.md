# Demo assets

## Fixtures

`python demo/make_fixtures.py` (needs Pillow) writes to `demo/fixtures/`:

| File | What it is |
|---|---|
| `inspection.pdf` | Hand-built 1-page digital PDF — Pump P-101 external inspection sheet (seal drip, scaling, bearing temp, vibration, coating loss) |
| `valve.png` | Synthetic equipment photo (shapes + a "corrosion" label) for the VLM |
| `nameplate.png` | Text-heavy plate (tag, model, pressure, serial, inspect-due) for OCR |
| `note.wav` | 2 s 16 kHz tone — exercises the audio decode + whisper path (transcribes to ~nothing, as expected for a non-speech tone) |

## Headless pipeline run

Needs a running Ollama with `llama3.2:3b` (or override), `moondream`, `nomic-embed-text`.

```bash
cargo run -p workbench-core --example e2e -- \
  "Assess corrosion risk on pump P-101 and check it against our SOPs." \
  demo/fixtures/inspection.pdf demo/fixtures/valve.png
```

Env overrides: `WB_LLM`, `WB_VISION`, `WB_EMBED`, `WB_MODELS_DIR` (whisper/ocrs `.bin`/`.rten` files), `WB_KB_DIR`, `WB_DATA_DIR`.

## GUI

```bash
npm run tauri dev
```
