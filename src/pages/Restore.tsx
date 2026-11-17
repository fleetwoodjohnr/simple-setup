import { Fragment, useEffect, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, FolderOpen } from "lucide-react";
import { api, errorMessage, isLocked } from "../api";
import { Compatibility, rowsFromPlan } from "../Compatibility";
import { useApp } from "../store";
import type { Conflict, ItemState, MethodKind, PlanItem, Progress, ReceiveEvent, RestorePlan, RestoreReport, VerifyReport } from "../types";
import { Badge, Empty, ErrorNote, Modal, ProgressBar, Spinner, StateBadge, TransferStatus, fmtBytes, platformLabel, targetLabel } from "../ui";
import { DestinationFacts, Steps } from "./CreateSnapshot";
import { SnapshotTree } from "../FolderTree";

/** The user's own install commands are never approved by choosing the item (resolve::CUSTOM_MARK). */
const CUSTOM_MARK = "your own install command";

const OUTCOME: Record<string, [string, "ok" | "warn" | "bad" | "neutral"]> = {
  installed: ["Installed", "ok"], restored: ["Restored", "ok"], partial: ["Partly restored", "warn"], failed: ["Failed", "bad"],
  manual: ["Needs manual action", "warn"], skipped: ["Skipped", "neutral"],
};

/** The groups of the restore preview, in the order things happen. */
const GROUPS = ["Installs from the snapshot", "Downloads and installs", "Needs your approval — tick to allow", "Settings and personal files", "Needs you", "Not possible on this computer — skipped"] as const;
function group(i: PlanItem): (typeof GROUPS)[number] {
  if (i.status === "unsupported") return GROUPS[5];
  if (i.status === "manual") return GROUPS[4];
  if (i.kind !== "installer") return GROUPS[3];
  if (i.verdict === "review" || i.channel === "source") return GROUPS[2];
  return i.channel === "online" ? GROUPS[1] : GROUPS[0];
}

/** Items started without asking: everything possible except what needs an approval. */
const byDefault = (i: PlanItem) => i.status !== "unsupported" && !(i.status === "ready" && (i.verdict === "review" || i.channel === "source"));

const STATE: Record<ItemState | "waiting", [string, "ok" | "warn" | "bad" | "neutral" | "info"]> = {
  waiting: ["Waiting", "neutral"], started: ["Working", "info"], done: ["Done", "ok"], failed: ["Failed", "bad"], skipped: ["Skipped", "neutral"],
};

