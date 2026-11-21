#!/usr/bin/env bash
set -euo pipefail

fail() {
  printf 'package-linux-candidate: %s\n' "$*" >&2
  exit 1
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/.." && pwd -P)
out_arg=${1:-"$repo_root/../workboard-linux-candidate"}
target=${WORKBOARD_LINUX_TARGET:-x86_64-unknown-linux-gnu}
jobs=${CARGO_BUILD_JOBS:-2}

[[ $(uname -s) == Linux ]] || fail 'build host must be Linux'
[[ $(uname -m) == x86_64 ]] || fail 'build host must be x86_64'
[[ $target == x86_64-unknown-linux-gnu ]] || fail "only $target is supported by this candidate entry"
[[ $jobs =~ ^[1-9][0-9]*$ ]] || fail 'CARGO_BUILD_JOBS must be a positive integer'

for command_name in cargo rustc git cc ld sha256sum tar gzip python3 ldd readelf install mktemp realpath; do
  command -v "$command_name" >/dev/null 2>&1 || fail "missing build prerequisite: $command_name"
done

git -C "$repo_root" rev-parse --is-inside-work-tree >/dev/null 2>&1 || fail 'source must be a Git working tree'
source_revision=$(git -C "$repo_root" rev-parse HEAD)
source_state=clean
if [[ -n $(git -C "$repo_root" status --porcelain --untracked-files=all) ]]; then
  source_state=dirty
fi
source_tag=$(git -C "$repo_root" describe --exact-match --tags HEAD 2>/dev/null || printf 'none')
lock_sha256=$(sha256sum "$repo_root/Cargo.lock" | cut -d ' ' -f 1)
source_tree_sha256=$(python3 - "$repo_root" <<'PY'
import hashlib
import os
import stat
import subprocess
import sys

root = os.path.realpath(sys.argv[1])
h = hashlib.sha256()
tracked_paths = subprocess.run(
    ['git', '-C', root, 'ls-files', '-z'], check=True, stdout=subprocess.PIPE
).stdout.split(b'\0')
untracked_paths = subprocess.run(
    ['git', '-C', root, 'ls-files', '--others', '--exclude-standard', '-z'],
    check=True,
    stdout=subprocess.PIPE,
).stdout.split(b'\0')
untracked_paths = [path for path in untracked_paths if path]
if untracked_paths:
    names = ', '.join(os.fsdecode(path) for path in sorted(untracked_paths))
    raise SystemExit(f'non-ignored untracked files prevent source digest: {names}')

for rel in sorted(path for path in tracked_paths if path):
    path = os.path.join(root, os.fsdecode(rel))
    metadata = os.lstat(path)
    if stat.S_ISLNK(metadata.st_mode):
        mode = b'120000'
        content_hash = hashlib.sha256(os.fsencode(os.readlink(path))).digest()
    elif stat.S_ISREG(metadata.st_mode):
        # Match Git's file type/executable bit, not checkout-dependent permissions.
        mode = b'100755' if metadata.st_mode & stat.S_IXUSR else b'100644'
        file_hash = hashlib.sha256()
        with open(path, 'rb') as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                file_hash.update(chunk)
        content_hash = file_hash.digest()
    else:
        raise SystemExit(f'unsupported tracked source type: {os.fsdecode(rel)}')
    h.update(len(rel).to_bytes(8, 'big'))
    h.update(rel)
    h.update(mode)
    h.update(content_hash)
print(h.hexdigest())
PY
)
source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repo_root" show -s --format=%ct HEAD)}
[[ $source_date_epoch =~ ^[0-9]+$ ]] || fail 'SOURCE_DATE_EPOCH must be an integer Unix timestamp'

# Resolve and validate both paths before creating the output or target tree.
out_dir=$(realpath -m -- "$out_arg") || fail 'cannot resolve candidate output path'
target_arg=${CARGO_TARGET_DIR:-"$(dirname -- "$repo_root")/target"}
target_dir=$(realpath -m -- "$target_arg") || fail 'cannot resolve CARGO_TARGET_DIR'

case "$out_dir/" in
  "$repo_root/"*) fail 'candidate output directory must be outside the source tree' ;;
esac
case "$target_dir/" in
  "$repo_root/"*) fail 'CARGO_TARGET_DIR must be outside the source tree' ;;
esac
[[ $target_dir != / ]] || fail 'CARGO_TARGET_DIR must not be the filesystem root'
[[ ! -e $target_dir || -d $target_dir ]] || fail 'CARGO_TARGET_DIR must be a directory'
[[ ! -e $out_arg && ! -L $out_arg && ! -e $out_dir && ! -L $out_dir ]] || \
  fail 'candidate output path must not exist; choose a fresh private output directory'
[[ -d $(dirname -- "$out_dir") ]] || fail 'candidate output parent must already exist'
if [[ $out_dir == "$target_dir" || $out_dir == "$target_dir/"* || $target_dir == "$out_dir/"* ]]; then
  fail 'candidate output and CARGO_TARGET_DIR must not overlap'
fi

mkdir -m 0700 -- "$out_dir" || fail 'cannot create fresh candidate output directory'
out_dir_after=$(realpath -- "$out_dir") || fail 'cannot resolve created candidate output directory'
[[ $out_dir_after == "$out_dir" ]] || fail 'candidate output path changed during creation'
case "$out_dir_after/" in
  "$repo_root/"*) fail 'created candidate output directory must be outside the source tree' ;;
esac
out_dir=$out_dir_after

