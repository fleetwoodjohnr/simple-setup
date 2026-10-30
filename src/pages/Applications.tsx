import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorMessage } from "../api";
import { useApp } from "../store";
import type { App, AppEdit, Arch, Download, Kind, LocKind, Location, OsFamily, Source } from "../types";
import { Badge, ConfidenceBadge, ErrorNote, Modal, OriginBadge, fmtBytes, osName } from "../ui";

export const SOURCE_LABEL: Record<Source, string> = {
  flatpak: "Flatpak", rpm: "RPM", dpkg: "DEB", pacman: "Pacman", snap: "Snap", app_image: "AppImage",
  custom: "Custom install", git_clone: "Source checkout", manual: "Added by you",
  arp: "Windows program", msix: "Microsoft Store / MSIX", scoop: "Scoop", choco: "Chocolatey",
  brew: "Homebrew", brew_cask: "Homebrew cask", app_bundle: "macOS app", pkg: "macOS installer package", macports: "MacPorts",
};
export const KIND_LABEL: Record<Kind, string> = { desktop: "App", command_line: "Command-line tool", source_build: "Source build" };

export function useFilteredApps(apps: App[], opts: { query: string; source: string; kind: string; showSystem: boolean }) {
  return useMemo(() => {
    const q = opts.query.trim().toLowerCase();
    return apps.filter(
      (a) =>
        (opts.showSystem || !a.system) &&
        (!opts.source || a.source === opts.source) &&
        (!opts.kind || a.kind === opts.kind) &&
        (!q || a.name.toLowerCase().includes(q) || a.native_id.toLowerCase().includes(q)),
    );
  }, [apps, opts.query, opts.source, opts.kind, opts.showSystem]);
}

export function AppFilters(p: {
  apps: App[]; query: string; setQuery: (s: string) => void; source: string; setSource: (s: string) => void;
  kind: string; setKind: (s: string) => void; showSystem: boolean; setShowSystem: (b: boolean) => void;
}) {
  const sources = [...new Set(p.apps.map((a) => a.source))];
  return (
    <div className="row">
      <input type="search" placeholder="Search applications" aria-label="Search applications" value={p.query} onChange={(e) => p.setQuery(e.target.value)} />
      <select aria-label="Installed with" value={p.source} onChange={(e) => p.setSource(e.target.value)}>
        <option value="">All sources</option>
        {sources.map((s) => <option key={s} value={s}>{SOURCE_LABEL[s]}</option>)}
      </select>
      <select aria-label="Type" value={p.kind} onChange={(e) => p.setKind(e.target.value)}>
        <option value="">All types</option>
        {Object.entries(KIND_LABEL).map(([k, v]) => <option key={k} value={k}>{v}</option>)}
      </select>
      <label className="check"><input type="checkbox" checked={p.showSystem} onChange={(e) => p.setShowSystem(e.target.checked)} /> Include apps that came with the system</label>
    </div>
  );
}

