import { useEffect, useState } from "react";
import { ArrowDownToLine, ArrowLeftRight, ArrowUpFromLine, History } from "lucide-react";
import { api, errorMessage } from "../api";
import { useApp } from "../store";
import type { SnapshotItem, Summary, TransferRecord } from "../types";
import { Badge, Empty, ErrorNote, Modal, fmtBytes } from "../ui";
import { ReceiveDialog } from "./Restore";
import { SendDialog } from "./Snapshots";

/** Computer-to-computer moves over the local network, and their history. */
export default function Transfers() {
  const { startFlow, set, go } = useApp();
  const [history, setHistory] = useState<TransferRecord[] | null>(null);
  const [receiving, setReceiving] = useState(false);
  const [picking, setPicking] = useState<SnapshotItem[] | null>(null);
  const [sending, setSending] = useState<Summary | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const reload = () => api.listTransfers().then(setHistory, (e) => { setHistory([]); setErr(errorMessage(e)); });
  useEffect(() => { void reload(); }, []);
  const pick = () => api.listSnapshots().then((l) => setPicking(l.filter((s) => s.summary)), (e) => setErr(errorMessage(e)));

  return (
    <div className="stack">
      <div className="page-head">
        <div><h1>Transfers</h1><p className="muted">Move straight to another computer on this network. Both computers need Simple Setup open; the receiving one shows a pairing code, and nothing is saved there until it accepts.</p></div>
      </div>
      <ErrorNote error={err} />
      <div className="starts">
        <button className="start" onClick={() => startFlow("transfer")}>
          <span className="icon-chip"><ArrowLeftRight size={18} aria-hidden /></span>
          <span><strong>Transfer to Another Computer</strong><span>Choose apps and files, connect, check compatibility, start.</span></span>
        </button>
        <button className="start" onClick={() => void pick()}>
          <span className="icon-chip"><ArrowUpFromLine size={18} aria-hidden /></span>
          <span><strong>Send a snapshot</strong><span>Send one you already made, as it is.</span></span>
        </button>
        <button className="start" onClick={() => setReceiving(true)}>
          <span className="icon-chip"><ArrowDownToLine size={18} aria-hidden /></span>
          <span><strong>Receive on this computer</strong><span>Show a pairing code and wait for the other computer.</span></span>
        </button>
      </div>

      <section className="card" aria-labelledby="hist">
        <h2 id="hist">History</h2>
        {history === null ? <p className="muted small">Loading…</p> : history.length === 0 ? (
          <Empty icon={<History size={20} />} title="No transfers yet">Snapshots sent from or received on this computer are listed here.</Empty>
        ) : (
          <table>
            <thead><tr><th>When</th><th>Direction</th><th>Snapshot</th><th>Other computer</th><th>Size</th><th><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {history.map((h, i) => (
                <tr key={i}>
                  <td className="small">{new Date(h.at).toLocaleString()}</td>
                  <td>{h.direction === "sent" ? <Badge tone="info">Sent</Badge> : <Badge tone="ok">Received</Badge>}</td>
                  <td>{h.snapshot}</td>
                  <td>{h.peer}</td>
                  <td className="small">{h.files.toLocaleString()} files · {fmtBytes(h.bytes)}</td>
                  <td>{h.direction === "received" && h.path && <button className="btn link" onClick={() => { set({ restorePath: h.path, restoreAuto: false }); go("restore"); }}>Restore</button>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <Modal open={!!picking} title="Send which snapshot?" onClose={() => setPicking(null)} footer={<button className="btn" onClick={() => setPicking(null)}>Cancel</button>}>
        {picking?.length === 0 ? <p className="small muted">No snapshot is available here. Create one first, or use Transfer to Another Computer.</p> : (
          <ul className="plain">
            {picking?.map((s) => (
              <li key={s.reference.path} className="row">
                <button className="btn link" onClick={() => { setSending(s.summary); setPicking(null); }}>{s.reference.name}</button>
                <span className="small muted">{new Date(s.reference.created_at).toLocaleDateString()} · {fmtBytes(s.summary?.bytes)}</span>
              </li>
            ))}
          </ul>
        )}
      </Modal>
      {sending && <SendDialog summary={sending} onClose={() => { setSending(null); void reload(); }} />}
      {receiving && <ReceiveDialog onClose={() => setReceiving(false)} onReceived={(p, auto) => { setReceiving(false); set({ restorePath: p, restoreAuto: auto }); go("restore"); }} />}
    </div>
  );
}
