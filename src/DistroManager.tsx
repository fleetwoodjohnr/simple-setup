// Settings → Operating systems → Linux distributions → Manage distributions: browse the
// profiles, import one, add one with a guided form (inheriting everything not changed), check
// it, export, switch off, remove, trust custom commands, and fetch official updates.
import { useEffect, useMemo, useState } from "react";
import { open, save } from "@tauri-apps/plugin-dialog";
import { Download, FilePlus2, RefreshCw, Upload } from "lucide-react";
import { api, errorMessage } from "./api";
import { useApp } from "./store";
import type { Adapter, Avail, DistroProfile, Listed, ProfileFile, Resolved, TestReport } from "./types";
import { Badge, ErrorNote, Modal, Spinner } from "./ui";

const LAYER: Record<Listed["layer"], [string, "neutral" | "info" | "ok"]> = {
  builtin: ["Built in", "neutral"],
  official: ["Official update", "ok"],
  custom: ["Yours", "info"],
};
const ADAPTERS: Adapter[] = ["apt", "dnf", "zypper", "pacman", "apk", "rpm-ostree", "transactional-update", "none", "custom"];
const ADAPTER_LABEL: Record<Adapter, string> = {
  apt: "APT", dnf: "DNF", zypper: "Zypper", pacman: "pacman", apk: "apk", "rpm-ostree": "rpm-ostree (image-based)",
  "transactional-update": "transactional-update (image-based)", none: "None (portable formats only)", custom: "Custom commands",
};
const file = (distros: DistroProfile[]): ProfileFile => ({ schema_version: 1, distros });