export default function Applications() {
  const { inventory, scanning, scan, scanError } = useApp();
  const [query, setQuery] = useState("");
  const [source, setSource] = useState("");
  const [kind, setKind] = useState("");
  const [showSystem, setShowSystem] = useState(false);
  const [detail, setDetail] = useState<App | null>(null);
  const [adding, setAdding] = useState(false);
  const [folders, setFolders] = useState(false);
  const apps = useFilteredApps(inventory?.apps ?? [], { query, source, kind, showSystem });

  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1>Applications</h1>
          <p className="muted">What Simple Setup found on this computer. Correct anything that looks wrong; your changes are marked.</p>
        </div>
        <div className="row">
          <button className="btn" onClick={() => setFolders(true)}>Folders to search…</button>
          <button className="btn" onClick={() => setAdding(true)} disabled={!inventory}>Add app</button>
          <button className="btn" onClick={() => void scan()} disabled={scanning}>{scanning ? "Scanning…" : "Scan again"}</button>
        </div>
      </div>
      <ErrorNote error={scanError} />
      {inventory?.notes.map((n) => <div key={n} className="notice warn small">{n}</div>)}
      {!inventory ? <p className="muted" role="status">{scanning ? "Scanning…" : "Not scanned yet."}</p> : (
        <>
          <AppFilters apps={inventory.apps} {...{ query, setQuery, source, setSource, kind, setKind, showSystem, setShowSystem }} />
          <p className="muted small" role="status">Showing {apps.length} of {inventory.apps.length}. {inventory.hidden_packages} libraries and system packages are not listed.</p>
          <div className="table-wrap">
            <table>
              <thead>
                <tr><th>Name</th><th>Version</th><th>Installed with</th><th>Size</th><th>Settings &amp; data</th><th>Identified</th><th>Available on</th><th>Reinstall</th></tr>
              </thead>
              <tbody>
                {apps.map((a) => (
                  <tr key={a.id}>
                    <td>
                      <button className="btn link" onClick={() => setDetail(a)} aria-label={`Details for ${a.name}`}>{a.name}</button>{" "}
                      {a.system && <Badge>System</Badge>} {a.user_fields.length > 0 && <Badge tone="info">Edited</Badge>}
                      <div className="muted small mono">{a.native_id}</div>
                    </td>
                    <td className="small">{a.version ?? "—"}</td>
                    <td>{SOURCE_LABEL[a.source]}<div className="muted small">{KIND_LABEL[a.kind]}</div></td>
                    <td>{fmtBytes(a.install_size)}</td>
                    <td>{a.locations.length ? `${a.locations.length} location${a.locations.length > 1 ? "s" : ""}` : <span className="muted">None known</span>}
                      {a.locations.some((l) => l.origin === "inferred") && <div><Badge tone="warn">Guessed</Badge></div>}</td>
                    <td><ConfidenceBadge value={a.confidence} /></td>
                    <td className="small">{a.available_on.length ? a.available_on.map(osName).join(", ") : <span className="muted">Unknown</span>}</td>
                    <td className="small">
                      {a.available_on.length > 0 || a.github
                        ? <Badge tone="ok" title="Installed on the new computer from the snapshot, its package managers or a verified download.">Automatic</Badge>
                        : <Badge tone="warn" title="No package or download is known for this app; add a GitHub address or a download link in its details.">By hand</Badge>}
                      {a.github && <div className="muted mono">github.com/{a.github.repo}</div>}
                      {a.locations.length > 0 && <div><Badge tone="ok">Settings/data copyable</Badge></div>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      {detail && <AppDetail app={detail} onClose={() => setDetail(null)} />}
      <AddApp open={adding} onClose={() => setAdding(false)} />
      <SourceFolders open={folders} onClose={() => setFolders(false)} />
    </div>
  );
}

function AppDetail({ app, onClose }: { app: App; onClose: () => void }) {
  const { setInventory } = useApp();
  const [editing, setEditing] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [edit, setEdit] = useState<AppEdit>({
    name: null, version: null, canonical: null, github: app.github?.origin === "user" ? app.github.repo : null,
    github_ref: null, add_locations: [], remove_locations: [], downloads: null,
  });
  const save = async () => {
    try {
      setInventory(app.source === "manual" ? await api.deleteCustomApp(app.id).then(() => api.saveCustomApp(applyEdit(app, edit))) : await api.saveAppEdit(app.id, edit));
      onClose();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const addLocation = async (kind: LocKind) => {
    const path = await open({ directory: true, title: `Choose a ${kind === "config" ? "settings" : "data"} folder for ${app.name}` });
    if (typeof path === "string") setEdit({ ...edit, add_locations: [...edit.add_locations, { kind, path, origin: "user" }] });
  };
  return (
    <Modal open title={app.name} onClose={onClose}>
      <div className="row"><Badge>{SOURCE_LABEL[app.source]}</Badge><ConfidenceBadge value={app.confidence} />{app.canonical && <Badge tone="info">Known as “{app.canonical}”</Badge>}</div>
      <p className="small mono">{app.id}{app.version ? ` · ${app.version}` : ""}</p>
      {app.github && (
        <p className="small">GitHub: <span className="mono">github.com/{app.github.repo}</span>{app.github.reference && ` @ ${app.github.reference}`}{" "}
          <ConfidenceBadge value={app.github.confidence} /> <OriginBadge value={app.github.origin} /></p>
      )}
      <h3>Settings and data locations</h3>
      {app.locations.length === 0 ? <p className="muted small">None known. Add one if this app keeps settings you want to move.</p> : (
        <ul className="plain small">
          {app.locations.map((l) => (
            <li key={l.path}><span className="mono">{l.path}</span> — {l.kind === "config" ? "settings" : "data"} <OriginBadge value={l.origin} />
              {editing && (
                <label className="check" style={{ marginLeft: 8 }}><input type="checkbox" checked={edit.remove_locations.includes(l.path)}
                  onChange={() => setEdit({ ...edit, remove_locations: edit.remove_locations.includes(l.path) ? edit.remove_locations.filter((p) => p !== l.path) : [...edit.remove_locations, l.path] })} /> remove</label>
              )}
            </li>
          ))}
          {edit.add_locations.map((l) => <li key={l.path}><span className="mono">{l.path}</span> — {l.kind} <Badge tone="info">new</Badge></li>)}
        </ul>
      )}
      {(app.downloads?.length ?? 0) > 0 && !editing && (
        <>
          <h3>Installer downloads you registered</h3>
          <ul className="plain small">{app.downloads!.map((d) => <li key={d.url}><span className="mono">{d.target.os}-{d.target.arch}</span>: {d.url}</li>)}</ul>
        </>
      )}
      <details>
        <summary>Why we think this ({app.evidence.length} clues)</summary>
        <ul className="plain small">{app.evidence.map((e) => <li key={e}>{e}</li>)}</ul>
      </details>
      {editing ? (
        <div className="stack">
          <label className="field"><span>Name</span><input type="text" defaultValue={app.name} onChange={(e) => setEdit({ ...edit, name: e.target.value || null })} /></label>
          <label className="field"><span>Version</span><input type="text" defaultValue={app.version ?? ""} onChange={(e) => setEdit({ ...edit, version: e.target.value || null })} /></label>
          <label className="field"><span>GitHub project (URL or owner/name)</span><input type="text" defaultValue={edit.github ?? app.github?.repo ?? ""} onChange={(e) => setEdit({ ...edit, github: e.target.value || null })} /></label>
          <label className="field"><span>GitHub tag or commit</span><input type="text" defaultValue={app.github?.reference ?? ""} onChange={(e) => setEdit({ ...edit, github_ref: e.target.value || null })} /></label>
          <div className="row"><button className="btn" onClick={() => void addLocation("config")}>Add settings folder…</button><button className="btn" onClick={() => void addLocation("data")}>Add data folder…</button></div>
          <DownloadsEditor value={edit.downloads ?? app.downloads ?? []} onChange={(downloads) => setEdit({ ...edit, downloads })} />
          <ErrorNote error={err} />
          <div className="row"><button className="btn primary" onClick={() => void save()}>Save changes</button><button className="btn" onClick={() => setEditing(false)}>Cancel</button></div>
        </div>
      ) : (
        <div className="row">
          <button className="btn" onClick={() => setEditing(true)}>Correct details…</button>
          {app.user_fields.length > 0 && app.source !== "manual" && (
            <button className="btn" onClick={() => void api.saveAppEdit(app.id, { name: null, version: null, canonical: null, github: null, github_ref: null, add_locations: [], remove_locations: [], downloads: null }).then((inv) => { setInventory(inv); onClose(); })}>Undo my corrections</button>
          )}
          {app.source === "manual" && <button className="btn danger" onClick={() => void api.deleteCustomApp(app.id).then((inv) => { setInventory(inv); onClose(); })}>Remove this app</button>}
          <button className="btn" onClick={onClose}>Close</button>
        </div>
      )}
    </Modal>
  );
}

function applyEdit(app: App, e: AppEdit): App {
  return {
    ...app,
    name: e.name ?? app.name,
    version: e.version ?? app.version,
    github: e.github ? { repo: e.github, reference: e.github_ref, asset: null, confidence: "confirmed", origin: "user" } : app.github,
    locations: [...app.locations.filter((l) => !e.remove_locations.includes(l.path)), ...e.add_locations],
    downloads: e.downloads ?? app.downloads,
  };
}

const TARGETS = (["windows", "macos", "linux"] as const).flatMap((os) => (["x86_64", "aarch64"] as const).map((arch) => `${os}-${arch}`));

/** Installer links for other computers: fetched only while a snapshot is built, refused unless the checksum matches. */
function DownloadsEditor({ value, onChange }: { value: Download[]; onChange: (d: Download[]) => void }) {
  const [target, setTarget] = useState(TARGETS[0]);
  const [url, setUrl] = useState("");
  const [sha, setSha] = useState("");
  const ok = url.startsWith("https://") && /^[0-9a-fA-F]{64}$/.test(sha.trim());
  const add = () => {
    const [os, arch] = target.split("-") as [OsFamily, Arch];
    onChange([...value.filter((d) => `${d.target.os}-${d.target.arch}` !== target), { target: { os, arch }, url: url.trim(), sha256: sha.trim() }]);
    setUrl("");
    setSha("");
  };
  return (
    <fieldset className="stack">
      <legend>Installer download for another computer</legend>
      {value.length > 0 && (
        <ul className="plain small">
          {value.map((d) => (
            <li key={`${d.target.os}-${d.target.arch}`}>
              <span className="mono">{d.target.os}-{d.target.arch}</span>: {d.url}{" "}
              <button className="btn" onClick={() => onChange(value.filter((x) => x !== d))}>Remove</button>
            </li>
          ))}
        </ul>
      )}
      <label className="field"><span>For</span>
        <select value={target} onChange={(e) => setTarget(e.target.value)}>{TARGETS.map((t) => <option key={t} value={t}>{t}</option>)}</select>
      </label>
      <label className="field"><span>Download link (https://…)</span><input type="url" value={url} onChange={(e) => setUrl(e.target.value)} /></label>
      <label className="field"><span>SHA-256 checksum from the publisher</span><input type="text" className="mono" value={sha} onChange={(e) => setSha(e.target.value)} /></label>
      <div className="row"><button className="btn" disabled={!ok} onClick={add}>Add download</button></div>
      <p className="muted small">Downloaded only while a snapshot is built, and refused unless the file matches this checksum. Restoring never downloads anything.</p>
    </fieldset>
  );
}

function AddApp({ open: isOpen, onClose }: { open: boolean; onClose: () => void }) {
  const { setInventory } = useApp();
  const [name, setName] = useState("");
  const [kind, setKind] = useState<Kind>("desktop");
  const [version, setVersion] = useState("");
  const [github, setGithub] = useState("");
  const [installer, setInstaller] = useState<string | null>(null);
  const [locations, setLocations] = useState<Location[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const pick = async (kind: LocKind) => {
    const path = await open({ directory: true });
    if (typeof path === "string") setLocations([...locations, { kind, path, origin: "user" }]);
  };
  const save = async () => {
    const app: App = {
      id: "", name, version: version || null, source: "manual", native_id: "", kind, canonical: null, available_on: [],
      repository: null, system: false, install_size: null, path: installer, locations,
      github: github ? { repo: github, reference: null, asset: null, confidence: "confirmed", origin: "user" } : null,
      confidence: "confirmed", evidence: ["Entered by you"], user_fields: [],
    };
    try {
      setInventory(await api.saveCustomApp(app));
      onClose();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  return (
    <Modal open={isOpen} title="Add an application by hand" onClose={onClose}>
      <p className="muted small">For programs Simple Setup did not find, such as portable apps or tools you built yourself.</p>
      <label className="field"><span>Name</span><input type="text" value={name} onChange={(e) => setName(e.target.value)} /></label>
      <label className="field"><span>Type</span>
        <select value={kind} onChange={(e) => setKind(e.target.value as Kind)}>{Object.entries(KIND_LABEL).map(([k, v]) => <option key={k} value={k}>{v}</option>)}</select></label>
      <label className="field"><span>Version (optional)</span><input type="text" value={version} onChange={(e) => setVersion(e.target.value)} /></label>
      <label className="field"><span>GitHub project (optional)</span><input type="text" placeholder="owner/name or https://github.com/…" value={github} onChange={(e) => setGithub(e.target.value)} /></label>
      <div className="row"><button className="btn" onClick={() => void open({ title: "Installer or program file" }).then((p) => typeof p === "string" && setInstaller(p))}>Installer or program file…</button><span className="small mono">{installer}</span></div>
      <div className="row"><button className="btn" onClick={() => void pick("config")}>Add settings folder…</button><button className="btn" onClick={() => void pick("data")}>Add data folder…</button></div>
      <ul className="plain small">{locations.map((l) => <li key={l.path} className="mono">{l.kind}: {l.path}</li>)}</ul>
      <ErrorNote error={err} />
      <div className="row"><button className="btn primary" disabled={!name.trim()} onClick={() => void save()}>Add</button><button className="btn" onClick={onClose}>Cancel</button></div>
    </Modal>
  );
}

function SourceFolders({ open: isOpen, onClose }: { open: boolean; onClose: () => void }) {
  const { scan, saveSettings } = useApp();
  const [dirs, setDirs] = useState<string[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    if (isOpen && dirs === null) void api.getSettings().then((s) => setDirs(s.source_dirs));
  }, [isOpen, dirs]);
  const add = async () => {
    const p = await open({ directory: true, title: "Folder with source checkouts or portable apps" });
    if (typeof p === "string") setDirs([...(dirs ?? []), p]);
  };
  const save = async () => {
    try {
      await saveSettings({ source_dirs: dirs ?? [] });
      onClose();
      void scan();
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  return (
    <Modal open={isOpen} title="Folders to search for source builds and portable apps" onClose={() => { setDirs(null); onClose(); }}>
      <p className="muted small">Simple Setup only looks in standard app locations unless you add folders here. It reads Git metadata (like the GitHub address) but never runs anything.</p>
      <ul className="plain">{(dirs ?? []).map((d) => <li key={d} className="row"><span className="mono">{d}</span><button className="btn link" onClick={() => setDirs((dirs ?? []).filter((x) => x !== d))}>Remove</button></li>)}</ul>
      <button className="btn" onClick={() => void add()}>Add folder…</button>
      <ErrorNote error={err} />
      <div className="row"><button className="btn primary" onClick={() => void save()}>Save and scan</button><button className="btn" onClick={() => { setDirs(null); onClose(); }}>Cancel</button></div>
    </Modal>
  );
}
