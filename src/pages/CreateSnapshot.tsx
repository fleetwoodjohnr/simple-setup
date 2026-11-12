import { Fragment, useEffect, useMemo, useState } from "react";
import { Check, Laptop, Monitor, Network, Shuffle } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errorMessage } from "../api";
import { Compatibility, rowsFromCoverage } from "../Compatibility";
import { FolderTree, MeasureText, useMeasure } from "../FolderTree";
import { useApp, type DestChoice } from "../store";
import * as S from "../selection";
import type { App, Arch, BuildReport, KnownFolderInfo, OsFamily, Part, Peer, Platform, Preflight, Probed, Profile, Progress, SendReport, Volume } from "../types";
import { Badge, ErrorNote, Modal, ProgressBar, Spinner, archName, distroName, fmtBytes, osName, platformLabel, targetLabel } from "../ui";
import { AppFilters, SOURCE_LABEL, useFilteredApps } from "./Applications";
import { SendDialog } from "./Snapshots";

const STEPS = {
  snapshot: ["Applications", "Files", "Destination", "Compatibility", "Create"],
  transfer: ["Applications", "Files", "Connect", "Compatibility", "Start migration"],
};
const PARTS: { part: Part; label: string }[] = [
  { part: "installer", label: "Reinstall" },
  { part: "config", label: "Settings" },
  { part: "data", label: "Data" },
];
const OSES: OsFamily[] = ["linux", "windows", "macos"];
const ARCHS: Arch[] = ["x86_64", "aarch64"];

/** The guided flow: choose apps and files, say where they go, see what will happen, start.
 * In Snapshot mode the result is saved to a folder or drive; in Transfer mode it is built in a
 * temporary folder and sent straight to the computer paired in step 3. */
