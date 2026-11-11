# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp>=1.26,<2", "jsonschema>=4.26,<5"]
# ///
"""Run: uv run scripts/validate_mcp.py target/debug/workboard.

Uses the official MCP Python SDK, JSON Schema validator and the actual CLI.
All mutations occur in a temporary database, removed after validation.
"""
import asyncio
import json
from pathlib import Path
import subprocess
import sys
import tempfile

from jsonschema import Draft202012Validator
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client


async def validate(binary: Path, database: Path):
    def cli(command):
        result = subprocess.run(
            [str(binary), "--db", str(database), "--actor", "worker", "run"],
            input=json.dumps(command), text=True, capture_output=True, check=True,
        )
        assert not result.stderr, result.stderr
        return json.loads(result.stdout)["result"]

    cli({"op": "agent_register", "id": "worker", "name": "SDK worker"})
    cli({"op": "project_create", "key": "SDK", "name": "SDK validation"})
    credential = cli({"op": "api_key_create", "agent": "worker", "label": "Disposable SDK key"})
    parameters = StdioServerParameters(
        command=str(binary), args=["--db", str(database), "mcp"],
        env={"WORKBOARD_API_KEY": credential["api_key"]},
    )
    async with stdio_client(parameters) as (read, write):
        async with ClientSession(read, write) as session:
            initialized = await session.initialize()
            assert initialized.serverInfo.name == "soulsteads-agent-workboard"
            tools = await session.list_tools()
            assert len(tools.tools) == 1
            schema = tools.tools[0].inputSchema
            Draft202012Validator.check_schema(schema)
            validator = Draft202012Validator(schema)

            async def call(op, **kwargs):
                command = {"op": op, **kwargs}
                validator.validate(command)
                response = await session.call_tool("workboard_execute", command)
                assert not response.isError, response.content
                value = json.loads(response.content[0].text)
                assert response.structuredContent["result"] == value
                return value

            epic = await call("issue_create", project="SDK", kind="epic", title="Ship agents")
            story = await call("issue_create", project="SDK", kind="story", title="SDK work loop", parent=epic["key"], acceptance_criteria=["Official SDK round trip passes"])
            bug = await call("issue_create", project="SDK", kind="bug", title="Dependency first", assignee="worker", parent=epic["key"])
            await call("link_add", source=bug["key"], target=story["key"], kind="blocks")
            await call("issue_transition", key=story["key"], expected_revision=1, state="ready")
            assert (await call("issue_claim", project="SDK"))["claimed"] is None
            for state in ["ready", "in_progress", "done"]:
                bug = await call("issue_transition", key=bug["key"], expected_revision=bug["revision"], state=state)
            claim = (await call("issue_claim", project="SDK", lease_seconds=60))["claimed"]
            assert claim["key"] == story["key"]
            token = claim["lease"]["token"]
            story = await call("issue_heartbeat", key=story["key"], lease_token=token, lease_seconds=900)
            story = await call("issue_update", key=story["key"], expected_revision=story["revision"], lease_token=token, labels=["validated"], metadata={"client": "official MCP Python SDK"})
            await call("comment_add", key=story["key"], body="Validated through official MCP SDK; schema, dependency, lease, revision and persistence checks passed.")
            for state in ["in_review", "done"]:
                story = await call("issue_transition", key=story["key"], expected_revision=story["revision"], lease_token=token, state=state)
            assert story["lease"] is None
            assert (await call("epic_progress", key=epic["key"]))["open"] == 0
            for state in ["ready", "in_progress", "done"]:
                epic = await call("issue_transition", key=epic["key"], expected_revision=epic["revision"], state=state)
            for uri in ["workboard://workflow", "workboard://projects", "workboard://agents", f"workboard://issues/{story['key']}", "workboard://projects/SDK/board"]:
                resource = await session.read_resource(uri)
                assert json.loads(resource.contents[0].text)
            assert len((await session.list_resources()).resources) == 3
            assert len((await session.list_resource_templates()).resourceTemplates) == 2
            assert (await session.list_prompts()).prompts[0].name == "work-issue"
            prompt = await session.get_prompt("work-issue", arguments={"project": "SDK"})
            assert "SDK" in prompt.messages[0].content.text
            error = await session.call_tool("workboard_execute", {"op": "issue_get", "key": "SDK-404"})
            assert error.isError
            assert error.structuredContent["error"]["code"] == "not_found"
            invalid = await session.call_tool("workboard_execute", {"op": "issue_claim", "project": "SDK", "typo": True})
            assert invalid.isError
            # Key-bound sessions cannot administer keys, and revocation is checked
            # again even for a server that is already connected.
            await session.send_ping()
            cli({"op": "api_key_revoke", "id": credential["key"]["id"]})
            try:
                await session.list_resources()
            except Exception as error:
                assert "revoked" in str(error), str(error)
            else:
                raise AssertionError("revoked key still had access")
    board = cli({"op": "board", "project": "SDK"})
    assert len(board["columns"]["done"]) == 3
    assert cli({"op": "comment_list", "key": story["key"]})[0]["actor"] == "worker"
    print("PASS: key-bound official MCP SDK handshake and live revocation, command schema, tools, resources, prompts, errors, full issue work loop, and shared CLI persistence")


if __name__ == "__main__":
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/workboard").resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="workboard-sdk-") as directory:
        asyncio.run(validate(binary, Path(directory) / "board.sqlite3"))
