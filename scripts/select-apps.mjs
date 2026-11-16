// Writes a selection (stdout) that reinstalls the named apps for one target, from a
// `simple-setup scan --json` inventory. Used by the migration workflow on real runners.
//   node scripts/select-apps.mjs scan.json windows-x86_64 keepassxc ripgrep > selection.json
import { readFileSync } from "node:fs";

const [scanFile, target, ...wanted] = process.argv.slice(2);
const inv = JSON.parse(readFileSync(scanFile, "utf8"));
const [os, arch] = target.split("-", 2).length === 2 ? [target.split("-")[0], target.slice(target.indexOf("-") + 1)] : [];
if (!os || !arch || !wanted.length) {
  console.error("usage: select-apps.mjs scan.json <os>-<arch> <registry id>...");
  process.exit(2);
}
const apps = wanted.map((w) => {
  const a = inv.apps.find((x) => x.canonical === w) ?? inv.apps.find((x) => x.name.toLowerCase() === w);
  if (!a) {
    console.error(`${w} was not found by the scan (${inv.apps.length} apps).`);
    process.exit(1);
  }
  console.error(`${w}: ${a.id} (${a.source})`);
  return { id: a.id, installer: true, config: false, data: false };
});
console.log(JSON.stringify({ apps, folders: [], targets: [{ os, arch }] }));
