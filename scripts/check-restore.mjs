// Checks a `simple-setup restore` log from a real runner: every named app must be reported as
// installed (or already installed), and nothing may have failed. With --again, every app must
// be found already installed (a repeated restore changes nothing).
//   node scripts/check-restore.mjs [--again] restore.log KeePassXC ripgrep
import { readFileSync } from "node:fs";

let args = process.argv.slice(2);
const again = args[0] === "--again";
if (again) args = args.slice(1);
const [logFile, ...names] = args;
const log = readFileSync(logFile, "utf8");
const lines = log.split(/\r?\n/);
const problems = [];
for (const n of names) {
  const line = lines.find((l) => new RegExp(`^\\s*installed: ${n.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`, "i").test(l));
  if (!line) problems.push(`${n} is not reported as installed`);
  else if (again && !/already installed/i.test(line)) problems.push(`${n} was installed again: ${line.trim()}`);
}
for (const l of lines.filter((l) => /^\s*failed: /.test(l))) problems.push(l.trim());
console.log(log);
if (problems.length) {
  console.error(problems.join("\n"));
  process.exit(1);
}
console.log(`OK: ${names.join(", ")}${again ? " (already installed)" : ""}`);
