import { Fragment, useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorMessage, isLocked } from "../api";
import { useApp } from "../store";
import type { Conflict, ItemState, PlanItem, Progress, ReceiveEvent, RestorePlan, RestoreReport, VerifyReport } from "../types";
import { Badge, ErrorNote, Modal, ProgressBar, Spinner, StatusBadge, fmtBytes, platformLabel } from "../ui";

const OUTCOME: Record<string, [string, "ok" | "warn" | "bad" | "neutral"]> = {
  installed: ["Installed", "ok"], restored: ["Restored", "ok"], partial: ["Partly restored", "warn"], failed: ["Failed", "bad"],
  manual: ["Needs manual action", "warn"], skipped: ["Skipped", "neutral"],
};

/** The groups of the restore preview, in the order things happen. */
const GROUPS = ["Installs from the snapshot", "Downloads and installs", "Builds from source code", "Settings and personal files", "Needs you", "Not possible on this computer — skipped"] as const;
function group(i: PlanItem): (typeof GROUPS)[number] {
  if (i.status === "unsupported") return GROUPS[5];
  if (i.status === "manual") return GROUPS[4];
  if (i.kind !== "installer") return GROUPS[3];
  return i.channel === "source" ? GROUPS[2] : i.channel === "online" ? GROUPS[1] : GROUPS[0];
}

const STATE: Record<ItemState | "waiting", [string, "ok" | "warn" | "bad" | "neutral" | "info"]> = {
  waiting: ["Waiting", "neutral"], started: ["Working", "info"], done: ["Done", "ok"], failed: ["Failed", "bad"], skipped: ["Skipped", "neutral"],
};

