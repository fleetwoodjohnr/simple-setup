"""Accessibility audit of the Simple Setup desktop app: axe-core (WCAG 2.x A/AA rules) on every
page and dialog, plus keyboard checks (reach, visible focus, Escape, focus return) and reflow at
the minimum window size. Writes a JSON report; exits 1 if anything fails.
Usage: a11y.py APP FIXTURE_DIR AXE_JS REPORT_JSON"""
import json, os, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wd import ENTER, ESCAPE, SHIFT, TAB, Session  # noqa: E402

APP, FIX, AXE, REPORT = sys.argv[1:5]
HOME, USB = f"{FIX}/home", f"{FIX}/usb"
w = Session(APP, f"{FIX}/shots")
results, failures = {}, []

AXE_RUN = """
const done = arguments[arguments.length - 1];
axe.run(document, {runOnly: {type: 'tag', values: ['wcag2a','wcag2aa','wcag21a','wcag21aa','wcag22aa']}, resultTypes: ['violations']})
  .then(r => done(r.violations.map(v => ({id: v.id, impact: v.impact, help: v.help,
     nodes: v.nodes.slice(0, 5).map(n => n.target.join(' ') + ' :: ' + (n.failureSummary || '').split('\\n').slice(0, 2).join(' '))}))))
  .catch(e => done([{id: 'axe-error', impact: 'critical', help: String(e), nodes: []}]));
"""


# Both themes are checked whatever the system prefers: the stylesheet's dark-mode rules are
# lifted out of their media query and switched on or off.
SET_THEME = """
if (!window.__forced) {
  window.__dark = [];
  for (const sh of document.styleSheets)
    for (let i = sh.cssRules.length - 1; i >= 0; i--) {
      const r = sh.cssRules[i];
      if (r.conditionText && r.conditionText.includes('prefers-color-scheme: dark')) {
        window.__dark.push([...r.cssRules].map(x => x.cssText).join('\\n'));
        sh.deleteRule(i);
      }
    }
  window.__forced = document.createElement('style');
  document.head.appendChild(window.__forced);
}
window.__forced.textContent = arguments[0] === 'dark' ? window.__dark.join('\\n') : '';
"""


def audit(name):
    if not w.js("return typeof axe !== 'undefined'"):
        w.js(open(AXE).read())
    for theme in ["light", "dark"]:
        w.js(SET_THEME, theme)
        v = w.js_async(AXE_RUN)
        results[f"{name} ({theme})"] = v
        w.shot(f"a11y-{name}-{theme}")
        for x in v:
            failures.append(f"{name} ({theme}): {x['id']} ({x['impact']}) {x['help']} -> {x['nodes'][:2]}")


def focused():
    return w.js("""const e = document.activeElement; const s = getComputedStyle(e);
      return {tag: e.tagName, text: (e.getAttribute('aria-label') || e.textContent || e.value || '').trim().slice(0, 50),
              outline: s.outlineStyle !== 'none' && parseFloat(s.outlineWidth) > 0, shadow: s.boxShadow !== 'none',
              inDialog: !!e.closest('dialog[open]')};""")


def tab_through(limit=80, back=False):
    """Tabs through the page; returns what got focus. Every focused control must show a focus ring."""
    seen = []
    for _ in range(limit):
        w.keys(SHIFT, TAB) if back else w.keys(TAB)
        f = focused()
        if f["tag"] == "BODY":
            continue
        seen.append(f)
        if not (f["outline"] or f["shadow"]):
            failures.append(f"no visible focus indicator on {f['tag']} '{f['text']}'")
    return seen


def escape_closes(opener_xpath, dialog_name):
    w.click(opener_xpath)
    w.wait("//dialog[@open]")
    audit(dialog_name)
    if not focused()["inDialog"]:
        failures.append(f"{dialog_name}: focus did not move into the dialog")
    w.keys(ESCAPE)
    time.sleep(0.5)
    if w.els("//dialog[@open]"):
        failures.append(f"{dialog_name}: Escape did not close it")
    if focused()["tag"] == "BODY":
        failures.append(f"{dialog_name}: focus was lost after closing")


