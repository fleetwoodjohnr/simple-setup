import { describe, expect, it } from "vitest";
import { canProceed, emptySelection, reconcile, setApp, setApps, setPart, setFolder, setTarget } from "./selection";
import type { App } from "./types";

const app = (id: string, kinds: ("config" | "data")[] = ["config"]): App => ({
  id, name: id, version: null, source: "rpm", native_id: id, kind: "desktop", canonical: null, available_on: [],
  repository: null, system: false, install_size: null, path: null, github: null, confidence: "confirmed",
  evidence: [], user_fields: [], locations: kinds.map((kind) => ({ kind, path: `/h/${id}/${kind}`, origin: "detected" })),
});

describe("selection", () => {
  it("starts empty and only adds what is ticked", () => {
    let s = emptySelection();
    s = setApp(s, app("a", ["config", "data"]), true);
    expect(s.apps).toEqual([{ id: "a", installer: true, config: true, data: false, exclude: [] }]);
    expect(setApp(s, app("a"), false).apps).toEqual([]);
  });

  it("removes an app when its last part is turned off", () => {
    let s = setApp(emptySelection(), app("a", []), true);
    expect(s.apps[0].config).toBe(false);
    s = setPart(s, "a", "installer", false);
    expect(s.apps).toEqual([]);
  });

  it("select all keeps existing part choices", () => {
    let s = setApp(emptySelection(), app("a"), true);
    s = setPart(s, "a", "installer", false);
    s = setApps(s, [app("a"), app("b")], true);
    expect(s.apps.map((c) => [c.id, c.installer])).toEqual([["a", false], ["b", true]]);
    expect(setApps(s, [app("a"), app("b")], false).apps).toEqual([]);
  });

  it("folders, targets and step gating", () => {
    const linux = { os: "linux", arch: "x86_64" } as const;
    let s = setFolder(emptySelection(), "/h/Documents", "Documents", true);
    expect(canProceed(2, s, "", null)).toBeNull();
    expect(canProceed(3, s, "x", "/media/usb")).toMatch(/target/);
    s = setTarget(s, linux, true);
    expect(canProceed(3, s, "x", "/media/usb")).toBeNull();
    expect(canProceed(2, emptySelection([linux]), "x", "/m")).toMatch(/at least one/);
  });

  it("reconcile reports apps that disappeared", () => {
    const s = setApps(emptySelection(), [app("a"), app("gone")], true);
    const r = reconcile(s, [app("a")]);
    expect(r.missing).toEqual(["gone"]);
    expect(r.selection.apps.map((c) => c.id)).toEqual(["a"]);
  });
});