export default function Restore() {
  const { restorePath, set } = useApp();
  const [verify, setVerify] = useState<VerifyReport | null>(null);
  const [plan, setPlan] = useState<RestorePlan | null>(null);
  const [chosen, setChosen] = useState<string[]>([]);
  const [conflict, setConflict] = useState<Conflict>("skip");
  const [progress, setProgress] = useState<Progress | null>(null);
  const [states, setStates] = useState<Record<string, ItemState>>({});
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState<"verify" | "restore" | null>(null);
  const [report, setReport] = useState<RestoreReport | null>(null);
  const [confirm, setConfirm] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  /** Password or recovery key of an encrypted snapshot, kept only while it is open here. */
  const [secret, setSecret] = useState<string | null>(null);
  const [locked, setLocked] = useState(false);
  const [receiving, setReceiving] = useState(false);
  /** Restore as soon as the plan is ready (chosen in the Receive dialog). */
  const [autoRun, setAutoRun] = useState(false);

  const loadPlan = async (path: string, s: string | null) => {
    const pl = await api.restorePlan(path, s);
    setPlan(pl);
    setSecret(s);
    setLocked(false);
    setChosen(pl.items.filter((i) => i.status !== "unsupported").map((i) => i.id));
    return pl;
  };
  useEffect(() => {
    setVerify(null); setPlan(null); setReport(null); setErr(null); setSecret(null); setLocked(false);
    if (!restorePath) return;
    setBusy("verify");
    (async () => {
      try {
        setVerify(await api.verifySnapshot(restorePath, (p) => p.kind === "copy" && setProgress(p)));
        await loadPlan(restorePath, null);
      } catch (e) {
        if (isLocked(e)) setLocked(true);
        else setErr(errorMessage(e));
      } finally {
        setBusy(null);
      }
    })();
  }, [restorePath]);
  const unlock = async (s: string) => {
    if (!restorePath) return;
    setErr(null);
    try {
      await loadPlan(restorePath, s);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };

  const choose = async () => {
    const p = await open({ directory: true, title: "Open the snapshot folder (it contains simple-setup.json)" });
    if (typeof p === "string") set({ restorePath: p });
  };
  const run = async (items = chosen) => {
    if (!restorePath) return;
    setConfirm(false);
    setBusy("restore");
    setErr(null);
    setStates({});
    setLog([]);
    setProgress(null);
    const note = (m: string) => setLog((l) => [...l.slice(-199), m]);
    try {
      setReport(await api.restoreExecute(restorePath, { items, conflict }, secret, (p) => {
        if (p.kind === "item") {
          setStates((s) => ({ ...s, [p.id]: p.state }));
          if (p.state !== "started") note(`${p.state === "done" ? "✓" : p.state === "failed" ? "✗" : "–"} ${p.message}`);
        } else if (p.kind === "note" || p.kind === "warning") note(p.message);
        else setProgress(p);
      }));
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  useEffect(() => {
    if (autoRun && plan && !busy && !report) {
      setAutoRun(false);
      void run(plan.items.filter((i) => i.status === "ready").map((i) => i.id));
    }
  }, [autoRun, plan, busy, report]);
  const retry = async () => {
    if (!restorePath || !report) return;
    const failed = report.items.filter((i) => i.outcome === "failed" || i.outcome === "partial").map((i) => i.id);
    setReport(null);
    const pl = await loadPlan(restorePath, secret);
    setChosen(pl.items.filter((i) => failed.includes(i.id) && i.status !== "unsupported").map((i) => i.id));
  };

  const selected = plan?.items.filter((i) => chosen.includes(i.id)) ?? [];
  const ready = selected.filter((i) => i.status === "ready");
  const conflicts = selected.reduce((n, i) => n + i.conflicts, 0);
  const finished = Object.values(states).filter((s) => s !== "started").length;

  return (
    <div className="stack">
      <div className="page-head">
        <div><h1>Restore</h1><p className="muted">Bring a snapshot onto this computer. Apps the snapshot holds no installer for are installed with this computer's own package managers or verified downloads. Nothing existing is overwritten.</p></div>
        <div className="row"><button className="btn" onClick={() => void choose()}>Open snapshot…</button>
          <button className="btn" onClick={() => setReceiving(true)}>Receive from another computer…</button></div>
      </div>
      <ErrorNote error={err} />
      {!restorePath && <p className="muted">Open a snapshot folder from an external drive, or pick one on the Snapshots page.</p>}
      <form className="row" onSubmit={(e) => { e.preventDefault(); const v = new FormData(e.currentTarget).get("path"); if (typeof v === "string" && v.trim()) set({ restorePath: v.trim() }); }}>
        <label className="field" style={{ flex: 1 }}><span>Or type the snapshot folder path</span><input name="path" type="text" className="mono" defaultValue={restorePath ?? ""} /></label>
        <button className="btn" type="submit" style={{ alignSelf: "flex-end" }}>Open</button>
      </form>
      {restorePath && <p className="small">Snapshot: <span className="mono">{restorePath}</span></p>}
      {busy === "verify" && (
        <div aria-live="polite"><p><Spinner /> Checking the snapshot's integrity…</p>{progress?.kind === "copy" && <ProgressBar value={progress.done_files} max={progress.total_files} label="Integrity check" />}</div>
      )}
      {verify && !verify.ok && (
        <div className="notice bad" role="alert">This snapshot is damaged: {verify.missing.length} missing and {verify.corrupt.length} changed file(s). Damaged files will not be restored; everything else can be.</div>
      )}
      {verify?.ok && <div className="notice ok small">Integrity check passed: {verify.checked} files match their checksums.</div>}
      {locked && (
        <form className="card stack" onSubmit={(e) => { e.preventDefault(); const v = new FormData(e.currentTarget).get("secret"); if (typeof v === "string" && v) void unlock(v); }}>
          <strong>This snapshot is encrypted</strong>
          <label className="field"><span>Password or recovery key</span><input name="secret" type="password" autoComplete="current-password" autoFocus /></label>
          <div><button className="btn primary" type="submit">Unlock</button></div>
        </form>
      )}

      {report ? <Report report={report} onRetry={() => void retry()} /> : plan && (
        <>
          <div className="grid two">
            <section className="card small" aria-labelledby="from">
              <h2 id="from">Snapshot</h2>
              <p><strong>“{plan.snapshot_name}”</strong> from {plan.source.hostname} ({platformLabel(plan.source.platform)}), made {new Date(plan.created_at).toLocaleString()}.</p>
              <p className="muted">Needs {fmtBytes(plan.required_bytes)} here; {fmtBytes(plan.free_bytes)} free.</p>
            </section>
            <section className="card small" aria-labelledby="here">
              <h2 id="here">This computer</h2>
              <dl className="kv">
                <dt>System</dt><dd>{plan.this_device.os_name} ({platformLabel(plan.this_device.platform)})</dd>
                <dt>Installs with</dt><dd><div className="chips">{plan.this_system.tools.filter((t) => t !== "curl").map((t) => <Badge key={t}>{t}</Badge>)}{plan.this_system.tools.length === 0 && <span className="muted">no package manager found</span>}</div></dd>
                <dt>Internet</dt><dd>{plan.online ? "Connected" : plan.this_system.online ? "Not used (turned off in Settings)" : "Not connected"}</dd>
                <dt>Administrator</dt><dd>{plan.needs_admin ? "Your password is asked for once, when the restore starts" : "Not needed"}</dd>
                <dt>Offline</dt><dd>{plan.offline_possible ? <Badge tone="ok">Everything chosen works offline</Badge> : <Badge tone="info">Some apps need the internet</Badge>}</dd>
              </dl>
            </section>
          </div>
          {plan.warnings.map((w) => <div key={w} className="notice warn small">{w}</div>)}
          <div className="table-wrap">
            <table>
              <caption className="sr-only">Restore plan</caption>
              <thead><tr><th><span className="sr-only">Restore</span></th><th>Item</th><th>Status</th><th>What will happen</th><th>Files</th></tr></thead>
              {GROUPS.map((g) => {
                const rows = plan.items.filter((i) => group(i) === g);
                if (!rows.length) return null;
                return (
                  <tbody key={g}>
                    <tr><th colSpan={5} scope="rowgroup" className="group">{g} ({rows.length})</th></tr>
                    {rows.map((i) => (
                      <tr key={i.id}>
                        <td><input type="checkbox" aria-label={`Restore ${i.label}`} checked={chosen.includes(i.id)} disabled={i.status === "unsupported"}
                          onChange={() => setChosen(chosen.includes(i.id) ? chosen.filter((x) => x !== i.id) : [...chosen, i.id])} /></td>
                        <td>{i.label}{i.method && i.status === "ready" && i.channel !== "snapshot" && <div className="muted small">{i.method}</div>}</td>
                        <td><StatusBadge status={i.status} /></td>
                        <td className="small">{i.reason}
                          {i.admin && i.status === "ready" && <div><Badge tone="warn">Needs administrator password</Badge></div>}
                          {i.conflicts > 0 && <div><Badge tone="warn">{i.conflicts} different file(s) already there</Badge> {i.conflict_examples.slice(0, 3).map((c) => <div key={c} className="mono">{c}</div>)}</div>}
                          {i.identical > 0 && <div className="muted">{i.identical} already identical — left as is</div>}</td>
                        <td className="small">{i.files ? `${i.files} · ${fmtBytes(i.bytes)}` : "—"}</td>
                      </tr>
                    ))}
                  </tbody>
                );
              })}
            </table>
          </div>
          {conflicts > 0 && (
            <fieldset className="card stack">
              <legend><strong>When a different file already exists ({conflicts})</strong></legend>
              <label className="check"><input type="radio" name="conflict" checked={conflict === "skip"} onChange={() => setConflict("skip")} /> Keep the existing file and skip the snapshot's copy</label>
              <label className="check"><input type="radio" name="conflict" checked={conflict === "keep_both"} onChange={() => setConflict("keep_both")} /> Keep both: restore the snapshot's copy as “name (restored)”</label>
            </fieldset>
          )}
          {busy === "restore" ? (
            <section className="card stack" aria-live="polite" aria-labelledby="running">
              <div className="spread"><h2 id="running"><Spinner /> Restoring…</h2><button className="btn danger" onClick={() => void api.cancelRestore()}>Cancel</button></div>
              <ProgressBar value={finished} max={selected.length || 1} label="Items finished" />
              {progress?.kind === "copy" && progress.current && <p className="small mono muted">{progress.current}</p>}
              <ul className="items small">
                {selected.map((i) => {
                  const [label, tone] = STATE[states[i.id] ?? "waiting"];
                  return <li key={i.id} className={states[i.id] === "started" ? "working" : ""}><Badge tone={tone}>{label}</Badge> {i.label}</li>;
                })}
              </ul>
              <details><summary className="small">Details ({log.length})</summary><pre className="log mono">{log.join("\n")}</pre></details>
            </section>
          ) : (
            <div className="row">
              <button className="btn primary big" disabled={chosen.length === 0} onClick={() => setConfirm(true)}>Restore {chosen.length} item(s)…</button>
              <span className="small muted">The table above is the dry run: it shows how each app is installed, every destination and conflict, before anything is written.</span>
            </div>
          )}
        </>
      )}
      <Modal open={confirm} title="Restore now?" onClose={() => setConfirm(false)}>
        <p>{ready.length} item(s) will be installed or copied onto this computer ({fmtBytes(ready.reduce((n, i) => n + i.bytes, 0))} from the snapshot).</p>
        {ready.some((i) => i.channel === "online") && <p className="small"><strong>Internet:</strong> {ready.filter((i) => i.channel === "online").map((i) => i.app).join(", ")} {ready.filter((i) => i.channel === "online").length === 1 ? "is" : "are"} installed with this computer's package managers or downloaded and checked against the published SHA-256.</p>}
        {ready.some((i) => i.channel === "source") && <p className="small"><strong>Built from source:</strong> {ready.filter((i) => i.channel === "source").map((i) => i.app).join(", ")} — the project's own build runs at the recorded commit. Untick it in the list to skip.</p>}
        {ready.some((i) => i.admin) && <p className="small"><strong>Administrator password:</strong> {ready.filter((i) => i.admin).map((i) => i.app).join(", ")} use the system package manager, so the system asks for your password once. Everything else runs as you.</p>}
        {selected.some((i) => i.status === "manual") && <p className="small">Items needing manual action are listed in the report with instructions.</p>}
        <p className="small muted">{conflict === "skip" ? "Existing different files are kept and skipped." : "Existing different files are kept; the snapshot's copies are added next to them."}</p>
        <div className="row"><button className="btn primary" onClick={() => void run()}>Restore</button><button className="btn" onClick={() => setConfirm(false)}>Cancel</button></div>
      </Modal>
      {receiving && <ReceiveDialog onClose={() => setReceiving(false)} onReceived={(p, auto) => { setReceiving(false); setAutoRun(auto); set({ restorePath: p }); }} />}
    </div>
  );
}

/** Listens on the local network only while open; nothing is saved until the offer is accepted. */
function ReceiveDialog({ onClose, onReceived }: { onClose: () => void; onReceived: (path: string, restore: boolean) => void }) {
  const { settings } = useApp();
  const [dest, setDest] = useState<string | null>(settings?.receive_folder ?? null);
  const [autoRestore, setAutoRestore] = useState(false);
  const [ready, setReady] = useState<Extract<ReceiveEvent, { kind: "ready" }> | null>(null);
  const [offer, setOffer] = useState<Extract<ReceiveEvent, { kind: "offer" }> | null>(null);
  const [answered, setAnswered] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const pick = async () => {
    const p = await open({ directory: true, title: "Folder to save the received snapshot in" });
    if (typeof p === "string") setDest(p);
  };
  const start = async () => {
    if (!dest) return;
    setRunning(true);
    setErr(null);
    try {
      const path = await api.receiveSnapshot(dest, (e) => {
        if (e.kind === "ready") setReady(e);
        else if (e.kind === "offer") { setOffer(e); setAnswered(false); }
        else if (e.progress.kind === "note") setNote(e.progress.message);
        else if (e.progress.kind === "copy") setProgress(e.progress);
      });
      onReceived(path, autoRestore);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setRunning(false);
      setReady(null);
      setOffer(null);
    }
  };
  const answer = (accept: boolean) => { setAnswered(true); void api.answerOffer(accept); };
  const close = () => { if (running) void api.cancelTransfer(); onClose(); };
  return (
    <Modal open title="Receive from another computer" onClose={close}>
      {!running ? (
        <div className="stack">
          <p className="small">This computer can be found on the local network only while this window is receiving. The sender must type the code shown here, and nothing is saved until you accept what it offers.</p>
          <div className="row"><button className="btn" onClick={() => void pick()}>Choose a folder…</button></div>
          <label className="field"><span>Or type a folder path</span><input type="text" className="mono" value={dest ?? ""} onChange={(e) => setDest(e.target.value.trim() || null)} /></label>
          <label className="check"><input type="checkbox" checked={autoRestore} onChange={(e) => setAutoRestore(e.target.checked)} /> Restore automatically when it arrives</label>
          {autoRestore && <p className="small muted">Everything that can be restored here is, as soon as the snapshot is checked (an encrypted one waits for its password). Apps that need an administrator password still ask for it.</p>}
          <ErrorNote error={err} />
          <div className="row"><button className="btn primary" disabled={!dest} onClick={() => void start()}>Start receiving</button><button className="btn" onClick={onClose}>Close</button></div>
        </div>
      ) : (
        <div className="stack" aria-live="polite">
          {ready && !offer && (
            <>
              <p>On the other computer, open Snapshots, choose <strong>Send…</strong>, pick this computer and enter:</p>
              <p className="mono" style={{ fontSize: "2rem", letterSpacing: "0.1em" }} aria-label={`Pairing code ${ready.code.split("").join(" ")}`}>{ready.code}</p>
              {ready.address && <p className="small">If this computer is not listed there, type its address: <span className="mono">{ready.address}</span></p>}
              {ready.announce_error && <p className="small muted">Not visible to automatic discovery ({ready.announce_error}).</p>}
              <p className="small muted"><Spinner /> Waiting for the other computer…</p>
            </>
          )}
          {offer && !answered && (
            <div className="card stack" role="alertdialog" aria-label="Incoming snapshot">
              <strong>{offer.from.hostname} wants to send “{offer.name}”</strong>
              <span className="small">{offer.from.os_name} ({platformLabel(offer.from.platform)}) · {offer.files} files, {fmtBytes(offer.bytes)}{offer.encrypted ? " · encrypted" : ""} · made for {offer.targets.map(platformLabel).join(", ")}</span>
              <div className="row"><button className="btn primary" onClick={() => answer(true)}>Accept</button><button className="btn" onClick={() => answer(false)}>Decline</button></div>
            </div>
          )}
          {answered && progress?.kind === "copy" && <><ProgressBar value={progress.done_bytes} max={progress.total_bytes || 1} label="Receiving" /><p className="small mono muted">{progress.current}</p></>}
          {note && <p className="small">{note}</p>}
          <div><button className="btn danger" onClick={close}>Stop receiving</button></div>
        </div>
      )}
    </Modal>
  );
}

function Report({ report, onRetry }: { report: RestoreReport; onRetry: () => void }) {
  const shown = report.items.filter((i) => i.outcome !== "not_selected");
  const count = (o: string[]) => shown.filter((i) => o.includes(i.outcome)).length;
  const failed = count(["failed", "partial"]);
  return (
    <section className="stack" aria-labelledby="rr">
      <h2 id="rr">Restore report</h2>
      <div className="row">
        <Badge tone="ok">{count(["installed", "restored"])} done</Badge>
        {failed > 0 && <Badge tone="bad">{failed} failed</Badge>}
        {count(["manual"]) > 0 && <Badge tone="warn">{count(["manual"])} need you</Badge>}
        {count(["skipped"]) > 0 && <Badge>{count(["skipped"])} skipped</Badge>}
        {failed > 0 && <button className="btn" onClick={onRetry}>Retry failed</button>}
      </div>
      {report.cancelled && <div className="notice warn small">The restore was cancelled. Finished items are kept; restore again to continue with the rest.</div>}
      {report.restart_required && <div className="notice info small">Restart the computer to finish installing. Restoring again afterwards completes anything left, and skips what is already done.</div>}
      <div className="table-wrap">
        <table>
          <thead><tr><th>Item</th><th>Result</th><th>Details</th></tr></thead>
          <tbody>
            {shown.map((i) => {
              const [label, tone] = OUTCOME[i.outcome] ?? [i.outcome, "neutral"];
              return (
                <tr key={i.id}>
                  <td>{i.label}</td>
                  <td><Badge tone={tone}>{label}</Badge></td>
                  <td className="small">{i.message}
                    {i.failed.filter((f) => f.error !== i.message).slice(0, 20).map((f) => <div key={f.path} className="mono">{f.path}: {f.error}</div>)}
                    {i.details.length > 0 && i.outcome !== "failed" && (
                      <details><summary>How</summary>{i.details.map((d, n) => <Fragment key={n}><span>{d}</span><br /></Fragment>)}</details>
                    )}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {report.manual_steps.length > 0 && (
        <div className="card"><h3>Manual follow-up</h3><ol className="small">{report.manual_steps.map((s) => <li key={s}>{s}</li>)}</ol></div>
      )}
      {report.report_file && <p className="small muted">Saved to <span className="mono">{report.report_file}</span></p>}
    </section>
  );
}