export default function CreateSnapshot() {
  const { step, set, selection, name, destination, inventory, encrypt, password, password2, base, mode, peer, device, startFlow } = useApp();
  const [phase, setPhase] = useState<"edit" | "building" | "sending" | "done" | "stopped">("edit");
  const [progress, setProgress] = useState<BuildProgress>({ warnings: [] });
  const [report, setReport] = useState<BuildReport | null>(null);
  const [sent, setSent] = useState<SendReport | null>(null);
  const [sendProgress, setSendProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reviewPf, setReviewPf] = useState<Preflight | null>(null);
  const [confirm, setConfirm] = useState(false);
  const transfer = mode === "transfer";
  const steps = STEPS[mode];
  const pwProblem = !encrypt || transfer ? null : password.length < 8 ? "Use a password of at least 8 characters." : password !== password2 ? "The two passwords do not match." : null;
  const blocker = S.canProceed(step, selection, name, destination, transfer ? { connected: !!peer } : undefined) ?? (step >= 3 ? pwProblem : null);

  const send = async (r: BuildReport) => {
    if (!peer) return;
    setPhase("sending");
    try {
      const done = await api.sendSnapshot(r.path, peer.address, peer.code, (p) => setSendProgress(p));
      setSent(done);
      // The copy here was only for sending; the other computer has checked its own.
      await api.deleteSnapshot(r.path, r.name).catch(() => {});
      setPhase("done");
    } catch (e) {
      setError(errorMessage(e));
      setPhase("stopped");
    }
  };
  const build = async () => {
    setPhase("building");
    setError(null);
    setProgress({ warnings: [] });
    try {
      const dest = transfer ? await api.outgoingFolder() : destination;
      if (!dest) return;
      const snapName = transfer ? transferName(device?.hostname, peer?.probed.device.hostname) : name.trim();
      const r = await api.buildSnapshot(selection, snapName, dest, encrypt && !transfer ? password : null, base?.path ?? null, peer?.probed.info ?? null, (p) =>
        setProgress((cur) =>
          p.kind === "stage" ? { ...cur, stage: p, note: undefined } : p.kind === "copy" ? { ...cur, copy: p } : p.kind === "note" ? { ...cur, note: p.message } : p.kind === "warning" ? { ...cur, warnings: [...cur.warnings, p.message] } : cur,
        ),
      );
      setReport(r);
      if (transfer) await send(r);
      else setPhase("done");
    } catch (e) {
      setError(errorMessage(e));
      setPhase("stopped");
    }
  };

  if (!inventory) return <p className="muted" role="status"><Spinner /> Scanning this computer… This opens when the scan finishes.</p>;
  if (phase === "building") return <Building progress={progress} title={transfer ? "Preparing the migration…" : "Building snapshot…"} />;
  if (phase === "sending") return <Sending progress={sendProgress} to={peer?.probed.device.hostname ?? "the other computer"} />;
  if (phase === "done" && report) {
    const again = () => { setPhase("edit"); set({ step: 1, base: null, password: "", password2: "", peer: null }); set({ selection: { ...S.emptySelection(selection.targets), linux_distro: selection.linux_distro, decide_on_destination: selection.decide_on_destination } }); };
    return transfer && sent ? <Sent report={report} sent={sent} probed={peer?.probed ?? null} onNew={again} /> : <Done report={report} onNew={again} />;
  }

  return (
    <div className="stack">
      <div className="page-head">
        <div>
          <h1>{transfer ? "Transfer to Another Computer" : "Create Snapshot"}</h1>
          <p className="muted">{transfer
            ? "Pick what to take along, connect to the new computer, check what will happen there, then start. It is sent over this network, encrypted, and checked file by file."
            : "Pick what to take along and where it is going. Nothing is copied until you review it and press Create."}</p>
        </div>
        <button className="btn ghost" onClick={() => startFlow(transfer ? "snapshot" : "transfer")}>
          <Shuffle size={15} aria-hidden /> {transfer ? "Save a snapshot instead" : "Send to a computer on this network instead"}
        </button>
      </div>
      <ol className="steps" aria-label="Steps">
        {steps.map((label, i) => (
          <li key={label} aria-current={step === i + 1 ? "step" : undefined} className={step > i + 1 ? "done" : ""}>
            <span className="num" aria-hidden>{step > i + 1 ? <Check size={13} /> : i + 1}</span>
            <span><span className="sr-only">Step {i + 1}: </span>{label}</span>
          </li>
        ))}
      </ol>
      {phase === "stopped" && (
        <div className="notice bad stack" role="alert">
          <strong>{transfer ? "The migration stopped." : "The snapshot was not finished."}</strong>
          <span>{error}</span>
          <span className="small">{transfer && report
            ? "Everything was prepared; sending again continues where it stopped, and the other computer keeps what already arrived."
            : "Files already copied are kept in a hidden partial folder, so starting again with the same choices continues where it stopped."}</span>
          <div className="row">
            {transfer && report ? <button className="btn primary" onClick={() => void send(report)}>Send again</button> : <button className="btn primary" onClick={() => void build()}>Resume</button>}
            {!transfer && <button className="btn" onClick={() => destination && void api.discardPartial(destination, name.trim()).then(() => setPhase("edit"), (e) => setError(errorMessage(e)))}>Discard the partial copy</button>}
            <button className="btn" onClick={() => setPhase("edit")}>Back to review</button>
          </div>
        </div>
      )}
      {step === 1 && <ChooseApps />}
      {step === 2 && <ChooseData />}
      {step === 3 && (transfer ? <Connect /> : <Destination />)}
      {step === 4 && <CompatibilityStep />}
      {step === 5 && <Review onPreflight={setReviewPf} />}
      <div className="footer-bar">
        <button className="btn" disabled={step === 1} onClick={() => set({ step: step - 1 })}>Back</button>
        <span className="small muted" role="status">{step < 5 && blocker}</span>
        {step < 5 && <button className="btn primary" disabled={!!blocker} onClick={() => set({ step: step + 1 })}>Next: {steps[step]}</button>}
        {step === 5 && <button className="btn primary big" disabled={!reviewPf?.fits || !!pwProblem} onClick={() => setConfirm(true)}>{transfer ? "Start Migration" : "Create Snapshot"}</button>}
      </div>
      <Modal open={confirm} title={transfer ? "Start the migration?" : "Create this snapshot?"} onClose={() => setConfirm(false)}
        footer={<><button className="btn" onClick={() => setConfirm(false)}>Cancel</button><button className="btn primary" onClick={() => { setConfirm(false); void build(); }}>{transfer ? "Start" : "Create"}</button></>}>
        {transfer ? (
          <p>Simple Setup prepares {reviewPf?.total_files.toLocaleString()} files ({fmtBytes(reviewPf?.total_bytes)}) and the installers that fit {peer?.probed.device.hostname}, then sends them. That computer must accept the transfer; it checks every file as it arrives.</p>
        ) : (
          <p>Simple Setup will copy {reviewPf?.total_files.toLocaleString()} files ({fmtBytes(reviewPf?.total_bytes)}) into “{name}” at <span className="mono">{destination}</span>, then check every copy.</p>
        )}
        {base && <p className="small">This updates “{base.name}”: files that have not changed are taken from it instead of being copied again.</p>}
        {encrypt && !transfer && <p className="small">The snapshot will be encrypted. Afterwards you get a recovery key; keep it safe.</p>}
        {!!reviewPf?.installer_bytes && <p className="small">Installers are collected too (roughly {fmtBytes(reviewPf.installer_bytes)}), so the new computer needs less internet; Flatpak runtimes can take several minutes each.</p>}
        <p className="muted small">Your original files are only read, never changed. Nothing existing is overwritten.</p>
      </Modal>
    </div>
  );
}

