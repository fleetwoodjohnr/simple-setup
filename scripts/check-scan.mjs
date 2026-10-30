// CI check for `simple-setup scan --json` on a real runner: each OS's main adapters must find
// something (one source from each group), IDs must be unique and every app must have a name.
import { readFileSync } from "node:fs";

const inv = JSON.parse(readFileSync(process.argv[2], "utf8"));
const expected = {
  linux: [["dpkg", "rpm", "pacman"]],
  windows: [["arp"]],
  macos: [["app_bundle"], ["brew", "brew_cask"]],
}[inv.device.platform.os];
const by = {};
for (const a of inv.apps) (by[a.source] ??= []).push(a.name);
console.log(`${inv.device.os_name} (${inv.device.platform.os}-${inv.device.platform.arch}): ${inv.apps.length} apps, ${inv.hidden_packages} hidden`);
for (const [s, names] of Object.entries(by)) console.log(`  ${s}: ${names.length} — ${names.slice(0, 6).join(", ")}`);
for (const n of inv.notes) console.log(`  note: ${n}`);
const problems = [];
for (const group of expected) if (!group.some((s) => by[s])) problems.push(`no apps from ${group.join(" or ")}`);
if (new Set(inv.apps.map((a) => a.id)).size !== inv.apps.length) problems.push("duplicate app IDs");
if (inv.apps.some((a) => !a.name.trim())) problems.push("an app has no name");
if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
