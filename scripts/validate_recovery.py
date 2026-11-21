"""Real-process race, lease recovery, retry and stopped backup acceptance.

python scripts/validate_recovery.py BINARY OWNED_EVIDENCE_DIRECTORY
The directory must be new; artifacts are intentionally retained for inspection.
"""
import concurrent.futures
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

binary = Path(sys.argv[1]).resolve(strict=True)
root = Path(sys.argv[2]).resolve()
root.mkdir(parents=True, exist_ok=False)
database = root / "board.sqlite"
checks = []


def run(actor, command, *, retry=None, db=database, success=True):
    args = [str(binary), "--db", str(db), "--actor", actor, "run"]
    if retry:
        args += ["--idempotency-key", retry]
    response = subprocess.run(args, input=json.dumps(command), text=True,
                              capture_output=True, timeout=15)
    value = json.loads(response.stdout)
    assert (response.returncode == 0) == success, value
    return value


for actor in ["operator", "a", "b", "c", "d"]:
    run(actor, {"op": "agent_register", "id": actor, "name": actor})
run("operator", {"op": "project_create", "key": "RC", "name": "Recovery"})
create = {"op": "issue_create", "project": "RC", "kind": "task", "title": "One owner"}
first = run("operator", create, retry="create-once")
replayed = run("operator", create, retry="create-once")
assert replayed["replayed"] and first["result"] == replayed["result"]
checks.append("idempotent retry across separate CLI processes creates one issue")
run("operator", {"op": "issue_transition", "key": "RC-1", "expected_revision": 1, "state": "ready"})
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    claims = list(pool.map(lambda a: (a, run(a, {"op": "issue_claim", "project": "RC", "lease_seconds": 30})["result"]["claimed"]), ["a", "b", "c", "d"]))
winners = [(a, i) for a, i in claims if i]
assert len(winners) == 1, claims
owner, old = winners[0]
checks.append("four concurrent real processes produce exactly one lease owner")
# Shorten only after every competing process has finished; otherwise a slow
# contender may legitimately reclaim a one-second lease during the race itself.
run(owner, {"op": "issue_heartbeat", "key": "RC-1", "lease_token": old["lease"]["token"], "lease_seconds": 1})
time.sleep(2)
new_owner = next(a for a, i in claims if a != owner)
fresh = run(new_owner, {"op": "issue_claim", "project": "RC"})["result"]["claimed"]
assert fresh["lease"]["token"] != old["lease"]["token"]
stale = run(owner, {"op": "issue_update", "key": "RC-1", "expected_revision": fresh["revision"], "lease_token": old["lease"]["token"], "title": "Stale write"}, success=False)
assert stale["error"]["code"] == "conflict" and "lease token" in stale["error"]["message"], stale
checks.append("expired lease reclaimed after process exit; old worker rejected even with fresh revision")
released = run(new_owner, {"op": "issue_release", "key": "RC-1", "lease_token": fresh["lease"]["token"]})["result"]
assert released["state"] == "ready" and released["lease"] is None
claimed = run(owner, {"op": "issue_claim", "project": "RC"})["result"]["claimed"]
checks.append("release and handoff preserve history and allocate a fresh claim")
before = run("operator", {"op": "issue_get", "key": "RC-1"})["result"]
# Every command above has exited. No service has this database open.
wal = database.with_name(database.name + "-wal")
assert not wal.exists() or wal.stat().st_size == 0, "stopped database has an uncheckpointed WAL"
backup = root / "stopped-backup.sqlite"
shutil.copy2(database, backup)
restored = root / "restored.sqlite"
shutil.copy2(backup, restored)
assert run("operator", {"op": "issue_get", "key": "RC-1"}, db=restored)["result"] == before
assert run("operator", {"op": "events", "key": "RC-1"}, db=restored)["result"] == run("operator", {"op": "events", "key": "RC-1"})["result"]
checks.append("stopped backup restores identical issue, lease and event history through a new process")
receipt = {"passed": True, "checks": checks, "stale_error": stale["error"],
           "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}
(root / "result.json").write_text(json.dumps(receipt, indent=2))
print(json.dumps(receipt, indent=2))
