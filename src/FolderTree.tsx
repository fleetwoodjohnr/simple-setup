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
      <Modal open={!!confirm} title="Include sensitive files?" onClose={() => setConfirm(null)}
        footer={<><button className="btn" onClick={() => setConfirm(null)}>Keep it out</button><button className="btn danger" onClick={() => { if (confirm) p.onToggleSecret(confirm.path); setConfirm(null); }}>Include it</button></>}>
        <p><strong>{confirm?.name}</strong> looks like it contains {confirm?.secret?.toLowerCase()}.</p>
        <p>Anyone with the snapshot drive could read it, unless you encrypt the snapshot with a password (step 3). Only include it if you need it on the new computer.</p>
      </Modal>
    </ul>
  );
}

/** The files of a folder in a snapshot, as a tree to leave parts out. `excluded` holds
 * `<prefix>/<path>` entries (a file or a folder); unticking a folder leaves out all in it. */
export function SnapshotTree({ load, prefix, excluded, onChange }: {
  load: () => Promise<[string, number][]>; prefix: string; excluded: string[]; onChange: (next: string[]) => void;
}) {
  const [files, setFiles] = useState<[string, number][] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => { load().then(setFiles, (e) => setErr(errorMessage(e))); }, []);
  if (err) return <p className="small muted">Cannot list it: {err}</p>;
  if (!files) return <p className="small muted">Loading…</p>;
  const key = (rel: string) => `${prefix}/${rel}`;
  const out = (rel: string) => excluded.some((x) => key(rel) === x || key(rel).startsWith(x + "/"));
  const toggle = (rel: string) => {
    const k = key(rel);
    // Ticking again also brings back what was unticked inside it.
    onChange(out(rel) ? excluded.filter((x) => x !== k && !x.startsWith(k + "/") && !k.startsWith(x + "/")) : [...excluded, k]);
  };
  return <SnapLevel files={files} dir="" out={out} toggle={toggle} depth={0} />;
}

function SnapLevel({ files, dir, out, toggle, depth }: {
  files: [string, number][]; dir: string; out: (rel: string) => boolean; toggle: (rel: string) => void; depth: number;
}) {
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const children = new Map<string, { dir: boolean; size: number; count: number }>();
  for (const [rel, size] of files) {
    if (dir && !rel.startsWith(dir + "/")) continue;
    const rest = dir ? rel.slice(dir.length + 1) : rel;
    const [head, ...more] = rest.split("/");
    const c = children.get(head) ?? { dir: more.length > 0, size: 0, count: 0 };
    c.size += size; c.count += 1;
    children.set(head, c);
  }
  return (
    <ul className={`tree${depth === 0 ? " root" : ""}`}>
      {[...children.entries()].sort(([a, x], [b, y]) => Number(y.dir) - Number(x.dir) || a.localeCompare(b)).map(([name, c]) => {
        const rel = dir ? `${dir}/${name}` : name;
        return (
          <li key={rel}>
            <div className="row" style={{ gap: 6 }}>
              {c.dir ? (
                <button className="btn link" aria-expanded={!!open[rel]} aria-label={`${open[rel] ? "Collapse" : "Expand"} ${name}`}
                  onClick={() => setOpen({ ...open, [rel]: !open[rel] })}>
                  {open[rel] ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                </button>
              ) : <span style={{ width: 22 }} />}
              <label className="check small">
                <input type="checkbox" checked={!out(rel)} onChange={() => toggle(rel)} />
                {name}<span className="muted"> · {fmtBytes(c.size)}{c.dir ? ` · ${c.count} file${c.count === 1 ? "" : "s"}` : ""}</span>
              </label>
            </div>
            {c.dir && open[rel] && !out(rel) && <SnapLevel files={files} dir={rel} out={out} toggle={toggle} depth={depth + 1} />}
          </li>
        );
      })}
    </ul>
  );
}
