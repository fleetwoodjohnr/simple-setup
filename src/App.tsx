import { useEffect } from "react";
import { Archive, ArrowUpCircle, LayoutDashboard, ListChecks, PackagePlus, RotateCcw, Settings as SettingsIcon } from "lucide-react";
import { useApp, type Page } from "./store";
import Dashboard from "./pages/Dashboard";
import Applications from "./pages/Applications";
import CreateSnapshot from "./pages/CreateSnapshot";
import Snapshots from "./pages/Snapshots";
import Restore from "./pages/Restore";
import Settings from "./pages/Settings";
import { Logo } from "./ui";

const NAV: { page: Page; label: string; icon: typeof Archive }[] = [
  { page: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { page: "applications", label: "Applications", icon: ListChecks },
  { page: "create", label: "Create Snapshot", icon: PackagePlus },
  { page: "snapshots", label: "Snapshots", icon: Archive },
  { page: "restore", label: "Restore", icon: RotateCcw },
  { page: "settings", label: "Settings", icon: SettingsIcon },
];

export default function App() {
  const { page, go, init, appInfo, update, updateReady } = useApp();
  useEffect(() => {
    void init();
  }, [init]);
  useEffect(() => {
    document.title = `${NAV.find((n) => n.page === page)?.label} — Simple Setup`;
  }, [page]);
  return (
    <div className="shell">
      <a className="skip" href="#main">Skip to content</a>
      <nav className="nav" aria-label="Main">
        <div className="brand"><Logo /> Simple Setup</div>
        {NAV.map(({ page: p, label, icon: Icon }) => (
          <button key={p} aria-current={page === p ? "page" : undefined} onClick={() => go(p)}>
            <Icon size={18} aria-hidden /> {label}
          </button>
        ))}
        <div className="nav-foot">
          {update?.newer && (
            <button className="pill" onClick={() => go("settings")}>
              <ArrowUpCircle size={14} aria-hidden /> {updateReady ? "Restart to update" : `Version ${update.latest} available`}
            </button>
          )}
          {appInfo && <span>Version {appInfo.version}</span>}
        </div>
      </nav>
      <main className="main" id="main" tabIndex={-1}>
        <div className="page">
          {page === "dashboard" && <Dashboard />}
          {page === "applications" && <Applications />}
          {page === "create" && <CreateSnapshot />}
          {page === "snapshots" && <Snapshots />}
          {page === "restore" && <Restore />}
          {page === "settings" && <Settings />}
        </div>
      </main>
    </div>
  );
}