try:
    w.window(1360, 960)
    w.wait("//div[@class='stat']", 180)
    audit("dashboard")
    names = [f["text"] for f in tab_through(12)]
    if not names or names[0] != "Skip to content":
        failures.append(f"keyboard: the first Tab stop should be 'Skip to content' (got {names[:1]})")
    for nav in ["Dashboard", "Applications", "Create Snapshot", "Snapshots", "Restore"]:
        if not any(nav in n for n in names):
            failures.append(f"keyboard: '{nav}' not reachable with Tab (got {names})")
    if w.js("return document.documentElement.lang") != "en":
        failures.append("html lang missing")

    w.click("//nav//button[normalize-space()='Applications']")
    w.wait("//table")
    audit("applications")
    escape_closes("(//table//button[starts-with(@aria-label,'Details for')])[1]", "app-details")

    w.click("//nav//button[normalize-space()='Create Snapshot']")
    w.wait("//h2[@id='s1']")
    audit("create-step1-apps")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s2']")
    w.click("//label[normalize-space()='Documents']/input")
    time.sleep(1)
    audit("create-step2-files")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s3']")
    w.type_into("//label[span='Snapshot name']/input", "Audit")
    w.type_into("//label[span='Or type a folder path']/input", USB)
    w.click("//label[contains(normalize-space(),'Encrypt the snapshot with a password')]/input")
    w.type_into("//label[span='Password (at least 8 characters)']/input", "audit password 1")
    w.type_into("//label[span='Type it again']/input", "audit password 1")
    time.sleep(1)
    audit("create-step3-configure")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s4']")
    w.wait("//button[normalize-space()='Build Snapshot' and not(@disabled)]", 60)
    audit("create-step4-review")
    w.click("//button[normalize-space()='Build Snapshot']")
    w.wait("//dialog[@open]")
    audit("build-confirm-dialog")
    w.click("//dialog//button[normalize-space()='Build']")
    w.wait("//h1[contains(.,'Snapshot built')]", 300)
    audit("build-done")

    w.click("//nav//button[normalize-space()='Snapshots']")
    w.wait("//td[contains(.,'Audit')]", 30)
    audit("snapshots")
    escape_closes("(//button[normalize-space()='Inspect'])[1]", "inspect-dialog")
    escape_closes("(//button[normalize-space()='Send…'])[1]", "send-dialog")
    escape_closes("(//button[normalize-space()='Delete'])[1]", "delete-dialog")

    w.click("//nav//button[normalize-space()='Restore']")
    w.type_into("//label[span='Or type the snapshot folder path']/input", f"{USB}/Audit")
    w.click("//form//button[normalize-space()='Open']")
    w.wait("//strong[normalize-space()='This snapshot is encrypted']", 60)
    audit("restore-locked")
    w.type_into("//label[span='Password or recovery key']/input", "audit password 1")
    w.click("//button[normalize-space()='Unlock']")
    w.wait("//table/caption[normalize-space()='Restore plan']", 60)
    audit("restore-plan")
    w.click("//button[contains(.,'Receive from another computer')]")
    w.wait("//dialog[@open]")
    audit("receive-dialog")
    w.type_into("//dialog//label[span='Or type a folder path']/input", f"{FIX}/inbox")
    w.click("//dialog//button[normalize-space()='Start receiving']")
    w.wait("//dialog//p[contains(@class,'mono')]", 30)
    audit("receive-waiting")
    w.click("//dialog//button[normalize-space()='Stop receiving']")
    time.sleep(1)

    # Reflow at the smallest window the app allows: nothing may need sideways scrolling.
    # And at 200% zoom (Ctrl +), which leaves 680 CSS pixels of width.
    for (width, height, zoom) in [(760, 540, "1"), (1360, 960, "2")]:
        w.window(width, height)
        w.js("document.documentElement.style.zoom = arguments[0]", zoom)
        for page in ["Dashboard", "Applications", "Create Snapshot", "Snapshots", "Restore"]:
            w.click(f"//nav//button[normalize-space()='{page}']")
            time.sleep(1)
            over = w.js("return document.documentElement.scrollWidth - document.documentElement.clientWidth")
            if over > 4:
                w.shot(f"reflow-{page}-{zoom}x")
                failures.append(f"reflow: {page} scrolls sideways by {over}px at {width}x{height}, zoom {zoom}")
    w.js("document.documentElement.style.zoom = '1'")
    if not w.js("return document.title").endswith("— Simple Setup"):
        failures.append("window title does not name the page")
finally:
    json.dump({"axe": results, "failures": failures}, open(REPORT, "w"), indent=1)
    w.close()
print(f"{len(results)} states audited, {len(failures)} problem(s)")
for f in failures:
    print(" -", f)
sys.exit(1 if failures else 0)
