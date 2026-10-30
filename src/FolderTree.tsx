import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { api, errorMessage } from "./api";
import type { DirItem, Measure } from "./types";
import { Badge, Modal, fmtBytes } from "./ui";

/** Size of what would be copied from `path`, refreshed when the exclusions change. */
export function useMeasure(path: string | null, exclude: string[]) {
  const [m, setM] = useState<Measure | null>(null);
  const key = exclude.join("\n");
  useEffect(() => {
    if (!path) return;
    let live = true;
    setM(null);
    api.measure(path, key ? key.split("\n") : []).then((r) => live && setM(r), () => live && setM(null));
    return () => { live = false; };
  }, [path, key]);
  return m;
}

export function MeasureText({ m }: { m: Measure | null }) {
  if (!m) return <span className="muted small">Measuring…</span>;
  if (!m.exists) return <Badge tone="warn">Missing</Badge>;
  return (
    <span className="small">
      {fmtBytes(m.bytes)} · {m.files.toLocaleString()} files{m.partial ? "+" : ""}
      {m.secrets > 0 && <> · <Badge tone="warn" title="Passwords, keys or tokens are left out unless you include them">{m.secrets} protected</Badge></>}
    </span>
  );
}

interface TreeProps {
  root: string;
  excluded: string[];
  onToggleExclude: (path: string) => void;
  secretsIncluded: string[];
  onToggleSecret: (path: string) => void;
}

/** Folder browser with per-item include/exclude. Lists names only; never reads file contents. */
export function FolderTree(props: TreeProps) {
  return <Level {...props} path={props.root} depth={0} />;
}

function Level({ path, depth, ...p }: TreeProps & { path: string; depth: number }) {
  const [items, setItems] = useState<DirItem[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const [confirm, setConfirm] = useState<DirItem | null>(null);
  useEffect(() => {
    api.listDir(path).then(setItems, (e) => setErr(errorMessage(e)));
  }, [path]);
  if (err) return <p className="small muted">Cannot open: {err}</p>;
  if (!items) return <p className="small muted">Loading…</p>;
  if (items.length === 0) return <p className="small muted" style={{ paddingLeft: 18 }}>Empty</p>;
  const under = (x: string, base: string) => x === base || x.startsWith(base + "/") || x.startsWith(base + "\\");
  return (
    <ul className={`tree${depth === 0 ? " root" : ""}`}>
      {items.map((it) => {
        const excluded = p.excluded.some((x) => under(it.path, x));
        const secretOk = p.secretsIncluded.some((x) => under(it.path, x));
        const protectedItem = !!it.secret && !secretOk;
        return (
          <li key={it.path}>
            <div className="row" style={{ gap: 6 }}>
              {it.is_dir ? (
                <button className="btn link" aria-expanded={!!open[it.path]} aria-label={`${open[it.path] ? "Collapse" : "Expand"} ${it.name}`}
                  onClick={() => setOpen({ ...open, [it.path]: !open[it.path] })}>
                  {open[it.path] ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                </button>
              ) : <span style={{ width: 22 }} />}
              {it.transient ? (
                <span className="small muted">{it.name} — cache, always skipped</span>
              ) : (
                <label className="check small">
                  <input type="checkbox" checked={!excluded && !protectedItem} disabled={protectedItem}
                    onChange={() => p.onToggleExclude(it.path)} />
                  {it.name}{it.size != null && <span className="muted"> · {fmtBytes(it.size)}</span>}
                </label>
              )}
              {it.secret && (secretOk
                ? <><Badge tone="bad">Included: {it.secret}</Badge><button className="btn link small" onClick={() => p.onToggleSecret(it.path)}>Leave out</button></>
                : <><Badge tone="warn">Protected: {it.secret}</Badge><button className="btn link small" onClick={() => setConfirm(it)}>Include anyway…</button></>)}
            </div>
            {it.is_dir && open[it.path] && !excluded && <Level {...p} path={it.path} depth={depth + 1} />}
          </li>
        );
      })}
      <Modal open={!!confirm} title="Include sensitive files?" onClose={() => setConfirm(null)}>
        <p><strong>{confirm?.name}</strong> looks like it contains {confirm?.secret?.toLowerCase()}.</p>
        <p>Anyone with the snapshot drive could read it, unless you encrypt the snapshot with a password (step 3). Only include it if you need it on the new computer.</p>
        <div className="row">
          <button className="btn danger" onClick={() => { if (confirm) p.onToggleSecret(confirm.path); setConfirm(null); }}>Include it</button>
          <button className="btn" onClick={() => setConfirm(null)}>Keep it out</button>
        </div>
      </Modal>
    </ul>
  );
}
