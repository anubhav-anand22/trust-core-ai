# 12 · The mesh: borrowing nearby machines without giving them your data

> Status: **Phases 1–5 of 6 landed**: the core scheduler, the encrypted link,
> the Host hub plus Worker runtime, delegated planning, and the app (Tauri
> commands plus the Mesh panel). Tested over loopback and type-checked. **Two
> real devices have not been tested yet**: that is phase 6, [the device test](#phase-6-the-two-device-test).

TrustCore is built to run on a weak, often GPU-less office machine. On a
machine like that a turn takes minutes. Most offices have a stronger
machine nearby. The mesh lets the weak one (the **Host**) hand pieces of a turn
to the stronger ones (**Workers**) on the same LAN, without the Host ever
giving up its data or its control.

## The mental model: a function call, not a shared brain

```text
            ┌──────────────────────── Host (keeps everything) ─────────────────────┐
 user ──▶   │ session · memory · KB · plan · every result                          │
            │                                                                      │
            │  plan: [p1 parse_pdf] [o1 ocr_image] [k1 search_knowledge] [s1 assess]│
            │            │               │                 │ (Host-only)    │      │
            └────────────┼───────────────┼─────────────────┼────────────────┼──────┘
                 StepJob │       StepJob │                 ▼        StepJob │
                         ▼               ▼              runs here          ▼
                    ┌─────────┐     ┌─────────┐                       ┌─────────┐
                    │Worker A │     │Worker B │                       │Worker A │
                    └─────────┘     └─────────┘                       └─────────┘
                 returns ToolResult, keeps nothing
```

The planner already turns a request into a plan: a small graph of `TaskStep`s,
each naming a tool and the earlier steps it `depends_on`
(`crates/workbench-core/src/engine/schemas.rs`). One step is one tool run, so
it is the natural unit to send away. A Worker gets a **`StepJob`**, which holds:

| In the job | Why it's there |
|---|---|
| the step (`task`, `args`, `depends_on`) | what to run |
| outputs of the steps it reads | its input |
| the one attachment it reads | resolved with the same `resolve_file` rule the tool uses, so a Worker can never pick a different file than the Host would |
| the user's prompt | the vision question, the RAG query fallback and the analysis question all need it |
| the session history: **only for `answer_followup`** | that tool answers from it. Every other job carries an empty history |
| Fast/Deep mode | so a Deep read stays deep |

These never leave the Host: cross-chat memory, the knowledge base (the
`search_knowledge` step always runs on the Host, and only its *results* go to the
steps that read them), other chats and the uploads folder.

## The two rules that keep results identical

The mesh runs independent steps at the same time on different machines. That
is the speed-up, and it is also how a parallel scheduler quietly changes
answers. Two rules prevent that (`crates/workbench-core/src/mesh.rs`):

1. **Analysis waits for everything before it.** The analysis tool falls back to
   *every output so far* when its declared dependencies give no evidence
   (`tools/analysis.rs`). In the sequential executor "so far" means "all
   earlier steps". So an analysis step (or any task the registry doesn't know,
   like the folded `assess`) waits for every earlier step and gets exactly
   their outputs. It does not get outputs from a *later* step that happened to
   finish first. Test: `analysis_waits_for_every_earlier_step_and_sees_only_those`.
2. **One job per machine at a time.** The executor's original rule was "one
   model resident at a time". The mesh keeps it *per device*: no Worker, and
   not the Host, ever runs two jobs at once. Test:
   `independent_steps_run_at_once_but_never_two_on_one_worker`.

Results always come back in plan order, so the quality gate and the report
compiler see the same thing they always did.

**Mesh off is the old code path, not a simulation of it.** `run_turn` and
`resume_turn` are unchanged and pass `None`; only `run_turn_with_mesh` /
`resume_turn_with_mesh` with a dispatcher reach `execute_plan_mesh`. Test:
`with_no_workers_the_mesh_matches_the_sequential_executor`.

## When a Worker fails

A Worker that drops, times out or refuses doesn't fail the step. The executor
warns the user (an amber strip, as today), tries the next candidate Worker,
and when none is left, runs it on the Host. A turn never dies because a
Worker did. A tool that *ran* and failed (`ok: false`) is a normal degraded
step, exactly as without the mesh. Stop cancels every Worker
(`Dispatcher::cancel_all`).

## How two devices meet (`crates/workbench-mesh`)

The network side is its own Tauri-free crate, so its tests don't build the
desktop app, and `workbench-core` stays network-free.

