import { useEffect, useState } from "react";
import { Archive, ArrowLeftRight, History, PackagePlus, Radar, RotateCcw } from "lucide-react";
import { api } from "../api";
import { useApp } from "../store";
import type { Peer, ReportSummary, SnapshotItem, SystemInfo, Volume } from "../types";
import { Badge, Empty, ErrorNote, ProgressBar, Spinner, distroName, fmtBytes, platformLabel } from "../ui";

function greeting() {
  const h = new Date().getHours();
  return h < 5 ? "Good evening" : h < 12 ? "Good morning" : h < 18 ? "Good afternoon" : "Good evening";
}

export default function Dashboard() {
  const { device, inventory, scanning, scanError, scan, go, appInfo, update, startFlow, set } = useApp();
  const [peers, setPeers] = useState<Peer[] | null>(null);
  const look = () => { setPeers(null); api.findReceivers().then(setPeers, () => setPeers([])); };
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
          <h1>{greeting()}</h1>
          <p>Move your apps, settings and files to another computer — Linux, Windows or macOS. Simple Setup works out how each app installs there, and shows you before anything starts.</p>
        </div>
      </div>

      <h2 className="label">What would you like to do?</h2>
      <div className="starts">
        <button className="start" onClick={() => startFlow("snapshot")}>
          <span className="icon-chip"><PackagePlus size={18} aria-hidden /></span>
          <span><strong>Create Snapshot</strong><span>Capture chosen apps, settings and files, on a drive or in a folder.</span></span>
        </button>
        <button className="start" onClick={() => startFlow("transfer")}>
          <span className="icon-chip"><ArrowLeftRight size={18} aria-hidden /></span>
          <span><strong>Transfer to Another Computer</strong><span>Connect over this network and migrate directly.</span></span>
        </button>
        <button className="start" onClick={() => { set({ restorePath: null }); go("restore"); }}>
          <span className="icon-chip"><RotateCcw size={18} aria-hidden /></span>
          <span><strong>Restore Snapshot</strong><span>Put a snapshot from a drive or another computer onto this one.</span></span>
        </button>
        <button className="start" onClick={() => go("snapshots")}>
          <span className="icon-chip"><Archive size={18} aria-hidden /></span>
          <span><strong>Manage Snapshots</strong><span>Browse, check, update and delete snapshots.</span></span>
        </button>
      </div>

      <h2 className="label" style={{ marginTop: 8 }}>This computer</h2>

      <div className="grid">
        <section className="card" aria-labelledby="dev">
          <h2 id="dev">This computer</h2>
          {device ? (
            <dl className="kv">
              <dt>Name</dt><dd>{device.hostname}</dd>
              <dt>System</dt><dd>{system?.linux ? system.linux.os_release.pretty_name ?? device.os_name : device.os_name}</dd>
              <dt>Type</dt><dd>{platformLabel(device.platform)}</dd>
              {system?.linux && <>
                <dt>Recognised as</dt><dd title={system.linux.explanation}>{system.linux.profile.name}{system.linux.matched.kind === "override" ? " (set by you)" : system.linux.matched.kind === "generic" ? " (unknown distribution)" : ""}
                  {system.linux.profile.chain.length > 2 && <span className="muted"> · {system.linux.profile.chain.slice(1, -1).map(distroName).join(" → ")}</span>}</dd>
                <dt>Packages</dt><dd>{system.linux.profile.formats.map((f) => f.toUpperCase()).join(", ") || "none"}{system.linux.immutable ? " · read-only system" : ""}{system.linux.profile.rolling ? " · rolling release" : ""}</dd>
              </>}
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
        <section className="card" aria-labelledby="near">
          <h2 id="near">Nearby computers</h2>
          {peers === null ? <p className="small muted">{device ? "Computers that are receiving show up here." : ""}</p> : peers.length === 0 ? (
            <p className="small muted">None is receiving right now. On the new computer, open Transfers → Receive.</p>
          ) : (
            <ul className="plain small">{peers.map((p) => <li key={p.name}><strong>{p.name}</strong> <span className="muted">{p.system ?? p.os}</span></li>)}</ul>
          )}
          <div className="row">
            <button className="btn" onClick={look}><Radar size={15} aria-hidden /> Look nearby</button>
            {!!peers?.length && <button className="btn link" onClick={() => startFlow("transfer")}>Transfer to one</button>}
          </div>
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
          {!snapshots?.length ? <Empty icon={<Archive size={20} />} title="No snapshots yet">Snapshots you make or open appear here.</Empty> : (
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
          {restores.length === 0 ? <Empty icon={<History size={20} />} title="Nothing restored yet">Restores on this computer are listed here with their reports.</Empty> : (
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
