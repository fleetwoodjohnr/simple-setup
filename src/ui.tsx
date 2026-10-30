import { useEffect, useRef, type ReactNode } from "react";
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

/** Native modal dialog: focus trapping, Escape to close and backdrop come from the browser. */
export function Modal({ open, title, onClose, children }: { open: boolean; title: string; onClose: () => void; children: ReactNode }) {
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
        <div className="dialog-body">
          <h2 id="dialog-title">{title}</h2>
          {children}
        </div>
      )}
    </dialog>
  );
}

export function ErrorNote({ error }: { error: string | null }) {
  return error ? <div className="notice bad" role="alert">{error}</div> : null;
}