```text
Host: Hub::open_pairing() → shows K7Q-4M9          advertises _trustcore._tcp {tag=K7}
Worker: user types it → discovery::find_hosts("K7") → Host's address
Worker ──ws──▶ Host   Opening{proto, pair, tag}            (plaintext, one message)
               ⇄      SPAKE2(secret "Q4M9") → 32-byte key
               ⇄      Noise XXpsk3 (psk = that key)       wrong code fails HERE, on the Host
               ⇄      Hello{name, capabilities} both ways  (encrypted from here on)
Host UI: "gpu-box wants to join, it will see …"  Approve → TrustStore.add (MESH-4)
later:  Worker ──ws──▶ Opening{reconnect} → Noise XX → each side checks the other's pinned key
```

| Piece | File | What to know |
|---|---|---|
| Code | `code.rs` | **6 characters, shown `K7Q-4M9`**: letters and digits without I/L/O/U (typing them is forgiven), so it reads out cleanly. 2 are a public **tag** (for discovery), 4 a **secret** (~1M values) that only ever feeds SPAKE2. With 5 failures allowed, a guesser has about a 1-in-200,000 chance per code. One code pairs any number of Workers for 10 minutes; each still needs consent. `Debug` prints `K7*-***`. Two Hosts share a tag about 1 time in 1,024; the Worker then simply tries both. |
| Why SPAKE2 | `link.rs` | A short secret used straight as a Noise PSK can be brute-forced *offline* by anyone who sits in the middle of one handshake. Through SPAKE2, each guess costs a live handshake, and the window allows 5 failures. |
| Keys | `identity.rs` | One X25519 key per device (`mesh_identity.json`, owner-only on Unix). Paired devices in `mesh_devices.json`. A corrupt file is an error, never silently "trust nobody". |
| Frames | `frame.rs` | JSON, one Noise message for an 8-byte length, then ≤64 KiB chunks. Max frame 256 MB. **Externally tagged** (see the log, part 22). |
| Discovery | `discovery.rs` | mDNS. Only the tag is broadcast. A network that blocks multicast finds nothing, and the UI then offers manual `IP:port`. Tested on this laptop's real interface (the test is `#[ignore]` for CI). |

## The Host at runtime (`hub.rs`) and the Worker (`worker.rs`)

- **Per Worker, three tasks:** a reader (a frame read can't be interrupted
  halfway, so it feeds a channel), a writer (one sender, so frames never
  interleave), and a supervisor that routes frames to waiting jobs, pings every
  5 s, and declares the Worker gone after 20 s of silence. A Worker that is gone
  fails its jobs, and the executor moves them on (MESH-7).
- **Consent gates work:** a newly paired Worker is connected but gets nothing
  until `Hub::approve`. Test: `a_worker_waiting_for_consent_gets_no_work`.
- **What leaves the Host per job:** the bytes and the *name* of the file. Never
  the Host's path, which would leak the user's home folder and username.
- **What a Worker may show in the Host's UI:** live text, warnings, and
  `ExecutingTool` only when labelled with its own task (deep-read progress). It
  can't fake a stage ("Done", "Awaiting you").
- **Worker keeps nothing:** files go in `scratch/job-<id>/`, removed however the
  job ends. The scratch dir is wiped on start. Logs hold job ids, task names and
  timings only. If the app drops the Worker loop, its link tasks are aborted too,
  so the Host never sends work to a Worker nobody is running.
- **Placement (`scheduler.rs`):** `search_knowledge` and unknown tasks are
  Host-only. PDF, OCR and whisper rank by cores; model work by GPU, then measured
  tok/s. Analysis needs the Host's *embedding model* too, because it ranks
  evidence with it. A file that would take >20 s on the measured link stays off
  that Worker. A GPU-less Host isn't a candidate (MESH-14) but is still the last resort.
- **Delegator (`scheduler::delegator`):** the pinned device, else the fastest
  Worker at ≥2× the Host's tok/s. If the Host's speed is unmeasured, the Host
  plans.

## Delegated planning (phase 4)

The planner already talks to its model through a two-method trait,
`PlanSource` (`parse_intent`, `build_plan`). `RemotePlanner` is one more
`PlanSource` that sends those two calls to the delegator. `plan_turn` runs **on
the Host, unchanged** around it:

