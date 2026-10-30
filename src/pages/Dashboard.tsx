import { useEffect, useState } from "react";
import { PackagePlus, RotateCcw, Send } from "lucide-react";
import { api } from "../api";
import { useApp } from "../store";
import type { ReportSummary, SnapshotItem, SystemInfo, Volume } from "../types";
import { Badge, ErrorNote, ProgressBar, Spinner, fmtBytes, platformLabel } from "../ui";

export default function Dashboard() {
  const { device, inventory, scanning, scanError, scan, go, appInfo, update } = useApp();
  const [snapshots, setSnapshots] = useState<SnapshotItem[] | null>(null);
  const [restores, setRestores] = useState<ReportSummary[]>([]);
  const [system, setSystem] = useState<SystemInfo | null>(null);
  const [volumes, setVolumes] = useState<Volume[]>([]);
  useEffect(() => {
    api.listSnapshots().then(setSnapshots, () => setSnapshots([]));
    api.recentRestores().then(setRestores, () => setRestores([]));
    api.systemInfo().then(setSystem, () => setSystem(null));
    api.volumes().then(setVolumes, () => setVolumes([]));
  }, []);
  const yours = inventory?.apps.filter((a) => !a.system).length ?? 0;
  const available = (snapshots ?? []).filter((s) => s.summary);
  const stored = available.reduce((n, s) => n + (s.summary?.bytes ?? 0), 0);
  const holding = volumes.filter((v) => available.some((s) => s.reference.path.startsWith(v.mount)));

  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1>Move your setup to another computer</h1>
          <p className="muted">Choose the apps, settings and files to bring. On the new computer — Linux, Windows or macOS — Simple Setup installs the apps and puts everything back.</p>
        </div>
        <div className="row">
          <button className="btn" onClick={() => go("restore")}><RotateCcw size={16} aria-hidden /> Restore</button>
          <button className="btn" onClick={() => go("snapshots")}><Send size={16} aria-hidden /> Send to Another Computer</button>
          <button className="btn primary big" onClick={() => go("create")}><PackagePlus size={18} aria-hidden /> Create Snapshot</button>
        </div>
      </div>

      <div className="grid">
        <section className="card" aria-labelledby="dev">
          <h2 id="dev">This computer</h2>
          {device ? (
            <dl className="kv">
              <dt>Name</dt><dd>{device.hostname}</dd>
              <dt>System</dt><dd>{device.os_name}</dd>
              <dt>Type</dt><dd>{platformLabel(device.platform)}</dd>
              {system && <>
                <dt>Processor</dt><dd>{system.cpu} ({system.cores} cores)</dd>
                <dt>Memory</dt><dd>{fmtBytes(system.memory_bytes)}</dd>
                <dt>Free space</dt><dd>{fmtBytes(system.free_bytes)}</dd>
                <dt>Installs with</dt><dd>{system.tools.filter((t) => !["cargo", "go", "pipx", "curl"].includes(t)).join(", ") || "downloads only"}</dd>
                <dt>Internet</dt><dd>{system.online ? "Connected" : "Not connected"}</dd>
              </>}
            </dl>
          ) : <p className="muted"><Spinner /> Detecting…</p>}
        </section>
        <section className="card" aria-labelledby="scan" aria-busy={scanning}>
          <h2 id="scan">Applications</h2>
          {scanning ? <p role="status"><Spinner /> Scanning installed applications…</p> : inventory ? (
            <>
              <div className="stat">{inventory.apps.length}</div>
              <p className="muted small">{yours} added by you · {inventory.apps.length - yours} came with the system · {inventory.hidden_packages} libraries not shown</p>
            </>
          ) : <p className="muted">Not scanned yet.</p>}
          <ErrorNote error={scanError} />
          <div className="row">
            <button className="btn" onClick={() => void scan()} disabled={scanning}>{inventory ? "Scan again" : "Scan Computer"}</button>
            <button className="btn link" onClick={() => go("applications")} disabled={!inventory}>View list</button>
          </div>
        </section>
        <section className="card" aria-labelledby="storage">
          <h2 id="storage">Backup storage</h2>
          {snapshots === null ? <p className="muted"><Spinner /> Looking…</p> : (
            <>
              <p className="small"><strong>{fmtBytes(stored)}</strong> in {available.length} snapshot{available.length === 1 ? "" : "s"} you can open now{snapshots.length > available.length ? ` (${snapshots.length - available.length} on drives not connected)` : ""}.</p>
              {(holding.length ? holding : volumes.filter((v) => v.removable)).slice(0, 3).map((v) => (
                <div key={v.mount} className="stack" style={{ gap: 4 }}>
                  <div className="spread small"><span className="mono">{v.name || v.mount}</span><span className="muted">{fmtBytes(v.free)} free of {fmtBytes(v.total)}</span></div>
                  <ProgressBar value={v.total - v.free} max={v.total} label={`Space used on ${v.name || v.mount}`} />
                </div>
              ))}
              {volumes.length === 0 && <p className="muted small">No external drive connected.</p>}
            </>
          )}
        </section>
        <section className="card" aria-labelledby="upd">
          <h2 id="upd">Simple Setup</h2>
          <p className="small">Version {appInfo?.version ?? "…"}</p>
          {update ? (
            <p className="small">{update.newer ? <Badge tone="info">Update available</Badge> : <Badge tone="ok">Up to date</Badge>} {update.message}</p>
          ) : <p className="small muted">{appInfo?.development ? "Development build: updates are checked only when you ask." : "Updates have not been checked yet."}</p>}
          <button className="btn link" onClick={() => go("settings")}>Updates and settings</button>
        </section>
      </div>

      <div className="grid two">
        <section className="card" aria-labelledby="recent">
          <div className="spread"><h2 id="recent">Recent snapshots</h2><button className="btn link" onClick={() => go("snapshots")}>All snapshots</button></div>
          {!snapshots?.length ? <p className="muted">No snapshots yet.</p> : (
            <table>
              <thead><tr><th>Name</th><th>Created</th><th>Size</th><th>Status</th></tr></thead>
              <tbody>
                {snapshots.slice(0, 5).map((s) => (
                  <tr key={s.reference.path}>
                    <td>{s.reference.name}</td>
                    <td>{new Date(s.reference.created_at).toLocaleString()}</td>
                    <td>{s.summary ? fmtBytes(s.summary.bytes) : "—"}</td>
                    <td>{s.summary ? (s.reference.last_verify_ok === false ? <Badge tone="bad">Damaged</Badge> : <Badge tone="ok">Available</Badge>) : <Badge>Drive not connected</Badge>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
        <section className="card" aria-labelledby="restores">
          <div className="spread"><h2 id="restores">Recent restores</h2><button className="btn link" onClick={() => go("restore")}>Restore</button></div>
          {restores.length === 0 ? <p className="muted">Nothing restored on this computer yet.</p> : (
            <table>
              <thead><tr><th>Snapshot</th><th>Finished</th><th>Result</th></tr></thead>
              <tbody>
                {restores.map((r) => (
                  <tr key={r.file}>
                    <td>{r.snapshot_name}</td>
                    <td>{new Date(r.finished_at).toLocaleString()}</td>
                    <td className="small">
                      <Badge tone={r.failed ? "bad" : r.cancelled ? "warn" : "ok"}>{r.failed ? `${r.failed} failed` : r.cancelled ? "Cancelled" : "Done"}</Badge> {r.done} done{r.other ? `, ${r.other} need you or skipped` : ""}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </section>
      </div>
    </div>
  );
}