function transferName(from?: string, to?: string) {
  const clean = (s?: string) => (s ?? "computer").replace(/[^\p{L}\p{N} ._-]/gu, "-").slice(0, 40);
  const at = new Date().toISOString().slice(0, 16).replace("T", " ").replace(":", "-");
  return `${clean(from)} to ${clean(to)} ${at}`;
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
                    <td>{a.name} {a.system && <Badge>System</Badge>}<div className="muted small">{SOURCE_LABEL[a.source]}{a.version ? ` · ${a.version}` : ""}
                      {a.github ? ` · github.com/${a.github.repo}` : ""}{a.available_on.length ? ` · installs on ${a.available_on.map(osName).join(", ")}` : ""}</div></td>
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
  const { selection, destination, peer, mode } = useApp();
  const [pf, setPf] = useState<Preflight | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const remote = mode === "transfer" ? peer?.probed.info ?? null : null;
  const key = JSON.stringify([selection, mode === "transfer" ? null : destination, remote?.device.hostname, remote?.linux?.profile.id]);
  useEffect(() => {
    let live = true;
    setPf(null);
    setErr(null);
    const t = setTimeout(() => {
      api.preflight(selection, mode === "transfer" ? null : destination, remote).then((r) => live && setPf(r), (e) => live && setErr(errorMessage(e)));
    }, 300);
    return () => { live = false; clearTimeout(t); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return { pf, err };
}

const CHOICES: { value: Exclude<DestChoice, "connect">; title: string; text: string; icon: typeof Laptop }[] = [
  { value: "same", title: "A computer like this one", text: "Same system and distribution. Installers that fit it are included, so it restores without internet.", icon: Laptop },
  { value: "choose", title: "Another kind of computer", text: "Pick its system, distribution and processor. Installers are chosen for it.", icon: Monitor },
  { value: "decide", title: "Not decided yet", text: "Restore programs for every system come along; each app is matched to the new computer when you restore.", icon: Shuffle },
];

/** Snapshot mode: where it is going, and where it is saved. */
function Destination() {
  const { selection, setSelection, destChoice, set, device, startFlow } = useApp();
  const [engines, setEngines] = useState<Platform[]>([]);
  useEffect(() => { void api.engines().then(setEngines); }, []);
  const choose = (c: Exclude<DestChoice, "connect">) => {
    set({ destChoice: c });
    if (!device) return;
    if (c === "choose") setSelection((s) => ({ ...s, decide_on_destination: false, linux_distro: s.linux_distro === "linux" ? null : s.linux_distro }));
    else setSelection((s) => ({ ...s, ...S.destinationTargets(c, device.platform, engines) }));
  };
  return (
    <section className="stack" aria-labelledby="s3">
      <h2 id="s3">Where is it going?</h2>
      <fieldset className="choices">
        <legend className="sr-only">Destination</legend>
        {CHOICES.map(({ value, title, text, icon: Icon }) => (
          <label key={value} className="choice">
            <input type="radio" name="destchoice" checked={destChoice === value} onChange={() => choose(value)} />
            <span className="icon-chip"><Icon size={18} aria-hidden /></span>
            <span><strong>{title}</strong><span>{value === "same" && device ? `${device.os_name}, ${platformLabel(device.platform).split(" ")[1]}. ` : ""}{text}</span></span>
          </label>
        ))}
        <button type="button" className="choice" onClick={() => startFlow("transfer")}>
          <span className="icon-chip"><Network size={18} aria-hidden /></span>
          <span><strong>A computer on this network</strong><span>Connect to it now: Simple Setup sees exactly what it is and sends everything there.</span></span>
        </button>
      </fieldset>
      {destChoice === "choose" && <ChooseSystems engines={engines} />}
      {destChoice === "decide" && (
        <div className="notice info small">Restore programs for {selection.targets.map(platformLabel).join(", ")} come along. Apps are not promised offline: the new computer installs each one with its own package manager, Flatpak or a verified download, and the restore shows what it found before anything starts.</div>
      )}
      <SaveSettings />
    </section>
  );
}

/** The systems, distributions and processors to prepare for (several at once if wanted). */
function ChooseSystems({ engines }: { engines: Platform[] }) {
  const { selection, setSelection, device, distros } = useApp();
  const isHere = (p: Platform) => !!device && S.samePlatform(p, device.platform);
  const linuxOn = selection.targets.some((t) => t.os === "linux");
  const fromLinux = device?.platform.os === "linux";
  const usable = distros.filter((d) => d.enabled && d.problems.length === 0 && d.id !== "linux");
  const families = [...new Set(usable.map((d) => d.family))];
  return (
    <div className="card stack">
      <fieldset className="stack" style={{ gap: 10 }}>
        <legend className="label">Prepare for</legend>
        {OSES.map((os) => (
          <div key={os} className="stack" style={{ gap: 6 }}>
            <strong>{osName(os)}</strong>
            {os === "linux" && (
              <select aria-label="Linux distribution" disabled={!linuxOn} value={selection.linux_distro ?? ""} style={{ width: "100%" }}
                onChange={(e) => setSelection((s) => ({ ...s, linux_distro: e.target.value || null }))}>
                <option value="">{fromLinux ? `Same as this computer (${device?.os_name})` : "Any distribution"}</option>
                {families.map((f) => (
                  <optgroup key={f} label={f}>
                    {usable.filter((d) => d.family === f).map((d) => <option key={d.id} value={d.id}>{d.name}{d.immutable ? " (read-only system)" : ""}</option>)}
                  </optgroup>
                ))}
                <option value="linux">Another distribution (portable formats only)</option>
              </select>
            )}
            <div className="toggles">
              {ARCHS.map((arch) => {
                const p = { os, arch };
                const on = selection.targets.some((t) => S.samePlatform(t, p));
                const engine = engines.some((e) => S.samePlatform(e, p));
                return (
                  <label key={arch} className="toggle">
                    <input type="checkbox" checked={on} onChange={(e) => setSelection((s) => S.setTarget(s, p, e.target.checked))} />
                    <span className="sr-only">{osName(os)} </span>{archName(arch)}
                    {isHere(p) && <Badge tone="info">This computer</Badge>}
                    {on && !engine && <Badge tone="warn" title="This copy of Simple Setup has no restore program for that system">No restore program</Badge>}
                  </label>
                );
              })}
            </div>
          </div>
        ))}
        {linuxOn && <p className="muted small" style={{ margin: 0 }}>Package files only install on the distributions they were made for, so Simple Setup collects what the chosen distribution can use (its own package format, Flatpak bundles or AppImages) and never converts one package format into another. Missing a distribution? Add it in Settings → Linux distributions.</p>}
      </fieldset>
    </div>
  );
}

/** Name, save location, encryption and saved profiles (Snapshot mode). */
function SaveSettings() {
  const { selection, name, destination, set, inventory } = useApp();
  const [volumes, setVolumes] = useState<Volume[]>([]);
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [profileName, setProfileName] = useState("");
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => {
    void api.volumes().then(setVolumes);
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
  return (
    <div className="grid">
      <div className="card stack">
        <label className="field"><span>Snapshot name</span><input type="text" value={name} onChange={(e) => set({ name: e.target.value })} /></label>
        <fieldset className="stack">
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
        </fieldset>
      </div>
      <div className="card stack"><Encryption /></div>
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
  );
}

/** Transfer mode: find the receiving computer, pair with its code, and learn what it is. */
function Connect() {
  const { peer, set, setSelection } = useApp();
  const [peers, setPeers] = useState<Peer[] | null>(null);
  const [address, setAddress] = useState(peer?.address ?? "");
  const [code, setCode] = useState(peer?.code ?? "");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const look = () => {
    setPeers(null);
    api.findReceivers().then(setPeers, (e) => { setPeers([]); setErr(errorMessage(e)); });
  };
  useEffect(look, []);
  const connect = async () => {
    setBusy(true);
    setErr(null);
    try {
      const probed = await api.probeReceiver(address, code);
      set({ peer: { address, code, probed } });
      setSelection((s) => ({ ...s, ...S.probedTargets(probed) }));
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section className="stack" aria-labelledby="s3">
      <h2 id="s3">Connect to the new computer</h2>
      <p className="small">On the new computer, open Simple Setup → <strong>Transfers</strong> → <strong>Receive on this computer</strong>. It shows a pairing code. Simple Setup then asks it what it is, so only installers that fit it are prepared.</p>
      <div className="grid two">
        <fieldset className="card stack">
          <legend className="label">Receiving computers nearby</legend>
          {peers === null ? <p className="small muted"><Spinner /> Looking…</p> : peers.length === 0 ? <p className="small muted">None found yet. Start receiving on the other computer, or type the address it shows.</p> : peers.map((p) => (
            <label key={p.name} className="check">
              <input type="radio" name="peer" checked={address === p.addresses[0]} onChange={() => setAddress(p.addresses[0])} />
              {p.name} <span className="muted small">{p.system ?? p.os ?? ""} · {p.addresses[0]}</span>
            </label>
          ))}
          <div><button className="btn link" onClick={look}>Look again</button></div>
          <label className="field"><span>Address</span><input type="text" className="mono" value={address} placeholder="192.168.1.20:41234" onChange={(e) => setAddress(e.target.value.trim())} /></label>
          <label className="field"><span>Pairing code shown on the other computer</span><input type="text" inputMode="numeric" autoComplete="off" className="mono" value={code} onChange={(e) => setCode(e.target.value)} /></label>
          <div className="row">
            <button className="btn primary" disabled={busy || !address || code.replace(/\D/g, "").length !== 8} onClick={() => void connect()}>{busy ? <><Spinner /> Connecting…</> : peer ? "Connect again" : "Connect"}</button>
          </div>
          <ErrorNote error={err} />
        </fieldset>
        {peer ? <SystemCard probed={peer.probed} /> : (
          <div className="empty"><span className="tile" aria-hidden><Network size={20} /></span><strong>Not connected</strong><p>The new computer's system appears here once paired.</p></div>
        )}
      </div>
    </section>
  );
}

/** What a paired computer said it is. */
export function SystemCard({ probed }: { probed: Probed }) {
  const { info, device } = probed;
  const l = info.linux;
  const tools = info.tools.filter((t) => !["cargo", "go", "pipx", "curl"].includes(t));
  return (
    <section className="card stack" aria-labelledby="peer-sys">
      <h3 id="peer-sys"><Badge tone="ok">Connected</Badge> {device.hostname}</h3>
      <dl className="kv small">
        <dt>System</dt><dd>{l ? l.os_release.pretty_name ?? l.os_release.name ?? info.device.os_name : info.device.os_name}</dd>
        <dt>Processor</dt><dd>{archName(info.device.platform.arch)}{l && l.machine !== info.device.platform.arch ? ` (${l.machine})` : ""}</dd>
        {l && <><dt>Profile</dt><dd>{l.profile.name} <span className="muted">({l.profile.chain.slice(1, -1).map(distroName).join(" → ") || "own family"})</span></dd>
          <dt>Packages</dt><dd>{l.profile.formats.map((f) => f.toUpperCase()).join(", ") || "none"} with {l.profile.package_manager === "none" ? "no package manager" : l.profile.package_manager}{l.immutable ? " · read-only system" : ""}</dd></>}
        <dt>Installs with</dt><dd>{tools.join(", ") || "downloads only"}</dd>
        <dt>Internet</dt><dd>{info.online ? "Connected" : "Not checked"}</dd>
      </dl>
      {l && <p className="muted small" style={{ margin: 0 }}>{l.explanation}</p>}
    </section>
  );
}

// ------------------------------------------------------------------ step 4

function CompatibilityStep() {
  const { inventory, selection, mode, peer, device } = useApp();
  const { pf, err } = usePreflight();
  const targets = selection.targets;
  const [target, setTarget] = useState(0);
  const appName = useMemo(() => new Map((inventory?.apps ?? []).map((a) => [a.id, a.name])), [inventory]);
  if (err) return <ErrorNote error={err} />;
  if (!pf) return <p className="muted" role="status"><Spinner /> Checking what fits the new computer…</p>;
  const t = targets[Math.min(target, targets.length - 1)];
  const likeThis = !selection.linux_distro && device && S.samePlatform(t, device.platform);
  const where = mode === "transfer" && peer ? `${peer.probed.device.hostname} (${targetLabel(t, selection.linux_distro)})`
    : selection.decide_on_destination ? "an undecided computer"
    : likeThis ? `a computer like this one (${device.os_name})` : targetLabel(t, selection.linux_distro);
  const rows = rowsFromCoverage(pf.coverage.filter((c) => t && S.samePlatform(c.target, t)), (id) => appName.get(id) ?? id);
  const readiness = pf.readiness.find((r) => t && S.samePlatform(r.target, t));
  const settings = pf.coverage.filter((c) => c.part !== "installer" && t && S.samePlatform(c.target, t));
  return (
    <section className="stack" aria-labelledby="s4">
      <div className="spread">
        <h2 id="s4" style={{ margin: 0 }}>What happens on {where}</h2>
        {targets.length > 1 && (
          <label className="row small">Show for
            <select value={target} onChange={(e) => setTarget(Number(e.target.value))}>
              {targets.map((x, i) => <option key={`${x.os}-${x.arch}`} value={i}>{targetLabel(x, selection.linux_distro)}</option>)}
            </select>
          </label>
        )}
      </div>
      {rows.length > 0 ? (
        <Compatibility rows={rows} title="Applications"
          caption={selection.decide_on_destination ? "A first guess: the new computer decides when it restores, from its own package lists." : "Checked against the new computer's profile; its package lists are checked again when it restores."} />
      ) : <p className="muted small">No applications are being reinstalled.</p>}
      {settings.length > 0 && (
        <div className="card small stack">
          <h3>Settings and data</h3>
          <p style={{ margin: 0 }}>{settings.filter((c) => c.status === "ready").length} of {settings.length} can be restored there{settings.some((c) => c.status !== "ready") ? "; the rest stay in the snapshot:" : "."}</p>
          {settings.filter((c) => c.status !== "ready").map((c) => <div key={`${c.app}-${c.part}`}>{appName.get(c.app) ?? c.app}: {c.reason}</div>)}
        </div>
      )}
      {readiness && (
        <div className={`notice ${readiness.offline_ready ? "ok" : "info"}`}>
          <strong>{readiness.offline_ready ? "Everything can be restored without internet." : "Offline readiness: some parts need the internet on the new computer."}</strong>
          {readiness.reasons.length > 0 && <details><summary className="small">What needs the internet or you ({readiness.reasons.length})</summary><ul className="plain small">{readiness.reasons.map((x) => <li key={x}>{x}</li>)}</ul></details>}
        </div>
      )}
      {pf.warnings.map((w) => <div key={w} className="notice warn small">{w}</div>)}
    </section>
  );
}

// ------------------------------------------------------------------ step 5

function Review({ onPreflight }: { onPreflight: (pf: Preflight | null) => void }) {
  const { selection, inventory, name, destination, set, mode, peer } = useApp();
  const { pf, err } = usePreflight();
  useEffect(() => onPreflight(pf), [pf, onPreflight]);
  const appName = useMemo(() => new Map((inventory?.apps ?? []).map((a) => [a.id, a.name])), [inventory]);
  const edit = (step: number) => <button className="btn link" onClick={() => set({ step })}>Change</button>;
  const counts = pf ? rowsFromCoverage(pf.coverage.filter((c) => selection.targets[0] && S.samePlatform(c.target, selection.targets[0])), (x) => x) : [];
  return (
    <section className="stack" aria-labelledby="s5">
      <h2 id="s5">Ready to start</h2>
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
        <div className="spread"><h3>{mode === "transfer" ? "Destination" : "Snapshot"}</h3>{edit(3)}</div>
        <p className="small">{mode === "transfer" && peer
          ? <>Sent to <strong>{peer.probed.device.hostname}</strong> ({targetLabel(peer.probed.info.device.platform, selection.linux_distro)}) over this network.</>
          : <>“{name}” → <span className="mono">{destination}</span> · {selection.decide_on_destination ? "for any computer (decided when restoring)" : `for ${selection.targets.map((t) => targetLabel(t, selection.linux_distro)).join(", ")}`}</>}</p>
        <div className="spread"><h3>Compatibility</h3>{edit(4)}</div>
        {counts.length > 0 && <p className="small">{counts.filter((r) => r.verdict === "ready").length} ready · {counts.filter((r) => r.verdict === "alternative").length} another way · {counts.filter((r) => r.verdict === "review").length} need you · {counts.filter((r) => r.verdict === "unavailable").length} not available</p>}
      </div>
      {err && <ErrorNote error={err} />}
      {pf && (
        <div className="card small">
          <div className="spread">
            <div><strong>{fmtBytes(pf.total_bytes)}</strong> in {pf.total_files.toLocaleString()} files, plus {fmtBytes(pf.engine_bytes)} of restore tools{!!pf.installer_bytes && <> and roughly {fmtBytes(pf.installer_bytes)} of installers</>}</div>
            {pf.destination_free != null && (pf.fits
              ? <Badge tone="ok">{fmtBytes(pf.destination_free)} free at destination</Badge>
              : <Badge tone="bad">Not enough space: needs {fmtBytes(pf.required_bytes)}, {fmtBytes(pf.destination_free)} free</Badge>)}
          </div>
          <p style={{ margin: "8px 0 0" }}><strong>Left out automatically:</strong> {pf.excluded.secrets} sensitive item(s), {pf.excluded.transient} cache/lock item(s), {pf.excluded.by_user} you excluded, {pf.excluded.unreadable} unreadable.</p>
          {pf.excluded.secret_examples.length > 0 && <details><summary>Protected items</summary><ul className="plain">{pf.excluded.secret_examples.map((s) => <li key={s} className="mono">{s}</li>)}</ul></details>}
          {pf.excluded.unreadable_examples.length > 0 && <details><summary>Could not read</summary><ul className="plain">{pf.excluded.unreadable_examples.map((s) => <li key={s} className="mono">{s}</li>)}</ul></details>}
        </div>
      )}
      {!pf && !err && <p className="muted" role="status"><Spinner /> Working out size and space…</p>}
    </section>
  );
}

type BuildProgress = { stage?: Progress; copy?: Progress; note?: string; warnings: string[] };

function Building({ progress, title }: { progress: BuildProgress; title: string }) {
  const stage = progress.stage?.kind === "stage" ? progress.stage : null;
  const copy = progress.copy?.kind === "copy" ? progress.copy : null;
  const installers = stage?.label.startsWith("Preparing installers");
  return (
    <section className="stack" aria-live="polite" aria-busy>
      <h1>{title}</h1>
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

function Sending({ progress, to }: { progress: Progress | null; to: string }) {
  return (
    <section className="stack" aria-live="polite" aria-busy>
      <h1>Sending to {to}…</h1>
      <p className="small">{to} must accept the transfer first. Every file is checked as it arrives; if the connection drops, sending again continues where it stopped.</p>
      {progress?.kind === "stage" && <p><strong>{progress.label}</strong></p>}
      {progress?.kind === "copy" && (
        <>
          <ProgressBar value={progress.done_bytes} max={progress.total_bytes || 1} label="Sending" />
          <p className="small">{fmtBytes(progress.done_bytes)} of {fmtBytes(progress.total_bytes)}</p>
          <p className="small mono muted" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{progress.current}</p>
        </>
      )}
      <div><button className="btn danger" onClick={() => void api.cancelTransfer()}>Stop</button></div>
    </section>
  );
}

/** Transfer finished: what arrived, and what happens next on the other computer. */
function Sent({ report, sent, probed, onNew }: { report: BuildReport; sent: SendReport; probed: Probed | null; onNew: () => void }) {
  const { go } = useApp();
  const r = report.readiness[0];
  return (
    <section className="stack">
      <h1>Sent to {sent.receiver.hostname}</h1>
      <div className="notice ok" role="status">{report.files.toLocaleString()} files ({fmtBytes(sent.bytes)}) arrived and were checked there{sent.resumed_bytes ? `, ${fmtBytes(sent.resumed_bytes)} of them from an earlier attempt` : ""}.</div>
      <div className="card stack">
        <h3>Next, on {sent.receiver.hostname}</h3>
        <p className="small">If “Restore automatically when it arrives” was chosen there, the restore has started; otherwise open <strong>Restore</strong> there. It shows the same compatibility summary, checked against its own package lists, and the final report with anything left to do.</p>
        {probed && <p className="small muted">Prepared for {probed.info.linux?.profile.name ?? probed.info.device.os_name} ({archName(probed.info.device.platform.arch)}).</p>}
        {r && !r.offline_ready && <details><summary className="small">Parts that need the internet or you there ({r.reasons.length})</summary><ul className="plain small">{r.reasons.map((x) => <li key={x}>{x}</li>)}</ul></details>}
      </div>
      {report.failed.length > 0 && (
        <div className="notice bad small"><strong>{report.failed.length} file(s) could not be read here</strong> and were not sent:
          <ul className="plain">{report.failed.slice(0, 20).map((f) => <li key={f.path}><span className="mono">{f.path}</span>: {f.error}</li>)}</ul></div>
      )}
      <div className="row">
        <button className="btn primary" onClick={() => go("transfers")}>View transfers</button>
        <button className="btn" onClick={onNew}>Start another</button>
      </div>
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
          <code id="recovery-key" className="mono" style={{ userSelect: "all", wordBreak: "break-all" }}>{report.recovery_key}</code>
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
          <strong>{targetLabel(r.target, selection.linux_distro)}: {r.offline_ready ? "ready to restore offline" : "not fully offline-ready"}</strong>
          <ul className="plain small">{r.reasons.map((x) => <li key={x}>{x}</li>)}</ul>
        </div>
      ))}
      {report.warnings.map((w) => <div key={w} className="notice warn small">{w}</div>)}
      <p className="small muted">Left out: {report.excluded.secrets} sensitive, {report.excluded.transient} cache/lock, {report.excluded.by_user} by you, {report.excluded.unreadable} unreadable. A full report is in the snapshot's reports folder.</p>
      <div className="row">
        <button className="btn primary" onClick={() => go("snapshots")}>View snapshots</button>
        <button className="btn" onClick={() => setSending(true)}>Send to another computer…</button>
        <button className="btn" onClick={() => { set({ restorePath: report.path }); go("restore"); }}>Check a restore here</button>
        <button className="btn" onClick={onNew}>Start a new snapshot</button>
      </div>
      {sending && <SendDialog summary={{ path: report.path, name: report.name, targets: selection.targets }} onClose={() => setSending(false)} />}
    </section>
  );
}
