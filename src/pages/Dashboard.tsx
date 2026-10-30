import { useEffect, useState } from "react";
import { api } from "../api";
import { useApp } from "../store";
import type { SnapshotItem } from "../types";
import { Badge, ErrorNote, fmtBytes, platformLabel } from "../ui";

export default function Dashboard() {
  const { device, inventory, scanning, scanError, scan, go } = useApp();
  const [snapshots, setSnapshots] = useState<SnapshotItem[]>([]);
  useEffect(() => {
    api.listSnapshots().then(setSnapshots, () => setSnapshots([]));
  }, []);
  const yours = inventory?.apps.filter((a) => !a.system).length ?? 0;

  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1>Move your setup to another computer</h1>
          <p className="muted">Pick the apps, settings and files you want. Nothing is copied until you choose it and press Build.</p>
        </div>
        <button className="btn primary big" onClick={() => go("create")}>Create Snapshot</button>
      </div>

      <div className="grid">
        <section className="card" aria-labelledby="dev">
          <h2 id="dev">This computer</h2>
          {device ? (
            <dl className="small" style={{ margin: 0 }}>
              <dt className="muted">Name</dt><dd style={{ margin: "0 0 6px" }}>{device.hostname}</dd>
              <dt className="muted">System</dt><dd style={{ margin: "0 0 6px" }}>{device.os_name}</dd>
              <dt className="muted">Type</dt><dd style={{ margin: 0 }}>{platformLabel(device.platform)}</dd>
            </dl>
          ) : <p className="muted">Detecting…</p>}
        </section>
        <section className="card" aria-labelledby="scan" aria-busy={scanning}>
          <h2 id="scan">Applications</h2>
          {scanning ? <p role="status">Scanning installed applications…</p> : inventory ? (
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
        <section className="card" aria-labelledby="restore">
          <h2 id="restore">Already have a snapshot?</h2>
          <p className="muted small">Open it from a drive and restore what you need on this computer. Works without internet.</p>
          <button className="btn" onClick={() => go("restore")}>Restore Snapshot</button>
        </section>
      </div>

      <section className="card" aria-labelledby="recent">
        <div className="spread"><h2 id="recent">Recent snapshots</h2><button className="btn link" onClick={() => go("snapshots")}>All snapshots</button></div>
        {snapshots.length === 0 ? <p className="muted">No snapshots yet.</p> : (
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
    </div>
  );
}
