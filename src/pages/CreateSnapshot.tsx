import { Fragment, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorMessage } from "../api";
import { FolderTree, MeasureText, useMeasure } from "../FolderTree";
import { useApp } from "../store";
import * as S from "../selection";
import type { App, BuildReport, KnownFolderInfo, Part, Platform, Preflight, Profile, Progress, Volume } from "../types";
import { Badge, ErrorNote, Modal, ProgressBar, StatusBadge, fmtBytes, platformLabel } from "../ui";
import { AppFilters, SOURCE_LABEL, useFilteredApps } from "./Applications";
import { SendDialog } from "./Snapshots";

const STEPS = ["Choose applications", "Choose personal files", "Configure snapshot", "Review and build"];
const PARTS: { part: Part; label: string }[] = [
  { part: "installer", label: "Reinstall" },
  { part: "config", label: "Settings" },
  { part: "data", label: "Data" },
];
const ALL_PLATFORMS: Platform[] = (["linux", "windows", "macos"] as const).flatMap((os) =>
  (["x86_64", "aarch64"] as const).map((arch) => ({ os, arch })),
);

export default function CreateSnapshot() {
  const { step, set, selection, name, destination, inventory, encrypt, password, password2, base } = useApp();
  const [phase, setPhase] = useState<"edit" | "building" | "done" | "stopped">("edit");
  const [progress, setProgress] = useState<BuildProgress>({ warnings: [] });
  const [report, setReport] = useState<BuildReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reviewPf, setReviewPf] = useState<Preflight | null>(null);
  const [confirm, setConfirm] = useState(false);
  const pwProblem = !encrypt ? null : password.length < 8 ? "Use a password of at least 8 characters." : password !== password2 ? "The two passwords do not match." : null;
  const blocker = S.canProceed(step, selection, name, destination) ?? (step >= 3 ? pwProblem : null);

  const build = async () => {
    if (!destination) return;
    setPhase("building");
    setError(null);
    setProgress({ warnings: [] });
    try {
      const r = await api.buildSnapshot(selection, name.trim(), destination, encrypt ? password : null, base?.path ?? null, (p) =>
        setProgress((cur) =>
          p.kind === "stage" ? { ...cur, stage: p, note: undefined } : p.kind === "copy" ? { ...cur, copy: p } : p.kind === "note" ? { ...cur, note: p.message } : { ...cur, warnings: [...cur.warnings, p.message] },
        ),
      );
      setReport(r);
      setPhase("done");
    } catch (e) {
      setError(errorMessage(e));
      setPhase("stopped");
    }
  };

  if (!inventory) return <p className="muted" role="status">Scanning this computer… The snapshot builder opens when the scan finishes.</p>;
  if (phase === "building") return <Building progress={progress} />;
  if (phase === "done" && report) return <Done report={report} onNew={() => { setPhase("edit"); set({ step: 1, selection: S.emptySelection(selection.targets), base: null, password: "", password2: "" }); }} />;

  return (
    <div className="stack">
      <div className="page-head"><div><h1>Create Snapshot</h1><p className="muted">Nothing is copied until you review and press Build.</p></div></div>
      <ol className="steps" aria-label="Steps">
        {STEPS.map((label, i) => (
          <li key={label} aria-current={step === i + 1 ? "step" : undefined} className={step > i + 1 ? "done" : ""}>
            <span className="small muted">Step {i + 1}</span><br />{label}
          </li>
        ))}
      </ol>
      {phase === "stopped" && (
        <div className="notice bad stack" role="alert">
          <strong>The snapshot was not finished.</strong>
          <span>{error}</span>
          <span className="small">Files already copied are kept in a hidden partial folder, so building again with the same name and selection continues where it stopped.</span>
          <div className="row">
            <button className="btn primary" onClick={() => void build()}>Resume build</button>
            <button className="btn" onClick={() => destination && void api.discardPartial(destination, name.trim()).then(() => setPhase("edit"), (e) => setError(errorMessage(e)))}>Discard the partial copy</button>
            <button className="btn" onClick={() => setPhase("edit")}>Back to review</button>
          </div>
        </div>
      )}
      {step === 1 && <ChooseApps />}
      {step === 2 && <ChooseData />}
      {step === 3 && <Configure />}
      {step === 4 && <Review onPreflight={setReviewPf} />}
      <div className="footer-bar">
        <button className="btn" disabled={step === 1} onClick={() => set({ step: step - 1 })}>Back</button>
        <span className="small muted" role="status">{step < 4 && blocker}</span>
        {step < 4 && <button className="btn primary" disabled={!!blocker} onClick={() => set({ step: step + 1 })}>Next: {STEPS[step]}</button>}
        {step === 4 && <button className="btn primary big" disabled={!reviewPf?.fits || !!pwProblem} onClick={() => setConfirm(true)}>Build Snapshot</button>}
      </div>
      <Modal open={confirm} title="Build this snapshot?" onClose={() => setConfirm(false)}>
        <p>Simple Setup will copy {reviewPf?.total_files.toLocaleString()} files ({fmtBytes(reviewPf?.total_bytes)}) into “{name}” at <span className="mono">{destination}</span>, then check every copy.</p>
        {base && <p className="small">This updates “{base.name}”: files that have not changed are taken from it instead of being copied again.</p>}
        {encrypt && <p className="small">The snapshot will be encrypted. After the build you get a recovery key; keep it safe.</p>}
        {!!reviewPf?.installer_bytes && <p className="small">It will also collect offline installers (roughly {fmtBytes(reviewPf.installer_bytes)}). Packages and snaps are downloaded from their usual sources now, so the new computer needs no internet; Flatpak runtimes can take several minutes each.</p>}
        <p className="muted small">Your original files are only read, never changed. Existing files at the destination are never overwritten.</p>
        <div className="row"><button className="btn primary" onClick={() => { setConfirm(false); void build(); }}>Build</button><button className="btn" onClick={() => setConfirm(false)}>Cancel</button></div>
      </Modal>
    </div>
  );
}

