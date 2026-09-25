#!/usr/bin/env bash
# End-to-end test of dd-gui on macOS: a smart backup and restore, a sector-by-sector copy
# and a wipe, run for real as root, on attached disk images only.
#
#   sudo bash ci/e2e-macos.sh /absolute/path/to/dd-gui      (or the path in $DD_GUI)
#
# It attaches a blank raw image as a disk (hdiutil attach -nomount), partitions it with
# APFS, HFS+, FAT32 and ExFAT, fills them with random files, backs the disk up with
# `dd-gui copy --mode=smart`, restores that onto a second attached image full of garbage,
# and checks every file system (fsck_*) and every file (SHA-256).
#
# It only ever writes to disks it attached itself from files in its own temporary
# folder: `ours` asks hdiutil and diskutil before every write that the device is that
# disk image, with that size. Only one copy of the test disk is attached at a time (APFS
# won't mount two containers with the same UUID).
#
# Settings: E2E_WORKDIR (default $RUNNER_TEMP or /tmp; needs about 5 GB), E2E_DISK_MB
# (default 1536), E2E_TIME_LIMIT (seconds, default 1500). Written for bash 3.2 as well.

set -euo pipefail
export LC_ALL=C
export PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

DD_GUI=${1:-${DD_GUI:-}}
DISK_MB=${E2E_DISK_MB:-1536}
TIME_LIMIT=${E2E_TIME_LIMIT:-1500}
STEP_LIMIT=600

PASSED=0
DISKS=()
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
    local code=$? dev
    set +e
    if [ -n "$WATCHDOG" ]; then
        pkill -P "$WATCHDOG" 2>/dev/null
        kill "$WATCHDOG" 2>/dev/null
    fi
    for dev in ${DISKS[@]+"${DISKS[@]}"}; do
        detach "$dev"
    done
    [ -n "$W" ] && [ -d "$W" ] && rm -rf -- "$W"
    if [ "$code" -eq 0 ]; then
        printf '\nE2E macOS: PASS (%d checks)\n' "$PASSED"
    else
        printf '\nE2E macOS: FAIL (exit code %d, after %d passed checks)\n' "$code" "$PASSED" >&2
    fi
    exit "$code"
}
trap cleanup EXIT
trap 'fail "interrupted (or over the time limit of ${TIME_LIMIT}s)"' INT TERM

detach() {
    hdiutil detach "$1" -force >/dev/null 2>&1 || diskutil eject "$1" >/dev/null 2>&1 || true
}

# Runs a command with a time limit (macOS has no timeout(1)), so nothing can hang the job.
# Its stdin is /dev/null, as for any background job.
run() {
    "$@" &
    local pid=$! code=0 dog
    (
        sleep "$STEP_LIMIT"
        kill -TERM "$pid" 2>/dev/null
        sleep 10
        kill -KILL "$pid" 2>/dev/null
    ) >/dev/null 2>&1 &
    dog=$!
    wait "$pid" || code=$?
    pkill -P "$dog" 2>/dev/null || true
    kill "$dog" 2>/dev/null || true
    return "$code"
}

# --- Setup --------------------------------------------------------------------------------

[ "$(id -u)" -eq 0 ] || fail "run this as root (sudo): it writes to disks"
[ -n "$DD_GUI" ] || fail "usage: $0 /path/to/dd-gui (or set DD_GUI)"
[ -x "$DD_GUI" ] || fail "$DD_GUI isn't an executable file"
command -v python3 >/dev/null || fail "python3 is needed"
for tool in hdiutil diskutil fsck_apfs fsck_hfs fsck_msdos fsck_exfat shasum cmp; do
    command -v "$tool" >/dev/null || fail "$tool is missing"
done

( sleep "$TIME_LIMIT" && kill -TERM $$ ) 2>/dev/null &
WATCHDOG=$!

base=${E2E_WORKDIR:-${RUNNER_TEMP:-/tmp}}
mkdir -p "$base"
W=$(mktemp -d "$base/dd-gui-e2e.XXXXXX")
W=$(cd "$W" && pwd -P)
say "working in $W"
"$DD_GUI" dd --version >/dev/null 2>&1 || fail "$DD_GUI doesn't run"
SIZE=$((DISK_MB * 1024 * 1024))

