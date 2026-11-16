import { useEffect, useRef, useState, type ReactNode } from "react";
import { api } from "./api";
import { X } from "lucide-react";
import { PLAN_STATE_LABELS } from "./types";
import type { Confidence, Origin, Platform, Progress, Status, Verdict, PlanState } from "./types";

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
const ARCH_NAMES: Record<string, string> = {
  x86_64: "x86-64", aarch64: "ARM64", i686: "x86 (32-bit)", armv7: "ARM (32-bit)", riscv64: "RISC-V 64",
  ppc64le: "POWER", s390x: "IBM Z", loong64: "LoongArch", unknown: "unknown processor",
};
export const archName = (a: string) => ARCH_NAMES[a] ?? a;
export const platformLabel = (p: Platform) => `${osName(p.os)} ${archName(p.arch)}`;

/** Names of the distribution profiles (loaded from the engine at start; IDs until then). */
const distroNames = new Map<string, string>();
export function setDistroNames(list: { id: string; name: string }[]) {
  distroNames.clear();
  for (const d of list) distroNames.set(d.id, d.name);
}
export const distroName = (id: string) => (id === "other" || id === "linux" ? "any Linux distribution" : distroNames.get(id) ?? id);

/** A target as the user chose it: Linux targets name their distribution when one was chosen. */
export function targetLabel(p: Platform, distro?: string | null): string {
  if (p.os === "linux" && distro && distro !== "other" && distro !== "linux") return `${distroName(distro)} ${archName(p.arch)}`;
  return platformLabel(p);
}

const VERDICT: Record<Verdict, [string, "ok" | "info" | "warn" | "bad"]> = {
  ready: ["Ready", "ok"],
  alternative: ["Another way", "info"],
  review: ["Needs you", "warn"],
  unavailable: ["Not available", "bad"],
};
/** The compatibility group of an app, as a badge. */
export function VerdictBadge({ verdict }: { verdict: Verdict }) {
  const [label, tone] = VERDICT[verdict];
  return <Badge tone={tone}>{label}</Badge>;
}

export function Badge({ tone = "neutral", children, title }: { tone?: "neutral" | "ok" | "warn" | "bad" | "info"; children: ReactNode; title?: string }) {
  return <span className={`badge ${tone}`} title={title}>{children}</span>;
}

const STATE_TONE: Record<PlanState, "ok" | "warn" | "bad" | "info" | "neutral"> = {
  ready_to_install: "ok", already_installed: "neutral", alternative_available: "info", requires_approval: "warn",
  internet_required: "warn", dependency_required: "info", incompatible: "bad",
};

/** Where a restore item stands, in the spec's words. */
export function StateBadge({ state }: { state: PlanState }) {
  return <Badge tone={STATE_TONE[state]}>{PLAN_STATE_LABELS[state]}</Badge>;
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

/** "about 3 min", "about 40 s", "about 1 h 5 min". */
export function fmtDuration(secs: number): string {
  const s = Math.max(1, Math.round(secs));
  if (s < 60) return `about ${s} s`;
  if (s < 3600) return `about ${Math.round(s / 60)} min`;
  return `about ${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min`;
}

/** Speed over the last ten seconds and the time left, from successive progress values. */
export function useRate(done: number, total: number): { perSec: number | null; left: number | null } {
  const samples = useRef<[number, number][]>([]);
  const now = performance.now();
  const s = samples.current;
  if (s.length && done < s[s.length - 1][1]) s.length = 0;
  if (!s.length || s[s.length - 1][1] !== done) s.push([now, done]);
  while (s.length > 2 && now - s[0][0] > 10_000) s.shift();
  const [t0, d0] = s[0];
  const secs = (now - t0) / 1000;
  const perSec = secs >= 1 && done > d0 ? (done - d0) / secs : null;
  return { perSec, left: perSec ? Math.max(0, total - done) / perSec : null };
}

type CopyProgress = Extract<Progress, { kind: "copy" }>;

/** A transfer under way: bar, amount, speed, time left and the file; and what the connection
 * is doing in between (reconnecting, paused), from its progress events. */
export function TransferStatus({ progress, label }: { progress: Progress | null; label: string }) {
  const [copy, setCopy] = useState<CopyProgress | null>(null);
  const [note, setNote] = useState<string | null>(null);
  useEffect(() => {
    if (progress?.kind === "copy") { setCopy(progress); setNote(null); } else if (progress?.kind === "note") setNote(progress.message);
  }, [progress]);
  const { perSec, left } = useRate(copy?.done_bytes ?? 0, copy?.total_bytes ?? 0);
  return (
    <>
      {copy && (
        <>
          <ProgressBar value={copy.done_bytes} max={copy.total_bytes || 1} label={label} />
          <p className="small">{fmtBytes(copy.done_bytes)} of {fmtBytes(copy.total_bytes)}
            {!note && perSec ? ` · ${fmtBytes(perSec)}/s` : ""}{!note && left != null && copy.done_bytes < copy.total_bytes ? ` · ${fmtDuration(left)} left` : ""}</p>
          <p className="small mono muted" style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{copy.current}</p>
        </>
      )}
      {note && <p className="small" role="status">{note}</p>}
    </>
  );
}

/** Pauses the send in progress (the other computer keeps what arrived) or resumes it. */
export function PauseButton() {
  const [paused, setPaused] = useState(false);
  return <button className="btn" onClick={() => { const p = !paused; setPaused(p); void api.pauseSend(p); }}>{paused ? "Resume" : "Pause"}</button>;
}

/** Native modal dialog: focus trapping, Escape to close and backdrop come from the browser.
 * `footer` holds the dialog's buttons, in a bar along the bottom. */
export function Modal({ open, title, onClose, children, footer, wide }: { open: boolean; title: string; onClose: () => void; children: ReactNode; footer?: ReactNode; wide?: boolean }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  // A dialog removed while open is closed first, so the browser returns focus to its opener
  // (the element is captured now: React has cleared the ref by the time this cleanup runs).
  useEffect(() => {
    const d = ref.current;
    return () => d?.close();
  }, []);
  return (
    <dialog ref={ref} onClose={onClose} aria-labelledby="dialog-title" className={wide ? "wide" : undefined}>
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

const iconCache = new Map<string, Promise<string | null>>();

/** An app's own icon (asked for once per session), or its initial. Decorative: the app's name
 * is always shown beside it. */
export function AppIcon({ id, name }: { id: string; name: string }) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    let p = iconCache.get(id);
    if (!p) {
      p = api.appIcon(id).catch(() => null);
      iconCache.set(id, p);
    }
    void p.then((s) => alive && setSrc(s));
    return () => { alive = false; };
  }, [id]);
  return src
    ? <img className="app-icon" src={src} alt="" width={24} height={24} />
    : <span className="app-icon initial" aria-hidden>{name.trim().charAt(0).toUpperCase() || "?"}</span>;
}
