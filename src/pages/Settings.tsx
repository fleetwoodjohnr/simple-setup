import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { Monitor, Moon, Sun } from "lucide-react";
import { api, errorMessage } from "../api";
import { useApp } from "../store";
import type { Settings as S, ThemePref } from "../types";
import { Badge, ErrorNote, ProgressBar, Spinner, fmtBytes } from "../ui";

const THEMES: { value: ThemePref; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

/** Every change is saved at once; the settings file keeps them for the CLI and next start. */
export default function Settings() {
  const { settings, saveSettings, scan } = useApp();
  const [err, setErr] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  if (!settings) return <p role="status"><Spinner /> Loading settings…</p>;
  const save = async (patch: Partial<S>) => {
    setErr(null);
    try {
      await saveSettings(patch);
      setSaved(true);
      setTimeout(() => setSaved(false), 1500);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const pickFolder = async (title: string) => {
    const p = await open({ directory: true, title });
    return typeof p === "string" ? p : null;
  };
  const toggle = (key: keyof S, label: string, hint?: string) => (
    <>
      <label className="check"><input type="checkbox" checked={!!settings[key]} onChange={(e) => void save({ [key]: e.target.checked })} /> {label}</label>
      {hint && <p className="muted small hint">{hint}</p>}
    </>
  );

  return (
    <div className="stack settings">
      <div className="page-head">
        <div><h1>Settings</h1><p className="muted">Saved as you change them.</p></div>
        <span role="status" className="small muted">{saved ? "Saved" : ""}</span>
      </div>
      <ErrorNote error={err} />

      <section className="card" aria-labelledby="appearance">
        <h2 id="appearance">Appearance</h2>
        <fieldset>
          <legend className="small muted">Theme</legend>
          <div className="segmented" style={{ marginTop: 6 }}>
            {THEMES.map(({ value, label, icon: Icon }) => (
              <label key={value}>
                <input type="radio" name="theme" checked={settings.theme === value} onChange={() => void save({ theme: value })} />
                <Icon size={15} aria-hidden /> {label}
              </label>
            ))}
          </div>
        </fieldset>
      </section>

      <Updates />

      <section className="card" aria-labelledby="snapshots">
        <h2 id="snapshots">Snapshots</h2>
        <div className="field">
          <span>Default location for new snapshots</span>
          <div className="row">
            <span className="mono small">{settings.default_destination ?? "None — chosen each time"}</span>
            <button className="btn" onClick={async () => { const p = await pickFolder("Default location for new snapshots"); if (p) void save({ default_destination: p }); }}>Choose…</button>
            {settings.default_destination && <button className="btn link" onClick={() => void save({ default_destination: null })}>Clear</button>}
          </div>
        </div>
        <div className="field">
          <span>Folders to search for source checkouts and portable apps</span>
          <p className="muted small">Only standard app locations are scanned unless you add folders here. Git metadata (like the GitHub address and commit) is read; nothing is run.</p>
          <ul className="plain">
            {settings.source_dirs.map((d) => (
              <li key={d} className="row"><span className="mono small">{d}</span>
                <button className="btn link" onClick={() => void save({ source_dirs: settings.source_dirs.filter((x) => x !== d) })}>Remove</button></li>
            ))}
          </ul>
          <div className="row">
            <button className="btn" onClick={async () => { const p = await pickFolder("Folder with source checkouts or portable apps"); if (p && !settings.source_dirs.includes(p)) void save({ source_dirs: [...settings.source_dirs, p] }); }}>Add folder…</button>
            <button className="btn link" onClick={() => void scan()}>Scan again</button>
          </div>
        </div>
      </section>

      <section className="card" aria-labelledby="restoring">
        <h2 id="restoring">Restoring</h2>
        {toggle("restore_online", "Install apps the snapshot has no installer for",
          "With internet access, the new computer's own package managers (winget, Homebrew, APT, DNF, Flatpak…) or the app's verified GitHub release are used. Turn off to use only what is in the snapshot.")}
        {toggle("allow_source_builds", "Build apps from their source code when nothing else installs them",
          "For projects you cloned from GitHub that use Cargo, Go or pipx: built at the exact commit you had. Each build is listed in the restore preview, where you can untick it.")}
      </section>

      <section className="card" aria-labelledby="transfer">
        <h2 id="transfer">Transfer</h2>
        <div className="field">
          <span>Folder for snapshots received from another computer</span>
          <div className="row">
            <span className="mono small">{settings.receive_folder ?? "None — chosen each time"}</span>
            <button className="btn" onClick={async () => { const p = await pickFolder("Folder for received snapshots"); if (p) void save({ receive_folder: p }); }}>Choose…</button>
            {settings.receive_folder && <button className="btn link" onClick={() => void save({ receive_folder: null })}>Clear</button>}
          </div>
        </div>
      </section>
    </div>
  );
}

function Updates() {
  const { settings, appInfo, update, updateReady, checkUpdate, saveSettings, set } = useApp();
  const [checking, setChecking] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [installing, setInstalling] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const check = async () => {
    setChecking(true);
    setErr(null);
    try {
      await checkUpdate();
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setChecking(false);
    }
  };
  const download = async () => {
    setErr(null);
    setProgress({ done: 0, total: update?.file?.size ?? 0 });
    try {
      await api.downloadUpdate(setProgress);
      set({ updateReady: true });
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setProgress(null);
    }
  };
  const install = async () => {
    setInstalling(true);
    setErr(null);
    try {
      await api.installUpdate(); // the app quits when this succeeds
    } catch (e) {
      setErr(errorMessage(e));
      setInstalling(false);
    }
  };
  const checkedAt = settings?.last_update_check ? new Date(settings.last_update_check).toLocaleString() : null;
  return (
    <section className="card" aria-labelledby="updates">
      <h2 id="updates">Updates</h2>
      <dl className="kv">
        <dt>Installed</dt><dd>Simple Setup {appInfo?.version ?? "…"}{appInfo?.development && <> <Badge>Development build</Badge></>}</dd>
        <dt>Latest release</dt><dd>{update ? (update.latest ?? "None published yet") : "Not checked yet"}{checkedAt && <span className="muted"> · checked {checkedAt}</span>}</dd>
        <dt>Source</dt><dd><span className="mono small">github.com/{appInfo?.repository ?? "…"}</span></dd>
      </dl>
      <div className="row">
        <button className="btn" onClick={() => void check()} disabled={checking || !!progress || installing}>
          {checking ? <><Spinner /> Checking…</> : "Check for Updates"}
        </button>
        {update && <span role="status" className="small">{update.message}</span>}
      </div>
      {update?.newer && (
        <div className="stack">
          {update.notes && (
            <details open>
              <summary>What's new in {update.latest}</summary>
              <pre className="notes">{update.notes}</pre>
            </details>
          )}
          {update.cannot_install ? (
            <div className="notice warn small">{update.cannot_install} {update.release_url && <>Release page: <span className="mono">{update.release_url}</span></>}</div>
          ) : updateReady ? (
            <div className="row">
              <button className="btn primary" onClick={() => void install()} disabled={installing}>{installing ? <><Spinner /> Installing…</> : "Restart and Install"}</button>
              <span className="small muted">Downloaded and verified ({update.file?.checked_by}). Your settings and snapshots stay as they are.</span>
            </div>
          ) : progress ? (
            <div className="stack" aria-live="polite">
              <ProgressBar value={progress.done} max={progress.total || 1} label="Downloading the update" />
              <div className="row small"><span>{fmtBytes(progress.done)} of {fmtBytes(progress.total)}</span><button className="btn link" onClick={() => void api.cancelUpdate()}>Cancel</button></div>
            </div>
          ) : (
            <div className="row">
              <button className="btn primary" onClick={() => void download()}>Download Update ({fmtBytes(update.file?.size)})</button>
              <span className="small muted">Checked against the release's published SHA-256 before anything is installed.</span>
            </div>
          )}
        </div>
      )}
      <ErrorNote error={err} />
      <label className="check"><input type="checkbox" checked={!!settings?.update_check} onChange={(e) => void saveSettings({ update_check: e.target.checked })} /> Check for updates when Simple Setup starts (once a day)</label>
      <label className="check"><input type="checkbox" checked={!!settings?.auto_update} disabled={!settings?.update_check} onChange={(e) => void saveSettings({ auto_update: e.target.checked })} /> Download updates automatically; install them when I restart</label>
    </section>
  );
}
