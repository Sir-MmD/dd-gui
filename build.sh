#!/usr/bin/env bash
#
# Builds DD-GUI on Linux, pacman style:
#   1. checks the build dependencies and offers to install what's missing,
#   2. downloads the crates,
#   3. builds the release binary,
#   4. puts it in dist/dd-gui.
#
#   ./build.sh [--check] [--clean] [--noconfirm]
#
# On macOS, run ./build.command instead; on Windows, build.bat.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

case "$(uname -s)" in
    Linux) ;;
    Darwin) exec "$ROOT/build.command" "$@" ;;
    *) echo "build.sh builds DD-GUI on Linux. Use build.command on macOS or build.bat on Windows." >&2; exit 1 ;;
esac

RUST_MIN=1.93 # sevenz-rust2 1.93, Slint 1.92
NAME=$(sed -n 's/^name = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
DIST=$ROOT/dist
CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}

CHECK=0 CLEAN=0 NOCONFIRM=0

# ---------------------------------------------------------------------------------------
# Looks: pacman's colors, "::" headers and progress bars.

TTY=0
[[ -t 1 ]] && TTY=1
if (( TTY )) && [[ -z ${NO_COLOR:-} ]]; then
    ALL_OFF=$'\e[0m' BOLD=$'\e[1m' BLUE=$'\e[1;34m' RED=$'\e[1;31m' YELLOW=$'\e[1;33m'
else
    ALL_OFF='' BOLD='' BLUE='' RED='' YELLOW=''
fi
COLS=80
if (( TTY )); then
    COLS=$(tput cols 2>/dev/null || echo 80)
    (( COLS >= 40 )) || COLS=80
fi
# Pac-Man eats the progress bar when pacman.conf says ILoveCandy.
CANDY=0
grep -qE '^[[:space:]]*ILoveCandy' /etc/pacman.conf 2>/dev/null && CANDY=1

section() { printf '%s::%s%s %s%s\n' "$BLUE" "$ALL_OFF" "$BOLD" "$*" "$ALL_OFF"; }
plain() { printf ' %s\n' "$*"; }
warning() { printf '%swarning:%s %s\n' "$YELLOW" "$ALL_OFF" "$*" >&2; }
error() { printf '%serror:%s %s\n' "$RED" "$ALL_OFF" "$*" >&2; }
die() {
    error "$@"
    exit 1
}

# ask "Proceed with installation?": true for yes. Enter means yes, as in pacman.
ask() {
    printf '%s::%s%s %s [Y/n] %s' "$BLUE" "$ALL_OFF" "$BOLD" "$1" "$ALL_OFF"
    if (( NOCONFIRM )); then
        echo
        return 0
    fi
    if [[ ! -t 0 ]]; then
        echo
        error "not a terminal, so nothing was installed (use --noconfirm to say yes)"
        return 1
    fi
    local reply=''
    read -r reply || reply=n
    [[ -z $reply || $reply == [Yy]* ]]
}