HELPER=$W/helper.py
cat >"$HELPER" <<'EOF'
"""Questions for hdiutil and diskutil, answered from their property lists."""
import json, os, plistlib, re, subprocess, sys


def plist(*args):
    return plistlib.loads(subprocess.run(args, check=True, capture_output=True).stdout)


def listing():
    return plist("diskutil", "list", "-plist").get("AllDisksAndPartitions", [])


def whole_disk(_):
    """The whole disk in `hdiutil attach -plist` output (stdin)."""
    for entity in plistlib.loads(sys.stdin.buffer.read()).get("system-entities", []):
        dev = entity.get("dev-entry", "")
        if re.fullmatch(r"/dev/disk\d+", dev):
            return dev
    sys.exit("hdiutil attached no whole disk")


def ours(dev, image):
    """Exits with a reason unless `dev` is a disk image attached from `image`, of its size."""
    if not re.fullmatch(r"/dev/disk\d+", dev):
        sys.exit(f"{dev} isn't a whole disk")
    real = os.path.realpath(image)
    attached = [
        entity.get("dev-entry")
        for img in plist("hdiutil", "info", "-plist").get("images", [])
        if os.path.realpath(img.get("image-path", "")) == real
        for entity in img.get("system-entities", [])
    ]
    if dev not in attached:
        sys.exit(f"{dev} isn't attached from {image} (that image has {attached})")
    info = plist("diskutil", "info", "-plist", dev)
    if info.get("BusProtocol") != "Disk Image":
        sys.exit(f"{dev} is on {info.get('BusProtocol')!r}, not a disk image")
    size = info.get("TotalSize") or info.get("Size")
    if size != os.path.getsize(image):
        sys.exit(f"{dev} holds {size} bytes, not the {os.path.getsize(image)} of {image}")
    return ""


def partitions(dev):
    """Lines "index kind" for the file systems on `dev`: apfs, hfs, fat or exfat."""
    name = dev.removeprefix("/dev/")
    disk = next((e for e in listing() if e.get("DeviceIdentifier") == name), {})
    lines = []
    for part in disk.get("Partitions", []):
        pid, content = part.get("DeviceIdentifier", ""), part.get("Content", "")
        index = pid.rsplit("s", 1)[-1]
        if content == "EFI":
            continue
        if content == "Apple_APFS":
            kind = "apfs"
        elif content in ("Apple_HFS", "Apple_HFSX"):
            kind = "hfs"
        else:
            fs = plist("diskutil", "info", "-plist", pid).get("FilesystemType", "")
            kind = {"msdos": "fat", "exfat": "exfat"}.get(fs)
        if kind:
            lines.append(f"{index} {kind}")
    return "\n".join(lines)


def containers(dev):
    """The APFS containers stored on `dev`, one per line."""
    name = dev.removeprefix("/dev/")
    return "\n".join(
        "/dev/" + c["DeviceIdentifier"]
        for c in listing()
        if any(s.get("DeviceIdentifier", "").startswith(name + "s") for s in c.get("APFSPhysicalStores", []))
    )


def mounts(dev):
    """Lines "index mountpoint" for the mounted file systems on `dev` (APFS ones included)."""
    entries = listing()
    name = dev.removeprefix("/dev/")
    disk = next((e for e in entries if e.get("DeviceIdentifier") == name), {})
    lines = []
    for part in disk.get("Partitions", []):
        pid = part.get("DeviceIdentifier", "")
        mount = part.get("MountPoint")
        for container in entries:
            if any(s.get("DeviceIdentifier") == pid for s in container.get("APFSPhysicalStores", [])):
                mount = next((v["MountPoint"] for v in container.get("APFSVolumes", []) if v.get("MountPoint")), mount)
        if mount:
            lines.append(f"{pid.rsplit('s', 1)[-1]} {mount}")
    return "\n".join(lines)


def drive(json_file, dev, size):
    """Checks dev in `dd-gui drives` output; prints its unmount list, one per line."""
    drives = json.load(open(json_file))
    d = next((d for d in drives if d["path"] == dev), None)
    if not d:
        sys.exit(f"{dev} isn't listed: {[x['path'] for x in drives]}")
    print(json.dumps(d), file=sys.stderr)
    want = {"kind": "Virtual", "bus": "Disk image", "size": int(size), "io_path": dev.replace("/dev/", "/dev/r"),
            "system": False, "read_only": False, "removable": False}
    wrong = {k: (d[k], v) for k, v in want.items() if d[k] != v}
    if wrong:
        sys.exit(f"{dev} is listed wrong (got, expected): {wrong}")
    if d["unmount"][-1:] != [dev]:
        sys.exit(f"{dev} has to be unmounted last: {d['unmount']}")
    return "\n".join(d["unmount"])


