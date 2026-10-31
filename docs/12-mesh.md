# 12 · The mesh: borrowing nearby machines without giving them your data

> Status: **Phases 1–3 of 6 landed**: the core scheduler, the encrypted link,
> and the Host hub plus Worker runtime, all tested over loopback. Not yet wired
> into the app: no UI, no Tauri commands, no delegated planning. See [Phases](#phases).

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
  plans. Wired into planning in phase 4.

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
| MESH-10 | Delegator election is **automatic** (≥2× Host decode tok/s) with a **user pin** | manual only | users pin constantly (the threshold is wrong) |
| MESH-11 | **Parallel** ready steps, only with the mesh on | always sequential: wastes the Workers | — |
| MESH-12 | Worker UI shows status and job *type*, never content, with thread/GPU caps. **Worker mode is dedicated** | showing content to the Worker's owner | — |
| MESH-13 | **A 6-character code (`K7Q-4M9`), valid 10 min for any number of Workers, 5 failed attempts**, then remember the device key. Lead, 2026-10-31: short and alphanumeric so it is easy to read out | fixed code (a leak lets anyone in); fresh code per Worker (more clicks); 4 digits (10,000 values, too guessable); 9 characters (hard to read out) | guessing attempts show up in the logs |
| MESH-14 | Host with a usable GPU (`HardwareInfo::has_usable_gpu`, which includes Apple unified memory) is also a node. **Without one it offloads everything except Host-only steps** | — | — |
| MESH-15 | Labels: **Host / Worker** | Master/Slave | — |

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
| 3 | Scheduler, delegator rule, Host hub (`Dispatcher` over real links), Worker runtime | **done**: 4 end-to-end hub tests + scheduler unit tests. Open: Worker `CallTiming`s don't reach the Host's turn breakdown; GPU cap (`num_gpu`) comes with the UI caps in phase 5 |
| 4 | Delegated planning | todo |
| 5 | UI: header toggle, sidebar, consent, Worker mode, `served_by` in the audit panel | todo |
| 6 | Two-machine LAN benchmark | todo |
