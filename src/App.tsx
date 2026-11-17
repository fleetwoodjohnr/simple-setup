import { useEffect, useState } from "react";
import { Archive, ArrowLeftRight, ArrowUpCircle, HardDriveDownload, HardDriveUpload, Home, Moon, PanelLeft, Settings as SettingsIcon, Sun } from "lucide-react";
import { useApp, type Mode, type Page } from "./store";
import Dashboard from "./pages/Dashboard";
import Applications from "./pages/Applications";
import CreateSnapshot from "./pages/CreateSnapshot";
import Snapshots from "./pages/Snapshots";
import Restore from "./pages/Restore";
import Settings from "./pages/Settings";
import Transfers from "./pages/Transfers";
import { resolveTheme } from "./theme";
import { Logo } from "./ui";

/** Home and the five things Simple Setup does. `flow`: the guided flow the entry opens. */
type Place = "home" | "create" | "restore" | "transfer" | "backups" | "settings";
const NAV: { place: Place; label: string; icon: typeof Archive; page: Page; flow?: Mode }[] = [
  { place: "home", label: "Home", icon: Home, page: "dashboard" },
  { place: "create", label: "Create Snapshot", icon: HardDriveUpload, page: "create", flow: "snapshot" },
  { place: "restore", label: "Restore Snapshot", icon: HardDriveDownload, page: "restore" },
  { place: "transfer", label: "Transfer", icon: ArrowLeftRight, page: "transfers" },
  { place: "backups", label: "Manage Backups", icon: Archive, page: "snapshots" },
  { place: "settings", label: "Settings", icon: SettingsIcon, page: "settings" },
];

/** Which entry a page belongs under: the transfer flow under Transfer, app details under Settings. */
function placeOf(page: Page, mode: Mode): Place {
  switch (page) {
    case "dashboard": return "home";
    case "create": return mode === "transfer" ? "transfer" : "create";
    case "restore": return "restore";
    case "transfers": return "transfer";
    case "snapshots": return "backups";
    case "applications": case "settings": return "settings";
  }
}
const NAV_KEY = "simple-setup-nav-collapsed";

export default function App() {
  const { page, go, init, appInfo, update, updateReady, settings, saveSettings, mode, startFlow } = useApp();
  const [collapsed, setCollapsed] = useState(() => {
    try { return localStorage.getItem(NAV_KEY) === "1"; } catch { return false; }
  });
  useEffect(() => {
    void init();
  }, [init]);
  const place = placeOf(page, mode);
  const label = page === "create" && mode === "transfer" ? "Transfer to Another Computer"
    : page === "applications" ? "Applications" : NAV.find((n) => n.place === place)?.label;
  // Opening Create Snapshot keeps a draft in progress; one of the other flow starts afresh.
  const open = (n: (typeof NAV)[number]) => {
    if (n.flow && mode !== n.flow) startFlow(n.flow);
    else go(n.page);
  };
  useEffect(() => {
    document.title = `${label} — Simple Setup`;
  }, [label]);
  // Alt+1 … Alt+6 open the places in the order of the navigation.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const i = Number(e.key) - 1;
      if (e.altKey && !e.ctrlKey && !e.metaKey && NAV[i]) {
        e.preventDefault();
        open(NAV[i]);
      }
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  });
  const toggleNav = () => {
    setCollapsed(!collapsed);
    try { localStorage.setItem(NAV_KEY, collapsed ? "0" : "1"); } catch { /* only a convenience */ }
  };
  const dark = resolveTheme(settings?.theme ?? "system", matchMedia("(prefers-color-scheme: dark)").matches) === "dark";
  const item = (n: (typeof NAV)[number]) => {
    const key = `Alt+${NAV.indexOf(n) + 1}`;
    const Icon = n.icon;
    return (
      <button key={n.place} aria-current={place === n.place ? "page" : undefined} onClick={() => open(n)} title={collapsed ? n.label : undefined}
        data-key={key} aria-keyshortcuts={key}>
        <Icon size={18} aria-hidden /><span className="text">{n.label}</span>
      </button>
    );
  };

  return (
    <div className="shell">
      <a className="skip" href="#main">Skip to content</a>
      <header className="titlebar">
        <span className="brand"><Logo size={22} /> Simple Setup</span>
        <span className="where">{label}</span>
        <div className="tools">
          {update?.newer && (
            <button className="pill" onClick={() => go("settings")}>
              <ArrowUpCircle size={14} aria-hidden /> {updateReady ? "Restart to update" : `Version ${update.latest} available`}
            </button>
          )}
          <button className="icon-btn" aria-label={dark ? "Use the light theme" : "Use the dark theme"} title={dark ? "Light theme" : "Dark theme"}
            onClick={() => void saveSettings({ theme: dark ? "light" : "dark" })}>
            {dark ? <Sun size={16} aria-hidden /> : <Moon size={16} aria-hidden />}
          </button>
          <button className="icon-btn" aria-label="Open settings" title="Settings" onClick={() => go("settings")}><SettingsIcon size={16} aria-hidden /></button>
        </div>
      </header>
      <div className="body">
        <nav className={`nav${collapsed ? " collapsed" : ""}`} aria-label="Main">
          {NAV.slice(0, -1).map(item)}
          <div className="nav-foot">
            {item(NAV[NAV.length - 1])}
            <button className="collapse" aria-expanded={!collapsed} onClick={toggleNav} title={collapsed ? "Show labels" : undefined}>
              <PanelLeft size={18} aria-hidden /><span className="text">{collapsed ? "Show labels" : "Hide labels"}</span>
            </button>
            {appInfo && <span className="version">Version {appInfo.version}</span>}
          </div>
        </nav>
        <main className="main" id="main" tabIndex={-1}>
          <div className="page">
            {page === "dashboard" && <Dashboard />}
            {page === "applications" && <Applications />}
            {page === "create" && <CreateSnapshot />}
            {page === "snapshots" && <Snapshots />}
            {page === "restore" && <Restore />}
            {page === "transfers" && <Transfers />}
            {page === "settings" && <Settings />}
          </div>
        </main>
      </div>
    </div>
  );
}
