# TrustCore AI — Sovereign AI Agentic Workbench

An air-gappable desktop assistant for industrial inspection work. You give it a
question plus attachments (a digital PDF, a photo, a voice note); it parses your
intent, builds an execution plan, **validates that plan deterministically in Rust
rather than asking an LLM to grade itself**, runs lightweight tools one at a time,
and synthesises a report with citations back to your standing SOPs.

Everything runs locally against [Ollama](https://ollama.com). No cloud calls, no
Python runtime, no interpreter to ship — one Rust binary plus a webview.

> SIH 2026 · PS SIH26117 (Mangalore Refinery & Petrochemicals) · branch
> `feature/sovereign-workbench`

---

## Status

Phases 0–4 are done and the pipeline runs end to end. Phase 5 (packaging into an
installer) has not started. See [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)
for the phase table and the deliberate deviations from the original blueprint.

---

## Prerequisites

Install these **before** cloning. On Windows the C toolchain pieces are not
optional — `whisper-rs` compiles whisper.cpp from source and needs them.

| Tool | Why | Windows |
|---|---|---|
| Rust (stable, MSVC) | everything | `winget install Rustlang.Rustup` |
| CMake | builds whisper.cpp | `winget install Kitware.CMake` |
| LLVM / libclang | `whisper-rs` bindgen | `winget install LLVM.LLVM` |
| protoc | `lance-encoding` compiles protobuf schemas at build time | `winget install Google.Protobuf` |
| Node 20+ | the frontend | `winget install OpenJS.NodeJS` |
| Ollama | local inference | `winget install Ollama.Ollama` |

**Linux (verified on Ubuntu 26.04, 2026-10-10).** One `sudo` line for the system
libraries, then everything else installs under `$HOME` with no admin rights:

```bash
sudo apt install build-essential curl wget file cmake clang libclang-dev pkg-config \
  libwebkit2gtk-4.1-dev libxdo-dev libssl-dev librsvg2-dev libayatana-appindicator3-dev \
  zstd protobuf-compiler
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y   # Rust
curl -fsSL https://fnm.vercel.app/install | bash && fnm install 22        # Node
export LIBCLANG_PATH=/usr/lib/llvm-21/lib   # match your installed llvm-* version
```

You do **not** need to install Ollama on Linux: the app installs its own copy for
your user on first launch, with no password prompt (see walkthrough step 1).

`winget install` puts these on the **machine** PATH, which an already-open shell
will not see. Open a fresh terminal afterwards, or export them:

```bash
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:$PATH"
export LIBCLANG_PATH="C:\\Program Files\\LLVM\\bin"
```

**Budget ~15 GB of free disk and 45–60 minutes for the first build.** The
dependency graph is ~400 crates (Apache Arrow + DataFusion + LanceDB + Tauri).
Later builds are seconds. (Measured on a 2-core i7-4600U, `-j 1`: ~26 min for
`cargo test -p workbench-core`, then +20 min for the Tauri crate.)

---

## Setup

```bash
git clone https://github.com/anubhav-anand22/trust-core-ai.git
cd trust-core-ai
git checkout feature/sovereign-workbench

npm install
ollama serve                  # Windows/macOS, if not already running. Linux: the app starts its own.
npm run tauri dev             # first run: long. be patient.
```

The app opens on a bootstrap screen that probes your hardware, recommends a
model tier you can actually run, pulls only what is missing (models you already
have are marked *installed* and never downloaded again), and hands over to the
workbench. **Let it pick the model** — the recommendation accounts for whether
you have a usable GPU, and a machine without one needs a much smaller model than
its RAM alone would suggest.

To pre-pull manually:

```bash
ollama pull qwen2.5:1.5b-instruct   # CPU-only hosts
ollama pull llama3.2:3b             # or this, if you have cores/GPU to spare
ollama pull moondream               # vision
ollama pull nomic-embed-text        # embeddings (required for retrieval)
```

### Optional tool models

Audio and OCR degrade gracefully without these — the step reports that the model
is missing rather than failing the turn. To enable them, drop the files into
`%APPDATA%\com.anubhav_anand.tauri-app\models\`:

| File | Source |
|---|---|
| `ggml-base.en.bin` | `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin` |
| `text-detection.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten` |
| `text-recognition.rten` | `https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten` |

### Demo fixtures

Not committed (they are generated). To recreate them:

```bash
python demo/make_fixtures.py    # needs Pillow
```

---

## Gotchas that will cost you an hour if you skip them

**Run tests with `-j 1`.**

```bash
cargo test -p workbench-core -j 1
```

`workbench-core`'s compiled library is large (all the Arrow/DataFusion generics
land in it). Under a parallel build the integration-test binaries try to link it
while it is still being written and fail with `E0463: can't find crate for
workbench_core`. That aborts the run and leaves the fingerprint dirty, so the
next run fails identically. `-j 1` serialises compilation so every `.rlib` is
finished before anything links it.

**Pick one cargo subcommand and stay on it.** `cargo build --lib`, `cargo test`
and `cargo check --all-targets` resolve *different* feature sets for the
`lance-*` crates under resolver v2. Alternating between them forces a full
recompile of the whole tree each time.

**Let rust-analyzer use its own target directory**, or it will run a second
`cargo check` that contends with your terminal builds and roughly doubles build
times. `.vscode/settings.json` in this repo sets that up.

**`Could not find protoc`** while building `lance-encoding` means protoc is
missing — it is not optional. Install it (table above), or point `PROTOC` at a
downloaded binary from the protobuf GitHub releases.

**Linux + VS Code from the snap store: the app crashes on launch** with
`symbol lookup error: /snap/core20/.../libpthread.so.0`. The build is fine; the
snap-packaged VS Code exports `GTK_PATH`, `GIO_MODULE_DIR`, `LOCPATH` and friends
into every terminal it opens, so the app loads GTK modules built against the
snap's older glibc. Launch from a normal terminal, or strip them:

```bash
env -u GTK_PATH -u GTK_EXE_PREFIX -u GIO_MODULE_DIR -u GDK_PIXBUF_MODULE_FILE \
    -u GDK_PIXBUF_MODULEDIR -u LOCPATH -u GTK_IM_MODULE_FILE -u GSETTINGS_SCHEMA_DIR \
    XDG_DATA_DIRS="$XDG_DATA_DIRS_VSCODE_SNAP_ORIG" npm run tauri dev
```

---

## Manual testing walkthrough

This is the exact sequence to reproduce what has been tested so far. Please
follow it in order — later steps assume earlier ones worked.

### 0. Before you start

- Windows/macOS: Ollama must be running (`ollama serve`, or as a service). Check
  with `curl http://127.0.0.1:11434/api/tags`. Linux: nothing to do — the app
  installs and starts its own per-user Ollama.
- Close anything heavy. Inference will use every core it is allowed to, and on a
  small machine you will feel it (see *Known problems* below).
- Generate the fixtures: `python demo/make_fixtures.py` (needs Pillow). You get
  `inspection.pdf`, `fee_card.pdf`, `valve.png`, `nameplate.png`, `note.wav` in
  `demo/fixtures/`. `fee_card.pdf` is the borderless rate card for testing the
  table path.

### 1. Bootstrap screen

`npm run tauri dev`. Expect:

- **"Checking for Ollama…"** → then straight to **"Detected hardware"**. If you
  instead see *"Installing Ollama…"* and it sits there, that is a bug we thought
  we fixed — please report it with your OS and how Ollama was installed.
- **Linux, first launch only:** *"Installing for this user only — no admin
  password needed"*, a ~1.4 GB download into
  `~/.local/share/trustcore-ai/ollama`, then *"Ollama server is up"*. **There
  must be no password prompt anywhere.** Later launches skip straight to the
  server start (~0.5 s). Check `ps -eo user,args | grep "ollama serve"` shows
  *your* username — that is what lets the anti-freeze priority drop apply
  (known problem 5).
- A hardware card: RAM, GPU name, CUDA yes/no, and a **tier label**.
  **Sanity-check this.** With no GPU the tier comes from **physical cores and
  total RAM**: 2 cores → `qwen2.5:0.5b-instruct`, 4 cores + ~8 GB →
  `qwen2.5:1.5b-instruct`, 6+ cores + ~16 GB → `qwen2.5:3b-instruct`. It must
  **not** change between launches just because you opened a browser — if it
  does, that is a regression. A 7B model on a machine with no GPU is too.
- **Low free RAM right now** (lots of apps open) shows a *caution* — "close
  other applications before starting a turn" — but does not block or change the
  tier. A model that can't fit in total RAM at all is *blocked*.
- **Models you already have** show *(installed)* in the dropdown; installed
  models outside the tested list show *(installed, untested)*. The line under
  the dropdown lists only what will actually download. If everything is
  present it says *nothing to download* and the button reads **Start**.
- Linux only: if an earlier *system-wide* Ollama left models in
  `/usr/share/ollama/.ollama/models`, a note explains why they will be
  downloaded again (the per-user server can't read another user's files without
  sudo).
- **Let it use the recommended model.** Overriding to something bigger on a
  CPU-only box is exactly the mistake that made the app unusable for us.
- **The *Resident LLM* dropdown must be dark and readable** when you open it,
  matching the rest of the screen. If it opens as a bright white panel with
  near-invisible text, the `color-scheme: dark` fix did not take — report it with
  your OS and webview version.
- Click **Download & start** (or **Start**), watch the pull progress, then it
  hands over.

### 2. One normal turn

Click the first **"Try one of these"** card in the empty chat (or type it
yourself): *"Assess corrosion risk on pump P-101 and check it against our
SOPs."* Attach `demo/fixtures/inspection.pdf` and `demo/fixtures/valve.png` —
the card names them so you don't have to remember. Run it.

What should happen:

- The **stepper** advances: parsing context → validating plan → each tool in
  order → quality check → synthesising → done.
- The **activity list** shows each tool with its elapsed time.
- The **report** shows a summary, findings, and **citations naming the SOP files**
  it retrieved (`SOP-COR-014…`, `SOP-ROT-007…`). Citations are the thing to check
  hardest — they are the evidence retrieval actually worked.
- `degraded: false` unless you are missing the whisper/OCR model files.

**This will take minutes, not seconds, on a CPU-only machine.** It is slow, not
hung. See *Known problems*.

### 2b. One input, one answer (regression check)

A bug found on 2026-09-11 rendered **three identical report cards** from a single
file + prompt. It was a React bug, not a model or pipeline bug (see *Known
problems* 6b). **Fixed and confirmed on the dev machine** — please confirm it
holds on yours too, because the symptom depended on React's dev-mode behaviour
and is easy to reintroduce.

After the turn in step 2 finishes, check all of these:

| Check | Expected |
|---|---|
| Report cards on screen | exactly **one** |
| User prompt bubbles | exactly **one** |
| `sessions/<id>.json` | exactly **one** `Exchange` for that turn |
| the log | exactly one `submit_turn: start` / `submit_turn: done` pair |
| **audit sidebar after the turn ends** | still shows the step timings — must **not** go blank or say "no tools run yet" |
| reload the app (Ctrl+R) | the transcript redraws from disk, still **one** card |
| the prompt box and attachment list | **both empty themselves** — a follow-up starts clean, with no manual clearing |

Then check the reset does **not** fire when it shouldn't: make a turn fail or park
(step 4), and confirm your typed prompt and attached files are **still there**.
Losing them on a failed turn is the bug this reset has to avoid.

Then two things that the fix could plausibly have broken:

- **Warnings must survive.** Attach a `.mov` alongside the PDF. The
  *"Skipped … unsupported file type"* strip must still be on screen **after** the
  report lands, not flash and vanish.
- **The HITL modal must still open** (step 4 below). `commitTurn` runs after
  every turn including a parked one; if the modal opens and instantly closes,
  that is this fix regressing.

Finally, **double-click Run** quickly, and press **Ctrl+Enter twice**. The log
must show exactly one `submit_turn: start` — before the fix, each of those fired
two real pipeline runs, which on CPU means double the wait and two exchanges
written to disk.

### 2c. Layout (quick look, no turn needed)

The shell was rewritten on 2026-09-11 to fill the window; before that a
`max-width` on the transcript column left a dead gutter on wide monitors.
Verified with a static harness at 1920 and 1366 — worth 10 seconds on your
display, especially if it is ultrawide or scaled:

- **No empty gutter** between the transcript and the audit rail at any width.
- **The page itself never scrolls.** The transcript scrolls inside its pane and
  the prompt panel stays pinned at the bottom. If the whole window scrolls and
  the prompt box rides off-screen, that is a regression.
- **The report reflows** — its Findings / Safety notes / Sources sit side by side
  on a wide screen and stack on a narrow one.
- **Scrolling up to re-read an earlier answer must not yank you back down** when
  the next step event arrives. Scrolling back to the bottom re-enables following.

### 2d. Cross-chat memory: propose → you confirm (rebuilt 2026-10-10)

See *Known problems* 8b/9. Two chats must not see each other's *conversation*;
only facts **you** confirmed cross between them.

1. In chat A, run step 2's turn (pump P-101). Under the report, a **"Remember
   across chats?"** row may offer facts like *equipment: P-101* with
   **Remember** / **Dismiss**. (A tiny model may propose none — that is allowed.)
   If none appear, ask *"Our plant is MRPL Phase-3 — assess P-101"*.
2. Do **not** click anything yet. Click **New chat** and ask *"what did we just
   talk about?"*. **Expected:** no memory of chat A at all.
3. Go back to chat A, click **Remember** on one fact. It must appear in the audit
   sidebar's **Remembered facts** list.
4. New chat again, ask *"which plant are we at?"*. **Expected:** it now knows
   the confirmed fact — and *only* that fact, not chat A's conversation.
5. Click **×** on the fact in the sidebar. Ask again in a new chat — it is gone.
6. Back in chat A, a genuine follow-up (*"what about the valve?"*) still has
   full context of *that* chat (per-chat memory is a separate store).

### 2f. A bare first message answers (regression check)

New chat, no attachments, type *"hi"*. **Expected:** a short reply saying what
the app can do. Never the *"Plan needs your input"* modal — that used to happen
whenever the small model fumbled the plan for a message with nothing to read.

### 2e. First impression (no turn needed)

Added 2026-09-15, for a jury walking up without a live walkthrough:

- **The empty chat shows two example prompts** as clickable cards, each naming
  the fixture(s) to attach. They are the exact prompts from steps 2 and 3 below —
  clicking one only fills the text box, since the app has no way to reach into
  the filesystem and attach the file itself; attaching is still one manual drop.
- **The audit sidebar opens with a permanent "How it works" list** — Parse
  intent → Validate plan → Execute tools → Quality check → Synthesize — visible
  before you run anything and still there after, unlike the stepper. Hover a row
  for the longer explanation. If a judge asks "how do you stop the model from
  hallucinating a bad plan," the second row is the answer: a Rust validator, not
  the model grading itself.

### 3. The interesting test: a document with a specific fact buried in it

This is where we currently have a **known failure we are not sure is fixed**.
Attach a long PDF (a service agreement, a rate card — anything with a specific
number deep inside it) and ask a question that requires finding that number and
doing something with it. For example: *"What transaction fee applies to a ₹1000
credit card payment?"*

- **Good outcome:** it quotes the rate exactly and does the arithmetic.
- **Bad outcome:** *"no relevant information available"* — which is the bug we
  hit. If you see this, please grab the PDF (or say what kind it is) and report
  it. We need to distinguish two different causes: the text not fitting in the
  model's context window (believed fixed), versus `pdfplumber` not being able to
  extract the text from that particular PDF at all (not fixed, and a different
  problem entirely).

### 4. Human-in-the-loop (currently not reachable from the GUI)

**As of 2026-10-10 there is no reliable way to open this modal by hand.** The old
trigger — a vague prompt with no attachments, e.g. *"compare everything"* —
only worked because of a bug: the deterministic fallback plan came back empty
for such a message and parked. That is fixed (known problem 4), and a fallback
built from real attachments always validates, so the modal is now reached only
if an attached file vanishes from disk mid-turn. The code path is covered by
`tests/planner_retry.rs::fallback_that_cannot_run_still_parks`. Making it
demonstrable again needs a deliberate entry point — see known problem 11.

### 5. Audit sidebar and memory

- Expand the audit sidebar: per-step timings, active model, and the resolved
  on-disk paths (`lancedb/`, `session_context.json`, `persistent_memory.json`).
- Run 2–3 turns, close the app, reopen. The knowledge base should **not** re-ingest
  (it is cached), and **Remembered facts** should reload.
- `persistent_memory.json` should be unreadable ciphertext on disk — worth
  opening in a text editor to confirm. It only exists once you confirm a fact.
- `sessions/<id>.json`: every `Exchange` has a real Unix `"ts"`, never `0`.

### What to report back

**Attach the log file.** Every UI click, every backend command, every pipeline
stage and every tool timing is written to a daily file:

```
Windows   %APPDATA%\com.anubhav_anand.tauri-app\logs\workbench.log.<date>
macOS     ~/Library/Application Support/com.anubhav_anand.tauri-app/logs/
Linux     ~/.local/share/com.anubhav_anand.tauri-app/logs/
```

The audit sidebar has an **Open log folder** button that takes you straight
there. For more detail than the default, set `WB_LOG` before launching — it takes
`RUST_LOG` syntax, e.g. `WB_LOG=debug` or `WB_LOG=workbench_core::tools=trace,info`.

Beyond the log, the useful details are: which step, the elapsed times from the
activity list, whether the report said `degraded`, and — if the answer was wrong —
what the source document actually contained. Running the headless `--example e2e`
(bottom of this file) prints every step event and is usually the fastest way to
see where a turn went wrong.

---

## Known problems and open questions

Please read this before concluding something is broken — and please do brainstorm
on any of it.

### 1. CPU-only performance is slow by nature (but no longer dangerous)

On a 4-core machine with no GPU, a full turn took roughly **8 minutes**, with the
two analysis steps (`summarize`, `compare_to_sop`) at ~200s *each*. The physics
does not change — every token is a CPU matrix multiply — but the milestone in
[docs/09-running-on-cpu.md](docs/09-running-on-cpu.md) made it *safe* and
*bounded*:

- The model recommender is rebuilt around a sized catalogue and gates tiers on
  **physical** cores and **available** RAM. An 8 GB / 4-core / no-GPU box gets
  `qwen2.5:1.5b-instruct`; a 2-core box gets `qwen2.5:0.5b-instruct`; below that
  the app says so instead of pretending.
- Overriding to a bigger model now shows a warning (or blocks entirely, with the
  RAM numbers) instead of a silent log line.
- Every model call has a 240 s timeout, and there is a **Stop** button.

Still open, and worth thinking about:
- The improvement in wall-clock time is **not yet measured** against a baseline.
- ~~Are two analysis steps justified?~~ No: they are now folded into one
  `assess` call after validation (execution log part 16), saving one evidence
  embedding pass and one ~half-window prefill per turn. Not yet measured on a
  real turn.
- Streaming: the prose steps (`assess` and the other analysis steps, follow-up
  and direct answers) now show their text as it is written, in a box under
  the activity list (execution log part 17). The final report (role C) still
  arrives all at once: it is schema-constrained JSON, unreadable half-written.
- `moondream` still costs ~48 s per photo on 4 cores and is pulled on every tier.

### 2. Long documents / tables (largely rewritten, still unverified end-to-end)

Symptom: asked about a fee in a PDF rate card, the model answered *"no relevant
information available"*. The README used to blame context-window truncation. That
was incomplete — a read-only trace found the real chain
([docs/10-documents-and-tables.md](docs/10-documents-and-tables.md)):

1. **Tables were extracted and then dropped.** `parse_pdf` filled `data["tables"]`
   and nothing ever read it. Fixed: a flat `tables_text` rendering (header
   repeated per row) now flows into the evidence.
2. **Borderless tables were never detected** (`Strategy::Lattice` needs ruled
   lines). Fixed: `Strategy::Stream` fallback per page, and `layout: true` text.
3. **Role C still dumped the whole document** into an 8k window — the half of
   commit `5c0a3b6` that was never applied. Fixed: it renders + narrows now.
4. **Chunking split `2.00%`** on the decimal point. Fixed: line-boundary splits.
5. Plus: `nomic-embed-text` task prefixes, a budget-filling passage selector, and
   role prompts that no longer frame a payments question as "out of scope".

There is also a new **Deep read** checkbox: map-reduce over the whole document,
slower, misses nothing.

**Still needs verifying end-to-end** against the real PayU document. Unit tests
cover `render_tables`, `chunk_text` and `collect()`; `demo/make_fixtures.py` now
emits a borderless `fee_card.pdf`. A full extract-to-answer test needs a running
Ollama and has not been done.

### 3. Scanned PDFs are not supported, by design

`pdfplumber` reads digital text only. A scanned or image-only PDF reports "no
extractable text" rather than being OCR'd. This was a deliberate scope decision,
but it is a real limitation for a document-heavy demo.

### 4. Small models write bad plans (now with a safety net)

`llama3.2:3b` and smaller have been observed to copy the registry's formatting
into the task name (`"parse_pdf [Extract]"`), hallucinate tasks, and invent
attachments. Name normalisation and prompt hardening reduced it; **it is still not
reliable** on a 0.5–1.5B CPU model.

What changed (Stage 3): when the planner exhausts its retries, `plan_turn` now
builds a **deterministic fallback plan** — one extraction step per attachment,
then analysis — validates it, and runs that instead of parking in the HITL modal.

**That safety net had one hole, fixed 2026-10-10.** A message with no
attachments, no knowledge-base need and no earlier turn ("hi", "what can you
do?") produced an *empty* fallback plan; `validate_plan` rejects empty plans, so
exactly the turn a 0.5B model is most likely to fumble parked in the modal. A new
`answer_directly` task (stage Retrieve, no file) now fills that case, in the
fallback and as a planner option. The old test that *asserted* the park
(`truly_empty_fallback_still_parks`) was rewritten — it had encoded the bug.
Execution log part 11.

**`qwen3:4b` is still out**: it returns an empty body under structured output (a
thinking-model incompatibility). It was removed from the bootstrap dropdown.

### 4b. Multi-file, mixed-type turns now work

Every extraction tool used to call `first_file_of(kind)` and ignore every other
file of that kind — three photos meant photo #1 read three times. Fixed:
`ToolContext::file_for(step, kind)` resolves `args["file"]` (the planner names
each attachment), and the planner is now instructed to emit one step per file. An
unsupported type (`.mp4`) is dropped with a warning rather than blocking the
turn. **Not yet verified end-to-end** with a running model — see
[docs/02-execution-log.md](docs/02-execution-log.md) "part 3".

### 5. The app could make the whole desktop unresponsive (should be fixed)

Two independent causes, both now addressed:

1. **Thread pools sized off logical cores.** Ollama, whisper.cpp and the OCR
   runtime each sized their pools off `available_parallelism()` — hyperthreads —
   so a 4-core/8-thread laptop ran ~7 compute threads per pool on 4 real cores.
   Everything is now derived from `sysinfo::physical_core_count()`.
2. **Ollama's scheduling priority.** Ollama is a separate process; capping its
   thread *count* did not stop it out-prioritising the window manager. Bootstrap
   now drops every `ollama*` process to below-normal priority.

**On Linux, cause 2's fix was silently a no-op until 2026-10-10.** The official
`install.sh` runs Ollama as a systemd service under a dedicated `ollama` system
user, and `renice` only works on processes you own — it failed with "Operation
not permitted", which the code swallowed. It also needed `sudo` with no terminal
to prompt on, so the install itself stalled invisibly. Linux now installs a
**per-user** Ollama under `~/.local/share/trustcore-ai/ollama` (no password) and
the app starts `ollama serve` itself, so the process is yours and `renice`
applies. Verified on a 2-core i7-4600U: server owned by the user, `renice` 0→5
succeeded, idle at 0.3% CPU. Execution log part 10.

**Still worth stress-testing:** run a video call during a turn and confirm the
shell stays responsive, and that **Stop** aborts within a second or two. A build
of this project at normal priority did freeze the shell once during development —
that is the failure mode to check has not returned.

### 6. The whole GUI needs a real click-through

Several code paths compile and are type-checked but nobody has driven them in a
running app:

- **HITL resume** (`resume_turn`) — the "Re-run with this plan" / "Run anyway"
  buttons. Shares its execution tail with the normal path.
- **Chat sessions** (Stage 4) — the session sidebar, New chat, rename, delete,
  reopening a past chat's transcript, and a **follow-up question** (a second turn
  with no attachments) resolving instead of parking. The Rust side has unit tests
  (`session.rs`, `planner_retry.rs`); the React side has not been exercised.
- **Deep read**, the **Stop** button, and the **override warning** on the
  bootstrap screen.
- **Added 2026-10-10, unit-tested but not clicked through with real models:** the
  *Remember / Dismiss* row and the *Remembered facts* list (2d), the
  *(installed)* labels and the **Start** button (step 1 — the dev laptop had no
  models pulled), and the `answer_directly` reply to a bare "hi" (2f).

See the walkthrough. When you run these, watch `workbench.log.*` (problem 8).

### 6b. One input rendered three identical answers (fixed and confirmed 2026-09-11)

Reported from a demo run: one file + one prompt produced **three identical report
cards**. Worth reading even though it is fixed, because the triage order is the
reusable part.

**It was not the model and not the pipeline.** The backend was cleared by
enumerating every path an answer can take to the screen: `StepEvent::Done` has
exactly one construction site, `submit_turn` returns `Ok(())` and discards the
report (so the return value is not a second path), there is no `app.emit` /
`listen` anywhere (the transport is a `tauri::ipc::Channel` built fresh per
invoke), and every loop around a model call either re-rolls the *plan* or reduces
to a single result. Note that `MAX_PLAN_ATTEMPTS` is **3** — a coincidence with
the symptom, and exactly the kind of thing that sends you down a dead end if you
start from a hunch instead of from the delivery paths.

Two React defects compounded:

1. **A side effect inside a state updater.** `commitTurn` called `setTranscript`
   from inside the `setReport` updater. React treats updaters as pure and may call
   them more than once; `<React.StrictMode>` double-invokes them *on purpose*, in
   dev, to surface exactly this. Each invocation queued its own append.
2. **The live pane was never retired.** `clearLive()` only ran at the *start of
   the next* turn, so a finished report kept rendering below its own transcript
   copies.

**StrictMode was not the bug** — it revealed one. Defect 2 is unconditional, so a
production build would still have shown two cards. StrictMode stays on. You can
see it working in any log file: `app started` appears **twice per launch**,
milliseconds apart, with the same session id. That is expected.

The fix carries the report out of the async channel callback on a ref (so the
updater is pure) and *moves* the result into the transcript instead of copying
it. Two traps were found while fixing it, both from **not asking who else reads
this state**: calling `clearLive()` on commit also clears `hitl` and would close
the HITL modal the instant it opened, and clearing `events` blanks the audit
sidebar, whose timings are derived from them.

A related, genuinely worse bug was fixed in the same pass: `PromptPanel.submit()`
guarded on the `busy` **prop**, which the parent sets asynchronously, so a
double-click on Run fired **two real `submit_turn` invokes** — two pipeline runs,
two exchanges on disk, double the CPU wait. `run()`/`resume()` now hold an
`inFlight` ref, which updates synchronously.

**Confirmed working** on the dev machine on 2026-09-11: one input now produces
one report card. Worth noting how thin the evidence was *before* that run — the
diagnosis rested entirely on a code trace and a clean `tsc`, because the logs
contained no `submit_turn` lines at all and the original buggy run was never
captured. Walkthrough step 2b remains the check to run on a **second** machine,
since the symptom depended on React's dev-mode behaviour.

**Still no automated guard.** There is no vitest/RTL in `package.json`, and
`workbench-core/tests/` has no assertion that a turn emits exactly one `done`
stage. Both would have caught this. Neither exists yet.

### 7. Build environment fragility

- **`cargo test` must use `-j 1`** or it fails with `E0463` and poisons the next
  run. Explained under *Gotchas*.
- Alternating cargo subcommands re-resolves features for the `lance-*` crates and
  forces a full recompile. Pick one and stay on it.
- `target/` reached **23 GB** before we trimmed dependency debuginfo; it is ~12 GB
  now. A build did once fail outright with "no space on device".
- `lancedb 0.38.0` needs `features = ["remote"]` to compile at all — a workaround
  for a bug in that release, not something we want.

### 8. The logging is new and has never watched a complete turn

The diagnostics above were only just fixed. Two bugs meant that until now the log
file held exactly one line: the `WorkerGuard` was dropped at the end of Tauri's
`setup` hook (which joins the writer thread, so every later line was discarded),
and the front-end's `ui` target matched no directive in the filter, so all React
`info`/`debug` events were dropped before either sink. Both are fixed and there is
a unit test on the filter, but **no one has yet watched a full turn go through
with logging working.**

So: the first turn you run is also the first real test of the logging. If the log
file has only a `workbench starting` line in it, the fix did not take — that is a
bug worth reporting on its own. A healthy file shows, in order: `workbench
starting` → `ui:` bootstrap lines → `submit_turn` → per-tool `tool: start` /
`tool: ok` → `report synthesised`.

### 8b. Cross-session memory leak — fixed properly 2026-10-10 (was a stopgap)

Reported 2026-09-12: one chat's content surfaced in an unrelated chat. The cause
was **the automatic copy, not the sharing**: after every turn,
`persist_long_term` folded that chat's prose summary into the single global
`persistent_memory.json`, and every other chat's report step read it as
"facility memory". The 2026-09-12 stopgap (`GLOBAL_MEMORY_ENABLED = false`)
turned cross-chat memory off entirely.

**Now:** the flag, `persist_long_term` and `end_session` are deleted. The store
holds **only facts you clicked Remember on** (walkthrough 2d). The report step
proposes up to three facts per answer at no extra model call; nothing is written
without a click. An old file's digest still decrypts but is never written back
(`#[serde(skip_serializing)]`), so the next save cleans it. Per-chat memory
(`sessions/<id>.json`) was never affected. Execution log part 12.

**Risk to watch:** the extra `memory_proposals` field in the report schema could
make a 0.5B model's JSON less reliable. It is optional and the plain-text
fallback is unchanged — watch the log for `compiler returned bad JSON`.

### 9. Persistent memory is visible now

The **Remembered facts** list in the audit sidebar shows what crosses chats, with
× to forget. `recurrent_tags` is still structurally present and never written.

### 10. Not started

Phase 5: packaging into an installer, bundling Ollama as a sidecar and the
whisper/OCR weights, and the true air-gapped test (disconnect networking, repeat
the demo). Today the app downloads Ollama (Linux) and the models on first
launch, so a first run is **not** offline.

### 11. The HITL modal is not demonstrable any more

A consequence of fixing problem 4, not a regression in the modal: every path that
used to reach it was the empty-fallback bug. It now opens only when an attached
file disappears mid-turn. If it matters for a demo, it needs a deliberate entry
point (e.g. a "review the plan before running" toggle) — a product decision, not
done.

### 12. Smaller things seen in the log

- **Every saved turn had `"ts": 0`** in `sessions/<id>.json` — `record_turn` never
  stamped it despite a comment saying it did. Fixed 2026-10-10 (part 11).
- **Dev mode runs each bootstrap step twice** — StrictMode double-runs
  `BootstrapGate`'s effect, which has no guard. Harmless (a second
  `ollama serve` just fails to bind) and absent from production builds.
