// The compatibility summary: how each app gets onto the destination, in four groups, with the
// reasoning behind every decision one click away. Used before building (from the preflight's
// coverage) and when restoring (from the restore plan).
import { useState } from "react";
import { AlertTriangle, CheckCircle2, Repeat, XCircle } from "lucide-react";
import type { Considered, Coverage, PlanItem, Status, Verdict } from "./types";
import { Modal, VerdictBadge } from "./ui";

export interface CompatRow {
  id: string;
  app: string;
  verdict: Verdict;
  /** How it gets installed, e.g. "APT package firefox". */
  plan: string;
  reason: string;
  considered: Considered[];
}

const VERDICT_OF: Record<Status, Verdict> = { ready: "ready", manual: "review", unsupported: "unavailable" };

const GROUPS: { verdict: Verdict; title: string; hint: string; icon: typeof CheckCircle2; tone: string }[] = [
  { verdict: "ready", title: "Ready to install", hint: "Installed as before, or with the new computer's own equivalent.", icon: CheckCircle2, tone: "ok" },
  { verdict: "alternative", title: "Another way", hint: "Installed differently than before, for example as a Flatpak.", icon: Repeat, tone: "info" },
  { verdict: "review", title: "Needs you", hint: "An approval, a download or a manual step is needed.", icon: AlertTriangle, tone: "warn" },
  { verdict: "unavailable", title: "Not available", hint: "No version for that computer is known; skipped and reported.", icon: XCircle, tone: "bad" },
];

/** Rows from the preflight: the installer row of each app for one target. */
export function rowsFromCoverage(coverage: Coverage[], name: (id: string) => string): CompatRow[] {
  return coverage
    .filter((c) => c.part === "installer")
    .map((c) => ({
      id: c.app,
      app: name(c.app),
      verdict: c.verdict ?? VERDICT_OF[c.status],
      plan: c.method ?? (c.status === "unsupported" ? "Skipped" : "—"),
      reason: c.reason,
      considered: c.considered ?? [],
    }));
}

/** Rows from a restore plan: its install items. */
export function rowsFromPlan(items: PlanItem[]): CompatRow[] {
  return items
    .filter((i) => i.kind === "installer")
    .map((i) => ({
      id: i.id,
      app: i.app ?? i.label,
      verdict: i.verdict,
      plan: i.channel === "snapshot" ? "From the snapshot (offline)" : i.method ?? (i.status === "unsupported" ? "Skipped" : "—"),
      reason: i.reason,
      considered: i.considered,
    }));
}

export function Compatibility({ rows, title, caption }: { rows: CompatRow[]; title: string; caption?: string }) {
  const [filter, setFilter] = useState<Verdict | null>(null);
  const [open, setOpen] = useState<CompatRow | null>(null);
  const shown = filter ? rows.filter((r) => r.verdict === filter) : rows;
  if (rows.length === 0) return null;
  return (
    <section className="stack" aria-labelledby="compat-title">
      <div className="spread">
        <h2 id="compat-title" style={{ margin: 0 }}>{title}</h2>
        {filter && <button className="btn link" onClick={() => setFilter(null)}>Show all {rows.length}</button>}
      </div>
      {caption && <p className="muted small" style={{ margin: 0 }}>{caption}</p>}
      <div className="tiles" role="group" aria-label="Filter by group">
        {GROUPS.map(({ verdict, title: t, hint, icon: Icon, tone }) => {
          const n = rows.filter((r) => r.verdict === verdict).length;
          return (
            <button key={verdict} className={`tile ${tone}`} aria-pressed={filter === verdict} disabled={n === 0}
              onClick={() => setFilter(filter === verdict ? null : verdict)} title={hint}>
              <Icon size={18} aria-hidden />
              <span className="count">{n}</span>
              <span className="what">{t}</span>
            </button>
          );
        })}
      </div>
      <div className="table-wrap">
        <table>
          <caption className="sr-only">{title}</caption>
          <thead><tr><th>Application</th><th>Compatibility</th><th>Installation plan</th><th><span className="sr-only">Details</span></th></tr></thead>
          <tbody>
            {shown.map((r) => (
              <tr key={r.id}>
                <td>{r.app}</td>
                <td><VerdictBadge verdict={r.verdict} /></td>
                <td className="small">{r.plan}</td>
                <td><button className="btn link" onClick={() => setOpen(r)} aria-label={`Why: ${r.app}`}>Why?</button></td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <Modal open={!!open} title={open?.app ?? ""} onClose={() => setOpen(null)} footer={<button className="btn" onClick={() => setOpen(null)}>Close</button>}>
        {open && (
          <>
            <div className="row"><VerdictBadge verdict={open.verdict} /> <strong>{open.plan}</strong></div>
            <p className="small">{open.reason}</p>
            {open.considered.length > 0 && (
              <>
                <h3>Everything that was considered</h3>
                <ol className="small considered">
                  {open.considered.map((c, i) => (
                    <li key={i}><strong>{c.option}</strong> — {c.outcome}</li>
                  ))}
                </ol>
              </>
            )}
            <p className="muted small">Decisions use the new computer's distribution profile, package lists and processor; package files are never converted between formats.</p>
          </>
        )}
      </Modal>
    </section>
  );
}
