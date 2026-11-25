#!/usr/bin/env bash
# Build the production GNU/Linux executable and its release notices in scratch.
set -euo pipefail
if (( $# != 1 )); then
  echo "Usage: $0 EXISTING_SCRATCH_DIRECTORY" >&2
  exit 2
fi
RUNNER_TEMP=$(realpath -- "$1")
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
bundle='soulsteads-agent-workboard-linux-x86_64'
mkdir "$RUNNER_TEMP/$bundle"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$RUNNER_TEMP/release-target}"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=debuginfo
# Ubuntu 24.04 GNU/Linux build; do not include dev/test executables.
cargo +1.100.0 build --release --locked --target x86_64-unknown-linux-gnu --bin soulsteads-workboard
install -m 0755 "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/soulsteads-workboard" "$RUNNER_TEMP/$bundle/soulsteads-workboard"
install -m 0644 LICENSE NOTICE "$RUNNER_TEMP/$bundle/"
"$RUNNER_TEMP/$bundle/soulsteads-workboard" --help > /dev/null
cargo +1.100.0 metadata --format-version 1 --locked --filter-platform x86_64-unknown-linux-gnu > "$RUNNER_TEMP/cargo-metadata.json"
python3 - "$RUNNER_TEMP/cargo-metadata.json" "$RUNNER_TEMP/$bundle" <<'PY'
import json
from pathlib import Path
import re
import shutil
import sys

metadata = json.loads(Path(sys.argv[1]).read_text())
bundle = Path(sys.argv[2])
notices = bundle / 'dependency-notices'
notices.mkdir()
resolve = metadata['resolve']
nodes = {node['id']: node for node in resolve['nodes']}
packages = {pkg['id']: pkg for pkg in metadata['packages']}
root = resolve['root']
selected = set()
pending = [root]
while pending:
    package = pending.pop()
    if package in selected:
        continue
    selected.add(package)
    pending.extend(dep['pkg'] for dep in nodes[package]['deps']
                   if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']))

rows = []
legal = re.compile(r'(license|licence|copying|copyright|notice|patents|unlicense)(?:$|[._-])', re.I)
for package in sorted(selected - {root}):
    pkg = packages[package]
    source = Path(pkg['manifest_path']).parent.resolve()
    target = notices / (pkg['name'] + '-' + pkg['version'])
    files = {path for path in source.rglob('*')
             if path.is_file() and legal.match(path.name) and path.suffix != '.rs'}
    if pkg['license_file']:
        files.add(source / pkg['license_file'])
    if not pkg['license'] and not files:
        raise SystemExit(f'No declared license or notice files for {target.name}')
    copied = []
    for path in sorted(files):
        relative = path.relative_to(source)
        if '..' in relative.parts or not path.resolve().is_relative_to(source):
            raise SystemExit(f'License file outside crate: {target.name}')
        dest = target / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, dest)
        copied.append(relative.as_posix())
    rows.append(f"{target.name}\t{pkg['license'] or 'See packaged license file'}\t{', '.join(copied)}\n")
(notices / 'INDEX.tsv').write_text('Crate\tDeclared license\tFiles supplied by the crate\n' + ''.join(rows))
PY