mkdir -p -- "$target_dir" || fail 'cannot create CARGO_TARGET_DIR'
target_dir_after=$(realpath -- "$target_dir") || fail 'cannot resolve created CARGO_TARGET_DIR'
[[ $target_dir_after == "$target_dir" ]] || fail 'CARGO_TARGET_DIR changed during creation'
case "$target_dir_after/" in
  "$repo_root/"*) fail 'created CARGO_TARGET_DIR must be outside the source tree' ;;
esac
target_dir=$target_dir_after

export CARGO_BUILD_JOBS="$jobs"
export CARGO_TARGET_DIR="$target_dir"
export SOURCE_DATE_EPOCH="$source_date_epoch"
(
  cd "$repo_root"
  cargo build --locked --release --target "$target"
)
binary="$target_dir/$target/release/workboard"
[[ -x $binary ]] || fail "release binary not found: $binary"

rustc_version=$(rustc --version)
cargo_version=$(cargo --version)
cc_version=$(cc --version)
cc_version=${cc_version%%$'\n'*}
linker_version=$(ld --version)
linker_version=${linker_version%%$'\n'*}
binary_sha256=$(sha256sum "$binary" | cut -d ' ' -f 1)
ldd_output=$(ldd "$binary") || fail 'ldd could not inspect the release binary'
runtime_libraries=$(printf '%s\n' "$ldd_output" | python3 -c '
import os, sys
names = set()
for line in sys.stdin:
    fields = line.strip().split()
    if not fields or fields[0] == "linux-vdso.so.1":
        continue
    if "=>" in fields:
        names.add(fields[0])
    elif fields[0].startswith("/"):
        names.add(os.path.basename(fields[0]))
print(", ".join(sorted(names)))
')
glibc_floor=$(readelf --version-info "$binary" | python3 -c '
import re, sys
versions = [tuple(map(int, value.split("."))) for value in re.findall(r"GLIBC_(\d+(?:\.\d+)+)", sys.stdin.read())]
print(".".join(map(str, max(versions))) if versions else "unknown")
')

short_revision=${source_revision:0:12}
package_name="workboard-${target}-${short_revision}-${source_tree_sha256:0:12}"
archive_name="${package_name}.tar.gz"
stage_root=$(mktemp -d -- "$out_dir/.workboard-candidate.XXXXXX")
tmp_archive=
cleanup() {
  if [[ -n ${tmp_archive:-} ]]; then rm -f -- "$tmp_archive"; fi
  rm -rf -- "$stage_root"
}
trap cleanup EXIT
package_dir="$stage_root/$package_name"
mkdir -p -- "$package_dir"
install -m 0755 "$binary" "$package_dir/workboard"
install -m 0644 "$repo_root/README.md" "$package_dir/README.md"
install -m 0644 "$repo_root/LICENSE" "$package_dir/LICENSE"
cat > "$package_dir/BUILD-INFO.txt" <<INFO
format=workboard-linux-candidate-v1
source_revision=$source_revision
source_tree_state=$source_state
source_tree_sha256=$source_tree_sha256
source_exact_tag=$source_tag
pinned_source_fallback=git checkout $source_revision (apply reviewed source changes if source_tree_state=dirty)
Cargo.lock_sha256=$lock_sha256
target=$target
rustc=$rustc_version
cargo=$cargo_version
cc=$cc_version
linker=$linker_version
build_jobs=$jobs
source_date_epoch=$source_date_epoch
binary_sha256=$binary_sha256
build_prerequisites=Linux x86_64, Bash, Git, Rust/Cargo, C compiler and linker, Python 3, GNU tar/gzip, readelf, ldd, sha256sum, curl, standard coreutils
runtime_libc=glibc >= $glibc_floor
runtime_libraries=$runtime_libraries
sqlite=rusqlite bundled SQLite; no SQLite server required
runtime_services=none
INFO
chmod 0644 "$package_dir/BUILD-INFO.txt"
find "$package_dir" -type f -exec touch -d "@$source_date_epoch" {} +
touch -d "@$source_date_epoch" "$package_dir"

archive_path="$out_dir/$archive_name"
tmp_archive=$(mktemp -- "$out_dir/.${archive_name}.tmp.XXXXXX")
(
  cd "$stage_root"
  tar --sort=name --mtime="@$source_date_epoch" --owner=0 --group=0 --numeric-owner -cf - "$package_name" | gzip -n > "$tmp_archive"
)
[[ ! -e $archive_path && ! -L $archive_path && ! -e $archive_path.sha256 && ! -L $archive_path.sha256 ]] || \
  fail 'candidate archive or checksum already exists in output directory'
mv -nT -- "$tmp_archive" "$archive_path" || fail 'cannot publish candidate archive without replacing an existing file'
[[ ! -e $tmp_archive ]] || fail 'candidate archive already exists in output directory'
tmp_archive=
(
  cd "$out_dir"
  set -o noclobber
  sha256sum "$archive_name" > "$archive_name.sha256"
)

"$script_dir/qa_linux_candidate.sh" "$archive_path" "$out_dir/QA.txt"
printf 'candidate=%s\n' "$archive_path"
printf 'checksum=%s\n' "$out_dir/$archive_name.sha256"
printf 'sha256=%s\n' "$(sha256sum "$archive_path" | cut -d ' ' -f 1)"
printf 'source_revision=%s\nsource_tree_sha256=%s\ntarget=%s\n' "$source_revision" "$source_tree_sha256" "$target"
