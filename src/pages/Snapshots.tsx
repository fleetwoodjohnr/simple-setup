import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorMessage } from "../api";
import { reconcile } from "../selection";
import { useApp } from "../store";
import type { Peer, Progress, SendReport, SnapshotItem, Summary, VerifyReport } from "../types";
import { Badge, ErrorNote, Modal, ProgressBar, fmtBytes, platformLabel } from "../ui";

export default function Snapshots() {
  const { go, set, inventory } = useApp();
  const [items, setItems] = useState<SnapshotItem[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [inspect, setInspect] = useState<Summary | null>(null);
  const [verifying, setVerifying] = useState<{ path: string; progress?: Progress; result?: VerifyReport } | null>(null);
  const [deleting, setDeleting] = useState<Summary | null>(null);
  const [confirmText, setConfirmText] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const [sending, setSending] = useState<Summary | null>(null);
  /** An encrypted snapshot waiting for its password before its choices can be used. */
  const [unlocking, setUnlocking] = useState<{ summary: Summary; then: "reuse" | "update" } | null>(null);
  const reload = () => api.listSnapshots().then(setItems, (e) => setErr(errorMessage(e)));
  useEffect(() => { void reload(); }, []);

  const openFolder = async () => {
    const p = await open({ directory: true, title: "Open a snapshot folder" });
    if (typeof p !== "string") return;
    try {
      await api.openSnapshot(p);
      await reload();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const verify = async (path: string) => {
    setVerifying({ path });
    try {
      const result = await api.verifySnapshot(path, (p) => setVerifying((v) => (v ? { ...v, progress: p.kind === "copy" ? p : v.progress } : v)));
      setVerifying({ path, result });
      void reload();
    } catch (e) {
      setVerifying(null);
      setErr(errorMessage(e));
    }
  };
  const reuse = (s: Summary) => {
    const r = reconcile(s.selection, inventory?.apps ?? []);
    set({ selection: r.selection, step: 1, base: null });
    setNote(r.missing.length ? `Some apps from that snapshot are not on this computer: ${r.missing.join(", ")}` : null);
    go("create");
  };
  /** A new snapshot of the same choices next to this one, taking unchanged files from it. */
  const update = (s: Summary, secret: string | null) => {
    const r = reconcile(s.selection, inventory?.apps ?? []);
    const today = new Date().toISOString().slice(0, 10);
    const parent = s.path.replace(/[\\/][^\\/]+[\\/]?$/, "");
    set({
      selection: r.selection, step: 4, base: { path: s.path, name: s.name }, name: `${s.name} (update ${today})`,
      destination: parent, encrypt: !!secret, password: secret ?? "", password2: secret ?? "",
    });
    go("create");
  };
  const withChoices = (s: Summary, then: "reuse" | "update") => {
    if (s.encrypted) setUnlocking({ summary: s, then });
    else if (then === "reuse") reuse(s);
    else update(s, null);
  };
  const unlock = async (secret: string) => {
    if (!unlocking) return;
    try {
      const full = await api.unlockSummary(unlocking.summary.path, secret);
      const then = unlocking.then;
      setUnlocking(null);
      if (then === "reuse") reuse(full);
      else update(full, secret);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const remove = async () => {
    if (!deleting) return;
    try {
      await api.deleteSnapshot(deleting.path, confirmText);
      setDeleting(null);
      setConfirmText("");
      await reload();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };

  return (
    <div className="stack">
      <div className="page-head">
        <div><h1>Snapshots</h1><p className="muted">Snapshots made on this computer or opened from a drive.</p></div>
        <div className="row"><button className="btn" onClick={() => void openFolder()}>Open snapshot folder…</button><button className="btn primary" onClick={() => go("create")}>Create Snapshot</button></div>
      </div>
      <ErrorNote error={err} />
      {note && <div className="notice warn small">{note}</div>}
      {items === null ? <p className="muted">Loading…</p> : items.length === 0 ? <p className="muted">No snapshots yet.</p> : (
        <div className="table-wrap">
          <table>
            <thead><tr><th>Name</th><th>Created</th><th>From</th><th>For</th><th>Size</th><th>Integrity</th><th>Offline</th><th>Actions</th></tr></thead>
            <tbody>
              {items.map(({ reference: r, summary: s, error }) => (
                <tr key={r.path}>
                  <td>{r.name}<div className="muted small mono">{r.path}</div></td>
                  <td className="small">{new Date(r.created_at).toLocaleString()}</td>
                  <td className="small">{s ? `${s.source.hostname} (${platformLabel(s.source.platform)})` : "—"}</td>
                  <td className="small">{s ? s.targets.map(platformLabel).join(", ") : "—"}</td>
                  <td>{s ? fmtBytes(s.bytes) : "—"}</td>
                  <td>{!s ? <Badge>{error ? "Not connected" : "—"}</Badge> : r.last_verify_ok === false ? <Badge tone="bad">Damaged</Badge> : r.last_verified ? <Badge tone="ok" title={`Checked ${new Date(r.last_verified).toLocaleString()}`}>Verified</Badge> : <Badge>Not checked</Badge>}</td>
                  <td>{s && (s.readiness.every((x) => x.offline_ready) ? <Badge tone="ok">Complete</Badge> : <Badge tone="warn">Partial</Badge>)}</td>
                  <td>
                    {s ? (
                      <div className="row">
                        <button className="btn link" onClick={() => setInspect(s)}>Inspect</button>
                        <button className="btn link" onClick={() => void verify(r.path)}>Verify</button>
                        <button className="btn link" onClick={() => { set({ restorePath: r.path }); go("restore"); }}>Restore</button>
                        <button className="btn link" onClick={() => withChoices(s, "reuse")}>Reuse choices</button>
                        <button className="btn link" onClick={() => withChoices(s, "update")} title="Make an updated snapshot next to this one; unchanged files are taken from it">Update…</button>
                        <button className="btn link" onClick={() => setSending(s)}>Send…</button>
                        <button className="btn link" style={{ color: "var(--bad)" }} onClick={() => setDeleting(s)}>Delete</button>
                      </div>
                    ) : <span className="small muted">{error}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="muted small">“Update…” makes a new snapshot next to an existing one: files that have not changed are taken from it (hard links where the drive allows), so only changes are copied.</p>

      <Modal open={!!inspect} title={inspect?.name ?? ""} onClose={() => setInspect(null)}>
        {inspect && (
          <>
            <p className="small">Created {new Date(inspect.created_at).toLocaleString()} on {inspect.source.hostname} — {inspect.source.os_name} ({platformLabel(inspect.source.platform)})</p>
            <p className="small">{inspect.files.toLocaleString()} files, {fmtBytes(inspect.bytes)}{inspect.installers > 0 && ` · offline installers for ${inspect.installers} app(s)`}</p>
            {inspect.base && <p className="small">An update of “{inspect.base}”.</p>}
            {inspect.encrypted ? <p className="notice small">Encrypted: its apps, folders and files are shown only after entering its password (Restore or Update).</p> : (
              <>
                <h3>Applications</h3><ul className="plain small">{inspect.apps.map((a) => <li key={a}>{a}</li>)}</ul>
                <h3>Folders</h3><ul className="plain small">{inspect.folders.map((f) => <li key={f} className="mono">{f}</li>)}</ul>
              </>
            )}
            {inspect.readiness.map((r) => (
              <div key={`${r.target.os}-${r.target.arch}`} className={`notice small ${r.offline_ready ? "ok" : "warn"}`}>
                <strong>{platformLabel(r.target)}: {r.offline_ready ? "offline-ready" : "not fully offline-ready"}</strong>
                <ul className="plain">{r.reasons.map((x) => <li key={x}>{x}</li>)}</ul>
              </div>
            ))}
            <button className="btn" onClick={() => setInspect(null)}>Close</button>
          </>
        )}
      </Modal>

      <Modal open={!!verifying} title="Verify snapshot" onClose={() => setVerifying(null)}>
        {verifying && !verifying.result && (
          <div aria-live="polite">
            <p>Checking every file against its checksum…</p>
            {verifying.progress?.kind === "copy" && <ProgressBar value={verifying.progress.done_files} max={verifying.progress.total_files} label="Verification progress" />}
          </div>
        )}
        {verifying?.result && (
          <>
            <div className={`notice ${verifying.result.ok ? "ok" : "bad"}`} role="status">{verifying.result.ok ? `Intact: ${verifying.result.checked} files match.` : "This snapshot is damaged."}</div>
            {[...verifying.result.missing.map((m) => `Missing: ${m}`), ...verifying.result.corrupt.map((m) => `Changed: ${m}`)].slice(0, 50).map((m) => <div key={m} className="small mono">{m}</div>)}
            <button className="btn" onClick={() => setVerifying(null)}>Close</button>
          </>
        )}
      </Modal>

      {sending && <SendDialog summary={sending} onClose={() => setSending(null)} />}
      <Modal open={!!unlocking} title="This snapshot is encrypted" onClose={() => setUnlocking(null)}>
        <form className="stack" onSubmit={(e) => { e.preventDefault(); const v = new FormData(e.currentTarget).get("secret"); if (typeof v === "string" && v) void unlock(v); }}>
          <p className="small">Enter its password to use its choices.{unlocking?.then === "update" ? " The update is encrypted with the same password." : ""}</p>
          <label className="field"><span>Password</span><input name="secret" type="password" autoComplete="current-password" autoFocus /></label>
          <div className="row"><button className="btn primary" type="submit">Continue</button><button className="btn" type="button" onClick={() => setUnlocking(null)}>Cancel</button></div>
        </form>
      </Modal>

      <Modal open={!!deleting} title="Delete snapshot?" onClose={() => { setDeleting(null); setConfirmText(""); }}>
        <p>This permanently deletes <span className="mono">{deleting?.path}</span> ({deleting?.files} files). Your original files are not affected.</p>
        <label className="field"><span>Type the snapshot name “{deleting?.name}” to confirm</span><input type="text" value={confirmText} onChange={(e) => setConfirmText(e.target.value)} /></label>
        <div className="row"><button className="btn danger" disabled={confirmText !== deleting?.name} onClick={() => void remove()}>Delete permanently</button><button className="btn" onClick={() => { setDeleting(null); setConfirmText(""); }}>Cancel</button></div>
      </Modal>
    </div>
  );
}

/** Sends a snapshot to a computer that is receiving; the receiver checks every file. */
export function SendDialog({ summary, onClose }: { summary: Pick<Summary, "path" | "name" | "targets">; onClose: () => void }) {
  const [peers, setPeers] = useState<Peer[] | null>(null);
  const [address, setAddress] = useState("");
  const [code, setCode] = useState("");
  const [progress, setProgress] = useState<Progress | null>(null);
  const [sending, setSending] = useState(false);
  const [done, setDone] = useState<SendReport | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const look = () => {
    setPeers(null);
    api.findReceivers().then(setPeers, (e) => { setPeers([]); setErr(errorMessage(e)); });
  };
  useEffect(look, []);
  const send = async () => {
    setSending(true);
    setErr(null);
    try {
      setDone(await api.sendSnapshot(summary.path, address, code, (p) => p.kind === "copy" && setProgress(p)));
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setSending(false);
    }
  };
  const close = () => { if (sending) void api.cancelTransfer(); onClose(); };
  return (
    <Modal open title={`Send “${summary.name}”`} onClose={close}>
      {done ? (
        <div className="stack">
          <div className="notice ok" role="status">Sent to {done.receiver.hostname} and checked there. Saved as <span className="mono">{done.saved_as}</span>.</div>
          <p className="small">{fmtBytes(done.bytes)}{done.resumed_bytes ? `, of which ${fmtBytes(done.resumed_bytes)} was already there from an earlier attempt` : ""}. Restore it on that computer.</p>
          <div><button className="btn" onClick={onClose}>Close</button></div>
        </div>
      ) : sending ? (
        <div className="stack" aria-live="polite">
          <p>Sending… The other computer must accept the snapshot first.</p>
          {progress?.kind === "copy" && <><ProgressBar value={progress.done_bytes} max={progress.total_bytes || 1} label="Sending" /><p className="small mono muted">{progress.current}</p></>}
          <div><button className="btn danger" onClick={close}>Stop</button></div>
          <p className="small muted">If the transfer stops, send again: it continues where it stopped.</p>
        </div>
      ) : (
        <div className="stack">
          <p className="small">On the other computer, open Simple Setup → Restore → <strong>Receive from another computer</strong>. It shows a pairing code.</p>
          <fieldset className="stack" style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="field"><span>Receiving computers nearby</span></legend>
            {peers === null ? <p className="small muted">Looking…</p> : peers.length === 0 ? <p className="small muted">None found. Type the address the other computer shows, or look again.</p> : peers.map((p) => (
              <label key={p.name} className="check"><input type="radio" name="peer" checked={address === p.addresses[0]} onChange={() => setAddress(p.addresses[0])} /> {p.name} <span className="muted small">{p.os ?? ""} · {p.addresses[0]}</span></label>
            ))}
            <div><button className="btn link" onClick={look}>Look again</button></div>
          </fieldset>
          <label className="field"><span>Address</span><input type="text" className="mono" value={address} placeholder="192.168.1.20:41234" onChange={(e) => setAddress(e.target.value.trim())} /></label>
          {(() => {
            const peer = peers?.find((p) => p.addresses[0] === address);
            return peer?.os && !summary.targets.some((t) => `${t.os}-${t.arch}` === peer.os) ? (
              <div className="notice warn small">{peer.name} is {peer.os}, which this snapshot was not prepared for. Personal files and converted settings can still be restored there; apps may not.</div>
            ) : null;
          })()}
          <label className="field"><span>Pairing code shown on the other computer</span><input type="text" inputMode="numeric" autoComplete="off" className="mono" value={code} onChange={(e) => setCode(e.target.value)} /></label>
          <ErrorNote error={err} />
          <div className="row"><button className="btn primary" disabled={!address || code.replace(/\D/g, "").length !== 8} onClick={() => void send()}>Send</button><button className="btn" onClick={onClose}>Cancel</button></div>
        </div>
      )}
    </Modal>
  );
}