def volumes(json_file, dev, *names):
    d = next(d for d in json.load(open(json_file)) if d["path"] == dev)
    missing = [n for n in names if not any(v.startswith(n) for v in d["volumes"])]
    if missing:
        sys.exit(f"volumes {missing} missing from {d['volumes']}")
    mounts = [m for m in d["mountpoints"] if m.startswith("/Volumes/")]
    if len(mounts) < len(names):
        sys.exit(f"mount points missing: {d['mountpoints']}")
    return ""


def same(a, b, size):
    """Whether the first `size` bytes of a and b match (in 1 MiB reads: raw disks want whole blocks)."""
    size, done = int(size), 0
    with open(a, "rb", buffering=0) as fa, open(b, "rb", buffering=0) as fb:
        while done < size:
            n = min(1 << 20, size - done)
            x, y = fa.read(n), fb.read(n)
            if x != y or len(x) != n:
                sys.exit(f"{a} and {b} differ within the MiB at {done}")
            done += n
    return ""


def zeros(path, size):
    size, done = int(size), 0
    with open(path, "rb", buffering=0) as f:
        while done < size:
            chunk = f.read(min(1 << 20, size - done))
            if not chunk or chunk.count(0) != len(chunk):
                sys.exit(f"{path} isn't all zeros within the MiB at {done}")
            done += len(chunk)
    return ""


def protocol(out_file, *want):
    lines = [l.split()[0] for l in open(out_file) if l.startswith("@") and not l.startswith("@progress")]
    it = iter(lines)
    if not all(w in it for w in want):
        sys.exit(f"worker lines {lines}, expected {list(want)} in that order")
    return ""


if __name__ == "__main__":
    result = globals()[sys.argv[1].replace("-", "_")](*sys.argv[2:])
    if result:
        print(result)
EOF
helper() { python3 "$HELPER" "$@"; }

# Attaches a raw image file as a disk, without mounting anything: ATTACHED=/dev/diskN.
attach() {
    local out
    out=$(hdiutil attach -plist -nomount -noverify -noautofsck -imagekey diskimage-class=CRawDiskImage "$1") ||
        fail "hdiutil couldn't attach $1"
    ATTACHED=$(printf '%s' "$out" | helper whole-disk) || fail "no disk from hdiutil for $1"
    DISKS+=("$ATTACHED")
}

# Before every write: DEV must be the disk image attached from FILE.
ours() {
    local why
    why=$(helper ours "$1" "$2" 2>&1) || fail "refusing to write to $1: $why"
}

# The drive's unmount list from `dd-gui drives` as --ddgui-unmount options: UNMOUNT=(...).
unmount_options() {
    local dev=$1 line
    run "$DD_GUI" drives >"$W/drives.json" 2>"$W/drives.err" || fail "dd-gui drives failed: $(cat "$W/drives.err")"
    helper drive "$W/drives.json" "$dev" "$SIZE" >"$W/unmount.txt" 2>"$W/drive.txt" ||
        fail "dd-gui drives doesn't list $dev properly: $(cat "$W/drive.txt")"
    UNMOUNT=()
    while read -r line; do
        UNMOUNT+=("--ddgui-unmount=$line")
    done <"$W/unmount.txt"
}

random_file() { head -c "$2" /dev/urandom >"$1"; }
size_of() { wc -c <"$1" | tr -d ' '; }

# --- The source disk ----------------------------------------------------------------------