export function DistroManager({ onClose }: { onClose: () => void }) {
  const { loadDistros } = useApp();
  const [list, setList] = useState<Awaited<ReturnType<typeof api.listDistros>> | null>(null);
  const [query, setQuery] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [details, setDetails] = useState<{ raw: DistroProfile | null; resolved: Resolved } | null>(null);
  const [editing, setEditing] = useState<DistroProfile | null>(null);
  const [importing, setImporting] = useState<{ file: ProfileFile; report: TestReport } | null>(null);
  const [busy, setBusy] = useState(false);
  const reload = async () => {
    try {
      setList(await api.listDistros());
      await loadDistros();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  useEffect(() => { void reload(); }, []);
  const act = async (f: () => Promise<unknown>, done?: string) => {
    setErr(null);
    setNote(null);
    try {
      await f();
      if (done) setNote(done);
      await reload();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const shown = (list?.profiles ?? []).filter((d) => !query || `${d.name} ${d.id} ${d.family}`.toLowerCase().includes(query.toLowerCase()));
  const pickImport = async () => {
    const p = await open({ title: "Import a distribution profile", filters: [{ name: "Profile", extensions: ["json", "yaml", "yml"] }] });
    if (typeof p !== "string") return;
    setErr(null);
    try {
      const f = await api.readDistroFile(p);
      setImporting({ file: f, report: await api.testDistros(f) });
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const exportOne = async (id: string) => {
    const to = await save({ title: "Export profile", defaultPath: `${id}.json`, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (to) void act(() => api.exportDistros([id], to), `Saved to ${to}`);
  };
  const showDetails = async (id: string) => {
    try {
      const [raw, resolved] = await api.distroDetails(id);
      setDetails({ raw, resolved });
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const update = async () => {
    setBusy(true);
    await act(async () => setNote((await api.updateDistros()).message));
    setBusy(false);
  };

  return (
    <Modal open wide title="Linux distributions" onClose={onClose}
      footer={<button className="btn" onClick={onClose}>Close</button>}>
      <p className="small muted" style={{ margin: 0 }}>
        Each distribution has a profile: how it is recognised, what it installs with, which repositories are its own and which portable formats it supports. A profile names a parent (Linux Mint → Ubuntu → Debian) and states only what differs. Profiles are data; custom package-manager commands run only after you trust them.
      </p>
      <div className="spread">
        <input type="search" aria-label="Search distributions" placeholder="Search" value={query} onChange={(e) => setQuery(e.target.value)} />
        <div className="row">
          <button className="btn" onClick={() => void pickImport()}><Upload size={15} aria-hidden /> Import…</button>
          <button className="btn primary" onClick={() => setEditing({ id: "", name: "", parent: "ubuntu", match: { ids: [] } })}><FilePlus2 size={15} aria-hidden /> Add distribution…</button>
          <button className="btn" disabled={busy} onClick={() => void update()}>{busy ? <Spinner /> : <RefreshCw size={15} aria-hidden />} Check for updates</button>
        </div>
      </div>
      {list && <p className="small muted" style={{ margin: 0 }}>Profiles version {list.version} (built in {list.builtin_version}{list.official_version ? `, official update ${list.official_version}` : ""}).</p>}
      {list?.problems.map((p) => <div key={p} className="notice warn small">{p}</div>)}
      {note && <div className="notice ok small" role="status">{note}</div>}
      <ErrorNote error={err} />
      {!list ? <p className="muted"><Spinner /> Loading…</p> : (
        <div className="table-wrap" style={{ maxHeight: "48vh" }}>
          <table>
            <thead><tr><th>Distribution</th><th>Based on</th><th>Installs with</th><th>Source</th><th>Used</th><th><span className="sr-only">Actions</span></th></tr></thead>
            <tbody>
              {shown.map((d) => (
                <tr key={d.id}>
                  <td>{d.name}<div className="muted small mono">{d.id}</div>
                    {d.problems.map((p) => <div key={p} className="small" style={{ color: "var(--danger)" }}>{p}</div>)}</td>
                  <td className="small">{d.parent ?? (d.id === "linux" ? "—" : "linux")}</td>
                  <td className="small">{ADAPTER_LABEL[d.package_manager]}{d.immutable && <div><Badge>Read-only system</Badge></div>}
                    {d.has_custom && <div>{d.trusted ? <Badge tone="ok">Commands trusted</Badge> : <Badge tone="warn">Commands not trusted</Badge>}</div>}</td>
                  <td><Badge tone={LAYER[d.layer][1]}>{LAYER[d.layer][0]}</Badge>{d.replaces && <div className="muted small">replaces {LAYER[d.replaces][0].toLowerCase()}</div>}</td>
                  <td><input type="checkbox" role="switch" className="switch" aria-label={`Use the ${d.name} profile`} checked={d.enabled} disabled={d.id === "linux"}
                    onChange={(e) => void act(() => api.setDistroEnabled(d.id, e.target.checked))} /></td>
                  <td>
                    <div className="row" style={{ flexWrap: "nowrap" }}>
                      <button className="btn link" onClick={() => void showDetails(d.id)}>Details</button>
                      <button className="btn link" onClick={() => void api.distroDetails(d.id).then(([raw]) => setEditing(d.layer === "custom" && raw ? raw : { id: d.layer === "custom" ? d.id : `${d.id}-custom`, name: `${d.name} (custom)`, parent: d.id, match: { ids: [] } }))}>
                        {d.layer === "custom" ? "Edit" : "Base a new one on it"}</button>
                      <button className="btn link" onClick={() => void exportOne(d.id)}>Export</button>
                      {d.has_custom && d.layer === "custom" && <button className="btn link" onClick={() => void showDetails(d.id)}>{d.trusted ? "Commands…" : "Trust…"}</button>}
                      {d.layer === "custom" && <button className="btn link" style={{ color: "var(--danger)" }} onClick={() => void act(() => api.deleteDistro(d.id), `Removed ${d.name}.`)}>Remove</button>}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {details && <Details {...details} onClose={() => setDetails(null)} onTrust={(on) => void act(() => api.trustDistro(details.resolved.id, on), on ? "Trusted." : "No longer trusted.").then(() => showDetails(details.resolved.id))} />}
      {editing && <Editor start={editing} profiles={list?.profiles ?? []} onClose={() => setEditing(null)} onSaved={(id) => { setEditing(null); void reload(); setNote(`Saved ${id}.`); }} />}
      {importing && (
        <Modal open title="Import distribution profiles" onClose={() => setImporting(null)}
          footer={<><button className="btn" onClick={() => setImporting(null)}>Cancel</button>
            <button className="btn primary" disabled={importing.report.problems.length > 0}
              onClick={() => void act(async () => { const ids = await api.saveDistros(importing.file); setImporting(null); setNote(`Imported ${ids.join(", ")}.${importing.file.distros.some((d) => d.custom) ? " Its custom commands run only after you trust them (Details)." : ""}`); })}>Import</button></>}>
          <TestView report={importing.report} />
          <ul className="plain small">{importing.file.distros.map((d) => <li key={d.id}><strong>{d.name ?? d.id}</strong> <span className="mono">{d.id}</span>{d.parent ? ` · based on ${d.parent}` : ""}</li>)}</ul>
          {importing.file.distros.filter((d) => d.custom).map((d) => <CommandsView key={d.id} profile={d} />)}
        </Modal>
      )}
    </Modal>
  );
}

function TestView({ report }: { report: TestReport }) {
  if (report.problems.length) {
    return <div className="notice bad small" role="alert"><strong>Not valid:</strong><ul className="plain">{report.problems.map((p) => <li key={p}>{p}</li>)}</ul></div>;
  }
  const r = report.resolved;
  return (
    <div className="notice ok small stack" role="status" style={{ gap: 4 }}>
      <strong>Valid.</strong>
      {r && <span>{r.name}: {r.chain.join(" → ")} · installs with {ADAPTER_LABEL[r.package_manager]} · {r.formats.join(", ") || "no native format"}{r.immutable ? " · read-only system" : ""}</span>}
      <span>{report.matches_here ? "It matches this computer." : "It does not match this computer (that is fine for another computer)."}</span>
      {report.tools.map(([t, have]) => <span key={t}>Needs <span className="mono">{t}</span>: {have ? "found here" : "not on this computer"}</span>)}
    </div>
  );
}

function CommandsView({ profile }: { profile: DistroProfile }) {
  const c = profile.custom;
  if (!c) return null;
  return (
    <div className="notice warn small stack" style={{ gap: 4 }}>
      <strong>{profile.name ?? profile.id} brings its own package-manager commands ({c.label})</strong>
      <span>Install: <code>{c.install.join(" ")}</code>{c.admin ? " (with administrator rights)" : ""}</span>
      <span>Check installed: <code>{c.query.join(" ")}</code></span>
      {c.available && c.available.length > 0 && <span>Check available: <code>{c.available.join(" ")}</code></span>}
      <span>They are run exactly like this, never through a shell, and only after you trust them.</span>
    </div>
  );
}

function Details({ raw, resolved: r, onClose, onTrust }: { raw: DistroProfile | null; resolved: Resolved; onClose: () => void; onTrust: (on: boolean) => void }) {
  return (
    <Modal open title={r.name} onClose={onClose} footer={<button className="btn" onClick={onClose}>Close</button>}>
      <dl className="kv small">
        <dt>ID</dt><dd className="mono">{r.id}</dd>
        <dt>Based on</dt><dd>{r.chain.slice(1).join(" → ") || "—"}</dd>
        <dt>Recognised by</dt><dd>{raw?.match ? <>os-release ID {raw.match.ids.join(", ")}{raw.match.variants?.length ? `, variant ${raw.match.variants.join(", ")}` : ""}{raw.match.min_version || raw.match.max_version ? `, version ${raw.match.min_version ?? "…"}–${raw.match.max_version ?? "…"}` : ""}</> : "Not detected by itself (a base for others)"}</dd>
        <dt>Installs with</dt><dd>{ADAPTER_LABEL[r.package_manager]} · {r.formats.join(", ") || "no native format"}</dd>
        <dt>Package names</dt><dd className="mono">{r.names.join(", ") || "—"}</dd>
        <dt>Own repositories</dt><dd>{r.repositories.map((x) => x.name ?? x.id).join(", ") || "—"}</dd>
        <dt>Flatpak · Snap · AppImage</dt><dd>{r.flatpak} · {r.snap} · {r.appimage}</dd>
        <dt>System</dt><dd>{r.immutable ? "Read-only (image-based)" : "Regular"}{r.rolling ? " · rolling release" : ""}</dd>
        <dt>Processors</dt><dd>{r.arch.join(", ") || "any"}</dd>
      </dl>
      {r.rules.length > 0 && <><h3>Rules</h3><ul className="plain small">{r.rules.map((x, i) => <li key={i}><strong>{x.app === "*" ? "All apps" : x.app}</strong>: {x.action} {x.method ?? ""} — {x.reason}</li>)}</ul></>}
      {r.notes && <p className="small muted">{r.notes}</p>}
      {r.custom && raw && (
        <>
          <CommandsView profile={{ ...raw, custom: r.custom }} />
          <div className="row">
            {r.trusted ? <button className="btn" onClick={() => onTrust(false)}>Stop trusting these commands</button>
              : <button className="btn primary" onClick={() => onTrust(true)}>Trust these commands</button>}
          </div>
        </>
      )}
    </Modal>
  );
}

/** Guided form for a new or edited profile: name, how it is recognised and what it is based
 * on are enough; everything else is inherited unless changed under "Differences". */
function Editor({ start, profiles, onClose, onSaved }: { start: DistroProfile; profiles: Listed[]; onClose: () => void; onSaved: (id: string) => void }) {
  const [p, setP] = useState<DistroProfile>(start);
  const [ids, setIds] = useState((start.match?.ids ?? []).join(" "));
  const [args, setArgs] = useState({ install: start.custom?.install.join("\n") ?? "", query: start.custom?.query.join("\n") ?? "", label: start.custom?.label ?? "" });
  const [repos, setRepos] = useState((start.repositories ?? []).map((r) => r.id).join("\n"));
  const [report, setReport] = useState<TestReport | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const parents = useMemo(() => profiles.filter((x) => x.id !== p.id), [profiles, p.id]);
  const lines = (s: string) => s.split("\n").map((x) => x.trim()).filter(Boolean);
  const profile = (): DistroProfile => {
    const out: DistroProfile = { ...p, id: p.id.trim(), name: p.name?.trim() || null, match: lines(ids.replace(/[ ,]+/g, "\n")).length ? { ...p.match, ids: lines(ids.replace(/[ ,]+/g, "\n")) } : null };
    out.repositories = lines(repos).length ? lines(repos).map((id) => ({ id, official: true })) : null;
    out.custom = out.package_manager === "custom" ? { label: args.label.trim(), install: lines(args.install), query: lines(args.query), admin: true } : null;
    for (const k of Object.keys(out) as (keyof DistroProfile)[]) if (out[k] === null || out[k] === undefined || out[k] === "") delete out[k];
    return out;
  };
  // Every change makes the last check stale.
  useEffect(() => setReport(null), [p, ids, args, repos]);
  const check = async () => {
    setErr(null);
    try {
      setReport(await api.testDistros(file([profile()])));
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const saveIt = async () => {
    setErr(null);
    try {
      const saved = await api.saveDistros(file([profile()]));
      onSaved(saved[0]);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const tri = (v: boolean | null | undefined) => (v === true ? "yes" : v === false ? "no" : "");
  const fromTri = (s: string) => (s === "yes" ? true : s === "no" ? false : null);
  const avail = (k: "flatpak" | "snap" | "appimage") => (
    <label className="field"><span>{k === "appimage" ? "AppImage" : k[0].toUpperCase() + k.slice(1)}</span>
      <select value={p.universal?.[k] ?? ""} onChange={(e) => setP({ ...p, universal: { ...p.universal, [k]: (e.target.value || null) as Avail | null } })}>
        <option value="">Same as {p.parent ?? "linux"}</option><option value="default">Installed by default</option><option value="available">Can be installed</option><option value="unsupported">Not available</option>
      </select></label>
  );
  return (
    <Modal open wide title={start.id && profiles.some((x) => x.id === start.id && x.layer === "custom") ? `Edit ${start.name ?? start.id}` : "Add a distribution"} onClose={onClose}
      footer={<><button className="btn" onClick={onClose}>Cancel</button><button className="btn" onClick={() => void check()}>Validate and test</button>
        <button className="btn primary" disabled={!report || report.problems.length > 0} onClick={() => void saveIt()} title={report ? undefined : "Validate it first"}>Save</button></>}>
      <div className="grid two">
        <label className="field"><span>Name</span><input type="text" value={p.name ?? ""} placeholder="Nimbus OS" onChange={(e) => setP({ ...p, name: e.target.value })} /></label>
        <label className="field"><span>Profile ID (lowercase, no spaces)</span><input type="text" className="mono" value={p.id} placeholder="nimbus" onChange={(e) => setP({ ...p, id: e.target.value.toLowerCase() })} /></label>
        <label className="field"><span>Recognised by os-release ID</span><input type="text" className="mono" value={ids} placeholder="nimbus" onChange={(e) => setIds(e.target.value)} />
          <span className="hint small muted">The <span className="mono">ID=</span> line of /etc/os-release on that system. Several: separate with spaces.</span></label>
        <label className="field"><span>Based on</span>
          <select value={p.parent ?? ""} onChange={(e) => setP({ ...p, parent: e.target.value || null })}>
            {parents.map((x) => <option key={x.id} value={x.id}>{x.name} ({x.id})</option>)}
          </select>
          <span className="hint small muted">Everything you do not change below comes from it.</span></label>
      </div>
      <details>
        <summary>Differences from {p.parent ?? "its parent"} (optional)</summary>
        <div className="grid two" style={{ marginTop: 10 }}>
          <label className="field"><span>Package manager</span>
            <select value={p.package_manager ?? ""} onChange={(e) => setP({ ...p, package_manager: (e.target.value || null) as Adapter | null })}>
              <option value="">Same as {p.parent ?? "linux"}</option>
              {ADAPTERS.map((a) => <option key={a} value={a}>{ADAPTER_LABEL[a]}</option>)}
            </select></label>
          <label className="field"><span>Read-only (image-based) system</span>
            <select value={tri(p.immutable)} onChange={(e) => setP({ ...p, immutable: fromTri(e.target.value) })}>
              <option value="">Same as {p.parent ?? "linux"}</option><option value="yes">Yes</option><option value="no">No</option></select></label>
          <label className="field"><span>Rolling release</span>
            <select value={tri(p.rolling)} onChange={(e) => setP({ ...p, rolling: fromTri(e.target.value) })}>
              <option value="">Same as {p.parent ?? "linux"}</option><option value="yes">Yes</option><option value="no">No</option></select></label>
          <label className="field"><span>Versions (VERSION_ID) from – to</span>
            <span className="row"><input type="text" aria-label="Lowest version" style={{ width: 90 }} value={p.match?.min_version ?? ""} onChange={(e) => setP({ ...p, match: { ids: [], ...p.match, min_version: e.target.value || null } })} />
              <input type="text" aria-label="Highest version" style={{ width: 90 }} value={p.match?.max_version ?? ""} onChange={(e) => setP({ ...p, match: { ids: [], ...p.match, max_version: e.target.value || null } })} /></span></label>
          {avail("flatpak")}{avail("snap")}{avail("appimage")}
          <label className="field"><span>Its own repositories (one per line, added to the parent's)</span>
            <textarea className="mono" value={repos} placeholder={"nimbus-main\nNimbus*"} onChange={(e) => setRepos(e.target.value)} /></label>
        </div>
      </details>
      {p.package_manager === "custom" && (
        <fieldset className="card stack">
          <legend className="label">Custom package manager</legend>
          <p className="small muted" style={{ margin: 0 }}>One argument per line, no shell. Use <code>{"{packages}"}</code> where the package names go and <code>{"{package}"}</code> for the one name being checked. Simple Setup asks for administrator rights itself. These run only after you trust them.</p>
          <label className="field"><span>Name shown in plans</span><input type="text" value={args.label} placeholder="xbps" onChange={(e) => setArgs({ ...args, label: e.target.value })} /></label>
          <div className="grid two">
            <label className="field"><span>Install</span><textarea className="mono" value={args.install} placeholder={"xbps-install\n-y\n{packages}"} onChange={(e) => setArgs({ ...args, install: e.target.value })} /></label>
            <label className="field"><span>Is it installed? (exit 0 = yes)</span><textarea className="mono" value={args.query} placeholder={"xbps-query\n{package}"} onChange={(e) => setArgs({ ...args, query: e.target.value })} /></label>
          </div>
        </fieldset>
      )}
      {report && <TestView report={report} />}
      <ErrorNote error={err} />
      <p className="small muted" style={{ margin: 0 }}><Download size={13} aria-hidden /> To share it, export it from the list after saving.</p>
    </Modal>
  );
}
