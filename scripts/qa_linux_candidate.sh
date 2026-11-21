#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() {
  printf 'qa-linux-candidate: %s\n' "$*" >&2
  exit 1
}

[[ $# -ge 1 && $# -le 2 ]] || fail 'usage: qa_linux_candidate.sh LOCAL_PRIVATE_ARCHIVE.tar.gz [NEW_EVIDENCE_FILE_IN_PRIVATE_DIR]'
[[ -f $1 && ! -L $1 ]] || fail 'QA accepts only a regular local archive, not a symlink'
archive=$(realpath -e -- "$1") || fail "archive not found: $1"
archive_dir=$(dirname -- "$archive")
archive_name=$(basename -- "$archive")
evidence_arg=${2:-"$archive.qa.txt"}
evidence_name=$(basename -- "$evidence_arg")
evidence_parent=$(realpath -e -- "$(dirname -- "$evidence_arg")") || fail 'evidence parent must already exist'
[[ $evidence_parent == "$archive_dir" && $evidence_name != . && $evidence_name != .. ]] || \
  fail 'QA evidence must be a new file inside the private archive directory'
evidence="$archive_dir/$evidence_name"
evidence_created=0

python3 - "$archive" "$archive_dir" "$archive_name" <<'PY' || fail 'archive is not a trusted local Linux candidate'
import hashlib
import os
import re
import stat
import sys
import tarfile

archive, archive_dir, archive_name = sys.argv[1:]
if not re.fullmatch(r'workboard-x86_64-unknown-linux-gnu-[0-9a-f]{12}-[0-9a-f]{12}\.tar\.gz', archive_name):
    raise SystemExit('unexpected candidate archive name')
directory_stat = os.stat(archive_dir)
archive_stat = os.lstat(archive)
if directory_stat.st_uid != os.getuid() or directory_stat.st_mode & 0o077 or directory_stat.st_mode & 0o700 != 0o700:
    raise SystemExit('candidate directory must be private and owned by the current user')
if archive_stat.st_uid != os.getuid() or not stat.S_ISREG(archive_stat.st_mode):
    raise SystemExit('candidate archive must be a regular file owned by the current user')

digest = hashlib.sha256()
with open(archive, 'rb') as stream:
    for chunk in iter(lambda: stream.read(1024 * 1024), b''):
        digest.update(chunk)
sidecar = archive + '.sha256'
if os.path.islink(sidecar) or not os.path.isfile(sidecar):
    raise SystemExit('candidate checksum sidecar must be a regular local file')
with open(sidecar, 'rb') as stream:
    sidecar_bytes = stream.read()
expected_sidecar = f'{digest.hexdigest()}  {archive_name}\n'.encode('ascii')
if sidecar_bytes != expected_sidecar:
    raise SystemExit('checksum sidecar must contain exactly this archive\'s SHA-256 record')

package_name = archive_name[:-7]
expected_files = {'workboard', 'README.md', 'LICENSE', 'BUILD-INFO.txt'}
expected_members = {package_name + '/'} | {package_name + '/' + name for name in expected_files}
with tarfile.open(archive, 'r:gz') as bundle:
    members = bundle.getmembers()
    normalized = []
    for member in members:
        name = member.name
        if member.isdir() and name.rstrip('/') == package_name:
            normalized.append(package_name + '/')
        else:
            normalized.append(name)
    if len(members) != len(expected_members) or set(normalized) != expected_members:
        raise SystemExit('candidate archive has an unexpected member list')
    for member, normalized_name in zip(members, normalized):
        if normalized_name.endswith('/'):
            if not member.isdir():
                raise SystemExit('candidate package root is not a directory')
        elif not member.isfile():
            raise SystemExit('candidate package contains a non-regular file')
    info_member = next(member for member in members if member.name.rstrip('/') == package_name + '/BUILD-INFO.txt')
    info = bundle.extractfile(info_member)
    if info is None:
        raise SystemExit('candidate provenance is missing')
    fields = {}
    for line in info.read().decode('utf-8').splitlines():
        if '=' not in line:
            raise SystemExit('candidate provenance is malformed')
        key, value = line.split('=', 1)
        if key in fields:
            raise SystemExit('candidate provenance contains duplicate fields')
        fields[key] = value

revision = fields.get('source_revision', '')
tree_digest = fields.get('source_tree_sha256', '')
if fields.get('format') != 'workboard-linux-candidate-v1':
    raise SystemExit('candidate provenance format is unexpected')
if not re.fullmatch(r'(?:[0-9a-f]{40}|[0-9a-f]{64})', revision):
    raise SystemExit('candidate source revision is malformed')
if not re.fullmatch(r'[0-9a-f]{64}', tree_digest):
    raise SystemExit('candidate source tree digest is malformed')
if fields.get('source_tree_state') not in {'clean', 'dirty'}:
    raise SystemExit('candidate source tree state is malformed')
if fields.get('target') != 'x86_64-unknown-linux-gnu':
    raise SystemExit('candidate target is unexpected')
expected_name = f'workboard-{fields["target"]}-{revision[:12]}-{tree_digest[:12]}'
if package_name != expected_name:
    raise SystemExit('archive name does not match embedded candidate provenance')
PY

sandbox=$(mktemp -d "${TMPDIR:-/tmp}/workboard-package-qa.XXXXXX")
server_pid=
current_step='archive extraction'
finish() {
  result=$?
  if [[ -n ${server_pid:-} ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ $result -ne 0 && $evidence_created -eq 1 ]]; then
    printf 'failure_step=%s\nresult=FAIL exit=%s\n' "$current_step" "$result" >> "$evidence"
  fi
  rm -rf -- "$sandbox"
}
trap finish EXIT

[[ ! -e $evidence && ! -L $evidence ]] || fail 'QA evidence path already exists; refusing to overwrite it'
if ! (set -o noclobber; : > "$evidence"); then
  fail 'cannot create QA evidence without overwriting an existing file'
fi
evidence_created=1
chmod 0600 "$evidence"
record() { printf '%s\n' "$1" | tee -a "$evidence"; }
record "archive=$archive_name"
record 'checksum=PASS exact sidecar record and local provenance verified'

mkdir -m 0700 "$sandbox/home" "$sandbox/unpacked" "$sandbox/bin"
tar -xzf "$archive" -C "$sandbox/unpacked"
package_dirs=("$sandbox/unpacked"/*)
[[ ${#package_dirs[@]} -eq 1 && -d ${package_dirs[0]} ]] || fail 'archive must contain exactly one package directory'
package_dir=${package_dirs[0]}
binary="$package_dir/workboard"
[[ -x $binary && -f "$package_dir/README.md" && -f "$package_dir/LICENSE" && -f "$package_dir/BUILD-INFO.txt" ]] || fail 'archive is missing a required package file'
register_payload='{"op":"agent_register","id":"local","name":"Local user"}'
project_payload='{"op":"project_create","key":"DEMO","name":"Disposable demo"}'
story_payload='{"op":"issue_create","project":"DEMO","kind":"story","title":"Try the first run"}'
ready_payload='{"op":"issue_transition","key":"DEMO-1","expected_revision":1,"state":"ready"}'
readback_payload='{"op":"issue_get","key":"DEMO-1"}'
credential_payload='{"op":"api_key_create","agent":"local","label":"Local dashboard"}'
for quickstart_guard in 'set -eu' 'umask 077' 'mkdir -m 700 -- workboard-first-run' 'cd -- ./workboard-first-run'; do
  grep -Fq -- "$quickstart_guard" "$package_dir/README.md" || fail 'quickstart must create and enter a fresh private directory with fail-fast shell behavior'
done
for payload in "$register_payload" "$project_payload" "$story_payload" "$ready_payload" "$readback_payload" "$credential_payload"; do
  grep -Fq -- "$payload" "$package_dir/README.md" || fail 'smoke operation is missing from the packaged quickstart'
done
grep -Fq 'export WORKBOARD_DB="$PWD/workboard.sqlite3"' "$package_dir/README.md" || fail 'documented disposable database path is missing'
grep -Fq 'unset WORKBOARD_API_KEY' "$package_dir/README.md" || fail 'quickstart must not require an existing Workboard credential'
ln -s "$binary" "$sandbox/bin/workboard"
curl_path=$(command -v curl) || fail 'curl is required for the loopback dashboard smoke test'
ln -s "$curl_path" "$sandbox/bin/curl"
smoke_path="$sandbox/bin"
home="$sandbox/home"
db="$sandbox/workboard.sqlite3"

if PATH="$smoke_path" command -v cargo >/dev/null 2>&1; then
  fail 'cargo unexpectedly visible in the extracted-package PATH'
fi
record 'runtime_path=extracted workboard and curl only; cargo unavailable'
record 'environment=env -i with a fresh HOME; no inherited AI or database credentials'

run_workboard() {
  local label=$1
  local payload=$2
  local output=$3
  current_step=$label
  if ! env -i HOME="$home" PATH="$smoke_path" WORKBOARD_DB="$db" WORKBOARD_ACTOR=local \
    "$binary" run "$payload" > "$output"; then
    if [[ $label != 'local Workboard credential creation' ]]; then
      printf '%s failed with output: ' "$label" >&2
      tr -d '\n' < "$output" >&2 || true
      printf '\n' >&2
    fi
    fail "$label failed"
  fi
  chmod 0600 "$output"
}

[[ ! -e $db ]] || fail 'smoke database unexpectedly existed before first run'
run_workboard 'agent registration' "$register_payload" "$sandbox/agent.json"
run_workboard 'project creation' "$project_payload" "$sandbox/project.json"
run_workboard 'story creation' "$story_payload" "$sandbox/story.json"
run_workboard 'ready transition' "$ready_payload" "$sandbox/ready.json"
run_workboard 'ready story readback' "$readback_payload" "$sandbox/issue.json"
python3 - "$sandbox/issue.json" <<'PY' || fail 'issue readback does not contain the expected ready story'
import json
import sys

with open(sys.argv[1], encoding='utf-8') as stream:
    response = json.load(stream)
story = response['result']['issue']
if response['ok'] is not True or (story['key'], story['kind'], story['state']) != ('DEMO-1', 'story', 'ready'):
    raise SystemExit('DEMO-1 must be a ready story')
PY
[[ -f $db ]] || fail 'first run did not create the disposable SQLite database'
record 'first_run=PASS one local agent, one project, one ready story in a fresh SQLite database'

run_workboard 'local Workboard credential creation' "$credential_payload" "$sandbox/api-key.json"
grep -Eq '"api_key"[[:space:]]*:' "$sandbox/api-key.json" || fail 'Workboard did not return its locally created API credential'
rm -f -- "$sandbox/api-key.json"
record 'workboard_credential=PASS created locally; secret output discarded without logging'

web_log="$sandbox/web.log"
current_step='loopback dashboard startup'
env -i HOME="$home" PATH="$smoke_path" WORKBOARD_DB="$db" \
  "$binary" web --listen 127.0.0.1:0 > "$sandbox/web.stdout" 2> "$web_log" &
server_pid=$!
for _ in $(seq 1 50); do
  if grep -Fq 'http://127.0.0.1:' "$web_log"; then
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    fail 'loopback dashboard exited before binding'
  fi
  sleep 0.1
done
endpoint=$(grep -oE 'http://127\.0\.0\.1:[0-9]+' "$web_log" | head -n 1 || true)
[[ $endpoint =~ ^http://127\.0\.0\.1:([0-9]+)$ ]] || fail 'dashboard did not report a loopback-only address'
port=${BASH_REMATCH[1]}
env -i HOME="$home" PATH="$smoke_path" "$sandbox/bin/curl" --noproxy '*' -fsS --max-time 3 \
  "http://127.0.0.1:$port/" >/dev/null || fail 'loopback dashboard HTTP smoke test failed'
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=
record 'dashboard=PASS bound to 127.0.0.1 only and served the local page'
record 'result=PASS extracted Linux candidate completed the documented no-model first run'