export default function Restore() {
  const { restorePath, restoreAuto, set, settings, saveSettings } = useApp();
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
  const [scriptNote, setScriptNote] = useState<string | null>(null);
  /** Parts of folders left out: `<item id>/<path in it>`. */
  const [exclude, setExclude] = useState<string[]>([]);
  const [choosing, setChoosing] = useState<PlanItem | null>(null);
  /** How each app is installed, when chosen instead of the automatic choice. */
  const [prefer, setPrefer] = useState<Record<string, MethodKind>>({});
  useEffect(() => { setExclude([]); setPrefer({}); }, [restorePath]);
  const [unpacking, setUnpacking] = useState<Progress | null | false>(false);
  // A snapshot saved as one .tar.gz file: unpacked (and checked) into a folder first.
  const openPacked = async () => {
    const file = await open({ title: "Open a packed snapshot", filters: [{ name: "Packed snapshot", extensions: ["gz", "tgz"] }] });
    if (typeof file !== "string") return;
    const dest = await open({ directory: true, title: "Unpack it into which folder?" });
    if (typeof dest !== "string") return;
    setErr(null);
    setUnpacking(null);
    try {
      const p = await api.unpackSnapshot(file, dest, setUnpacking);
      set({ restorePath: p });
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setUnpacking(false);
    }
  };
  // The apps of this plan as a script for this computer that runs without Simple Setup.
  const saveScript = async () => {
    if (!restorePath || !plan) return;
    const windows = plan.this_device.platform.os === "windows";
    const to = await save({ title: "Save install script", defaultPath: windows ? "install-apps.ps1" : "install-apps.sh",
      filters: [windows ? { name: "PowerShell script", extensions: ["ps1"] } : { name: "Shell script", extensions: ["sh"] }] });
    if (!to) return;
    setErr(null);
    setScriptNote(null);
    try {
      await api.exportScript(restorePath, to, secret);
      setScriptNote(`Saved to ${to}. It installs the apps only (settings and files need Simple Setup); ways that need your approval are comments in it to review.`);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
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
    setChosen(pl.items.filter(byDefault).map((i) => i.id));
    return pl;
  };
  useEffect(() => {
    setVerify(null); setPlan(null); setReport(null); setErr(null); setSecret(null); setLocked(false);
    if (!restorePath) return;
    if (restoreAuto) {
      setAutoRun(true);
      set({ restoreAuto: false });
    }
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
      setReport(await api.restoreExecute(restorePath, { items, conflict, exclude, prefer, approvals: (plan?.items ?? []).filter((i) => items.includes(i.id)).map((i) => ({ item: i.id, considered: i.considered.filter((c) => !c.option.includes(CUSTOM_MARK)) })) }, secret, (p) => {
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
      void run(plan.items.filter((i) => i.status === "ready" && byDefault(i)).map((i) => i.id));
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
        <div><h1>Restore Snapshot</h1><p className="muted">Bring a snapshot onto this computer. Each app is matched to this computer first: the snapshot's installer if it fits, otherwise this system's own packages, Flatpak or a verified download. Nothing existing is overwritten.</p></div>
        <div className="row"><button className="btn" onClick={() => void choose()}>Open snapshot…</button>
          <button className="btn" onClick={() => void openPacked()}>Open packed snapshot…</button>
          <button className="btn" onClick={() => setReceiving(true)}>Receive from another computer…</button></div>
      </div>
      <Steps labels={["Requirements", "Review apps", "Confirm", "Restore", "Report"]}
        current={report ? 5 : busy === "restore" ? 4 : plan ? 3 : 1} />
      <ErrorNote error={err} />
      {unpacking !== false && <div aria-live="polite"><p><Spinner /> Unpacking and checking the snapshot…</p><TransferStatus progress={unpacking} label="Unpacking" /></div>}
      {!restorePath && <Empty icon={<FolderOpen size={20} />} title="No snapshot open">Open a snapshot folder from an external drive, or pick one on the Snapshots page.</Empty>}
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
              <p>Prepared for {plan.selection.targets.map((t) => targetLabel(t, plan.selection.linux_distro)).join(", ")}.</p>
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
          <Compatibility rows={rowsFromPlan(plan.items)} title="Compatibility with this computer"
            picker={{
              options: (r) => plan.items.find((i) => i.id === r.id)?.ways ?? [],
              value: (r) => prefer[r.id],
              onChange: (r, kind) => setPrefer((p) => { const n = { ...p }; if (kind) n[r.id] = kind; else delete n[r.id]; return n; }),
            }}
            caption={`Checked against ${plan.this_system.linux ? plan.this_system.linux.profile.name : plan.this_device.os_name} and its package lists. Choose “Why?” to see every option that was considered.`} />
          <details className="card"><summary><strong>What this computer offers</strong></summary><DestinationFacts info={plan.this_system} /></details>
          <h2 style={{ margin: "8px 0 0" }}>Restore plan</h2>
          <div className="row" role="group" aria-label="Choose items">
            <button className="btn" onClick={() => setChosen(plan.items.filter((i) => i.status !== "unsupported").map((i) => i.id))}>Select all</button>
            <button className="btn" onClick={() => setChosen([])}>Select none</button>
            <button className="btn" onClick={() => setChosen(plan.items.filter((i) => i.state === "ready_to_install" || i.state === "already_installed" || i.state === "alternative_available").map((i) => i.id))}>Only what needs nothing from you</button>
          </div>
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
                        <td><StateBadge state={i.state} /></td>
                        <td className="small">{i.reason}
                          {i.admin && i.status === "ready" && <div><Badge tone="warn">Needs administrator password</Badge></div>}
                          {i.conflicts > 0 && <div><Badge tone="warn">{i.conflicts} different file(s) already there</Badge> {i.conflict_examples.slice(0, 3).map((c) => <div key={c} className="mono">{c}</div>)}</div>}
                          {i.identical > 0 && <div className="muted">{i.identical} already identical — left as is</div>}</td>
                        <td className="small">{i.files ? `${i.files} · ${fmtBytes(i.bytes)}` : "—"}
                          {i.kind !== "installer" && i.files > 1 && <div><button className="btn link small" onClick={() => setChoosing(i)}>Choose contents…</button>
                            {exclude.some((x) => x.startsWith(`${i.id}/`)) && <Badge tone="info">{exclude.filter((x) => x.startsWith(`${i.id}/`)).length} left out</Badge>}</div>}</td>
                      </tr>
                    ))}
                  </tbody>
                );
              })}
            </table>
          </div>
          <details className="card">
            <summary><strong>Advanced options</strong> <span className="small muted">— where apps may come from, for this restore and later ones</span></summary>
            <div className="stack" style={{ marginTop: 10 }}>
              {settings && ([
                ["restore_online", "Install apps the snapshot has no installer for (needs the internet)"],
                ["prefer_flatpak", "Prefer Flatpak over the distribution's own packages"],
                ["allow_snap", "Allow snaps"],
                ["allow_downloads", "Allow verified downloads (checked against their published SHA-256)"],
                ["allow_unverified_downloads", "Also allow downloads that publish no checksum (each needs your approval)"],
                ["allow_source_builds", "Build apps from their source code when nothing else installs them"],
              ] as const).map(([key, label]) => (
                <label key={key} className="check small">
                  <input type="checkbox" checked={!!settings[key]} onChange={(e) => void saveSettings({ [key]: e.target.checked }).then(() => { if (restorePath) void loadPlan(restorePath, secret); }).catch((x) => setErr(errorMessage(x)))} />
                  {label}
                </label>
              ))}
              <p className="small muted" style={{ margin: 0 }}>Changing one plans again. They are the same switches as in Settings → Installation sources.</p>
            </div>
          </details>
          {conflicts > 0 && (
            <fieldset className="card stack">
              <legend><strong>When a different file already exists ({conflicts})</strong></legend>
              <label className="check"><input type="radio" name="conflict" checked={conflict === "skip"} onChange={() => setConflict("skip")} /> Keep the existing file and skip the snapshot's copy</label>
              <label className="check"><input type="radio" name="conflict" checked={conflict === "keep_both"} onChange={() => setConflict("keep_both")} /> Keep both: restore the snapshot's copy as “name (restored)”</label>
              <label className="check"><input type="radio" name="conflict" checked={conflict === "replace"} onChange={() => setConflict("replace")} /> Replace it with the snapshot's copy; the existing file is kept in the hidden folder .simple-setup-backup in your home folder</label>
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
          ) : (<>
            <div className="row">
              <button className="btn primary big" disabled={chosen.length === 0} onClick={() => setConfirm(true)}>Start Restore ({chosen.length} item{chosen.length === 1 ? "" : "s"})…</button>
              <button className="btn" onClick={() => void saveScript()} title="The apps only, as a script that runs without Simple Setup">Save as install script…</button>
              <span className="small muted">The plan above is the dry run: it shows how each app is installed, every destination and conflict, before anything is written. Items that need your approval start unticked.</span>
            </div>
            {scriptNote && <div className="notice ok small" role="status">{scriptNote}</div>}
          </>
          )}
        </>
      )}
      {choosing && restorePath && (
        <Modal open wide title={`Choose what to restore: ${choosing.label}`} onClose={() => setChoosing(null)}
          footer={<button className="btn primary" onClick={() => setChoosing(null)}>Done</button>}>
          <SnapshotTree load={() => api.restoreFiles(restorePath, secret, choosing.id)} prefix={choosing.id}
            excluded={exclude} onChange={setExclude} />
        </Modal>
      )}
      <Modal open={confirm} title="Restore now?" onClose={() => setConfirm(false)}
        footer={<><button className="btn" onClick={() => setConfirm(false)}>Cancel</button><button className="btn primary" onClick={() => void run()}>Restore</button></>}>
        <p>{ready.length} item(s) will be installed or copied onto this computer ({fmtBytes(ready.reduce((n, i) => n + i.bytes, 0))} from the snapshot).</p>
        {ready.some((i) => i.channel === "online") && <p className="small"><strong>Internet:</strong> {ready.filter((i) => i.channel === "online").map((i) => i.app).join(", ")} {ready.filter((i) => i.channel === "online").length === 1 ? "is" : "are"} installed with this computer's package managers or downloaded and checked against the published SHA-256.</p>}
        {ready.some((i) => i.channel === "source") && <p className="small"><strong>Built from source:</strong> {ready.filter((i) => i.channel === "source").map((i) => i.app).join(", ")} — the project's own build runs at the recorded commit. Untick it in the list to skip.</p>}
        {ready.some((i) => i.admin) && <p className="small"><strong>Administrator password:</strong> {ready.filter((i) => i.admin).map((i) => i.app).join(", ")} use the system package manager, so the system asks for your password once. Everything else runs as you.</p>}
        {selected.some((i) => i.status === "manual") && <p className="small">Items needing manual action are listed in the report with instructions.</p>}
        <p className="small muted">{conflict === "skip" ? "Existing different files are kept and skipped." : "Existing different files are kept; the snapshot's copies are added next to them."}</p>
      </Modal>
      {receiving && <ReceiveDialog onClose={() => setReceiving(false)} onReceived={(p, auto) => { setReceiving(false); setAutoRun(auto); set({ restorePath: p }); }} />}
    </div>
  );
}