```text
Host: plan_turn ── parse_intent ──▶ delegator Worker (its model, maybe bigger)
                ◀── IntentResult ──
                ── build_plan ────▶
                ◀── Plan ─────────
      validate_plan (checks files exist, which they do only here) ── errors? ──▶ build_plan again
      complete_plan · fold · HITL park      all on the Host, as always
```

- Why only the model calls move: validation checks that attachments exist on
  disk, and they exist only on the Host. MESH-9 wanted exactly this split: the
  delegator plans, the Host decides.
- The delegator gets the prompt, the session history the local planner would
  see (MESH-5), the persona, and attachment *names and kinds*, with each path
  replaced by its name.
- If the delegator can't answer, the Host warns once and plans the rest of the
  turn itself. Stop works while waiting on it.
- With no mesh, or no delegator chosen, `run_turn_with_mesh` makes the same
  `plan_turn(engine, …)` call as before.

## In the app (phase 5)

| Piece | File | What it does |
|---|---|---|
| Header button | `src/App.tsx` | "Mesh" pill on the right of the top bar. Its dot is grey when off, green with a role, and amber when a device is waiting for consent ("· approve?"). |
| Mesh panel | `src/components/MeshSidebar.tsx` (+ `.css`) | A drawer over the right edge, so the layout underneath never moves. **Off:** "Use nearby devices" or "Lend this device" (code, or address as a fallback), plus one-click Reconnect to remembered Hosts. **Host:** the code with a countdown, the consent card per new device, approved devices (GPU/cores/RAM/speed/link, Working/Idle, "Plans turns", pin, remove), this device's speed. **Worker:** Host name, Idle / "Running a step: parse pdf" (the type, never the content), and a thread slider. |
| Consent card | same | What the device will see (questions and conversation when planning; files and extracted text for its steps), what it never gets (memory, knowledge base, other chats), and "only allow devices you trust". Allow / Don't allow. |
| Worker lock | `src/components/PromptPanel.tsx` | Optional `lockedReason`: the prompt box is read-only with the reason above it. The backend refuses too (`mesh::refuse_if_worker`). |
| Commands | `src-tauri/src/mesh.rs` | `mesh_status`, `mesh_start_host`, `mesh_new_code`, `mesh_approve`, `mesh_remove`, `mesh_pin_delegator`, `mesh_join`, `mesh_reconnect`, `mesh_forget_host`, `mesh_set_threads`, `mesh_leave`. |
| Turn wiring | `src-tauri/src/lib.rs` | `submit_turn` / `resume_turn` use `run_turn_with_mesh` **only when this device hosts at least one approved Worker**. Otherwise it is the literal call from before the mesh. |

