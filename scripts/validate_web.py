# /// script
# requires-python = ">=3.11"
# dependencies = ["playwright>=1.58,<2"]
# ///
"""Browser integration test of the installed web dashboard and authenticated API.

uv run --locked scripts/validate_web.py target/debug/workboard
Use --install-browser to install the Chromium version in the locked test environment
(or set WORKBOARD_CHROMIUM to an existing Chromium executable).
All agents, API keys and work are disposable fixtures in a temporary database.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import sys
import urllib.error
import urllib.request

from playwright.sync_api import sync_playwright, expect


def validate(binary, screenshots, chromium):
    with tempfile.TemporaryDirectory(prefix="workboard-web-") as temp:
        database = Path(temp) / "web.sqlite3"

        def cli(command, actor="coordinator"):
            response = subprocess.run([str(binary), "--db", str(database), "--actor", actor, "run"],
                                      input=json.dumps(command), capture_output=True, text=True, check=True)
            return json.loads(response.stdout)["result"]

        def create(kind, title, **extra):
            return cli({"op": "issue_create", "project": "AG", "kind": kind, "title": title, **extra})

        def transition(issue, state):
            return cli({"op": "issue_transition", "key": issue["key"], "expected_revision": issue["revision"], "state": state})

        cli({"op": "agent_register", "id": "coordinator", "name": "Coordinator", "capabilities": ["planning", "triage"]})
        for actor, name, capabilities in [("forge", "Forge", ["rust", "infrastructure"]), ("atlas", "Atlas", ["research", "planning"]), ("sentinel", "Sentinel", ["testing", "validation"])]:
            cli({"op": "agent_register", "id": actor, "name": name, "capabilities": capabilities})
        cli({"op": "project_create", "key": "AG", "name": "Agent platform", "description": "Building the infrastructure for independent agents."})
        cli({"op": "project_create", "key": "LAB", "name": "Research lab"})
        key = cli({"op": "api_key_create", "agent": "coordinator", "label": "Disposable browser validation"})
        forge_key = cli({"op": "api_key_create", "agent": "forge", "label": "Disposable HTTP validation"})
        epic = create("epic", "A shared workspace for autonomous agents", priority="high", labels=["platform"])
        story = create("story", "Coordinate work with atomic issue claims", parent=epic["key"], priority="high", labels=["coordination", "rust"], description="Agents need a reliable way to pick up work without duplicate execution.", acceptance_criteria=["Only one agent can claim an issue", "Expired claims can be recovered"], metadata={"repository": "soulsteads-agent-workboard"})
        story = transition(story, "ready")
        claim = cli({"op": "issue_claim", "project": "AG", "key": story["key"]}, "forge")["claimed"]
        cli({"op": "comment_add", "key": story["key"], "body": "Atomic selection is implemented. Validating the cross-process race test now."}, "forge")
        bug = create("bug", "Recover work after a worker disconnects", parent=epic["key"], priority="critical", assignee="sentinel", labels=["reliability"])
        bug = transition(bug, "ready")
        bug = transition(bug, "blocked")
        ready = create("task", "Document the agent handoff protocol", assignee="atlas", labels=["docs"])
        ready = transition(ready, "ready")
        review = create("story", "Capture validation evidence on issues", assignee="sentinel", labels=["observability"])
        for state in ["ready", "in_progress", "in_review"]:
            review = transition(review, state)
        done = create("task", "Persist board state across restarts", assignee="forge", labels=["storage"])
        for state in ["ready", "in_progress", "done"]:
            done = transition(done, state)
        create("spike", "Explore agent capability-based routing", assignee="atlas", labels=["research"])
        injection = create("bug", "<img src=x onerror=alert('xss')>", description="<script>alert('unsafe')</script>")
        cli({"op": "link_add", "source": ready["key"], "target": bug["key"], "kind": "blocks"})
        process = subprocess.Popen([str(binary), "--db", str(database), "web", "--listen", "127.0.0.1:0"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        lines = queue.Queue()
        def read_lines():
            for line in process.stderr:
                lines.put(line)
        threading.Thread(target=read_lines, daemon=True).start()
        try:
            line = lines.get(timeout=15)
            assert "http://" in line, line
            url = line.split("http://", 1)[1].split()[0]
            origin = "http://" + url
            request = urllib.request.Request(origin + "/api/commands",
                data=json.dumps({"op": "issue_heartbeat", "key": story["key"], "lease_token": claim["lease"]["token"]}).encode(),
                headers={"Authorization": "Bearer " + forge_key["api_key"], "Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=10) as response:
                assert json.load(response)["assignee"] == "forge"
            with sync_playwright() as p:
                browser = p.chromium.launch(**({"executable_path": chromium} if chromium else {}))
                context = browser.new_context(viewport={"width": 1440, "height": 1000})
                page = context.new_page()
                errors = []
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.goto(origin)
                expect(page.get_by_role("heading", name="Your agents, in view.")).to_be_visible()
                page.get_by_label("API key", exact=True).fill("invalid")
                page.get_by_role("button", name="Connect to workboard").click()
                expect(page.locator("#auth-error")).to_contain_text("invalid")
                page.get_by_label("API key", exact=True).fill(key["api_key"])
                page.get_by_role("button", name="Connect to workboard").click()
                expect(page.locator("#auth-dialog")).not_to_be_visible()
                expect(page.locator(".issue-card")).to_have_count(8)
                expect(page.locator("#connection")).to_have_text("Live")
                if screenshots:
                    Path(screenshots).mkdir(parents=True, exist_ok=True)
                    page.screenshot(path=str(Path(screenshots) / "board-desktop.png"), full_page=True)
                page.get_by_role("searchbox", name="Search issues").fill("atomic")
                expect(page.locator(".issue-card")).to_have_count(1)
                page.get_by_role("button", name=f"{story['key']}: {story['title']}", exact=True).click()
                expect(page.locator("#detail-title")).to_have_text(story["title"])
                expect(page.locator("#detail-content")).to_contain_text("Atomic selection is implemented")
                expect(page.locator("#detail-content")).to_contain_text("Only one agent can claim an issue")
                assert claim["lease"]["token"] not in page.content()
                if screenshots:
                    page.screenshot(path=str(Path(screenshots) / "issue-detail.png"), full_page=True)
                page.get_by_role("button", name="Close issue details").click()
                page.get_by_role("button", name="Clear filters").click()
                page.get_by_label("Filter by agent").select_option("atlas")
                expect(page.locator(".issue-card")).to_have_count(2)
                page.get_by_role("button", name="Clear filters").click()
                page.get_by_label("Filter by issue type").select_option("spike")
                expect(page.locator(".issue-card")).to_have_count(1)
                page.get_by_role("button", name="Clear filters").click()
                page.get_by_role("button", name="Agents", exact=False).filter(has=page.locator(".nav-count")).click()
                expect(page.locator(".agent-card")).to_have_count(4)
                if screenshots:
                    page.screenshot(path=str(Path(screenshots) / "agents-desktop.png"), full_page=True)
                page.get_by_role("button", name="Activity", exact=False).click()
                expect(page.locator(".activity-list")).to_contain_text("claimed work")
                page.get_by_role("button", name="Issue board", exact=False).click()
                # A real CLI mutation must become visible without pressing Refresh.
                live = create("task", "Live refresh integration evidence", labels=["browser-test"])
                expect(page.get_by_role("button", name=f"{live['key']}: {live['title']}", exact=True)).to_be_visible(timeout=10000)
                page.get_by_role("searchbox", name="Search issues").fill("nothing-matches-10487")
                expect(page.get_by_role("heading", name="No matching issues")).to_be_visible()
                page.get_by_role("button", name="Clear filters").click()
                page.get_by_label("Project", exact=True).select_option("LAB")
                expect(page.get_by_role("heading", name="A little quiet. Ready for what’s next.")).to_be_visible()
                page.get_by_label("Project", exact=True).select_option("AG")
                expect(page.locator(".issue-card")).to_have_count(9)
                # Rendering hostile issue text must not interpret markup or script.
                assert page.locator("img[src=x]").count() == 0
                page.get_by_role("searchbox", name="Search issues").fill("<img")
                page.locator(".issue-card").click()
                expect(page.locator("#detail-title")).to_have_text(injection["title"])
                assert page.locator("#detail-content script").count() == 0
                page.keyboard.press("Escape")
                page.get_by_role("button", name="Clear filters").click()
                page.set_viewport_size({"width": 390, "height": 844})
                expect(page.locator(".sidebar")).to_be_visible()
                if screenshots:
                    page.screenshot(path=str(Path(screenshots) / "board-mobile.png"), full_page=True)
                # CSS should constrain horizontal scrolling to the board, not page.
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "mobile page overflows"
                page.get_by_role("searchbox", name="Search issues").fill("atomic")
                page.locator(".issue-card").click()
                expect(page.locator("#detail-title")).to_have_text(story["title"])
                assert page.locator("#issue-dialog").bounding_box()["width"] <= 390
                page.keyboard.press("Escape")
                page.get_by_role("button", name="Sign out").click()
                expect(page.locator("#auth-dialog")).to_be_visible()
                assert "Atomic selection is implemented" not in page.content()
                page.get_by_label("API key", exact=True).fill(key["api_key"])
                page.get_by_role("button", name="Connect to workboard").click()
                expect(page.locator("#auth-dialog")).not_to_be_visible()
                cli({"op": "api_key_revoke", "id": key["key"]["id"]})
                expect(page.locator("#auth-dialog")).to_be_visible(timeout=10000)
                expect(page.locator("#auth-error")).to_contain_text("revoked")
                assert not errors, errors
                browser.close()
            print("PASS: real Chromium login, key rejection/revocation, project switching, filters, issue detail, agents, activity, live CLI updates, XSS-safe text, mobile layout and sign-out")
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/debug/workboard")
    parser.add_argument("--install-browser", action="store_true", help="Install matching Chromium before validation")
    parser.add_argument("--with-deps", action="store_true", help="Install Chromium system dependencies too (Linux/CI)")
    parser.add_argument("--screenshots", help="Optional directory for desktop/mobile QA screenshots")
    parser.add_argument("--chromium", default=os.environ.get("WORKBOARD_CHROMIUM"), help="Use an already installed Chromium executable")
    args = parser.parse_args()
    if args.install_browser:
        subprocess.run([sys.executable, "-m", "playwright", "install", "chromium"] + (["--with-deps"] if args.with_deps else []), check=True)
    validate(Path(args.binary).resolve(strict=True), args.screenshots, args.chromium)
