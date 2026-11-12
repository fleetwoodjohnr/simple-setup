"""Accessibility audit of the Simple Setup desktop app: axe-core (WCAG 2.x A/AA rules) on every
page and dialog, plus keyboard checks (reach, visible focus, Escape, focus return) and reflow at
the minimum window size. Writes a JSON report; exits 1 if anything fails.
Usage: a11y.py APP FIXTURE_DIR AXE_JS REPORT_JSON"""
import json, os, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wd import ENTER, ESCAPE, SHIFT, TAB, Session  # noqa: E402

APP, FIX, AXE, REPORT = sys.argv[1:5]
NAV = ["Dashboard", "Snapshots", "Transfers", "Applications", "Settings"]
START = "//button[contains(@class,'start')][.//strong[normalize-space()='{}']]"
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


# Both themes are checked whatever the system prefers: the theme is the `data-theme` attribute
# on <html> (src/theme.ts), set here directly.
SET_THEME = "document.documentElement.dataset.theme = arguments[0];"


def audit(name):
    if not w.js("return typeof axe !== 'undefined'"):
        w.js(open(AXE).read())
    for theme in ["light", "dark"]:
        w.js(SET_THEME, theme)
        time.sleep(0.4)  # let colour transitions finish, or axe measures in-between colours
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
    for nav in NAV:
        if not any(nav in n for n in names):
            failures.append(f"keyboard: '{nav}' not reachable with Tab (got {names})")
    if w.js("return document.documentElement.lang") != "en":
        failures.append("html lang missing")

    w.click("//nav//button[normalize-space()='Applications']")
    w.wait("//table")
    audit("applications")
    escape_closes("(//table//button[starts-with(@aria-label,'Details for')])[1]", "app-details")

    w.click("//nav//button[normalize-space()='Dashboard']")
    w.click(START.format("Create Snapshot"))
    w.wait("//h2[@id='s1']")
    # One app, to see its compatibility in step 4; unticked again before building, so the
    # build does not download installers.
    w.click("(//input[starts-with(@aria-label,'Include ')])[1]")
    audit("create-step1-apps")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s2']")
    w.click("//label[normalize-space()='Documents']/input")
    time.sleep(1)
    audit("create-step2-files")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s3']")
    # The three ways to say where it is going; "Another kind" shows the system pickers.
    w.click("//label[contains(@class,'choice')][.//strong[normalize-space()='Another kind of computer']]")
    w.wait("//select[@aria-label='Linux distribution']")
    audit("create-step3-choose-system")
    w.click("//label[contains(@class,'choice')][.//strong[normalize-space()='Not decided yet']]")
    time.sleep(0.5)
    audit("create-step3-undecided")
    w.click("//label[contains(@class,'choice')][.//strong[normalize-space()='A computer like this one']]")
    w.type_into("//label[span='Snapshot name']/input", "Audit")
    w.type_into("//label[span='Or type a folder path']/input", USB)
    w.click("//label[contains(normalize-space(),'Encrypt the snapshot with a password')]/input")
    w.type_into("//label[span='Password (at least 8 characters)']/input", "audit password 1")
    w.type_into("//label[span='Type it again']/input", "audit password 1")
    time.sleep(1)
    audit("create-step3-destination")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s4']")
    w.wait("//h2[@id='compat-title'] | //div[@role='alert']", 120)
    time.sleep(1)
    audit("create-step4-compatibility")
    escape_closes("(//button[starts-with(@aria-label,'Why:')])[1]", "compatibility-why")
    for _ in range(3):
        w.click("//div[contains(@class,'footer-bar')]//button[normalize-space()='Back']")
    w.wait("//h2[@id='s1']")
    w.click("(//input[starts-with(@aria-label,'Include ')][@checked or true])[1]")
    if w.js("return [...document.querySelectorAll('input[aria-label^=\"Include \"]')].some((i) => i.checked)"):
        failures.append("could not untick the app again")
    for step in ["s2", "s3", "s4", "s5"]:
        w.click("//button[starts-with(normalize-space(),'Next')]")
        w.wait(f"//h2[@id='{step}']")
    w.wait("//button[normalize-space()='Create Snapshot' and not(@disabled)]", 60)
    audit("create-step5-review")
    w.click("//div[contains(@class,'footer-bar')]//button[normalize-space()='Create Snapshot']")
    w.wait("//dialog[@open]")
    audit("build-confirm-dialog")
    w.click("//dialog//button[normalize-space()='Create']")
    w.wait("//h1[contains(.,'Snapshot built')]", 300)
    audit("build-done")

    w.click("//nav//button[normalize-space()='Snapshots']")
    w.wait("//td[contains(.,'Audit')]", 30)
    audit("snapshots")
    escape_closes("(//button[normalize-space()='Inspect'])[1]", "inspect-dialog")
    escape_closes("(//button[normalize-space()='Send…'])[1]", "send-dialog")
    escape_closes("(//button[normalize-space()='Delete'])[1]", "delete-dialog")

    w.click("//div[@class='page-head']//button[normalize-space()='Restore Snapshot']")
    w.type_into("//label[span='Or type the snapshot folder path']/input", f"{USB}/Audit")
    w.click("//form//button[normalize-space()='Open']")
    w.wait("//strong[normalize-space()='This snapshot is encrypted']", 60)
    audit("restore-locked")
    w.type_into("//label[span='Password or recovery key']/input", "audit password 1")
    w.click("//button[normalize-space()='Unlock']")
    w.wait("//table/caption[normalize-space()='Restore plan']", 60)
    audit("restore-plan")
    if w.els("//button[starts-with(@aria-label,'Why:')]"):
        escape_closes("(//button[starts-with(@aria-label,'Why:')])[1]", "restore-why")

    # Transfers: history, receiving, and the Transfer flow's Connect step.
    w.click("//nav//button[normalize-space()='Transfers']")
    w.wait("//h2[@id='hist']")
    audit("transfers")
    w.click(START.format("Receive on this computer"))
    w.wait("//dialog[@open]")
    audit("receive-dialog")
    w.type_into("//dialog//label[span='Or type a folder path']/input", f"{FIX}/inbox")
    w.click("//dialog//button[normalize-space()='Start receiving']")
    w.wait("//dialog//p[contains(@class,'mono')]", 30)
    audit("receive-waiting")
    w.click("//dialog//button[normalize-space()='Stop receiving']")
    time.sleep(1)
    w.click(START.format("Transfer to Another Computer"))
    w.wait("//h2[@id='s1']")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s2']")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s3']")
    time.sleep(4)  # the nearby-computer search
    audit("transfer-connect")

    # Settings: the theme applies at once and is saved; checking for updates answers without
    # blocking the window (a result or an error, never a hang).
    w.click("//nav//button[normalize-space()='Settings']")
    w.wait("//h2[@id='updates']")
    audit("settings")
    for choice, want in [("Dark", "dark"), ("Light", "light")]:
        w.click(f"//fieldset//label[normalize-space()='{choice}']")
        time.sleep(0.5)
        if w.js("return document.documentElement.dataset.theme") != want:
            failures.append(f"theme: choosing {choice} did not switch the window to {want}")
    w.click("//fieldset//label[normalize-space()='System']")
    time.sleep(0.5)
    saved = json.load(open(f"{HOME}/.config/simple-setup/settings.json"))
    if saved.get("theme") != "system":
        failures.append(f"theme: the choice was not saved (settings.json has {saved.get('theme')!r})")
    # Settings → Linux distributions: the manager, the guided form, a profile's details.
    w.click("//button[normalize-space()='Manage distributions…']")
    w.wait("//dialog[@open]//td[contains(.,'Linux Mint')]", 30)
    audit("distros-manager")
    w.click("//dialog[@open]//button[normalize-space()='Add distribution…']")
    w.wait("//dialog[@open]//label[span='Recognised by os-release ID']")
    audit("distros-add")
    w.type_into("//dialog[@open]//label[span='Name']/input", "Audit OS")
    w.type_into("//dialog[@open]//label[span='Profile ID (lowercase, no spaces)']/input", "auditos")
    w.type_into("//dialog[@open]//label[span='Recognised by os-release ID']/input", "auditos")
    w.click("//dialog[@open]//button[normalize-space()='Validate and test']")
    w.wait("//dialog[@open]//strong[normalize-space()='Valid.']", 30)
    audit("distros-add-validated")
    w.click("//dialog[@open]//button[normalize-space()='Save']")
    w.wait("//dialog[@open]//td[contains(.,'Audit OS')]", 30)
    saved = json.load(open(f"{HOME}/.config/simple-setup/distros/user.json"))
    if not any(p["id"] == "auditos" and p.get("parent") == "ubuntu" for p in saved["profiles"]):
        failures.append(f"distros: the new profile was not saved as based on Ubuntu ({saved})")
    w.click("(//dialog[@open]//tr[contains(.,'Audit OS')]//button[normalize-space()='Details'])[1]")
    w.wait("//dialog[@open]//dt[normalize-space()='Based on']")
    audit("distros-details")
    w.keys(ESCAPE)
    time.sleep(0.5)
    w.keys(ESCAPE)
    time.sleep(0.5)
    if w.els("//dialog[@open]"):
        failures.append("distros: Escape did not close the dialogs")

    w.click("//button[normalize-space()='Check for Updates']")
    try:
        w.wait("//section[@aria-labelledby='updates']//*[@role='status' and normalize-space()] | //section[@aria-labelledby='updates']//*[@role='alert']", 60)
        audit("settings-update-checked")
    except SystemExit:
        failures.append("updates: no answer within 60 s after Check for Updates")

    # Reflow at the smallest window the app allows: nothing may need sideways scrolling.
    # And at 200% zoom (Ctrl +), which leaves 680 CSS pixels of width.
    for (width, height, zoom) in [(760, 540, "1"), (1360, 960, "2")]:
        w.window(width, height)
        w.js("document.documentElement.style.zoom = arguments[0]", zoom)
        for page in NAV:
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
