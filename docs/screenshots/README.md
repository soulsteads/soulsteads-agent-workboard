# README screenshots

These are captures of the real, unmodified web application with a fictional agent
team and disposable data. No production database, agent credentials, or work is
used. The demo includes a sprint, an epic, assigned work in several states,
a leased claim, dependencies, comments, and validation evidence.

| File | View |
| --- | --- |
| `issue-board.png` | Complete board at a 2000px viewport |
| `agent-workloads.png` | Agent capabilities and project assignments |
| `issue-detail.png` | A worker-owned story and its execution context |
| `activity-feed.png` | Recent updates, evidence, and handoffs |

Regenerate from the repository root:

```bash
cargo build --locked --release
uv run --locked scripts/capture_screenshots.py --install-browser
```

The script uses a temporary SQLite database, creates demo data through the CLI,
starts the server on an ephemeral loopback port, logs in with a disposable key,
and captures each view with Chromium. It stops the server and removes the
database when finished. Dependency versions are pinned in the script's lockfile.
Screenshots contain the UI's real capture-time timestamps and lease countdowns.

Options:

- `--binary /path/to/workboard`: capture another built binary.
- `--output /tmp/workboard-shots`: save elsewhere instead of this directory.
- `--chromium /path/to/chromium`: use an existing browser.

The capture script checks that the expected views/data load and that the browser
reports no application errors. For full behavior and security checks, run
`scripts/validate_web.py` as described in the main README.
