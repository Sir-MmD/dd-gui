#!/usr/bin/env bash
# End-to-end test of dd-gui on Linux: a smart backup and restore, a sector-by-sector copy
# and a wipe, run for real as root, on loop devices only.
#
#   sudo bash ci/e2e-linux.sh /absolute/path/to/dd-gui      (or the path in $DD_GUI)
#
# It builds a GPT disk with FAT32, ext4, NTFS and exFAT partitions full of random files,
# backs it up with `dd-gui copy --mode=smart`, restores that onto a second disk full of
# garbage, and checks every file system (fsck) and every file (SHA-256). Everything it
# writes to is a loop device it set up itself, over a file in its own temporary folder:
# `ours` checks that before every single write, and it never touches anything else.
#
# Settings: E2E_WORKDIR (where the temporary folder goes; default $RUNNER_TEMP or /tmp,
# needs about 2 GB), E2E_DISK_MB (default 512), E2E_TIME_LIMIT (seconds, default 1500).
# Tools it lacks are installed with apt-get (Ubuntu); elsewhere they must be there.

set -euo pipefail
export LC_ALL=C MTOOLS_SKIP_CHECK=1

DD_GUI=${1:-${DD_GUI:-}}
DISK_MB=${E2E_DISK_MB:-512}
TIME_LIMIT=${E2E_TIME_LIMIT:-1500}
STEP_LIMIT=600

PASSED=0
LOOPS=()
MOUNTS=()
W=
WATCHDOG=

say() { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
pass() {
    PASSED=$((PASSED + 1))
    printf 'PASS  %s\n' "$*"
}
fail() {
    printf 'FAIL  %s\n' "$*" >&2
    exit 1
}

cleanup() {
    local code=$?
    set +e
    if [ -n "$WATCHDOG" ]; then
        pkill -P "$WATCHDOG" 2>/dev/null
        kill "$WATCHDOG" 2>/dev/null
    fi
    for m in "${MOUNTS[@]}"; do
        mountpoint -q "$m" && umount "$m"
    done
    for dev in "${LOOPS[@]}"; do
        losetup -d "$dev" 2>/dev/null
    done
    [ -n "$W" ] && [ -d "$W" ] && rm -rf -- "$W"
    if [ "$code" -eq 0 ]; then
        printf '\nE2E Linux: PASS (%d checks)\n' "$PASSED"
    else
        printf '\nE2E Linux: FAIL (exit code %d, after %d passed checks)\n' "$code" "$PASSED" >&2
    fi
    exit "$code"
}
trap cleanup EXIT
trap 'fail "interrupted (or over the time limit of ${TIME_LIMIT}s)"' INT TERM

# --- Setup --------------------------------------------------------------------------------

[ "$(id -u)" -eq 0 ] || fail "run this as root (sudo): it sets up loop devices"
[ -n "$DD_GUI" ] || fail "usage: $0 /path/to/dd-gui (or set DD_GUI)"
[ -x "$DD_GUI" ] || fail "$DD_GUI isn't an executable file"
DD_GUI=$(readlink -f "$DD_GUI")

( sleep "$TIME_LIMIT" && kill -TERM $$ ) 2>/dev/null &
WATCHDOG=$!

step "Tools"
missing=()
for pair in losetup:util-linux sfdisk:fdisk blockdev:util-linux mkfs.vfat:dosfstools \
    fsck.vfat:dosfstools mcopy:mtools mke2fs:e2fsprogs e2fsck:e2fsprogs debugfs:e2fsprogs \
    mkntfs:ntfs-3g ntfscp:ntfs-3g ntfscat:ntfs-3g ntfsfix:ntfs-3g mkfs.exfat:exfatprogs \
    fsck.exfat:exfatprogs zstd:zstd python3:python3 cmp:diffutils timeout:coreutils; do
    command -v "${pair%%:*}" >/dev/null || missing+=("${pair#*:}")
done
if [ "${#missing[@]}" -gt 0 ]; then
    command -v apt-get >/dev/null || fail "missing tools, from: ${missing[*]}"
    say "installing: ${missing[*]}"
    export DEBIAN_FRONTEND=noninteractive
    timeout 300 apt-get update -qq
    # shellcheck disable=SC2046 # word splitting of the unique package names is wanted
    timeout 600 apt-get install -y -qq --no-install-recommends $(printf '%s\n' "${missing[@]}" | sort -u) >/dev/null
fi
pass "tools are there"

base=${E2E_WORKDIR:-${RUNNER_TEMP:-/tmp}}
mkdir -p "$base"
W=$(mktemp -d "$base/dd-gui-e2e.XXXXXX")
W=$(readlink -f "$W")
say "working in $W"
"$DD_GUI" dd --version >/dev/null 2>&1 || fail "$DD_GUI doesn't run"

# Runs a command with a time limit, so nothing can hang the job.
run() { timeout --kill-after=10 "$STEP_LIMIT" "$@"; }

# Every write goes through here first: DEV must be a loop device that we set up over
# FILE (inside our folder), and have FILE's size. PART checks a partition of it.
ours() {
    local dev=$1 file=$2 back size
    [[ $dev =~ ^/dev/loop[0-9]+$ ]] || fail "refusing to write to $dev: not a loop device"
    [ -b "$dev" ] || fail "refusing to write to $dev: not a block device"
    case " ${LOOPS[*]} " in *" $dev "*) ;; *) fail "refusing to write to $dev: not set up by this script" ;; esac
    case "$(readlink -f "$file")" in "$W"/*) ;; *) fail "refusing to write to $dev: $file is outside $W" ;; esac
    back=$(losetup --noheadings --output BACK-FILE "$dev" | sed 's/[[:space:]]*$//')
    [ "$(readlink -f "$back")" = "$(readlink -f "$file")" ] ||
        fail "refusing to write to $dev: it's backed by '$back', not $file"
    size=$(blockdev --getsize64 "$dev")
    [ "$size" = "$(stat -c %s "$file")" ] || fail "refusing to write to $dev: $size bytes, not the size of $file"
}
ours_part() {
    [[ $1 =~ ^(/dev/loop[0-9]+)p[0-9]+$ ]] || fail "refusing to write to $1: not a partition of a loop device"
    ours "${BASH_REMATCH[1]}" "$2"
    [ -b "$1" ] || fail "$1 doesn't exist"
}

# Sets up FILE as a loop device (with its partitions) in $ATTACHED.
attach() {
    ATTACHED=$(losetup --find --show --partscan "$1")
    LOOPS+=("$ATTACHED")
    settle
}
settle() { udevadm settle --timeout=30 2>/dev/null || sleep 2; }

# Re-reads DEV's partition table and waits for N partitions to show up.
partitions() {
    local dev=$1 n=$2
    blockdev --rereadpt "$dev" 2>/dev/null || partx -u "$dev" 2>/dev/null || true
    settle
    for _ in $(seq 1 50); do
        [ -b "${dev}p$n" ] && return 0
        sleep 0.2
    done
    fail "$dev doesn't show $n partitions"
}

# --- The source disk ----------------------------------------------------------------------

step "A GPT disk with FAT32, ext4, NTFS and exFAT, full of random files"
TREE=$W/tree
for fs in fat ext ntfs exfat; do mkdir -p "$TREE/$fs"; done
mkdir -p "$TREE/fat/dir" "$TREE/ext/sub/deeper" "$TREE/exfat/folder"
random_file() { head -c "$2" /dev/urandom >"$1"; }
random_file "$TREE/fat/big.bin" $((9 * 1024 * 1024 + 123))
random_file "$TREE/fat/dir/small.bin" 4097
echo "hello from dd-gui" >"$TREE/fat/hello.txt"
random_file "$TREE/ext/big.bin" $((12 * 1024 * 1024))
random_file "$TREE/ext/sub/odd.bin" 777777
random_file "$TREE/ext/sub/deeper/tiny.bin" 1
random_file "$TREE/ntfs/big.bin" $((7 * 1024 * 1024 + 5))
random_file "$TREE/ntfs/small.bin" 60000
random_file "$TREE/exfat/big.bin" $((5 * 1024 * 1024 + 1))
random_file "$TREE/exfat/folder/small.bin" 3000
for fs in fat ext ntfs exfat; do
    (cd "$TREE/$fs" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sed 's#  \./#  #') >"$W/$fs.sums"
done

truncate -s "${DISK_MB}M" "$W/src.img"
sfdisk --quiet "$W/src.img" <<'EOF'
label: gpt
size=120MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="FAT"
size=160MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="EXT"
size=120MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="NTFS"
type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="EXFAT"
EOF
attach "$W/src.img"
SRC=$ATTACHED
partitions "$SRC" 4
say "source disk: $SRC"

ours_part "${SRC}p1" "$W/src.img"
mkfs.vfat -F 32 -n FATVOL "${SRC}p1" >/dev/null
mcopy -s -i "${SRC}p1" "$TREE/fat/"* ::/
ours_part "${SRC}p2" "$W/src.img"
mke2fs -q -F -t ext4 -L EXTVOL -d "$TREE/ext" "${SRC}p2"
ours_part "${SRC}p3" "$W/src.img"
# Where the partition starts, for the boot sector (mkntfs can't ask a loop device).
start=$(cat "/sys/class/block/$(basename "${SRC}p3")/start")
mkntfs -Q -q -F -p "$start" -H 255 -S 63 -L NTFSVOL "${SRC}p3" >/dev/null
for f in big.bin small.bin; do ntfscp -q "${SRC}p3" "$TREE/ntfs/$f" "$f"; done
ours_part "${SRC}p4" "$W/src.img"
mkfs.exfat -L EXFATVOL "${SRC}p4" >/dev/null
mkdir -p "$W/mnt"
# exFAT through the kernel, or else through FUSE (exfat-fuse), if either is there.
mount_exfat() {
    modprobe exfat 2>/dev/null || true
    if mount -t exfat "$@" 2>/dev/null; then
        return 0
    fi
    if ! command -v mount.exfat-fuse >/dev/null && command -v apt-get >/dev/null; then
        timeout 300 apt-get install -y -qq --no-install-recommends exfat-fuse >/dev/null 2>&1 || true
    fi
    command -v mount.exfat-fuse >/dev/null && mount.exfat-fuse "$@" 2>/dev/null
}
if mount_exfat "${SRC}p4" "$W/mnt"; then
    MOUNTS+=("$W/mnt")
    cp -r "$TREE/exfat/." "$W/mnt/"
    # A deleted file: its blocks are free space, which a smart copy skips.
    random_file "$W/mnt/deleted.bin" $((3 * 1024 * 1024))
    sync
    rm "$W/mnt/deleted.bin"
    umount "$W/mnt"
    EXFAT_FILES=1
else
    say "note: exFAT can't be mounted here, so its partition stays empty"
    : >"$W/exfat.sums"
    EXFAT_FILES=0
fi
settle
pass "source disk built"

# Checks every file system on DEV (a loop device) and every file against the sums.
check_disk() {
    local dev=$1 what=$2 sum path got
    fsck.vfat -n "${dev}p1" >"$W/fsck.log" 2>&1 || fail "$what: fsck.vfat found problems: $(tail -3 "$W/fsck.log")"
    e2fsck -f -n "${dev}p2" >"$W/fsck.log" 2>&1 || fail "$what: e2fsck found problems: $(tail -3 "$W/fsck.log")"
    ntfsfix -n "${dev}p3" >"$W/fsck.log" 2>&1 || fail "$what: ntfsfix found problems: $(tail -3 "$W/fsck.log")"
    fsck.exfat -n "${dev}p4" >"$W/fsck.log" 2>&1 || fail "$what: fsck.exfat found problems: $(tail -3 "$W/fsck.log")"
    pass "$what: FAT32, ext4, NTFS and exFAT check clean"
    while read -r sum path; do
        got=$(mcopy -n -i "${dev}p1" "::/$path" - | sha256sum | cut -c1-64) || fail "$what: couldn't read FAT file $path"
        [ "$got" = "$sum" ] || fail "$what: FAT file $path differs"
    done <"$W/fat.sums"
    while read -r sum path; do
        got=$(debugfs -R "cat /$path" "${dev}p2" 2>/dev/null | sha256sum | cut -c1-64) || fail "$what: couldn't read ext4 file $path"
        [ "$got" = "$sum" ] || fail "$what: ext4 file $path differs"
    done <"$W/ext.sums"
    while read -r sum path; do
        got=$(ntfscat "${dev}p3" "$path" | sha256sum | cut -c1-64) || fail "$what: couldn't read NTFS file $path"
        [ "$got" = "$sum" ] || fail "$what: NTFS file $path differs"
    done <"$W/ntfs.sums"
    if [ "$EXFAT_FILES" = 1 ]; then
        mount_exfat "${dev}p4" "$W/mnt" -o ro || fail "$what: couldn't mount the exFAT partition"
        MOUNTS+=("$W/mnt")
        (cd "$W/mnt" && sha256sum --quiet -c "$W/exfat.sums") >"$W/sums.log" 2>&1 ||
            { umount "$W/mnt"; fail "$what: exFAT files differ: $(head -3 "$W/sums.log")"; }
        umount "$W/mnt"
    fi
    pass "$what: every file matches ($(cat "$W"/*.sums | wc -l) files)"
}

check_disk "$SRC" "source disk"

# --- dd-gui drives ------------------------------------------------------------------------

step "dd-gui drives"
mkdir -p "$W/mnt-ext"
mount -o ro "${SRC}p2" "$W/mnt-ext"
MOUNTS+=("$W/mnt-ext")
run "$DD_GUI" drives >"$W/drives.json" 2>"$W/drives.err" || fail "dd-gui drives failed: $(cat "$W/drives.err")"
python3 - "$W/drives.json" "$SRC" "$((DISK_MB * 1024 * 1024))" "$W/mnt-ext" <<'EOF' || fail "dd-gui drives doesn't list $SRC properly"
import json, sys
drives, path, size, mount = json.load(open(sys.argv[1])), sys.argv[2], int(sys.argv[3]), sys.argv[4]
d = next((d for d in drives if d["path"] == path), None)
assert d, f"{path} isn't listed: {[x['path'] for x in drives]}"
print(json.dumps(d))
assert (d["kind"], d["bus"], d["size"], d["io_path"]) == ("Virtual", "Loop", size, path), d
assert not d["system"] and not d["read_only"], d
assert mount in d["mountpoints"] and path + "p2" in d["unmount"], d
for v in ["FATVOL (FAT)", "EXTVOL (ext4)", "NTFSVOL (NTFS)", "EXFATVOL (exFAT)"]:
    assert v in d["volumes"], f"{v} missing from {d['volumes']}"
EOF
pass "dd-gui drives lists $SRC as a loop device, with its volumes and mount"

# --- Smart backup and restore -------------------------------------------------------------

# The @-lines a worker printed must include these, in this order.
protocol() {
    local file=$1
    shift
    python3 - "$file" "$@" <<'EOF' || fail "unexpected worker output in $file: $(grep -v '^@progress' "$file" | head -8 | tr '\n' ' ')"
import sys
lines = [l.split()[0] for l in open(sys.argv[1]) if l.startswith("@") and not l.startswith("@progress")]
want = sys.argv[2:]
it = iter(lines)
assert all(w in it for w in want), (lines, want)
EOF
}

step "Smart backup (dd-gui copy --mode=smart), unmounting the source"
run "$DD_GUI" copy --ddgui-worker "--ddgui-unmount=${SRC}p2" --mode=smart --from="$SRC" --to="$W/backup.img.zst" \
    >"$W/smart.out" 2>"$W/smart.err" || fail "smart backup failed: $(tail -3 "$W/smart.err")"
protocol "$W/smart.out" @ready @copy @total @sync
mountpoint -q "$W/mnt-ext" && fail "the worker didn't unmount ${SRC}p2"
used=$(awk '$1 == "@total" { print $2 }' "$W/smart.out")
if [ "$used" -le 0 ] || [ "$used" -ge $((DISK_MB * 1024 * 1024 / 2)) ]; then
    fail "the smart copy read $used bytes of a ${DISK_MB} MiB disk"
fi
image=$(stat -c %s "$W/backup.img.zst")
magic=$(od -An -tx1 -N4 "$W/backup.img.zst" | tr -d ' ')
case $magic in
    # A zstd frame, or a skippable frame (0x184D2A5?) in front of one.
    28b52ffd | 5?2a4d18) format=zstd ;;
    1f8b*) format=gzip ;;
    *) fail "the smart image starts with $magic, neither zstd nor gzip" ;;
esac
pass "smart backup: $used bytes of data, a $format image of $image bytes; ${SRC}p2 was unmounted"

step "The smart image is a plain compressed disk image for other tools too"
case $format in
    zstd) run zstd -q -d -c --long=31 "$W/backup.img.zst" >"$W/plain.img" ;;
    gzip) run gzip -d -c "$W/backup.img.zst" >"$W/plain.img" ;;
esac || fail "$format couldn't unpack the smart image"
[ "$(stat -c %s "$W/plain.img")" = "$((DISK_MB * 1024 * 1024))" ] || fail "the unpacked image isn't ${DISK_MB} MiB"
attach "$W/plain.img"
PLAIN=$ATTACHED
partitions "$PLAIN" 4
check_disk "$PLAIN" "smart image unpacked by $format"
losetup -d "$PLAIN"
rm -f "$W/plain.img"

step "Smart restore (dd-gui copy --mode=restore) onto a disk full of garbage"
head -c "$((DISK_MB * 1024 * 1024))" /dev/urandom >"$W/tgt.img"
attach "$W/tgt.img"
TGT=$ATTACHED
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" copy --ddgui-worker "--ddgui-sync=$TGT" --mode=restore --from="$W/backup.img.zst" --to="$TGT" \
    >"$W/restore.out" 2>"$W/restore.err" || fail "smart restore failed: $(tail -3 "$W/restore.err")"
protocol "$W/restore.out" @ready @copy @total @sync
partitions "$TGT" 4
pass "smart restore onto $TGT"
check_disk "$TGT" "restored disk"

# --- Sector by sector ---------------------------------------------------------------------

step "Sector by sector with the bundled dd"
run "$DD_GUI" dd if="$SRC" of="$W/full.img" bs=4M status=progress 2>"$W/dd.err" || fail "dd from $SRC failed: $(tail -3 "$W/dd.err")"
cmp "$W/src.img" "$W/full.img" || fail "dd's copy of $SRC differs from the disk"
pass "dd read $SRC into an identical image"
ours "$TGT" "$W/tgt.img"
# As the GUI runs it: through the worker, direct I/O, flushed at the end.
run "$DD_GUI" dd --ddgui-worker "--ddgui-sync=$TGT" if="$W/full.img" of="$TGT" bs=4M oflag=direct status=progress \
    >"$W/dd.out" 2>"$W/dd.err" || fail "dd onto $TGT failed: $(tail -3 "$W/dd.err")"
protocol "$W/dd.out" @ready @copy @sync
cmp "$W/full.img" "$TGT" || fail "$TGT differs from the image dd wrote"
pass "dd wrote the image onto $TGT, identical sector by sector"

# --- Wiping -------------------------------------------------------------------------------

size=$(blockdev --getsize64 "$TGT")
step "Wipe with dd (what the GUI does on Linux)"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" dd --ddgui-worker "--ddgui-sync=$TGT" if=/dev/zero of="$TGT" bs=16M count="$size" iflag=count_bytes \
    oflag=direct status=progress >"$W/wipe.out" 2>"$W/wipe.err" || fail "dd wipe failed: $(tail -3 "$W/wipe.err")"
cmp -n "$size" "$TGT" /dev/zero || fail "$TGT isn't all zeros after the dd wipe"
pass "dd wiped $TGT"

step "Wipe with dd-gui copy --mode=zeros (what the GUI does on Windows)"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" dd if="$W/full.img" of="$TGT" bs=4M oflag=direct status=none || fail "couldn't refill $TGT"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" copy --ddgui-worker "--ddgui-sync=$TGT" --mode=zeros --to="$TGT" \
    >"$W/zeros.out" 2>"$W/zeros.err" || fail "copy --mode=zeros failed: $(tail -3 "$W/zeros.err")"
protocol "$W/zeros.out" @ready @copy @total @sync
cmp -n "$size" "$TGT" /dev/zero || fail "$TGT isn't all zeros after copy --mode=zeros"
pass "copy --mode=zeros wiped $TGT"

# --- Cancelling ---------------------------------------------------------------------------

# How the GUI stops a worker: it closes the worker's stdin (Linux, Windows), or the worker
# watches a cancel file and the GUI's process (macOS). Each worker here waits for an
# answer to --ask that never comes, so only the cancelling can end it.
step "Cancelling"
cancelled() {
    local what=$1 code=0
    shift
    run "$DD_GUI" copy --ddgui-worker "$@" --mode=smart --ask --from="$W/full.img" --to="$W/cancelled.img.zst" \
        </dev/null >"$W/cancel.out" 2>&1 || code=$?
    [ "$code" = 130 ] || fail "$what should cancel the worker (exit code 130), not end it with $code: $(tail -2 "$W/cancel.out")"
    pass "$what cancels the worker"
}
cancelled "closing stdin" --ddgui-watch-stdin
sleep 0 &
gone=$!
wait "$gone" || true
cancelled "a --ddgui-parent that is gone" "--ddgui-parent=$gone" "--ddgui-answer-file=$W/never"
touch "$W/cancel"
cancelled "a --ddgui-cancel-file that exists" "--ddgui-cancel-file=$W/cancel" "--ddgui-answer-file=$W/never"
[ ! -e "$W/cancelled.img.zst" ] || [ ! -s "$W/cancelled.img.zst" ] || fail "a cancelled worker still wrote an image"