# One line of a bar, like pacman's: "(  5/709) compiling libc    [####-------]  42%".
LASTHASH=-1 MOUTH=0
progress() {
    (( TTY )) || return 0
    local i=$1 n=$2 label=$3
    (( n > 0 )) || n=1
    (( i <= n )) || i=$n
    local pct=$(( i * 100 / n ))
    local width=$(( COLS - 1 ))
    local infolen=$(( width * 6 / 10 ))
    (( infolen >= 50 )) || infolen=50
    local hashlen=$(( width - infolen - 8 ))
    local text
    printf -v text '(%*d/%d) %s' "${#n}" "$i" "$n" "$label"
    (( ${#text} <= infolen )) || text=${text:0:infolen}
    if (( hashlen < 5 )); then
        printf '\r\033[K%s %3d%%' "$text" "$pct"
        return 0
    fi
    local hash=$(( hashlen * pct / 100 )) bar='' k
    if (( CANDY )); then
        if (( hash != LASTHASH )); then
            LASTHASH=$hash
            MOUTH=$(( 1 - MOUTH ))
        fi
        for (( k = hashlen; k > 0; k-- )); do
            if (( k > hashlen - hash )); then
                bar+='-'
            elif (( k == hashlen - hash )); then
                if (( MOUTH )); then bar+=$'\e[1;33mC\e[0m'; else bar+=$'\e[1;33mc\e[0m'; fi
            elif (( k % 3 == 0 )); then
                bar+=$'\e[0;37mo\e[0m'
            else
                bar+=' '
            fi
        done
    else
        local done_part todo_part
        printf -v done_part '%*s' "$hash" ''
        printf -v todo_part '%*s' "$(( hashlen - hash ))" ''
        bar=${done_part// /#}${todo_part// /-}
    fi
    printf '\r%-*s [%s] %3d%%' "$infolen" "$text" "$bar" "$pct"
}

progress_done() {
    (( TTY )) && printf '\n'
    return 0
}

cursor_hidden=0
hide_cursor() { (( TTY )) && printf '\033[?25l' && cursor_hidden=1; return 0; }
show_cursor() { (( cursor_hidden )) && printf '\033[?25h' && cursor_hidden=0; return 0; }
trap show_cursor EXIT
trap 'show_cursor; printf "\n"; error "interrupted"; exit 130' INT TERM

human_size() { # bytes → "24.81 MiB", as pacman shows sizes
    awk -v b="$1" 'BEGIN {
        split("B KiB MiB GiB", u); i = 1
        while (b >= 1024 && i < 4) { b /= 1024; i++ }
        fmt = (i == 1) ? "%d %s" : "%.2f %s"
        printf fmt, b, u[i]
    }'
}

took() { # seconds → "6m 02s"
    local s=$1
    if (( s >= 60 )); then printf '%dm %02ds' $(( s / 60 )) $(( s % 60 )); else printf '%ds' "$s"; fi
}

# ---------------------------------------------------------------------------------------
# Build dependencies: Rust, a C compiler (libzstd), pkg-config and fontconfig's headers.

have() { command -v "$1" >/dev/null 2>&1; }

version_ge() { # version_ge 1.94.1 1.93 → true
    local -a a b
    local k
    IFS=. read -r -a a <<< "$1"
    IFS=. read -r -a b <<< "$2"
    for k in 0 1 2; do
        (( ${a[k]:-0} > ${b[k]:-0} )) && return 0
        (( ${a[k]:-0} < ${b[k]:-0} )) && return 1
    done
    return 0
}

# The distribution's package manager, from os-release (Debian has a game called pacman).
package_manager() {
    local ids=' '
    if [[ -r /etc/os-release ]]; then
        ids+=$(sed -n 's/^ID\(_LIKE\)\{0,1\}=//p' /etc/os-release | tr -d '"' | tr '\n' ' ')
    fi
    case $ids in
        *' arch '* | *' archlinux '*) echo pacman ;;
        *' debian '* | *' ubuntu '*) echo apt-get ;;
        *' fedora '* | *' rhel '* | *' centos '*) echo dnf ;;
        *' suse '* | *' opensuse '*) echo zypper ;;
        *)
            if [[ -f /etc/pacman.conf ]] && have pacman; then echo pacman
            elif have apt-get; then echo apt-get
            elif have dnf; then echo dnf
            elif have zypper; then echo zypper
            fi ;;
    esac
}

# The package that provides a dependency, per package manager.
package_for() {
    case $PM:$1 in
        pacman:rust) echo rust ;;
        pacman:cc) echo gcc ;;
        pacman:pkg-config) echo pkgconf ;;
        pacman:fontconfig) echo fontconfig ;;
        apt-get:cc) echo build-essential ;;
        apt-get:pkg-config) echo pkg-config ;;
        apt-get:fontconfig)
            if apt-cache show libfontconfig-dev >/dev/null 2>&1; then echo libfontconfig-dev; else echo libfontconfig1-dev; fi ;;
        dnf:cc | zypper:cc) echo gcc ;;
        dnf:pkg-config) echo pkgconf-pkg-config ;;
        zypper:pkg-config) echo pkg-config ;;
        dnf:fontconfig | zypper:fontconfig) echo fontconfig-devel ;;
        *:curl) echo curl ;;
    esac
}

# "1.98.1" (a nightly's "-nightly" left off).
rust_version() { rustc --version 2>/dev/null | awk '{print $2}' | sed 's/[^0-9.].*//'; }