step "A disk image with APFS, HFS+, FAT32 and ExFAT, full of random files"
TREE=$W/tree
for fs in apfs hfs fat exfat; do mkdir -p "$TREE/$fs/folder"; done
random_file "$TREE/apfs/big.bin" $((11 * 1024 * 1024 + 7))
random_file "$TREE/apfs/folder/small.bin" 5000
random_file "$TREE/hfs/big.bin" $((9 * 1024 * 1024))
random_file "$TREE/hfs/folder/odd.bin" 777777
random_file "$TREE/fat/big.bin" $((6 * 1024 * 1024 + 3))
echo "hello from dd-gui" >"$TREE/fat/folder/hello.txt"
random_file "$TREE/exfat/big.bin" $((4 * 1024 * 1024 + 1))
random_file "$TREE/exfat/folder/small.bin" 3000
for fs in apfs hfs fat exfat; do
    (cd "$TREE/$fs" && find . -type f -print0 | sort -z | xargs -0 shasum -a 256 | sed 's#  \./#  #') >"$W/$fs.sums"
done

dd if=/dev/zero of="$W/src.img" bs=1m count=0 seek="$DISK_MB" 2>/dev/null
attach "$W/src.img"
SRC=$ATTACHED
say "source disk: $SRC"
ours "$SRC" "$W/src.img"
run diskutil partitionDisk "$SRC" GPT APFS SRCAPFS 400M JHFS+ SRCHFS 300M FAT32 SRCFAT 200M ExFAT SRCEXFAT R \
    >"$W/partition.log" 2>&1 || fail "diskutil couldn't partition $SRC: $(tail -3 "$W/partition.log")"
helper partitions "$SRC" >"$W/kinds.txt"
[ "$(awk '{print $2}' "$W/kinds.txt" | sort | tr '\n' ' ')" = "apfs exfat fat hfs " ] ||
    fail "unexpected file systems on $SRC: $(tr '\n' ' ' <"$W/kinds.txt")"
helper mounts "$SRC" >"$W/mounts.txt"
while read -r index kind; do
    mount=$(awk -v i="$index" '$1 == i { sub(/^[^ ]+ /, ""); print }' "$W/mounts.txt")
    [ -n "$mount" ] || fail "the $kind volume on ${SRC}s$index isn't mounted"
    cp -R "$TREE/$kind/." "$mount/"
    # A deleted file: its blocks are free space, which a smart copy skips.
    random_file "$mount/deleted.bin" $((2 * 1024 * 1024))
    sync
    rm "$mount/deleted.bin"
done <"$W/kinds.txt"
sync
pass "source disk built: $(tr '\n' ' ' <"$W/kinds.txt")"

# Checks every file system on DEV (fsck, unmounted), then mounts it and checks every file.
check_disk() {
    local dev=$1 what=$2 index kind raw mount container
    for container in $(helper containers "$dev") "$dev"; do
        run diskutil unmountDisk force "$container" >/dev/null 2>&1 || true
    done
    while read -r index kind; do
        raw="/dev/r${dev#/dev/}s$index"
        case $kind in
            apfs) run fsck_apfs -n "$raw" ;;
            hfs) run fsck_hfs -fn "$raw" ;;
            fat) run fsck_msdos -n "$raw" ;;
            exfat) run fsck_exfat -n "$raw" ;;
        esac >"$W/fsck.log" 2>&1 || fail "$what: fsck of the $kind file system on $raw failed: $(tail -3 "$W/fsck.log")"
    done <"$W/kinds.txt"
    pass "$what: APFS, HFS+, FAT32 and ExFAT check clean"
    run diskutil mountDisk "$dev" >"$W/mount.log" 2>&1 || fail "$what: couldn't mount $dev: $(cat "$W/mount.log")"
    for container in $(helper containers "$dev"); do
        run diskutil mountDisk "$container" >/dev/null 2>&1 || true
    done
    helper mounts "$dev" >"$W/mounts.txt"
    while read -r index kind; do
        mount=$(awk -v i="$index" '$1 == i { sub(/^[^ ]+ /, ""); print }' "$W/mounts.txt")
        [ -n "$mount" ] || fail "$what: the $kind volume didn't mount"
        (cd "$mount" && shasum -a 256 -c "$W/$kind.sums") >"$W/sums.log" 2>&1 ||
            fail "$what: $kind files differ: $(grep -v ': OK$' "$W/sums.log" | head -3)"
    done <"$W/kinds.txt"
    pass "$what: every file matches ($(cat "$W"/*.sums | wc -l | tr -d ' ') files)"
}

check_disk "$SRC" "source disk"

# --- dd-gui drives ------------------------------------------------------------------------

