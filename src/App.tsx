import { useEffect, useState } from "react";
import { Archive, ArrowLeftRight, ArrowUpCircle, LayoutDashboard, ListChecks, Moon, PanelLeft, Settings as SettingsIcon, Sun } from "lucide-react";
import { useApp, type Page } from "./store";
import Dashboard from "./pages/Dashboard";
import Applications from "./pages/Applications";
import CreateSnapshot from "./pages/CreateSnapshot";
import Snapshots from "./pages/Snapshots";
import Restore from "./pages/Restore";
import Settings from "./pages/Settings";
import Transfers from "./pages/Transfers";
import { resolveTheme } from "./theme";
import { Logo } from "./ui";

const NAV: { page: Page; label: string; icon: typeof Archive }[] = [
  { page: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { page: "snapshots", label: "Snapshots", icon: Archive },
  { page: "transfers", label: "Transfers", icon: ArrowLeftRight },
  { page: "applications", label: "Applications", icon: ListChecks },
  { page: "settings", label: "Settings", icon: SettingsIcon },
];
const NAV_KEY = "simple-setup-nav-collapsed";

export default function App() {
  const { page, go, init, appInfo, update, updateReady, settings, saveSettings, mode } = useApp();
  const [collapsed, setCollapsed] = useState(() => {
    try { return localStorage.getItem(NAV_KEY) === "1"; } catch { return false; }
  });
  useEffect(() => {
    void init();
  }, [init]);
  // The flows belong under the place they are started from.
  const place: Page = page === "create" ? (mode === "transfer" ? "transfers" : "snapshots") : page === "restore" ? "snapshots" : page;
  const flow = page === "create" ? (mode === "transfer" ? "Transfer to Another Computer" : "Create Snapshot") : page === "restore" ? "Restore" : null;
  const label = flow ?? NAV.find((n) => n.page === place)?.label;
  useEffect(() => {
    document.title = `${label} — Simple Setup`;
  }, [label]);
  // Alt+1 … Alt+5 open the pages in the order of the navigation.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const i = Number(e.key) - 1;
      if (e.altKey && !e.ctrlKey && !e.metaKey && NAV[i]) {
        e.preventDefault();
        go(NAV[i].page);
      }
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, [go]);
  const toggleNav = () => {
    setCollapsed(!collapsed);
    try { localStorage.setItem(NAV_KEY, collapsed ? "0" : "1"); } catch { /* only a convenience */ }
  };
  const dark = resolveTheme(settings?.theme ?? "system", matchMedia("(prefers-color-scheme: dark)").matches) === "dark";
  const item = ({ page: p, label, icon: Icon }: (typeof NAV)[number]) => (
    <button key={p} aria-current={place === p ? "page" : undefined} onClick={() => go(p)} title={collapsed ? label : undefined}
      data-key={`Alt+${NAV.findIndex((n) => n.page === p) + 1}`} aria-keyshortcuts={`Alt+${NAV.findIndex((n) => n.page === p) + 1}`}>
      <Icon size={18} aria-hidden /><span className="text">{label}</span>
    </button>
  );

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