// ------------------------------------------------------------------ step 1

function ChooseApps() {
  const { inventory, selection, setSelection } = useApp();
  const [query, setQuery] = useState("");
  const [source, setSource] = useState("");
  const [kind, setKind] = useState("");
  const [showSystem, setShowSystem] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const apps = useFilteredApps(inventory?.apps ?? [], { query, source, kind, showSystem });
  const chosen = (id: string) => selection.apps.find((c) => c.id === id);

  const togglePart = (app: App, part: Part, value: boolean) =>
    setSelection((s) =>
      chosen(app.id) ? S.setPart(s, app.id, part, value) : { ...s, apps: [...s.apps, { id: app.id, installer: false, config: false, data: false, exclude: [], [part]: value }] },
    );

  return (
    <section className="stack" aria-labelledby="s1">
      <h2 id="s1">Which applications should come along?</h2>
      <p className="muted small">For each app choose whether to reinstall it, bring its settings, and/or bring its data. You can bring settings without reinstalling, or reinstall without old data.</p>
      <AppFilters apps={inventory?.apps ?? []} {...{ query, setQuery, source, setSource, kind, setKind, showSystem, setShowSystem }} />
      <div className="spread">
        <div className="row">
          <button className="btn" onClick={() => setSelection((s) => S.setApps(s, apps, true))}>Select all shown ({apps.length})</button>
          <button className="btn" onClick={() => setSelection((s) => S.setApps(s, apps, false))}>Deselect all shown</button>
        </div>
        <strong role="status">{selection.apps.length} selected</strong>
      </div>
      <div className="table-wrap">
        <table>
          <thead><tr><th><span className="sr-only">Include</span></th><th>Application</th>{PARTS.map((p) => <th key={p.part}>{p.label}</th>)}<th>Size</th><th /></tr></thead>
          <tbody>
            {apps.map((a) => {
              const c = chosen(a.id);
              return (
                <Fragment key={a.id}>
                  <tr className={c ? "selected" : ""}>
                    <td><input type="checkbox" aria-label={`Include ${a.name}`} checked={!!c} onChange={(e) => setSelection((s) => S.setApp(s, a, e.target.checked))} /></td>
                    <td>{a.name} {a.system && <Badge>System</Badge>}<div className="muted small">{SOURCE_LABEL[a.source]}{a.version ? ` · ${a.version}` : ""}</div></td>
                    {PARTS.map(({ part, label }) => {
                      const possible = part === "installer" || S.hasLocations(a, part as "config" | "data");
                      return (
                        <td key={part}>
                          <input type="checkbox" aria-label={`${label} for ${a.name}`} disabled={!possible} checked={!!c?.[part]}
                            title={possible ? undefined : "No known location for this app"} onChange={(e) => togglePart(a, part, e.target.checked)} />
                        </td>
                      );
                    })}
                    <td className="small">{fmtBytes(a.install_size)}</td>
                    <td><button className="btn link" aria-expanded={expanded === a.id} onClick={() => setExpanded(expanded === a.id ? null : a.id)}>{expanded === a.id ? "Hide" : "Details"}</button></td>
                  </tr>
                  {expanded === a.id && <tr><td /><td colSpan={6}><AppParts app={a} /></td></tr>}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function AppParts({ app }: { app: App }) {
  const { selection, setSelection } = useApp();
  const c = selection.apps.find((x) => x.id === app.id);
  const deps = app.evidence.filter((e) => e.startsWith("Requires"));
  return (
    <div className="stack small">
      {deps.length > 0 && <div><strong>Needs:</strong> {deps.join("; ")}. This is installed together with the app; no other apps are added.</div>}
      {app.locations.length === 0 && <p className="muted">No settings or data locations are known. You can add them on the Applications page.</p>}
      {app.locations.map((l) => (
        <LocationRow key={l.path} path={l.path} label={l.kind === "config" ? "Settings" : "Data"} guessed={l.origin === "inferred"}
          exclude={c?.exclude ?? []} enabled={!!c?.[l.kind]}
          onToggleExclude={(p) => setSelection((s) => S.setAppExclude(s, app.id, p))}
          secrets={selection.include_secrets} onToggleSecret={(p) => setSelection((s) => ({ ...s, include_secrets: S.toggle(s.include_secrets, p) }))} />
      ))}
    </div>
  );
}

function LocationRow(p: {
  path: string; label: string; guessed?: boolean; exclude: string[]; enabled: boolean;
  onToggleExclude: (p: string) => void; secrets: string[]; onToggleSecret: (p: string) => void;
}) {
  const m = useMeasure(p.path, p.exclude.filter((x) => x.startsWith(p.path)));
  const [browse, setBrowse] = useState(false);
  return (
    <div>
      <div className="row">
        <strong>{p.label}</strong> <span className="mono">{p.path}</span> {p.guessed && <Badge tone="warn">Guessed — check it</Badge>} <MeasureText m={m} />
        {p.enabled && <button className="btn link" aria-expanded={browse} onClick={() => setBrowse(!browse)}>{browse ? "Hide contents" : "Choose contents…"}</button>}
        {!p.enabled && <span className="muted">(not selected)</span>}
      </div>
      {browse && p.enabled && (
        <FolderTree root={p.path} excluded={p.exclude} onToggleExclude={p.onToggleExclude} secretsIncluded={p.secrets} onToggleSecret={p.onToggleSecret} />
      )}
    </div>
  );
}

// ------------------------------------------------------------------ step 2

const DEV_NAMES = ["project", "projects", "Projects", "src", "code", "dev", "git", "repos", "workspace"];

function ChooseData() {
  const { selection, setSelection } = useApp();
  const [known, setKnown] = useState<KnownFolderInfo[]>([]);
  const [dev, setDev] = useState<string[]>([]);
  useEffect(() => {
    void api.knownFolders().then(async (k) => {
      setKnown(k);
      const anyPath = k.find((x) => x.path)?.path;
      if (!anyPath) return;
      const home = anyPath.replace(/[\\/][^\\/]+$/, "");
      const items = await api.listDir(home).catch(() => []);
      setDev(items.filter((i) => i.is_dir && DEV_NAMES.includes(i.name)).map((i) => i.path));
    });
  }, []);
  const has = (path: string) => selection.folders.some((f) => f.path === path);
  const addPicked = async (directory: boolean) => {
    const p = await open({ directory, title: directory ? "Choose a folder to include" : "Choose a file to include" });
    if (typeof p === "string" && !has(p)) setSelection((s) => S.setFolder(s, p, null, true));
  };
  const others = selection.folders.filter((f) => !f.known && !dev.includes(f.path));

  return (
    <section className="stack" aria-labelledby="s2">
      <h2 id="s2">Which personal files should come along?</h2>
      <p className="muted small">Passwords, keys and tokens (for example SSH keys, password databases, browser logins) are left out automatically. You can include one on purpose from a folder's contents.</p>
      <div className="card stack">
        <h3>Standard folders</h3>
        {known.filter((k) => k.exists && k.path).map((k) => (
          <FolderRow key={k.folder} title={k.folder} path={k.path!} checked={has(k.path!)}
            onCheck={(on) => setSelection((s) => S.setFolder(s, k.path!, k.folder, on))} />
        ))}
      </div>
      {dev.length > 0 && (
        <div className="card stack">
          <h3>Development projects</h3>
          {dev.map((d) => <FolderRow key={d} title={d.split(/[\\/]/).pop()!} path={d} checked={has(d)} onCheck={(on) => setSelection((s) => S.setFolder(s, d, null, on))} />)}
          <p className="muted small">Rebuildable folders like node_modules and caches are skipped.</p>
        </div>
      )}
      <div className="card stack">
        <div className="spread"><h3>Other folders and files</h3>
          <div className="row"><button className="btn" onClick={() => void addPicked(true)}>Add folder…</button><button className="btn" onClick={() => void addPicked(false)}>Add file…</button></div></div>
        {others.length === 0 ? <p className="muted small">None added.</p> : others.map((f) => (
          <FolderRow key={f.path} title={f.path.split(/[\\/]/).pop()!} path={f.path} checked onCheck={(on) => setSelection((s) => S.setFolder(s, f.path, null, on))} />
        ))}
      </div>
    </section>
  );
}

function FolderRow({ title, path, checked, onCheck }: { title: string; path: string; checked: boolean; onCheck: (on: boolean) => void }) {
  const { selection, setSelection } = useApp();
  const f = selection.folders.find((x) => x.path === path);
  const m = useMeasure(checked ? path : null, f?.exclude ?? []);
  const [browse, setBrowse] = useState(false);
  return (
    <div>
      <div className="row">
        <label className="check"><input type="checkbox" checked={checked} onChange={(e) => onCheck(e.target.checked)} /> <strong>{title}</strong></label>
        <span className="mono small muted">{path}</span>
        {checked && <MeasureText m={m} />}
        {checked && f && f.exclude.length > 0 && <Badge>{f.exclude.length} left out</Badge>}
        {checked && <button className="btn link" aria-expanded={browse} onClick={() => setBrowse(!browse)}>{browse ? "Hide contents" : "Choose contents…"}</button>}
      </div>
      {checked && browse && f && (
        <FolderTree root={path} excluded={f.exclude} onToggleExclude={(p) => setSelection((s) => S.setFolderExclude(s, path, p))}
          secretsIncluded={selection.include_secrets} onToggleSecret={(p) => setSelection((s) => ({ ...s, include_secrets: S.toggle(s.include_secrets, p) }))} />
      )}
    </div>
  );
}

// ------------------------------------------------------------------ step 3

function usePreflight() {
  const { selection, destination } = useApp();
  const [pf, setPf] = useState<Preflight | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const key = JSON.stringify([selection, destination]);
  useEffect(() => {
    let live = true;
    setPf(null);
    setErr(null);
    const t = setTimeout(() => {
      api.preflight(selection, destination).then((r) => live && setPf(r), (e) => live && setErr(errorMessage(e)));
    }, 300);
    return () => { live = false; clearTimeout(t); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return { pf, err };
}

function Configure() {
  const { selection, setSelection, name, destination, set, device, inventory } = useApp();
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [engines, setEngines] = useState<Platform[]>([]);
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [profileName, setProfileName] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const { pf, err } = usePreflight();
  useEffect(() => {
    void api.volumes().then(setVolumes);
    void api.engines().then(setEngines);
    void api.listProfiles().then(setProfiles);
  }, []);
  const pick = async () => {
    const p = await open({ directory: true, title: "Where should the snapshot be saved?" });
    if (typeof p === "string") set({ destination: p });
  };
  const loadProfile = (p: Profile) => {
    const r = S.reconcile(p.selection, inventory?.apps ?? []);
    set({ selection: r.selection });
    setNote(r.missing.length ? `Loaded “${p.name}”. Not on this computer any more: ${r.missing.join(", ")}.` : `Loaded “${p.name}”.`);
  };
  const saveProfile = async () => {
    try {
      await api.saveProfile(profileName, selection);
      setProfiles(await api.listProfiles());
      setNote(`Saved profile “${profileName}”.`);
    } catch (e) {
      setNote(errorMessage(e));
    }
  };
  const isHere = (p: Platform) => !!device && S.samePlatform(p, device.platform);

  return (
    <section className="stack" aria-labelledby="s3">
      <h2 id="s3">Configure the snapshot</h2>
      <div className="grid">
        <div className="card stack">
          <label className="field"><span>Snapshot name</span><input type="text" value={name} onChange={(e) => set({ name: e.target.value })} /></label>
          <fieldset className="stack" style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="field"><span>Save to</span></legend>
            {volumes.map((v) => (
              <label key={v.mount} className="check">
                <input type="radio" name="dest" checked={destination === v.mount} onChange={() => set({ destination: v.mount })} />
                {v.name || v.mount} <span className="muted small">{v.mount} · {fmtBytes(v.free)} free of {fmtBytes(v.total)}</span>
                {v.removable && <Badge tone="info">External</Badge>}
              </label>
            ))}
            {volumes.length === 0 && <p className="muted small">No external drives found. Connect one, or choose any folder.</p>}
            <div className="row"><button className="btn" onClick={() => void pick()}>Choose folder…</button>
              <button className="btn link" onClick={() => void api.volumes().then(setVolumes)}>Refresh drives</button></div>
            <label className="field"><span>Or type a folder path</span>
              <input type="text" className="mono" value={destination ?? ""} placeholder="/run/media/you/USB-DRIVE" onChange={(e) => set({ destination: e.target.value.trim() || null })} /></label>
            <p className="small muted">Moving to another computer on this network? Save the snapshot to any folder here; when it is built, choose <strong>Send to another computer</strong>. The other computer shows a pairing code and must accept the transfer.</p>
          </fieldset>
        </div>
        <div className="card stack">
          <fieldset style={{ border: 0, padding: 0, margin: 0 }}>
            <legend className="field"><span>Restore on</span></legend>
            <p className="muted small">Choose the kind of computer you are moving to. Restore tools are only added for these.</p>
            {ALL_PLATFORMS.map((p) => {
              const on = selection.targets.some((t) => S.samePlatform(t, p));
              const engine = engines.some((e) => S.samePlatform(e, p));
              return (
                <div key={`${p.os}-${p.arch}`} className="row">
                  <label className="check"><input type="checkbox" checked={on} onChange={(e) => setSelection((s) => S.setTarget(s, p, e.target.checked))} /> {platformLabel(p)}</label>
                  {isHere(p) && <Badge tone="info">This computer</Badge>}
                  {on && !engine && <Badge tone="warn" title="This copy of Simple Setup has no restore program for that system">No restore program available</Badge>}
                </div>
              );
            })}
          </fieldset>
          <Encryption />
        </div>
        <div className="card stack">
          <h3>Profiles</h3>
          <p className="muted small">Save these choices (for example “Work Setup”) to reuse next time.</p>
          {profiles.map((p) => (
            <div key={p.name} className="row">
              <span>{p.name}</span><span className="muted small">{p.selection.apps.length} apps · {p.selection.folders.length} folders</span>
              <button className="btn link" onClick={() => loadProfile(p)}>Use</button>
              <button className="btn link" onClick={() => void api.deleteProfile(p.name).then(() => api.listProfiles()).then(setProfiles)}>Delete</button>
            </div>
          ))}
          <div className="row"><input type="text" aria-label="Profile name" placeholder="Profile name" value={profileName} onChange={(e) => setProfileName(e.target.value)} />
            <button className="btn" disabled={!profileName.trim()} onClick={() => void saveProfile()}>Save current choices</button></div>
          {note && <p className="small" role="status">{note}</p>}
        </div>
      </div>
      <PreflightView pf={pf} err={err} />
    </section>
  );
}

function PreflightView({ pf, err }: { pf: Preflight | null; err: string | null }) {
  const { inventory, selection } = useApp();
  if (err) return <ErrorNote error={err} />;
  if (!pf) return <p className="muted" role="status">Working out size and coverage…</p>;
  const appName = (id: string) => inventory?.apps.find((a) => a.id === id)?.name ?? id;
  return (
    <div className="stack">
      <div className="card">
        <div className="spread">
          <div><strong>{fmtBytes(pf.total_bytes)}</strong> in {pf.total_files.toLocaleString()} files, plus {fmtBytes(pf.engine_bytes)} of restore tools
            {!!pf.installer_bytes && <> and roughly {fmtBytes(pf.installer_bytes)} of offline installers (exact size known after the build)</>}</div>
          {pf.destination_free != null && (pf.fits
            ? <Badge tone="ok">{fmtBytes(pf.destination_free)} free at destination</Badge>
            : <Badge tone="bad">Not enough space: needs {fmtBytes(pf.required_bytes)}, {fmtBytes(pf.destination_free)} free</Badge>)}
        </div>
      </div>
      {pf.readiness.map((r) => (
        <div key={`${r.target.os}-${r.target.arch}`} className={`notice ${r.offline_ready ? "ok" : "warn"}`}>
          <strong>{platformLabel(r.target)}: {r.offline_ready ? "everything can be restored offline" : "not fully offline-ready"}</strong>
          {r.reasons.length > 0 && <ul className="plain small">{r.reasons.map((x) => <li key={x}>{x}</li>)}</ul>}
        </div>
      ))}
      {selection.apps.length > 0 && (
        <div className="table-wrap">
          <table>
            <caption className="sr-only">Offline coverage per application and target</caption>
            <thead><tr><th>Application</th>{selection.targets.map((t) => <th key={`${t.os}-${t.arch}`}>{platformLabel(t)}</th>)}</tr></thead>
            <tbody>
              {selection.apps.map((c) => (
                <tr key={c.id}>
                  <td>{appName(c.id)}</td>
                  {selection.targets.map((t) => (
                    <td key={`${t.os}-${t.arch}`} className="small">
                      {pf.coverage.filter((x) => x.app === c.id && S.samePlatform(x.target, t)).map((x) => (
                        <div key={x.part} title={x.reason}>{PARTS.find((p) => p.part === x.part)?.label}: <StatusBadge status={x.status} /></div>
                      ))}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {pf.warnings.map((w) => <div key={w} className="notice warn small">{w}</div>)}
    </div>
  );
}

// ------------------------------------------------------------------ step 4

function Review({ onPreflight }: { onPreflight: (pf: Preflight | null) => void }) {
  const { selection, inventory, name, destination, set } = useApp();
  const { pf, err } = usePreflight();
  useEffect(() => onPreflight(pf), [pf, onPreflight]);
  const appName = useMemo(() => new Map((inventory?.apps ?? []).map((a) => [a.id, a.name])), [inventory]);
  const edit = (step: number) => <button className="btn link" onClick={() => set({ step })}>Change</button>;
  return (
    <section className="stack" aria-labelledby="s4">
      <h2 id="s4">Review</h2>
      <div className="card stack">
        <div className="spread"><h3>Applications ({selection.apps.length})</h3>{edit(1)}</div>
        {selection.apps.length === 0 ? <p className="muted small">None.</p> : (
          <ul className="plain">
            {selection.apps.map((c) => (
              <li key={c.id}>{appName.get(c.id) ?? c.id}: {PARTS.filter((p) => c[p.part]).map((p) => p.label.toLowerCase()).join(", ")}
                {c.exclude.length > 0 && <span className="muted small"> · {c.exclude.length} item(s) left out</span>}</li>
            ))}
          </ul>
        )}
        <div className="spread"><h3>Personal files ({selection.folders.length})</h3>{edit(2)}</div>
        {selection.folders.length === 0 ? <p className="muted small">None.</p> : (
          <ul className="plain">{selection.folders.map((f) => <li key={f.path} className="mono small">{f.path}{f.exclude.length > 0 && ` (${f.exclude.length} left out)`}</li>)}</ul>
        )}
        {selection.include_secrets.length > 0 && <div className="notice bad small">Sensitive items you chose to include: {selection.include_secrets.join(", ")}</div>}
        <div className="spread"><h3>Snapshot</h3>{edit(3)}</div>
        <p className="small">“{name}” → <span className="mono">{destination}</span> · for {selection.targets.map(platformLabel).join(", ")}</p>
      </div>
      {pf && (
        <div className="card small">
          <strong>Left out automatically:</strong> {pf.excluded.secrets} sensitive item(s), {pf.excluded.transient} cache/lock item(s), {pf.excluded.by_user} you excluded, {pf.excluded.unreadable} unreadable.
          {pf.excluded.secret_examples.length > 0 && <details><summary>Protected items</summary><ul className="plain">{pf.excluded.secret_examples.map((s) => <li key={s} className="mono">{s}</li>)}</ul></details>}
          {pf.excluded.unreadable_examples.length > 0 && <details><summary>Could not read</summary><ul className="plain">{pf.excluded.unreadable_examples.map((s) => <li key={s} className="mono">{s}</li>)}</ul></details>}
        </div>
      )}
      <PreflightView pf={pf} err={err} />
    </section>
  );
}

type BuildProgress = { stage?: Progress; copy?: Progress; note?: string; warnings: string[] };

function Building({ progress }: { progress: BuildProgress }) {
  const stage = progress.stage?.kind === "stage" ? progress.stage : null;
  const copy = progress.copy?.kind === "copy" ? progress.copy : null;
  const installers = stage?.label.startsWith("Preparing installers");
  return (
    <section className="stack" aria-live="polite" aria-busy>
      <h1>Building snapshot…</h1>
      {stage && <p><strong>Step {stage.step} of {stage.steps}:</strong> {stage.label}</p>}
      <ProgressBar value={stage?.step ?? 0} max={stage?.steps ?? 1} label="Overall progress" />
      {copy && installers && (
        <>
          <ProgressBar value={copy.done_files} max={copy.total_files || 1} label="Installers prepared" />
          <p className="small">{copy.done_files} of {copy.total_files} app installer(s) ready</p>
          {progress.note && <p className="small muted">{progress.note}</p>}
        </>
      )}
      {copy && !installers && (
        <>
          <ProgressBar value={copy.done_bytes} max={copy.total_bytes || 1} label="Current step progress" />
          <p className="small">{copy.done_files.toLocaleString()} of {copy.total_files.toLocaleString()} files · {fmtBytes(copy.done_bytes)} of {fmtBytes(copy.total_bytes)}</p>
          <p className="small mono muted" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{copy.current}</p>
        </>
      )}
      {progress.warnings.map((w, i) => <div key={i} className="notice warn small">{w}</div>)}
      <div><button className="btn danger" onClick={() => void api.cancelBuild()}>Cancel</button></div>
    </section>
  );
}

function Encryption() {
  const { encrypt, password, password2, base, set } = useApp();
  return (
    <fieldset className="stack" style={{ border: 0, padding: 0, margin: 0 }}>
      <legend className="sr-only">Encryption</legend>
      {base && (
        <div className="notice small">Updating “{base.name}”: unchanged files are taken from it. <button className="btn link" onClick={() => set({ base: null })}>Make a separate snapshot instead</button></div>
      )}
      <label className="check"><input type="checkbox" checked={encrypt} onChange={(e) => set({ encrypt: e.target.checked })} /> Encrypt the snapshot with a password</label>
      {encrypt && (
        <>
          <p className="small muted">File names and contents are encrypted. Installer downloads (public files) are not. Without the password or the recovery key shown after the build, nobody can open the snapshot — not even you.</p>
          <label className="field"><span>Password (at least 8 characters)</span><input type="password" autoComplete="new-password" value={password} onChange={(e) => set({ password: e.target.value })} /></label>
          <label className="field"><span>Type it again</span><input type="password" autoComplete="new-password" value={password2} onChange={(e) => set({ password2: e.target.value })} /></label>
        </>
      )}
    </fieldset>
  );
}

function Done({ report, onNew }: { report: BuildReport; onNew: () => void }) {
  const { go, set, selection } = useApp();
  const [sending, setSending] = useState(false);
  return (
    <section className="stack">
      <h1>{report.failed.length ? "Snapshot built with problems" : "Snapshot built and verified"}</h1>
      {report.recovery_key && (
        <div className="notice warn stack" role="alert">
          <strong>Write down this recovery key and keep it away from the drive</strong>
          <span className="small">It opens the snapshot if the password is forgotten. It is shown only now and is not stored anywhere.</span>
          <code className="mono" style={{ userSelect: "all", wordBreak: "break-all" }}>{report.recovery_key}</code>
        </div>
      )}
      {report.reused_files > 0 && <p className="small">{report.reused_files} file(s), {fmtBytes(report.reused_bytes)}, were taken from the earlier snapshot instead of copied again.</p>}
      <div className="card">
        <p>“{report.name}” — {report.files.toLocaleString()} files, {fmtBytes(report.bytes)}{report.resumed_files ? ` (${report.resumed_files} reused from the interrupted build)` : ""}.</p>
        <p className="mono small">{report.path}</p>
        <p className="small muted">Every copied file was read back and matched its checksum.</p>
      </div>
      {report.failed.length > 0 && (
        <div className="notice bad">
          <strong>{report.failed.length} file(s) could not be copied</strong> (they are not in the snapshot):
          <ul className="plain small">{report.failed.slice(0, 50).map((f) => <li key={f.path}><span className="mono">{f.path}</span>: {f.error}</li>)}</ul>
          <p className="small">Close any app that may be using them, check permissions, then build again under a new name.</p>
        </div>
      )}
      {report.readiness.map((r) => (
        <div key={`${r.target.os}-${r.target.arch}`} className={`notice ${r.offline_ready ? "ok" : "warn"}`}>
          <strong>{platformLabel(r.target)}: {r.offline_ready ? "ready to restore offline" : "not fully offline-ready"}</strong>
          <ul className="plain small">{r.reasons.map((x) => <li key={x}>{x}</li>)}</ul>
        </div>
      ))}
      {report.warnings.map((w) => <div key={w} className="notice warn small">{w}</div>)}
      <p className="small muted">Left out: {report.excluded.secrets} sensitive, {report.excluded.transient} cache/lock, {report.excluded.by_user} by you, {report.excluded.unreadable} unreadable. A full report is in the snapshot's reports folder.</p>
      <div className="row">
        <button className="btn primary" onClick={() => go("snapshots")}>View snapshots</button>
        <button className="btn" onClick={() => setSending(true)}>Send to another computer…</button>
        <button className="btn" onClick={() => { set({ restorePath: report.path }); go("restore"); }}>Preview a restore</button>
        <button className="btn" onClick={onNew}>Start a new snapshot</button>
      </div>
      {sending && <SendDialog summary={{ path: report.path, name: report.name, targets: selection.targets }} onClose={() => setSending(false)} />}
    </section>
  );
}
