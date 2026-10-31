import { useState } from "react";
import type { MeshDevice, MeshStatus } from "../types";
import {
  meshApprove,
  meshForgetHost,
  meshJoin,
  meshLeave,
  meshNewCode,
  meshPinDelegator,
  meshReconnect,
  meshRemove,
  meshSetLendGpu,
  meshSetThreads,
  meshStartHost,
} from "../lib/pipeline";
import "./MeshSidebar.css";

// The LAN mesh (docs/12-mesh.md). A Host keeps every chat, memory and the
// knowledge base, and sends single steps to nearby devices the user approved.
// A Worker lends its CPU/GPU and keeps nothing. Off at every launch.

function mmss(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

function speed(tok_s: number | null): string {
  return tok_s == null ? "speed not measured" : `${Math.round(tok_s)} tok/s`;
}

/** Shown before a device gets any work (MESH-4/5). Consent is a screen, not a policy line. */
function ConsentCard({ device, onAllow, onDeny, pending }: {
  device: MeshDevice;
  onAllow: () => void;
  onDeny: () => void;
  pending: boolean;
}) {
  return (
    <div className="mesh-consent" role="alertdialog" aria-label={`${device.name} wants to join`}>
      <h5>{device.name} wants to help this device</h5>
      <p>If you allow it, that device will see:</p>
      <ul>
        <li>your questions, and the conversation so far when it plans a turn</li>
        <li>the files and extracted text for each step it runs</li>
        <li>when it plans a turn, every step's results, so it can write the answer</li>
      </ul>
      <p>It never receives your saved memory, your knowledge base or your other chats, and it keeps nothing after a step.</p>
      <p className="mesh-muted">Only allow devices you trust.</p>
      <div className="mesh-row">
        <button className="run-btn" disabled={pending} onClick={onAllow}>Allow</button>
        <button className="mesh-secondary" disabled={pending} onClick={onDeny}>Don't allow</button>
      </div>
    </div>
  );
}

export function MeshSidebar({ status, onStatus, onClose }: {
  status: MeshStatus | null;
  onStatus: (s: MeshStatus) => void;
  onClose: () => void;
}) {
  const [code, setCode] = useState("");
  const [address, setAddress] = useState("");
  const [showAddress, setShowAddress] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function act(label: string, run: () => Promise<MeshStatus>) {
    setPending(label);
    setError(null);
    try {
      onStatus(await run());
    } catch (e) {
      setError(String(e));
    } finally {
      setPending(null);
    }
  }

  const host = status?.host ?? null;
  const worker = status?.worker ?? null;
  const waiting = host?.devices.filter((d) => !d.consented) ?? [];
  const approved = host?.devices.filter((d) => d.consented) ?? [];

  return (
    <aside className="mesh-sidebar" aria-label="Mesh">
      <div className="mesh-head">
        <h4 className="mesh-title">Mesh</h4>
        <button className="mesh-close" onClick={onClose} aria-label="Close mesh panel">✕</button>
      </div>

      {!status && <p className="mesh-muted">Loading…</p>}

      {status?.role === "off" && (
        <>
          <p className="mesh-note">
            Use stronger devices on this network for the heavy steps, or lend this one. Everything
            stays on your network; nothing goes to the internet.
          </p>

          <section className="mesh-card">
            <h5>Use nearby devices</h5>
            <p className="mesh-muted">
              This device keeps your chats, memory and knowledge base, and sends single steps to
              devices you approve.
            </p>
            <button className="run-btn" disabled={!!pending} onClick={() => act("host", meshStartHost)}>
              {pending === "host" ? "Starting…" : "Start hosting"}
            </button>
          </section>

          <section className="mesh-card">
            <h5>Lend this device</h5>
            <p className="mesh-muted">
              Enter the code shown on the other device. While it works, your own questions here are paused.
            </p>
            <form
              className="mesh-form"
              onSubmit={(e) => {
                e.preventDefault();
                act("join", () => meshJoin(code, showAddress ? address : null));
              }}
            >
              <input
                className="mesh-input mesh-code-input"
                value={code}
                onChange={(e) => setCode(e.target.value.toUpperCase())}
                placeholder="K7Q-4M9"
                maxLength={9}
                autoComplete="off"
                spellCheck={false}
                aria-label="Pairing code"
                required
              />
              {showAddress ? (
                <input
                  className="mesh-input"
                  value={address}
                  onChange={(e) => setAddress(e.target.value)}
                  placeholder="Host address, e.g. 192.168.1.20:52345"
                  aria-label="Host address"
                />
              ) : (
                <button type="button" className="mesh-link" onClick={() => setShowAddress(true)}>
                  Can't find the Host? Enter its address
                </button>
              )}
              <button type="submit" className="run-btn" disabled={!!pending || !code.trim()}>
                {pending === "join" ? "Measuring this device and connecting…" : "Join"}
              </button>
            </form>

            {status.known_hosts.length > 0 && (
              <ul className="mesh-list">
                {status.known_hosts.map((h) => (
                  <li key={h.device_id} className="mesh-item">
                    <span className="mesh-name">{h.name}</span>
                    <div className="mesh-row">
                      <button
                        className="mesh-secondary"
                        disabled={!!pending}
                        onClick={() => act(`re-${h.device_id}`, () => meshReconnect(h.device_id))}
                      >
                        {pending === `re-${h.device_id}` ? "Connecting…" : "Reconnect"}
                      </button>
                      <button className="mesh-link" disabled={!!pending} onClick={() => act("forget", () => meshForgetHost(h.device_id))}>
                        Forget
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </>
      )}

      {host && (
        <>
          <section className="mesh-card">
            <h5>Pairing code</h5>
            {host.code ? (
              <>
                <div className="mesh-code" aria-label="Pairing code">{host.code}</div>
                <p className="mesh-muted">
                  Type it on the other device · valid {mmss(host.code_seconds_left)} · works for any number of devices
                </p>
              </>
            ) : (
              <p className="mesh-muted">The code expired.</p>
            )}
            <button className="mesh-secondary" disabled={!!pending} onClick={() => act("code", meshNewCode)}>
              New code
            </button>
            {!host.discoverable && (
              <p className="mesh-warn">This network blocks device discovery. On the other device, choose "Enter its address".</p>
            )}
            {host.address && <p className="mesh-muted">This device's address: <code>{host.address}</code></p>}
          </section>

          {waiting.map((d) => (
            <ConsentCard
              key={d.device_id}
              device={d}
              pending={!!pending}
              onAllow={() => act("allow", () => meshApprove(d.device_id))}
              onDeny={() => act("deny", () => meshRemove(d.device_id))}
            />
          ))}

          <section className="mesh-card">
            <h5>Devices helping ({approved.length})</h5>
            {approved.length === 0 ? (
              <p className="mesh-muted">None yet. Until one joins, everything runs on this device as usual.</p>
            ) : (
              <ul className="mesh-list">
                {approved.map((d) => (
                  <li key={d.device_id} className="mesh-item">
                    <div className="mesh-row mesh-spread">
                      <span className="mesh-name">{d.name}</span>
                      <span className={`mesh-pill${d.busy ? " mesh-busy" : ""}`}>{d.busy ? "Working" : "Idle"}</span>
                    </div>
                    <p className="mesh-muted">
                      {d.gpu ? d.gpu_name || "GPU" : "CPU only"} · {d.cores} cores · {Math.round(d.ram_gb)} GB ·{" "}
                      {speed(d.tok_s)}
                      {d.link_mbps != null && ` · ${Math.round(d.link_mbps)} Mbps`}
                    </p>
                    <div className="mesh-row">
                      {host.delegator === d.device_id && <span className="mesh-pill mesh-planner">Plans turns</span>}
                      {host.pinned === d.device_id ? (
                        <button className="mesh-link" disabled={!!pending} onClick={() => act("pin", () => meshPinDelegator(null))}>
                          Let speed decide the planner
                        </button>
                      ) : (
                        <button className="mesh-link" disabled={!!pending || !d.llm_model} onClick={() => act("pin", () => meshPinDelegator(d.device_id))}>
                          Always plan here
                        </button>
                      )}
                      <button className="mesh-link mesh-danger" disabled={!!pending} onClick={() => act("remove", () => meshRemove(d.device_id))}>
                        Remove
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            )}
            <p className="mesh-muted">
              This device: {host.tok_s == null ? "measuring speed…" : speed(host.tok_s)}
              {host.delegator == null && " · plans its own turns"}
            </p>
          </section>

          <button className="mesh-secondary" disabled={!!pending} onClick={() => act("leave", meshLeave)}>
            Stop hosting
          </button>
        </>
      )}

      {worker && (
        <>
          <section className="mesh-card">
            <h5>Working for {worker.host_name}</h5>
            <p className="mesh-status-line">
              {worker.activity === "idle" ? "Idle, waiting for a step" : `Running a step: ${worker.activity.replace(/_/g, " ")}`}
            </p>
            <p className="mesh-muted">
              This device sees only the step it runs, and deletes its files as soon as the step ends.
            </p>
          </section>
          <section className="mesh-card">
            <h5>How much of this device to lend</h5>
            <label className="mesh-slider">
              <input
                type="range"
                min={1}
                max={worker.max_threads}
                value={worker.threads}
                disabled={!!pending || worker.max_threads <= 1}
                onChange={(e) => act("threads", () => meshSetThreads(Number(e.currentTarget.value)))}
              />
              <span>{worker.threads} of {worker.max_threads} CPU threads</span>
            </label>
            {worker.has_gpu && (
              <label className="mesh-check">
                <input
                  type="checkbox"
                  checked={worker.lend_gpu}
                  disabled={!!pending}
                  onChange={(e) => act("gpu", () => meshSetLendGpu(e.currentTarget.checked))}
                />
                Lend the GPU (off = CPU only, slower steps)
              </label>
            )}
            <p className="mesh-muted">One core always stays free for this device. Changes apply from the next step.</p>
          </section>
          <button className="mesh-secondary" disabled={!!pending} onClick={() => act("leave", meshLeave)}>
            Leave and use this device again
          </button>
        </>
      )}

      {error && <div className="mesh-error">{error}</div>}
    </aside>
  );
}
