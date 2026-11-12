"""Acceptance flow in the real desktop app: build an encrypted snapshot of Documents, send it to a
receiving computer (the CLI), receive one from another computer (the CLI again), unlock and
restore it, then build an encrypted update. Usage: acceptance.py APP FIXTURE_DIR CLI"""
import json, os, re, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from wd import Session  # noqa: E402

APP, FIX, CLI = sys.argv[1:4]
HOME, USB = f"{FIX}/home", f"{FIX}/usb"
PW = "correct horse battery"
CODE = re.compile(r"^\s+(\d{4}-\d{4})\s*$")


def cli(home, *args):
    return subprocess.Popen([CLI, *args], env={**os.environ, "HOME": home}, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


w = Session(APP, f"{FIX}/shots")
out = {}
try:
    w.window(1360, 960)
    w.wait("//div[@class='stat']", 180)
    START = "//button[contains(@class,'start')][.//strong[normalize-space()='{}']]"
    # 1. An encrypted snapshot of Documents.
    w.click(START.format("Create Snapshot"))
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s2']")
    w.click("//label[normalize-space()='Documents']/input")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s3']")
    w.type_into("//label[span='Snapshot name']/input", "GUI Locked")
    w.type_into("//label[span='Or type a folder path']/input", USB)
    w.click("//label[contains(normalize-space(),'Encrypt the snapshot with a password')]/input")
    w.type_into("//label[span='Password (at least 8 characters)']/input", PW)
    w.type_into("//label[span='Type it again']/input", PW)
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s4']")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s5']")
    w.click("//div[contains(@class,'footer-bar')]//button[normalize-space()='Create Snapshot' and not(@disabled)]", 60)
    w.click("//dialog//button[normalize-space()='Create']")
    w.wait("//h1[contains(.,'Snapshot built')]", 300)
    out["recovery_key_shown"] = w.text("//code[@id='recovery-key']").startswith("AGE-SECRET-KEY-1")
    w.shot("1-built")

    # 2. Send it to a computer that is receiving (the CLI).
    os.makedirs(f"{FIX}/cli1/inbox", exist_ok=True)
    recv = cli(f"{FIX}/cli1", "receive", "--dest", f"{FIX}/cli1/inbox", "--yes")
    code = addr = None
    while not (code and addr):
        line = recv.stdout.readline()
        if m := CODE.match(line):
            code = m.group(1)
        if "Address" in line:
            addr = line.rsplit(" ", 1)[-1].strip()
    w.click("//button[contains(.,'Send to another computer')]")
    w.wait("//dialog//p[contains(.,'Looking')] | //dialog//input[@name='peer'] | //dialog//p[contains(.,'None found')]")
    time.sleep(4)
    # The receiver may be listed under another of its addresses; its port identifies it.
    port = addr.rsplit(":", 1)[-1]
    if w.els(f"//dialog//label[contains(.,':{port}')]/input"):
        w.click(f"//dialog//label[contains(.,':{port}')]/input")
        out["found_by_mdns"] = True
    else:
        w.type_into("//dialog//label[span='Address']/input", addr)
        out["found_by_mdns"] = False
    w.type_into("//dialog//label[span='Pairing code shown on the other computer']/input", code)
    w.click("//dialog//button[normalize-space()='Send' and not(@disabled)]")
    w.wait("//dialog//div[contains(.,'Sent to')]", 120)
    w.shot("2-sent")
    out["cli_received"] = "Received and checked" in recv.communicate(timeout=60)[0]
    w.click("//dialog//button[normalize-space()='Close']")

    # 3. Receive a snapshot from another computer (the CLI sends it back).
    w.click("//nav//button[normalize-space()='Transfers']")
    w.click(START.format("Receive on this computer"))
    w.type_into("//dialog//label[span='Or type a folder path']/input", f"{FIX}/inbox")
    w.click("//dialog//label[contains(normalize-space(),'Restore automatically when it arrives')]/input")
    w.click("//dialog//button[normalize-space()='Start receiving']")
    code = w.text("//dialog//p[contains(@class,'mono')]")
    addr = w.text("//dialog//p[contains(.,'type its address')]/span")
    send = cli(f"{FIX}/cli2", "send", f"{USB}/GUI Locked", "--to", addr, "--code", code, "--yes")
    w.wait("//dialog//button[normalize-space()='Accept']", 60)
    w.shot("3-offer")
    os.remove(f"{HOME}/Documents/report.txt")  # so the restore has something to do
    w.click("//dialog//button[normalize-space()='Accept']")
    out["cli_sent"] = "it was checked there" in send.communicate(timeout=120)[0]

    # 4. The received snapshot opens in Restore, locked until the password is given; then it
    #    restores by itself (chosen before receiving), without another click.
    w.wait("//strong[normalize-space()='This snapshot is encrypted']", 60)
    w.type_into("//label[span='Password or recovery key']/input", PW)
    w.click("//button[normalize-space()='Unlock']")
    w.wait("//h2[@id='rr']", 120)
    w.shot("4-restored")
    out["restored"] = open(f"{HOME}/Documents/report.txt").read() == "quarterly report\n"

    # 5. An encrypted update of the first snapshot: only the changed file is copied.
    w.click("//nav//button[normalize-space()='Snapshots']")
    w.click("(//button[normalize-space()='Update…'])[1]")
    w.type_into("//dialog//label[span='Password']/input", PW)
    w.click("//dialog//button[normalize-space()='Continue']")
    w.wait("//h2[@id='s5']")
    with open(f"{HOME}/Documents/notes/todo.md", "a") as f:
        f.write("- changed after the first snapshot\n")
    w.click("//div[contains(@class,'footer-bar')]//button[normalize-space()='Create Snapshot' and not(@disabled)]", 60)
    w.click("//dialog//button[normalize-space()='Create']")
    w.wait("//h1[contains(.,'Snapshot built')]", 300)
    out["update_reused"] = "1 file(s)" in w.text("//p[contains(.,'taken from the earlier snapshot')]")
    w.shot("5-update")

    # 6. Transfer to another computer: pair with a receiving computer (the CLI), which describes
    #    itself; the compatibility step plans for it; the migration is sent and arrives.
    os.makedirs(f"{FIX}/cli1/inbox2", exist_ok=True)
    recv = cli(f"{FIX}/cli1", "receive", "--dest", f"{FIX}/cli1/inbox2", "--yes", "--no-announce")
    code = addr = None
    while not (code and addr):
        line = recv.stdout.readline()
        if m := CODE.match(line):
            code = m.group(1)
        if "Address" in line:
            addr = line.rsplit(" ", 1)[-1].strip()
    w.click("//nav//button[normalize-space()='Transfers']")
    w.click(START.format("Transfer to Another Computer"))
    w.wait("//h2[@id='s1']")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s2']")
    documents_on = "return [...document.querySelectorAll('label')].find((l) => l.textContent.trim() === 'Documents')?.querySelector('input')?.checked"
    w.wait("//label[normalize-space()='Documents']/input")
    if not w.js(documents_on):
        w.click("//label[normalize-space()='Documents']/input")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s3']")
    w.type_into("//label[span='Address']/input", addr)
    w.type_into("//label[span='Pairing code shown on the other computer']/input", code)
    w.click("//button[normalize-space()='Connect' and not(@disabled)]")
    w.wait("//h3[@id='peer-sys']", 60)
    out["probed"] = "Connected" in w.text("//h3[@id='peer-sys']")
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s4'][starts-with(normalize-space(),'What happens on')]", 60)
    w.click("//button[starts-with(normalize-space(),'Next')]")
    w.wait("//h2[@id='s5']")
    w.click("//div[contains(@class,'footer-bar')]//button[normalize-space()='Start Migration' and not(@disabled)]", 60)
    w.click("//dialog//button[normalize-space()='Start']")
    w.wait("//h1[starts-with(normalize-space(),'Sent to')]", 300)
    w.shot("6-transferred")
    log = recv.communicate(timeout=60)[0]
    out["transfer_arrived"] = "looked up this computer" in log and "Received and checked" in log
    w.click("//button[normalize-space()='View transfers']")
    w.wait("//td[contains(.,'Sent')] | //span[normalize-space()='Sent']", 30)
    out["history"] = len(w.els("//span[normalize-space()='Sent']")) >= 2
finally:
    w.close()
print(json.dumps(out, indent=1))
sys.exit(0 if all(v for k, v in out.items() if k != "found_by_mdns") and len(out) == 9 else 1)
