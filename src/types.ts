// Mirrors of the Rust types crossing the IPC boundary (crates/core). Keep in sync.

export type OsFamily = "linux" | "windows" | "macos";
export type Arch = "x86_64" | "aarch64";
export interface Platform { os: OsFamily; arch: Arch }
export interface Device { hostname: string; os_name: string; platform: Platform }

export type Source = "flatpak" | "rpm" | "dpkg" | "pacman" | "snap" | "app_image" | "custom" | "git_clone" | "manual"
  | "arp" | "msix" | "scoop" | "choco" | "brew" | "brew_cask" | "app_bundle" | "pkg" | "macports";
export type Kind = "desktop" | "command_line" | "source_build";
export type Confidence = "unknown" | "probable" | "confirmed";
export type Origin = "detected" | "inferred" | "user";
export type LocKind = "config" | "data";
export interface Location { kind: LocKind; path: string; origin: Origin }
export interface GitHub { repo: string; reference: string | null; asset: string | null; confidence: Confidence; origin: Origin }
export interface App {
  id: string; name: string; version: string | null; source: Source; native_id: string; kind: Kind;
  canonical: string | null; available_on: OsFamily[]; repository: string | null; system: boolean; install_size: number | null;
  path: string | null; locations: Location[]; github: GitHub | null; confidence: Confidence;
  evidence: string[]; user_fields: string[]; downloads?: Download[];
}
/** An installer link the user registered for one target; fetched only while building, checked by SHA-256. */
export interface Download { target: Platform; url: string; sha256: string }
export interface Inventory { device: Device; scanned_at: string; apps: App[]; hidden_packages: number; notes: string[] }

export type KnownFolder = "Documents" | "Desktop" | "Downloads" | "Pictures" | "Videos" | "Music";
export interface AppChoice { id: string; installer: boolean; config: boolean; data: boolean; exclude: string[] }
export interface FolderChoice { path: string; known: KnownFolder | null; exclude: string[] }
export interface Selection { apps: AppChoice[]; folders: FolderChoice[]; include_secrets: string[]; targets: Platform[] }

export interface Measure { path: string; exists: boolean; files: number; bytes: number; secrets: number; transient_bytes: number; partial: boolean }
export interface DirItem { name: string; path: string; is_dir: boolean; size: number | null; secret: string | null; transient: boolean }
export interface KnownFolderInfo { folder: KnownFolder; path: string | null; exists: boolean }
export interface Volume { name: string; mount: string; file_system: string; total: number; free: number; removable: boolean }
export interface Settings { source_dirs: string[] }
export interface AppEdit {
  name: string | null; version: string | null; canonical: string | null; github: string | null; github_ref: string | null;
  add_locations: Location[]; remove_locations: string[]; downloads: Download[] | null;
}
export interface Profile { name: string; saved_at: string; selection: Selection }

export type Status = "ready" | "manual" | "unsupported";
export type Part = "installer" | "config" | "data";
export type RootKind = "config" | "data" | "folder";
export interface Coverage { app: string; target: Platform; part: Part; status: Status; reason: string }
export interface Readiness { target: Platform; offline_ready: boolean; engine_included: boolean; reasons: string[] }
export interface ExcludedSummary {
  secrets: number; transient: number; by_user: number; unreadable: number; special: number;
  secret_examples: string[]; unreadable_examples: string[];
}
export interface RootSummary { id: string; kind: RootKind; app: string | null; label: string; original: string; files: number; bytes: number }
export interface Preflight {
  roots: RootSummary[]; total_files: number; total_bytes: number; engine_bytes: number; installer_bytes: number; excluded: ExcludedSummary;
  coverage: Coverage[]; readiness: Readiness[]; warnings: string[]; destination_free: number | null;
  required_bytes: number; fits: boolean;
}
export type Progress =
  | { kind: "stage"; label: string; step: number; steps: number }
  | { kind: "copy"; done_bytes: number; total_bytes: number; done_files: number; total_files: number; current: string }
  | { kind: "warning"; message: string }
  | { kind: "note"; message: string };
export interface Failure { path: string; error: string }
export interface BuildReport {
  path: string; id: string; name: string; files: number; bytes: number; resumed_files: number; failed: Failure[];
  warnings: string[]; excluded: ExcludedSummary; coverage: Coverage[]; readiness: Readiness[];
  reused_files: number; reused_bytes: number;
  /** Encrypted snapshots only: opens it without the password. Shown once; never stored. */
  recovery_key?: string | null;
}
export interface SnapshotRef { path: string; id: string; name: string; created_at: string; last_verified: string | null; last_verify_ok: boolean | null }
export interface Summary {
  path: string; id: string; name: string; created_at: string; source: Device; targets: Platform[]; apps: string[];
  folders: string[]; files: number; bytes: number; installers: number; readiness: Readiness[]; warnings: string[]; selection: Selection;
  encrypted: boolean; base: string | null;
}
export interface Peer { name: string; os: string | null; addresses: string[] }
export interface SendReport { receiver: Device; files: number; bytes: number; resumed_bytes: number; saved_as: string }
export type ReceiveEvent =
  | { kind: "ready"; code: string; address: string | null; announce_error: string | null }
  | { kind: "offer"; from: Device; name: string; files: number; bytes: number; encrypted: boolean; targets: Platform[] }
  | { kind: "progress"; progress: Progress };
export interface SnapshotItem { reference: SnapshotRef; summary: Summary | null; error: string | null }
export interface VerifyReport { ok: boolean; checked: number; bytes: number; missing: string[]; corrupt: string[] }

export type ItemKind = "installer" | "config" | "data" | "folder";
export interface PlanItem {
  id: string; kind: ItemKind; app: string | null; label: string; status: Status; reason: string; admin: boolean; destination: string | null;
  files: number; bytes: number; conflicts: number; identical: number; kept: number; conflict_examples: string[];
}
export interface RestorePlan {
  snapshot_id: string; snapshot_name: string; created_at: string; source: Device; this_device: Device;
  prepared_for_this_platform: boolean; selection: Selection; items: PlanItem[]; required_bytes: number;
  free_bytes: number | null; needs_admin: boolean; warnings: string[];
}
export type Conflict = "skip" | "keep_both";
export type Outcome = "installed" | "restored" | "partial" | "failed" | "manual" | "skipped" | "not_selected";
export interface ItemResult {
  id: string; label: string; outcome: Outcome; message: string; restored: number; identical: number;
  kept_both: number; skipped_conflicts: number; kept: number; failed: Failure[];
}
export interface RestoreReport { snapshot_id: string; snapshot_name: string; finished_at: string; items: ItemResult[]; manual_steps: string[]; report_file: string | null }
export interface CmdError { code: string; message: string }