step "dd-gui drives"
unmount_options "$SRC"
helper volumes "$W/drives.json" "$SRC" SRCAPFS SRCHFS SRCFAT SRCEXFAT >/dev/null 2>"$W/volumes.txt" ||
    fail "dd-gui drives: $(cat "$W/volumes.txt")"
pass "dd-gui drives lists $SRC as a disk image, with its volumes, mounts and ${#UNMOUNT[@]} things to unmount"

# --- Smart backup ---------------------------------------------------------------------------

step "Smart backup (dd-gui copy --mode=smart), unmounting the source as the GUI does"
run "$DD_GUI" copy --ddgui-worker "${UNMOUNT[@]}" --mode=smart --from="/dev/r${SRC#/dev/}" --to="$W/backup.img.zst" \
    >"$W/smart.out" 2>"$W/smart.err" || fail "smart backup failed: $(tail -3 "$W/smart.err")"
helper protocol "$W/smart.out" @ready @copy @total @sync >/dev/null || fail "smart backup: unexpected worker output"
helper mounts "$SRC" >"$W/mounts.txt"
[ ! -s "$W/mounts.txt" ] || fail "the worker left volumes of $SRC mounted: $(tr '\n' ' ' <"$W/mounts.txt")"
used=$(awk '$1 == "@total" { print $2 }' "$W/smart.out")
if [ "$used" -le 0 ] || [ "$used" -ge $((SIZE / 2)) ]; then
    fail "the smart copy read $used bytes of a ${DISK_MB} MiB disk"
fi
magic=$(od -An -tx1 -N4 "$W/backup.img.zst" | tr -d ' \n')
case $magic in
    28b52ffd | 5?2a4d18) format=zstd ;;
    1f8b*) format=gzip ;;
    *) fail "the smart image starts with $magic, neither zstd nor gzip" ;;
esac
pass "smart backup: $used bytes of data, a $format image of $(size_of "$W/backup.img.zst") bytes; volumes unmounted"

step "Sector by sector with the bundled dd, from the source"
run "$DD_GUI" dd if="/dev/r${SRC#/dev/}" of="$W/full.img" bs=4M status=progress 2>"$W/dd.err" ||
    fail "dd from $SRC failed: $(tail -3 "$W/dd.err")"
cmp "$W/src.img" "$W/full.img" || fail "dd's copy of $SRC differs from the disk"
pass "dd read $SRC into an identical image"
detach "$SRC"

# --- The image works without DD-GUI -------------------------------------------------------

step "The smart image is a plain compressed disk image for other tools too"
if ! command -v zstd >/dev/null && [ -n "${SUDO_USER:-}" ] && command -v brew >/dev/null; then
    # Homebrew refuses to run as root.
    run sudo -u "$SUDO_USER" -H env HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 "$(command -v brew)" install zstd \
        >/dev/null 2>&1 || true
fi
if [ "$format" = gzip ] || command -v zstd >/dev/null; then
    case $format in
        zstd) run zstd -q -d -c --long=31 "$W/backup.img.zst" >"$W/plain.img" ;;
        gzip) run gzip -d -c "$W/backup.img.zst" >"$W/plain.img" ;;
    esac || fail "$format couldn't unpack the smart image"
    [ "$(size_of "$W/plain.img")" = "$SIZE" ] || fail "the unpacked image isn't ${DISK_MB} MiB"
    attach "$W/plain.img"
    PLAIN=$ATTACHED
    check_disk "$PLAIN" "smart image unpacked by $format"
    detach "$PLAIN"
    rm -f "$W/plain.img"
else
    say "note: no zstd here, so the smart image isn't unpacked with it"
fi

# --- Smart restore ------------------------------------------------------------------------

