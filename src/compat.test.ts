import { describe, expect, it } from "vitest";
import { rowsFromCoverage, rowsFromPlan } from "./Compatibility";
import type { Coverage, PlanItem } from "./types";

const linux = { os: "linux", arch: "x86_64" } as const;

describe("compatibility rows", () => {
  it("uses the engine's verdict, and the status for older snapshots", () => {
    const cov: Coverage[] = [
      { app: "rpm:firefox", target: linux, part: "installer", status: "manual", reason: "r", verdict: "ready", method: "APT package firefox" },
      { app: "rpm:tool", target: linux, part: "installer", status: "unsupported", reason: "none" },
      { app: "rpm:old", target: linux, part: "installer", status: "manual", reason: "x" },
      { app: "rpm:firefox", target: linux, part: "config", status: "ready", reason: "copied" },
    ];
    const rows = rowsFromCoverage(cov, (id) => id.split(":")[1]);
    expect(rows.map((r) => [r.app, r.verdict, r.plan])).toEqual([
      ["firefox", "ready", "APT package firefox"],
      ["tool", "unavailable", "Skipped"],
      ["old", "review", "—"],
    ]);
  });

  it("plan items keep what was considered", () => {
    const item = {
      id: "install:rpm:firefox", kind: "installer", app: "Firefox", label: "Firefox — installer", status: "ready", reason: "r",
      channel: "online", method: "APT package firefox", admin: true, destination: null, files: 0, bytes: 0, conflicts: 0, identical: 0,
      kept: 0, conflict_examples: [], verdict: "ready",
      considered: [{ option: "The installer in the snapshot", outcome: "Not usable here: RPM packages are never converted." }],
    } as PlanItem;
    const folder = { ...item, id: "r0", kind: "folder", app: null } as PlanItem;
    const rows = rowsFromPlan([item, folder]);
    expect(rows).toHaveLength(1);
    expect(rows[0].considered[0].outcome).toMatch(/never converted/);
    expect(rowsFromPlan([{ ...item, channel: "snapshot" }])[0].plan).toMatch(/snapshot/);
  });
});