cc_version() {
    local cc
    for cc in cc gcc clang; do
        if have "$cc"; then
            # gcc's -dumpversion is just "16"; clang has no -dumpfullversion.
            if "$cc" --version 2>/dev/null | head -n1 | grep -qi clang; then
                echo "clang $("$cc" -dumpversion 2>/dev/null)"
            else
                echo "gcc $("$cc" -dumpfullversion 2>/dev/null || "$cc" -dumpversion 2>/dev/null)"
            fi
            return 0
        fi
    done
    return 1
}

pkg_config() { if have pkg-config; then pkg-config "$@"; elif have pkgconf; then pkgconf "$@"; else return 1; fi; }

# Lists what's there and fills MISSING (dependencies), PACKAGES (to install) and RUST_PLAN.
MISSING=() PACKAGES=() RUST_PLAN=''
check_deps() {
    local print=$1 v
    MISSING=() PACKAGES=() RUST_PLAN=''
    row() { (( print )) && printf ' %-12s %s\n' "$1" "$2"; return 0; }
    missing_row() { (( print )) && printf ' %-12s %smissing%s\n' "$1" "$RED" "$ALL_OFF"; return 0; }

    v=$(rust_version || true)
    if [[ -n $v ]] && version_ge "$v" "$RUST_MIN" && have cargo; then
        row rust "$v"
    else
        if [[ -n $v ]]; then
            (( print )) && printf ' %-12s %s %s(%s or newer needed)%s\n' rust "$v" "$RED" "$RUST_MIN" "$ALL_OFF"
        else
            missing_row rust
        fi
        MISSING+=(rust)
        if have rustup; then
            RUST_PLAN=rustup-update
        elif [[ $PM == pacman && -z $v ]]; then
            PACKAGES+=("$(package_for rust)")
        else
            RUST_PLAN=rustup-install
            have curl || PACKAGES+=("$(package_for curl)")
        fi
    fi

    if v=$(cc_version); then row cc "$v"; else missing_row cc; MISSING+=(cc); PACKAGES+=("$(package_for cc)"); fi

    if v=$(pkg_config --version 2>/dev/null); then
        row pkg-config "$v"
        if v=$(pkg_config --modversion fontconfig 2>/dev/null); then
            row fontconfig "$v"
        else
            missing_row fontconfig
            MISSING+=(fontconfig)
            PACKAGES+=("$(package_for fontconfig)")
        fi
    else
        missing_row pkg-config
        missing_row fontconfig
        MISSING+=(pkg-config fontconfig)
        PACKAGES+=("$(package_for pkg-config)" "$(package_for fontconfig)")
    fi
}

as_root() {
    if (( EUID == 0 )); then "$@"
    elif have sudo; then sudo "$@"
    else die "installing needs root: run '$*' as root, then build.sh again"
    fi
}

install_packages() {
    case $PM in
        pacman) as_root pacman -S --needed --noconfirm "$@" ;;
        apt-get) as_root apt-get update -q && as_root apt-get install -y "$@" ;;
        dnf) as_root dnf install -y "$@" ;;
        zypper) as_root zypper --non-interactive install "$@" ;;
    esac
}