- **Off at every launch** (lead's choice). The Host listens on a port the OS
  picks, on all interfaces, only while hosting. Windows will show a firewall
  prompt the first time: allow it on private networks.
- **Speed:** each device measures its own decode speed with one short answer
  from its model (a few seconds on a CPU). A Host measures in the background,
  and plans its own turns until the measurement lands. A Worker measures
  before joining: "Measuring this device and connecting…".
- **Caps (lead's choice):** a Worker lends all cores but one, the app's own
  rule (`HardwareInfo::worker_threads`). The slider applies from the next step.
  With a GPU, "Lend the GPU" (on by default) can be unticked to run jobs
  CPU-only (`ResourceLimits::num_gpu = Some(0)`; `None`, Ollama's choice, for
  every normal turn). The Host ranked the Worker by the GPU it reported at
  join, so a withheld GPU still gets model steps, just slower.
- **Where each step ran:** `StepEvent::ToolFinished` has an optional `device`,
  shown in the audit panel as "parse_pdf · on gpu-box". It is omitted from the
  JSON when the step ran here, so turns without the mesh emit exactly the
  events they always did.
- **Status is polled:** every 1 s with the panel open, 3 s otherwise. Plain
  `invoke`, not `timed`, so the log isn't flooded.
- **Files:** `<app data>/mesh/identity.json`, `workers.json` (Host side),
  `hosts.json` (Worker side), `scratch/` (Worker temp folders).

## Phase 6: the two-device test

Needs two machines on the same Wi-Fi/LAN, both on this branch, both through
setup with Ollama running. Ideally A is weak (no GPU) and B is stronger.

1. **A:** Mesh → **Start hosting**. Expect a code like `K7Q-4M9` and a 10:00 countdown.
2. **B:** Mesh → **Join** with that code. Expect "Measuring this device and
   connecting…" for a few seconds, then "Working for A", and B's prompt box locked.
3. **A:** expect an amber dot and a consent card naming B. Click **Allow**.
   Expect B under "Devices helping", with its speed and link.
4. **A:** ask a question with a PDF attached. Expect the answer as usual. In A's
   log (`open_log_dir`): `submit_turn: running on the mesh`, `mesh: job sent`. On
   B, while it runs: "Running a step: parse pdf".
5. **Speed:** run the same question with the mesh off (Stop hosting) and on.
   Compare the turn times in the log (`turn phase` lines). Report both numbers.
6. **Failure:** start a long question on A, then turn B's Wi-Fi off. Expect an
   amber warning on A, and the turn still finishing on A.
7. **Privacy:** after a step on B, B's `<app data>/mesh/scratch/` is empty.
8. **Reconnect:** Leave on B, then **Reconnect** to A. No code is needed and no
   consent card appears.
9. **Discovery blocked:** if step 2 says no Host found, use "Enter its address"
   with the address A's panel shows.

## Security review (2026-10-31)

The questions asked: what can a device on the same Wi-Fi do *before* it is
trusted, and what can a paired device make the other side do? Seven
findings, all fixed, each with a test:

| # | Finding | Fix | Test |
|---|---|---|---|
| A | The 5-attempt limit was checked when a handshake *started* and counted when it *ended*, so 1,000 simultaneous connections got 1,000 guesses | Each attempt holds a unit of the budget from `begin` to `finish`; a drop guard spends it on every exit but a completed pairing (wrong code, timeout, disconnect) | `a_burst_of_simultaneous_guesses_gets_no_more_than_the_budget`, `simultaneous_attempts_cannot_exceed_the_budget` |
| B | tungstenite accepts 64 MB messages by default, and the first one is read before any authentication | `ws_config()`: max message and frame ≈ 64 KiB on both sides, since every real message is a small handshake message or one Noise chunk | `an_unauthenticated_device_cannot_send_a_huge_message` |
| C | A Worker ran any registered tool the Host named, so a *modified* Host could make it search the Worker's own knowledge base and send the passages back | The Worker refuses Host-only steps itself, using the same `scheduler::needs` table the Host uses | `a_worker_refuses_to_search_its_own_knowledge_base_for_a_host` |
| D | Unbounded handshakes in flight, each allowed 15 s | At most 16 at once; further connections are closed immediately | — (a bound, not behaviour) |
| E | A frame header announcing 256 MB reserved 256 MB before any data | The buffer grows as chunks arrive (≤1 MiB up front) | existing 3 MB frame test |
| F | The peer's self-chosen name was unbounded and could contain control characters | Trimmed, control characters dropped, 64 characters max | `a_hostile_device_name_is_trimmed` |
| G | Joining tried only the first Host discovered, so a tag collision or a fake advertisement blocked pairing | Discovery listens 1 s past the first match; `pair_any` / `reconnect_any` try each candidate | `the_worker_tries_every_candidate_until_one_pairs` |

What was checked and is fine as designed: a wrong code is only detectable
at the Host's read of the last Noise message, so abandoning earlier teaches
an attacker nothing. A fake Host can test one guess per Worker attempt, and
the Worker's user must start each one. A reconnect is accepted only for a
pinned key on both sides, and a device waiting for consent can't reconnect
around the consent screen. Names and capabilities reach the UI through
React, which escapes them.

Left as is, with the reason:
- A Worker sees its job in plain text (see above). That is inherent to
  running a model on it, and it is what the consent screen says.
- Anyone on the LAN can see that a TrustCore Host exists, plus its tag and
  device id (mDNS). Neither is enough to pair or reconnect.

## Decisions

Recorded so any session can defend them, or notice when a request contradicts
one. The lead decided these on 2026-10-31.

| ID | Decision | Rejected, and why | Revisit when |
|---|---|---|---|
| MESH-1 | Star topology. **Workers dial the Host** over a persistent, encrypted WebSocket | Host dials Workers: every Worker would need an open port | never, unless Workers must be reachable without a Host |
| MESH-2 | **LAN only** | internet relay: a live server, a monthly cost, breaks "offline" | a customer needs devices across sites and accepts a hosted rendezvous |
| MESH-3 | Job unit = **one plan step** | whole turn (one Worker sees everything); raw model calls (OCR/whisper still crush the Host) | profiling shows per-step round trips dominate |
| MESH-4 | Any device with the code may join. **Consent screen on the Host, once per pairing**, naming what it will see | per-job approval: a click per step, slower turns | a deployment needs per-job audit |
| MESH-5 | Delegated planning sends the **full history** (best follow-up plans) | summary-only, prompt-only | the consent screen tests badly, or a privacy review objects |
| MESH-6 | Raw attachments go **only to the step that uses them**, in a per-job temp dir wiped after | extract-locally-only: whisper/OCR stay on the weak Host | a file class must never leave (add a per-file toggle) |
| MESH-7 | Worker failure → **next Worker, then the Host** | fail and ask: interrupts the turn | — |
| MESH-8 | Workers **may run a bigger model** | same-model-only | structured output from a bigger model breaks the validators |
| MESH-9 | A delegator Worker **plans and assigns; the Host validates and relays every job** | delegator dispatches: it would see every job | — |
| MESH-10 | Delegator election is **automatic** with a **user pin**. A Worker plans if it decodes ≥2× the Host's tok/s, **or (revised 2026-11-01) runs a model ≥2× the Host's parameters at no less than the Host's speed**; biggest model wins, then fastest. Size is read from the Ollama tag (`llama3.2:3b` → 3B); an unknown size never wins on size | manual only; speed only (the first device test: a 3B Worker at 28 tok/s lost to a 0.5B Host measured at 25, so the 0.5B planned) | users pin constantly (the threshold is wrong), or a bigger model plans worse than the Host's |
| MESH-11 | **Parallel** ready steps, only with the mesh on | always sequential: wastes the Workers | — |
| MESH-12 | Worker UI shows status and job *type*, never content, with thread/GPU caps. **Worker mode is dedicated** | showing content to the Worker's owner | — |
| MESH-13 | **A 6-character code (`K7Q-4M9`), valid 10 min for any number of Workers, 5 failed attempts**, then remember the device key. Lead, 2026-10-31: short and alphanumeric so it is easy to read out | fixed code (a leak lets anyone in); fresh code per Worker (more clicks); 4 digits (10,000 values, too guessable); 9 characters (hard to read out) | guessing attempts show up in the logs |
| MESH-14 | Host with a usable GPU (`HardwareInfo::has_usable_gpu`, which includes Apple unified memory) is also a node. **Without one it offloads everything except Host-only steps** | — | — |
| MESH-15 | Labels: **Host / Worker** | Master/Slave | — |
| MESH-16 | **The delegator also writes the report** (role C), lead 2026-11-01: "plan and make workers do the heavy work". It gets the prompt, the conversation summary and every step's result, **never facility memory**. Any failure → the Host writes it, with memory, as before. Protocol bumped to 2 | Host always writes (the first device test: a 3B Worker's analysis was rewritten by the Host's 0.5B into half a sentence, 57 s at 1 thread); send memory too (the consent screen promises it stays) | reports lose context users expect from saved facts |

## What a Worker can see, plainly

A model can only run on text it can read. There is no practical way to run an
LLM on encrypted prompts on a CPU. So a Worker's *process* sees the job in
plain text while it runs it. The protections are: an encrypted, authenticated
link (nobody else on the Wi-Fi can read it or join); no disk writes and
metadata-only logs on the Worker; the job holds only what that step needs; and
the user approves each Worker on a screen that says this. Pair devices you
trust.

## Phases

| # | What | Status |
|---|---|---|
| 1 | Core: `StepJob`, `Dispatcher`, `run_job`, `execute_plan_mesh`, pipeline seam | **done**: 6 integration tests (`tests/mesh_executor.rs`) + 3 unit tests |
| 2 | Link: code → SPAKE2 → Noise, WebSocket frames, mDNS (`crates/workbench-mesh`) | **done**: 6 link tests (+1 multicast, run by hand) + unit tests |
| 3 | Scheduler, delegator rule, Host hub (`Dispatcher` over real links), Worker runtime | **done**: 4 end-to-end hub tests + scheduler unit tests. A Worker's model-call timings come back in `Done` and are logged on the Host as `mesh model call timing` (with the device) |
| 4 | Delegated planning (`RemotePlanner`, `PlanJob`/`Planned` frames) | **done**: 3 core tests (Worker plans while the Host validates and retries; fallback after one failure; Stop) + 1 end-to-end over a real link |
| 5 | App: Tauri commands, header button, Mesh panel, consent card, Worker lock, turn wiring, "on <device>" per step in the audit panel, Worker GPU switch | **done**: `tsc` clean, workspace tests green |
| 6 | Two-device test and benchmark | **todo, needs the lead's two devices**: [steps above](#phase-6-the-two-device-test) |
