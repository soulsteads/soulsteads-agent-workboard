# Changelog

## 0.3.0 (proposed)

Changes since [`v0.1.0`](https://github.com/soulsteads/soulsteads-agent-workboard/releases/tag/v0.1.0), whose peeled commit is [`f11ca5b`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/f11ca5bce1f9ffdaa767c440a21dd419521d5d16). The prior 0.2.0 entry was a draft, not a published release.

### Highlights

- **Executable:** install and invoke the CLI as `soulsteads-workboard`; crate, SQLite database, `WORKBOARD_*` settings, MCP names, and HTTP API identities are unchanged.
- **systemd:** add `service install --user|--system [--run-as USER] [--dry-run]`. It renders or creates one non-overwriting unit for the foreground loopback server and never opens a database, invokes a service manager, enables, or starts a service.
- **Containers and releases:** add non-root Docker/Podman deployment guidance and release asset builds for published GitHub releases.
- **Review closure:** require submitted evidence and an independent review before new work can complete.
- **SQLite startup:** retry temporary lock contention when enabling WAL so concurrent openers can finish upgrading an older board.

### Included commits

All commits already on the public default branch in this release range, before this version update. Unmerged branches are not included.

- [`9833be9`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/9833be988577718aa96d21cf3be521015361b8b2) Require evidence and independent review for Workboard completion
- [`143704c`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/143704cf433349ae80592200600e80f66307fcfc) chore(release): prepare v0.2.0
- [`cd16201`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/cd162017a9835671efd665b4549cdb07f78259ba) feat: add Docker and Podman deployment
- [`8c2d093`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/8c2d0937f952c6f0537aea7c785a3bb06ec65b3d) docs: require Docker 28 for loopback publishing
- [`8d60296`](https://github.com/soulsteads/soulsteads-agent-workboard/commit/8d60296e4d5c28f1f29eeb47721d073d7259895d) ci: build assets when a GitHub release is published