step "Smart restore (dd-gui copy --mode=restore) onto a disk full of garbage"
head -c "$SIZE" /dev/urandom >"$W/tgt.img"
attach "$W/tgt.img"
TGT=$ATTACHED
unmount_options "$TGT"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" copy --ddgui-worker "${UNMOUNT[@]}" "--ddgui-sync=/dev/r${TGT#/dev/}" --mode=restore \
    --from="$W/backup.img.zst" --to="/dev/r${TGT#/dev/}" >"$W/restore.out" 2>"$W/restore.err" ||
    fail "smart restore failed: $(tail -3 "$W/restore.err")"
helper protocol "$W/restore.out" @ready @copy @total @sync >/dev/null || fail "smart restore: unexpected worker output"
pass "smart restore onto $TGT"
# Attached again, macOS reads the new partition table.
detach "$TGT"
attach "$W/tgt.img"
TGT=$ATTACHED
check_disk "$TGT" "restored disk"

# --- Sector by sector onto a disk -----------------------------------------------------------

step "Sector by sector onto a disk, as the GUI runs dd"
unmount_options "$TGT"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" dd --ddgui-worker "${UNMOUNT[@]}" "--ddgui-sync=/dev/r${TGT#/dev/}" if="$W/full.img" \
    of="/dev/r${TGT#/dev/}" bs=4M status=progress >"$W/dd.out" 2>"$W/dd.err" ||
    fail "dd onto $TGT failed: $(tail -3 "$W/dd.err")"
helper protocol "$W/dd.out" @ready @copy @sync >/dev/null || fail "dd: unexpected worker output"
helper same "$W/full.img" "/dev/r${TGT#/dev/}" "$SIZE" >/dev/null 2>"$W/same.txt" || fail "$(cat "$W/same.txt")"
pass "dd wrote the image onto $TGT, identical sector by sector"

# --- Wiping -------------------------------------------------------------------------------

step "Wipe with dd (what the GUI does on macOS)"
unmount_options "$TGT"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" dd --ddgui-worker "${UNMOUNT[@]}" "--ddgui-sync=/dev/r${TGT#/dev/}" if=/dev/zero of="/dev/r${TGT#/dev/}" \
    bs=16M count="$SIZE" iflag=count_bytes status=progress >"$W/wipe.out" 2>"$W/wipe.err" ||
    fail "dd wipe failed: $(tail -3 "$W/wipe.err")"
helper zeros "/dev/r${TGT#/dev/}" "$SIZE" >/dev/null 2>"$W/zeros.txt" || fail "$(cat "$W/zeros.txt")"
pass "dd wiped $TGT"

step "Wipe with dd-gui copy --mode=zeros (what the GUI does on Windows)"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" dd --ddgui-worker "--ddgui-unmount=$TGT" if="$W/full.img" of="/dev/r${TGT#/dev/}" bs=4M status=none \
    >/dev/null 2>&1 || fail "couldn't refill $TGT"
ours "$TGT" "$W/tgt.img"
run "$DD_GUI" copy --ddgui-worker "--ddgui-unmount=$TGT" "--ddgui-sync=/dev/r${TGT#/dev/}" --mode=zeros \
    --to="/dev/r${TGT#/dev/}" >"$W/zeros.out" 2>"$W/zeros.err" ||
    fail "copy --mode=zeros failed: $(tail -3 "$W/zeros.err")"
helper protocol "$W/zeros.out" @ready @copy @total @sync >/dev/null || fail "zeros: unexpected worker output"
helper zeros "/dev/r${TGT#/dev/}" "$SIZE" >/dev/null 2>"$W/zeros.txt" || fail "$(cat "$W/zeros.txt")"
pass "copy --mode=zeros wiped $TGT"

# --- Cancelling ---------------------------------------------------------------------------

# How the GUI stops a worker: on macOS, the worker (run through osascript) watches a
# cancel file and the GUI's process; otherwise it watches its stdin. Each worker here
# waits for an answer to --ask that never comes, so only the cancelling can end it.
step "Cancelling"
cancelled() {
    local what=$1 code=0
    shift
    run "$DD_GUI" copy --ddgui-worker "$@" --mode=smart --ask --from="$W/full.img" --to="$W/cancelled.img.zst" \
        >"$W/cancel.out" 2>&1 || code=$?
    [ "$code" = 130 ] || fail "$what should cancel the worker (exit code 130), not end it with $code: $(tail -2 "$W/cancel.out")"
    pass "$what cancels the worker"
}
touch "$W/cancel"
cancelled "a --ddgui-cancel-file that exists" "--ddgui-cancel-file=$W/cancel" "--ddgui-answer-file=$W/never"
sleep 0 &
gone=$!
wait "$gone" || true
cancelled "a --ddgui-parent that is gone" "--ddgui-parent=$gone" "--ddgui-answer-file=$W/never"
cancelled "closing stdin" --ddgui-watch-stdin
if [ -s "$W/cancelled.img.zst" ]; then
    fail "a cancelled worker still wrote an image"
fi