- **Every fresh launch logs `ERROR load_session: no session …`** — expected for
  a brand-new chat, but it is noise in the file people attach to bug reports.
- **Line endings:** `.gitattributes` now exists (LF, `.bat` CRLF, binaries
  marked). Existing files were deliberately not renormalised in one huge diff;
  each picks up LF the next time it is edited.

---

## Documentation

The [`docs/`](docs/) folder is written for someone learning the codebase, not
just running it:

| | |
|---|---|
| [README](docs/README.md) | index + glossary of every term used |
| [01 Overview](docs/01-overview.md) | what the system is, the shape of one turn |
| [02 Execution log](docs/02-execution-log.md) | how it was built, including the walls hit |
| [03 Codebase tour](docs/03-codebase-tour.md) | every module and why it exists |
| [04 Agentic pipeline](docs/04-agentic-pipeline.md) | one real turn traced event by event |
| [05 Rust for this codebase](docs/05-rust-for-this-codebase.md) | 15 concepts tied to concrete lines |
| [06 RAG and the vector store](docs/06-rag-and-why-not-a-vector-db.md) | embeddings, cosine, the LanceDB store |
| [07 Build / test / run](docs/07-build-test-run.md) | commands, env, expected times |
| [08 Extending](docs/08-extending.md) | adding a tool, model, event, or KB doc |

---

## Layout

```
crates/workbench-core/   the pipeline. no Tauri, unit-testable.
  engine/                one resident Ollama model; roles A/B/C are prompt swaps
  planner/               TASK_REGISTRY + deterministic validate_plan + retry ceiling
  executor/              strict sequential tool loop, quality gate, report compiler
  tools/                 pdf · ocr · audio · vision · rag · analysis
  memory/                session context + AES-256-GCM long-term memory
  kb/                    seed SOP documents

src-tauri/               the desktop host: bootstrap, commands, event bridge
src/                     React frontend: bootstrap gate, stepper, HITL modal, report
```

## Running the pipeline without the GUI

Useful for debugging — needs Ollama running, prints every step event:

```bash
cargo run -p workbench-core --example e2e -j 1 -- \
  "Assess corrosion risk on pump P-101 and check it against our SOPs." \
  demo/fixtures/inspection.pdf demo/fixtures/valve.png
```
