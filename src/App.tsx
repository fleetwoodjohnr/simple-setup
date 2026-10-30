import { useEffect } from "react";
import { Archive, LayoutDashboard, ListChecks, PackagePlus, RotateCcw, MoveRight } from "lucide-react";
import { useApp, type Page } from "./store";
import Dashboard from "./pages/Dashboard";
import Applications from "./pages/Applications";
import CreateSnapshot from "./pages/CreateSnapshot";
import Snapshots from "./pages/Snapshots";
import Restore from "./pages/Restore";

const NAV: { page: Page; label: string; icon: typeof Archive }[] = [
  { page: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { page: "applications", label: "Applications", icon: ListChecks },
  { page: "create", label: "Create Snapshot", icon: PackagePlus },
  { page: "snapshots", label: "Snapshots", icon: Archive },
  { page: "restore", label: "Restore", icon: RotateCcw },
];

export default function App() {
  const { page, go, init } = useApp();
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
        <div className="brand"><MoveRight size={20} aria-hidden /> Simple Setup</div>
        {NAV.map(({ page: p, label, icon: Icon }) => (
          <button key={p} aria-current={page === p ? "page" : undefined} onClick={() => go(p)}>
            <Icon size={18} aria-hidden /> {label}
          </button>
        ))}
      </nav>
      <main className="main" id="main" tabIndex={-1}>
        {page === "dashboard" && <Dashboard />}
        {page === "applications" && <Applications />}
        {page === "create" && <CreateSnapshot />}
        {page === "snapshots" && <Snapshots />}
        {page === "restore" && <Restore />}
      </main>
    </div>
  );
}
