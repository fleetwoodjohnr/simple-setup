import { useEffect, useRef, type ReactNode } from "react";
import { X } from "lucide-react";
import type { Confidence, Origin, Platform, Status } from "./types";

export function fmtBytes(n: number | null | undefined): string {
  if (n == null) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1000 && i < u.length - 1) {
    v /= 1000;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${u[i]}`;
}

export const osName = (os: string) => ({ linux: "Linux", windows: "Windows", macos: "macOS" })[os] ?? os;
export const archName = (a: string) => (a === "aarch64" ? "ARM64" : "x86-64");
export const platformLabel = (p: Platform) => `${osName(p.os)} ${archName(p.arch)}`;

/** Linux distributions a snapshot can be prepared for (os-release ID). Mirrors
 * LINUX_DISTROS in crates/core/src/platform.rs; "other" is any other distribution. */
export const LINUX_DISTROS: { id: string; name: string }[] = [
  { id: "fedora", name: "Fedora" },
  { id: "ubuntu", name: "Ubuntu" },
  { id: "linuxmint", name: "Linux Mint" },
  { id: "debian", name: "Debian" },
  { id: "pop", name: "Pop!_OS" },
  { id: "zorin", name: "Zorin OS" },
  { id: "elementary", name: "elementary OS" },
  { id: "opensuse-tumbleweed", name: "openSUSE Tumbleweed" },
  { id: "opensuse-leap", name: "openSUSE Leap" },
  { id: "arch", name: "Arch Linux" },
  { id: "manjaro", name: "Manjaro" },
  { id: "endeavouros", name: "EndeavourOS" },
];

/** A target as the user chose it: Linux targets name their distribution when one was chosen. */
export function targetLabel(p: Platform, distro?: string | null): string {
  const name = p.os === "linux" && distro && LINUX_DISTROS.find((d) => d.id === distro)?.name;
  return name ? `${name} ${archName(p.arch)}` : platformLabel(p);
}

export function Badge({ tone = "neutral", children, title }: { tone?: "neutral" | "ok" | "warn" | "bad" | "info"; children: ReactNode; title?: string }) {
  return <span className={`badge ${tone}`} title={title}>{children}</span>;
}

export function StatusBadge({ status }: { status: Status }) {
  if (status === "ready") return <Badge tone="ok">Supported</Badge>;
  if (status === "manual") return <Badge tone="warn">Requires manual action</Badge>;
  return <Badge tone="bad">Unsupported — will skip</Badge>;
}

export function ConfidenceBadge({ value }: { value: Confidence }) {
  const tone = value === "confirmed" ? "ok" : value === "probable" ? "warn" : "neutral";
  return <Badge tone={tone}>{value[0].toUpperCase() + value.slice(1)}</Badge>;
}

export function OriginBadge({ value }: { value: Origin }) {
  const text = { detected: "Detected", inferred: "Guessed", user: "Entered by you" }[value];
  return <Badge tone={value === "inferred" ? "warn" : value === "user" ? "info" : "neutral"}>{text}</Badge>;
}

export function ProgressBar({ value, max, label }: { value: number; max: number; label: string }) {
  const pct = max > 0 ? Math.min(100, Math.round((value / max) * 100)) : 0;
  return (
    <div className="progress" role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
      <div style={{ width: `${pct}%` }} />
    </div>
  );
}

/** Native modal dialog: focus trapping, Escape to close and backdrop come from the browser.
 * `footer` holds the dialog's buttons, in a bar along the bottom. */
export function Modal({ open, title, onClose, children, footer }: { open: boolean; title: string; onClose: () => void; children: ReactNode; footer?: ReactNode }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  // A dialog removed while open is closed first, so the browser returns focus to its opener.
  useEffect(() => () => ref.current?.close(), []);
  return (
    <dialog ref={ref} onClose={onClose} aria-labelledby="dialog-title">
      {open && (
        <>
          <div className="dialog-head">
            <h2 id="dialog-title">{title}</h2>
            <button className="icon-btn" aria-label="Close" title="Close" onClick={() => ref.current?.close()}><X size={16} aria-hidden /></button>
          </div>
          <div className="dialog-body">{children}</div>
          {footer && <div className="dialog-foot">{footer}</div>}
        </>
      )}
    </dialog>
  );
}

/** An empty list: what is missing and what to do about it. */
export function Empty({ icon, title, children }: { icon: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      <span className="tile" aria-hidden>{icon}</span>
      <strong>{title}</strong>
      {children && <p>{children}</p>}
    </div>
  );
}

export function ErrorNote({ error }: { error: string | null }) {
  return error ? <div className="notice bad" role="alert">{error}</div> : null;
}

/** The app icon, simplified for small sizes (src-tauri/icons/icon.svg is the full one). */
export function Logo({ size = 28 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="64 64 896 896" aria-hidden focusable="false">
      <defs>
        <linearGradient id="logo-tile" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#2fc6d4" /><stop offset=".5" stopColor="#2680e6" /><stop offset="1" stopColor="#1f3d9e" />
        </linearGradient>
      </defs>
      <rect x="64" y="64" width="896" height="896" rx="200" fill="url(#logo-tile)" />
      <rect x="226" y="222" width="428" height="428" rx="64" fill="#fff" fillOpacity=".28" stroke="#fff" strokeOpacity=".6" strokeWidth="12" />
      <rect x="370" y="374" width="428" height="428" rx="64" fill="#fff" />
      <path d="M460 588H690M610 508L692 588L610 668" fill="none" stroke="#2160c8" strokeWidth="68" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

export function Spinner({ label }: { label?: string }) {
  return <span className="spinner" role={label ? "status" : undefined} aria-label={label} />;
}
