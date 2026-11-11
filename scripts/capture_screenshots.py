# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright>=1.58,<2"]
# ///
"""Capture README screenshots from the real app using disposable demo data.

cargo build --locked --release
uv run --locked scripts/capture_screenshots.py --install-browser

No production database or credentials are read. The app is not modified for the
screenshots; all displayed data is created through the public CLI operations.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading

from playwright.sync_api import expect, sync_playwright


def capture(binary: Path, output: Path, chromium: str | None):
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="workboard-marketing-") as directory:
        database = Path(directory) / "demo.sqlite3"
        environment = os.environ.copy()
        for name in ("WORKBOARD_API_KEY", "WORKBOARD_DB", "WORKBOARD_ACTOR"):
            environment.pop(name, None)

        def command(op, actor="coordinator", **fields):
            response = subprocess.run(
                [str(binary), "--db", str(database), "--actor", actor, "run"],
                input=json.dumps({"op": op, **fields}), text=True,
                capture_output=True, check=True, env=environment,
            )
            return json.loads(response.stdout)["result"]

        def issue(kind, title, state="backlog", assignee=None, **fields):
            value = command("issue_create", project="AG", kind=kind, title=title,
                            assignee=assignee, **fields)
            if state != "backlog":
                steps = ["ready"]
                if state in ("in_progress", "in_review", "done"):
                    steps.append("in_progress")
                if state in ("in_review", "done"):
                    steps.append("in_review")
                if state == "done":
                    steps.append("done")
                if state in ("blocked", "canceled"):
                    steps.append(state)
                for target in steps:
                    value = command("issue_transition", key=value["key"],
                                    expected_revision=value["revision"], state=target)
            return value

        agents = [
            ("coordinator", "Coordinator", ["planning", "triage"]),
            ("forge", "Forge", ["rust", "infrastructure"]),
            ("atlas", "Atlas", ["research", "documentation"]),
            ("sentinel", "Sentinel", ["testing", "validation"]),
        ]
        for agent, name, capabilities in agents:
            command("agent_register", id=agent, name=name, capabilities=capabilities)
        command("project_create", key="AG", name="Agent platform")
        command("project_create", key="LAB", name="Research lab")
        sprint = command("sprint_create", project="AG", name="Sprint 01", goal="Reliable agent coordination")
        command("sprint_transition", id=sprint["id"], state="active")
        credential = command("api_key_create", agent="coordinator", label="Disposable screenshot session")
        epic = issue("epic", "A shared workspace for autonomous agents", priority="high", labels=["platform"])
        active = issue(
            "story", "Coordinate work with atomic issue claims", "ready", "forge",
            parent=epic["key"], sprint=sprint["id"], priority="high", estimate=5,
            labels=["coordination", "rust"],
            description="Let independent agents pick up the next ready issue without duplicate execution.\n\nSelection, assignment and lease issuance happen in one transaction. A worker retains its claim token while it implements and validates the acceptance criteria.",
            acceptance_criteria=[
                "Only one worker can claim a ready issue at a time.",
                "Heartbeats renew ownership before the lease expires.",
                "An interrupted claim can be recovered by another agent.",
            ],
            metadata={"repository": "soulsteads-agent-workboard", "component": "work coordination"},
        )
        command("issue_claim", actor="forge", project="AG", key=active["key"], lease_seconds=900)
        issue("task", "Document the agent handoff protocol", "in_progress", "atlas", sprint=sprint["id"], labels=["docs"])
        issue("bug", "Recover work after a worker disconnects", "blocked", "sentinel", parent=epic["key"], priority="critical", labels=["reliability"])
        ready = issue("task", "Validate the recovery path across processes", "ready", "sentinel", sprint=sprint["id"], labels=["tests"])
        issue("story", "Capture validation evidence on every issue", "in_review", "sentinel", sprint=sprint["id"], labels=["observability"])
        issue("task", "Persist board state across restarts", "done", "forge", sprint=sprint["id"], labels=["storage"])
        issue("spike", "Explore capability-based work routing", assignee="atlas", labels=["research"])
        issue("story", "Make every agent handoff traceable", "ready", "atlas", parent=epic["key"], labels=["coordination"])
        issue("task", "Validate per-agent access and revocation", "in_review", "sentinel", labels=["access", "tests"])
        issue("task", "Connect agent tools through MCP", "done", "forge", labels=["mcp"])
        issue("task", "Package the agent execution skill", "done", "atlas", labels=["skills"])
        command("link_add", source=active["key"], target=ready["key"], kind="blocks")
        command("comment_add", actor="forge", key=active["key"], body="Atomic selection and lease renewal are implemented. The cross-process claim test confirms a single owner; validating interrupted-worker recovery next.")
        command("comment_add", actor="sentinel", key=active["key"], body="Review checklist: claim race, expired-token rejection, heartbeat renewal and a complete audit trail. Evidence will be attached before this moves to done.")

        process = subprocess.Popen(
            [str(binary), "--db", str(database), "web", "--listen", "127.0.0.1:0"],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=environment,
        )
        lines = queue.Queue()

        def read_stderr():
            for line in process.stderr:
                lines.put(line)

        threading.Thread(target=read_stderr, daemon=True).start()
        try:
            line = lines.get(timeout=15)
            if "http://" not in line:
                raise RuntimeError("Demo server did not announce its listener")
            origin = "http://" + line.split("http://", 1)[1].split()[0]
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch(**({"executable_path": chromium} if chromium else {}))
                context = browser.new_context(viewport={"width": 2000, "height": 1060}, device_scale_factor=1)
                page = context.new_page()
                errors = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.goto(origin)
                page.get_by_label("API key", exact=True).fill(credential["api_key"])
                page.get_by_role("button", name="Connect to workboard").click()
                expect(page.locator("#auth-dialog")).not_to_be_visible()
                expect(page.locator(".issue-card")).to_have_count(12)
                expect(page.locator("#connection")).to_have_text("Live")
                page.screenshot(path=str(output / "issue-board.png"), full_page=True)

                page.set_viewport_size({"width": 1440, "height": 800})
                page.get_by_role("button", name="Agents", exact=False).filter(has=page.locator(".nav-count")).click()
                expect(page.locator(".agent-card")).to_have_count(4)
                page.screenshot(path=str(output / "agent-workloads.png"), full_page=True)

                page.set_viewport_size({"width": 1440, "height": 1240})
                page.get_by_role("button", name="Issue board", exact=False).click()
                page.get_by_role("button", name=f"{active['key']}: {active['title']}", exact=True).click()
                expect(page.locator("#detail-title")).to_have_text(active["title"])
                expect(page.locator("#detail-content")).to_contain_text("Only one worker can claim")
                page.screenshot(path=str(output / "issue-detail.png"), full_page=True)
                page.get_by_role("button", name="Close issue details").click()

                page.set_viewport_size({"width": 1440, "height": 960})
                page.get_by_role("button", name="Activity", exact=False).click()
                expect(page.locator(".activity-list")).to_contain_text("added a comment")
                page.screenshot(path=str(output / "activity-feed.png"), full_page=False)
                if errors:
                    raise RuntimeError(f"Browser errors during capture: {errors}")
                browser.close()
            print(f"Captured four real-app screenshots in {output}")
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/workboard")
    parser.add_argument("--output", default="docs/screenshots")
    parser.add_argument("--chromium", default=os.environ.get("WORKBOARD_CHROMIUM"))
    parser.add_argument("--install-browser", action="store_true")
    args = parser.parse_args()
    if args.install_browser:
        subprocess.run([sys.executable, "-m", "playwright", "install", "chromium"], check=True)
    capture(Path(args.binary).resolve(strict=True), Path(args.output).resolve(), args.chromium)