/** Listens on the local network only while open; nothing is saved until the offer is accepted. */
export function ReceiveDialog({ onClose, onReceived }: { onClose: () => void; onReceived: (path: string, restore: boolean) => void }) {
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
  const close = () => { if (running) void api.cancelTransfer("receive"); onClose(); };
  return (
    <Modal open title="Receive from another computer" onClose={close}
      footer={running ? <button className="btn danger" onClick={close}>Stop receiving</button>
        : <><button className="btn" onClick={onClose}>Close</button><button className="btn primary" disabled={!dest} onClick={() => void start()}>Start receiving</button></>}>
      {!running ? (
        <div className="stack">
          <p className="small">This computer can be found on the local network only while this window is receiving. The sender must type the code shown here; with it, the sender can see what this computer is (system, distribution, package managers) to prepare apps for it. Nothing is saved until you accept what it offers.</p>
          <div className="row"><button className="btn" onClick={() => void pick()}>Choose a folder…</button></div>
          <label className="field"><span>Or type a folder path</span><input type="text" className="mono" value={dest ?? ""} onChange={(e) => setDest(e.target.value.trim() || null)} /></label>
          <label className="check"><input type="checkbox" checked={autoRestore} onChange={(e) => setAutoRestore(e.target.checked)} /> Restore automatically when it arrives</label>
          {autoRestore && <p className="small muted">Everything that can be restored here is, as soon as the snapshot is checked (an encrypted one waits for its password). Apps that need an administrator password still ask for it.</p>}
          <ErrorNote error={err} />
        </div>
      ) : (
        <div className="stack" aria-live="polite">
          {ready && !offer && (
            <>
              <p>On the other computer, choose <strong>Transfer to Another Computer</strong> (or Send… on a snapshot), pick this computer and enter:</p>
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
          {answered && <TransferStatus progress={progress} label="Receiving" />}
          {note && <p className="small">{note}</p>}
        </div>
      )}
    </Modal>
  );
}

function Report({ report, onRetry }: { report: RestoreReport; onRetry: () => void }) {
  const shown = report.items.filter((i) => i.outcome !== "not_selected");
  const count = (o: string[]) => shown.filter((i) => o.includes(i.outcome)).length;
  const failed = count(["failed", "partial"]);
  const [note, setNote] = useState<string | null>(null);
  const exportReport = async () => {
    if (!report.report_file) return;
    const to = await save({
      title: "Save the migration report",
      defaultPath: `${report.snapshot_name} restore report.txt`,
      filters: [{ name: "Text", extensions: ["txt"] }, { name: "JSON", extensions: ["json"] }],
    });
    if (!to) return;
    try {
      await api.exportReport(report.report_file, to);
      setNote(`Saved to ${to}`);
    } catch (e) {
      setNote(errorMessage(e));
    }
  };
  return (
    <section className="stack" aria-labelledby="rr">
      <div className="spread">
        <h2 id="rr" style={{ margin: 0 }}>Migration summary</h2>
        {report.report_file && <button className="btn" onClick={() => void exportReport()}><Download size={15} aria-hidden /> Export report…</button>}
      </div>
      {note && <p className="small" role="status">{note}</p>}
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