resolve_deps() {
    PM=$(package_manager)
    section "Checking build dependencies..."
    check_deps 1
    (( ${#MISSING[@]} == 0 )) && return 0

    if (( ${#PACKAGES[@]} )) && [[ -z $PM ]]; then
        die "no supported package manager found; please install: ${MISSING[*]}"
    fi
    printf 'resolving dependencies...\n\n'
    local list=() p
    for p in ${PACKAGES[@]+"${PACKAGES[@]}"}; do list+=("$p"); done
    case $RUST_PLAN in
        rustup-install) list+=("rust (rustup.rs)") ;;
        rustup-update) list+=("rust (rustup update stable)") ;;
    esac
    printf '%sPackages (%d)%s' "$BOLD" "${#list[@]}" "$ALL_OFF"
    printf ' %s ' "${list[@]}"
    printf '\n\n'
    ask "Proceed with installation?" || die "missing build dependencies: ${MISSING[*]}"

    if (( ${#PACKAGES[@]} )); then
        section "Installing packages..."
        install_packages "${PACKAGES[@]}" || die "couldn't install: ${PACKAGES[*]}"
        hash -r
    fi
    case $RUST_PLAN in
        rustup-install)
            section "Installing Rust (rustup)..."
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
                || die "couldn't install Rust with rustup"
            export PATH="$CARGO_HOME/bin:$PATH"
            hash -r ;;
        rustup-update)
            section "Updating Rust (rustup)..."
            rustup toolchain install stable --profile minimal --no-self-update || die "couldn't update Rust"
            # Build with the new stable without changing the user's default toolchain.
            if ! version_ge "$(rust_version || echo 0)" "$RUST_MIN"; then export RUSTUP_TOOLCHAIN=stable; fi ;;
    esac

    check_deps 0
    (( ${#MISSING[@]} == 0 )) || die "still missing after installing: ${MISSING[*]}"
    section "Checking build dependencies..."
    check_deps 1
}

# ---------------------------------------------------------------------------------------
# Sources and the build itself.

# How many crates in Cargo.lock aren't in cargo's download cache yet.
crates_to_download() {
    local line name='' version='' n=0
    while IFS= read -r line; do
        case $line in
            'name = "'*) name=${line#name = \"}; name=${name%\"} ;;
            'version = "'*) version=${line#version = \"}; version=${version%\"} ;;
            'source = "registry+'*) compgen -G "$CARGO_HOME/registry/cache/*/$name-$version.crate" >/dev/null || n=$(( n + 1 )) ;;
        esac
    done < "$ROOT/Cargo.lock"
    echo "$n"
}

fetch_sources() {
    section "Retrieving sources..."
    local total
    total=$(crates_to_download)
    if (( total == 0 )); then
        plain "all $(grep -c '^source = "registry+' "$ROOT/Cargo.lock") crates are here already"
        return 0
    fi
    local log=$TARGET/fetch.log i=0 line status
    mkdir -p "$TARGET"
    : > "$log"
    hide_cursor
    set +e
    while IFS= read -r line; do
        if [[ $line =~ Downloaded[[:space:]]+([^[:space:]]+)[[:space:]]+v ]]; then
            i=$(( i + 1 ))
            progress "$i" "$total" "downloading ${BASH_REMATCH[1]}"
        else
            printf '%s\n' "$line" >> "$log"
        fi
    done < <(cargo fetch --locked 2>&1; echo "@status $?")
    set -e
    status=$(sed -n 's/^@status //p' "$log")
    if [[ $status == 0 ]]; then
        progress "$total" "$total" "downloaded $i crates"
        progress_done
        show_cursor
    else
        progress_done
        show_cursor
        grep -v '^@status ' "$log" >&2
        die "couldn't download the crates"
    fi
}

# Exactly how many units cargo will build and build scripts it will run (the unit graph is
# an unstable cargo flag, so it can go away: then this is 0 and the bar guesses).
count_units() {
    local graph n
    graph=$(RUSTC_BOOTSTRAP=1 cargo "$@" --unit-graph -Z unstable-options 2>/dev/null) || graph=''
    n=$(printf '%s' "$graph" | grep -oE '"mode":"(build|run-custom-build|test)"' | wc -l) || n=0
    echo $(( n + 0 ))
}

# cargo's "   Compiling libc v0.2.177" status line, colored or not: the crate is group 2.
COMPILING_RE=$'Compiling(\e\\[[0-9;]*m)*[[:space:]]+([[:alnum:]_-]+)[[:space:]]+v'
# The package of a compiler-artifact message: "…#libc@0.2.177", or "…/dd-gui#0.1.0" for a
# path dependency named like its folder.
PKG_RE='"package_id":"[^"]*#([^"@]+)@'
PATH_PKG_RE='"package_id":"[^"]*/([^"/#]+)#'
# The same for every status line cargo prints, to leave them out of error output.
STATUS_RE=$'^[[:space:]]*(\e\\[[0-9;]*m)*[[:space:]]*(Compiling|Checking|Fresh|Finished|Running|Downloaded|Downloading|Blocking|Locking|Updating)[^[:alnum:]]'

# cargo_with_bar <what> <cargo args...>: runs cargo with a progress bar instead of its output,
# which goes to target/<what>.log. Shows the errors if it fails.
cargo_with_bar() {
    local what=$1
    shift
    local log=$TARGET/$what.log total i=0 current=$NAME line status pkg
    # Crates being compiled, newest last: the bar names the newest one still going.
    local inflight=' '
    total=$(count_units "$@")
    (( total > 0 )) || total=600
    mkdir -p "$TARGET"
    : > "$log"
    hide_cursor
    progress 0 "$total" "compiling"
    set +e
    while IFS= read -r line; do
        case $line in
            '{"reason":"compiler-artifact"'* | '{"reason":"build-script-executed"'*)
                i=$(( i + 1 ))
                (( i <= total )) || total=$i
                # A crate is done once its library or binary is (not its build script).
                if [[ $line == '{"reason":"compiler-artifact"'* && $line != *'"kind":["custom-build"]'* ]]; then
                    pkg=''
                    if [[ $line =~ $PKG_RE ]]; then pkg=${BASH_REMATCH[1]}; elif [[ $line =~ $PATH_PKG_RE ]]; then pkg=${BASH_REMATCH[1]}; fi
                    [[ -z $pkg ]] || inflight=${inflight/ $pkg / }
                    current=${inflight% }
                    current=${current##* }
                    [[ -n $current ]] || current=$NAME
                fi
                progress "$i" "$total" "compiling $current" ;;
            '{'*) ;;
            *)
                printf '%s\n' "$line" >> "$log"
                if [[ $line =~ $COMPILING_RE ]]; then
                    current=${BASH_REMATCH[2]}
                    inflight+="$current "
                    progress "$i" "$total" "compiling $current"
                fi ;;
        esac
    done < <(cargo "$@" --message-format=json-render-diagnostics --color always 2>&1; echo "@status $?")
    set -e
    status=$(sed -n 's/^@status //p' "$log")
    if [[ $status == 0 ]]; then
        progress "$total" "$total" "compiling $NAME"
        progress_done
        show_cursor
        # cargo sums them up per crate: "`dd-gui` (bin "dd-gui") generated 2 warnings".
        local warnings
        warnings=$( { grep -oE 'generated [0-9]+ warnings?' "$log" || true; } | awk '{ n += $2 } END { print n + 0 }')
        (( warnings == 0 )) || warning "the compiler printed $warnings warnings (see ${log#"$ROOT"/})"
    else
        progress_done
        show_cursor
        # The compiler's messages, without cargo's "Compiling …" lines.
        grep -vE "$STATUS_RE|^@status " "$log" >&2 || true
        die "the build failed (full log: ${log#"$ROOT"/})"
    fi
}

build_release() {
    section "Building $NAME $VERSION..."
    cargo_with_bar build build --release --locked
}

run_tests() {
    section "Building the tests..."
    cargo_with_bar tests test --release --locked --no-run
    section "Running the tests..."
    cargo test --release --locked --quiet || die "some tests failed"
}

package() {
    section "Packaging..."
    mkdir -p "$DIST"
    install -m755 "$TARGET/release/$NAME" "$DIST/$NAME"
    printf ' %-40s %s\n' "dist/$NAME" "$(human_size "$(stat -c %s "$DIST/$NAME")")"
}

usage() {
    cat <<EOF
Usage: ./build.sh [options]

Builds $NAME $VERSION for Linux into dist/$NAME. Missing build dependencies (Rust $RUST_MIN
or newer, a C compiler, pkg-config, fontconfig) are installed after asking.

Options:
  -c, --check       also build and run the tests
  -C, --clean       delete the previous build first
      --noconfirm   install missing dependencies without asking
  -h, --help        show this help
EOF
}

main() {
    local arg
    for arg in "$@"; do
        case $arg in
            -c | --check) CHECK=1 ;;
            -C | --clean) CLEAN=1 ;;
            --noconfirm) NOCONFIRM=1 ;;
            -h | --help) usage; exit 0 ;;
            *) usage >&2; die "unknown option: $arg" ;;
        esac
    done
    cd "$ROOT"
    local start=$SECONDS

    resolve_deps
    if (( CLEAN )); then
        section "Removing the previous build..."
        cargo clean --quiet
    fi
    fetch_sources
    build_release
    if (( CHECK )); then run_tests; fi
    package
    section "Finished $NAME $VERSION in $(took $(( SECONDS - start ))): dist/$NAME"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
