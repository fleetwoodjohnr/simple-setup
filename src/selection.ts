// Pure selection helpers. Nothing is ever selected implicitly: every app or folder enters the
// selection through an explicit user action.
import type { App, AppChoice, FolderChoice, KnownFolder, Part, Platform, Selection } from "./types";

export const emptySelection = (targets: Platform[] = []): Selection => ({ apps: [], folders: [], include_secrets: [], targets });

export const samePlatform = (a: Platform, b: Platform) => a.os === b.os && a.arch === b.arch;

export const hasLocations = (app: App, kind: "config" | "data") => app.locations.some((l) => l.kind === kind);

/** Sensible starting parts when the user ticks an app: reinstall + settings; data is opt-in. */
export function defaultChoice(app: App): AppChoice {
  return { id: app.id, installer: true, config: hasLocations(app, "config"), data: false, exclude: [] };
}

export function setApp(sel: Selection, app: App, on: boolean): Selection {
  const apps = sel.apps.filter((c) => c.id !== app.id);
  return { ...sel, apps: on ? [...apps, defaultChoice(app)] : apps };
}

export function setApps(sel: Selection, apps: App[], on: boolean): Selection {
  return apps.reduce((s, a) => (on && s.apps.some((c) => c.id === a.id) ? s : setApp(s, a, on)), sel);
}

/** Turning off the last part removes the app, so a selection never holds an empty choice. */
export function setPart(sel: Selection, id: string, part: Part, value: boolean): Selection {
  const apps = sel.apps
    .map((c) => (c.id === id ? { ...c, [part]: value } : c))
    .filter((c) => c.installer || c.config || c.data);
  return { ...sel, apps };
}

export const toggle = (list: string[], item: string) => (list.includes(item) ? list.filter((x) => x !== item) : [...list, item]);

export function setAppExclude(sel: Selection, id: string, path: string): Selection {
  return { ...sel, apps: sel.apps.map((c) => (c.id === id ? { ...c, exclude: toggle(c.exclude, path) } : c)) };
}

export function setFolder(sel: Selection, path: string, known: KnownFolder | null, on: boolean): Selection {
  const folders = sel.folders.filter((f) => f.path !== path);
  return { ...sel, folders: on ? [...folders, { path, known, exclude: [] } satisfies FolderChoice] : folders };
}

export function setFolderExclude(sel: Selection, folder: string, path: string): Selection {
  return { ...sel, folders: sel.folders.map((f) => (f.path === folder ? { ...f, exclude: toggle(f.exclude, path) } : f)) };
}

export function setTarget(sel: Selection, p: Platform, on: boolean): Selection {
  const targets = sel.targets.filter((t) => !samePlatform(t, p));
  return { ...sel, targets: on ? [...targets, p] : targets };
}

/** Drops choices for apps that are no longer installed (e.g. when reusing an old profile). */
export function reconcile(sel: Selection, apps: App[]): { selection: Selection; missing: string[] } {
  const ids = new Set(apps.map((a) => a.id));
  const missing = sel.apps.filter((c) => !ids.has(c.id)).map((c) => c.id);
  return { selection: { ...sel, apps: sel.apps.filter((c) => ids.has(c.id)) }, missing };
}

export function canProceed(step: number, sel: Selection, name: string, destination: string | null): string | null {
  if (step >= 2 && sel.apps.length === 0 && sel.folders.length === 0) return "Choose at least one application or folder.";
  if (step >= 3) {
    if (!name.trim()) return "Give the snapshot a name.";
    if (!destination) return "Choose where to save the snapshot.";
    if (sel.targets.length === 0) return "Choose at least one target computer type.";
  }
  return null;
}
