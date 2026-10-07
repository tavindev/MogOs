#!/bin/bash
# Cross-OS benchmark comparison (docs/BENCHMARKS.md, "Cross-OS comparison"): the same C source (c/oscb.c) on MogOs,
# Alpine Linux (mitigations default and off) in the same QEMU setup, and the macOS host as a bare-metal reference,
# plus MogOs's native Rust benchmarks. Fetches and builds everything into third_party/oscompare, boots each OS once
# per run, interleaved, and prints a markdown table.
# Usage: scripts/oscompare.sh [runs]   (default 21)
set -euo pipefail

RUNS=${1:-21}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TP=$ROOT/third_party/oscompare
OUT=$TP/out
LLVM=/opt/homebrew/opt/llvm/bin
SYSROOT=$(rustc --print sysroot)
LLD=$SYSROOT/lib/rustlib/aarch64-apple-darwin/bin/gcc-ld/ld.lld

BENCHES="syscalls yield pipe spawn files readdir fileio"
# MogOs runs oscb from msh (`sh -c`), files in the MogFS root; its raw disk has no C path, so that row is native.
MOGOS_SCRIPT="sh -c '$(for b in $BENCHES; do printf 'oscb %s / oscnop; ' "$b"; done)'"
MOGOS_NATIVE="test=bench-syscall test=bench-pipe test=bench-spawn test=bench-fs test=bench-disk"

ALPINE_ISO=alpine-virt-3.24.2-aarch64.iso
ALPINE_URL=https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/$ALPINE_ISO
ALPINE_SHA=a57ba668b5f6b17a670fcf8e799d5d7fe43766ed086d6ce2927b0625bf43dbf6
# The same musl release and pin as MogOs's (c/Makefile), unpatched, for Linux.
MUSL=$(sed -n 's/^MUSL := //p' "$ROOT/c/Makefile")
MUSL_SHA=$(sed -n 's/^MUSL_SHA := //p' "$ROOT/c/Makefile")
MUSL_URL=https://musl.libc.org/releases/$MUSL.tar.gz
# mke2fs and its shared libraries, from the ISO's package repository.
APKS="e2fsprogs-1.47.4-r0 e2fsprogs-libs-1.47.4-r0 libcom_err-1.47.4-r0 libblkid-2.42.3-r1 libuuid-2.42.3-r1"
# mog-cc's optimization level, for musl and oscb alike.
OPT=-Os

DISK_MIB=64
QEMU=(qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a72 -accel hvf -m 128M -smp 1 -nographic
    -global virtio-mmio.force-legacy=false -global virtio-mmio.ioeventfd=off
    -drive file="$OUT/disk.img",if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0)
MOGOS=(-kernel "$ROOT/target/aarch64-unknown-none-softfloat/release/mog_os")
# Linux also gets Alpine's modloop (ext4's module) as a second, read-only disk; panic=-1 with -no-reboot ends a failed boot.
LINUX=(-kernel "$TP/linux/boot/vmlinuz-virt" -initrd "$TP/linux/initrd" -no-reboot
    -drive file="$TP/linux/boot/modloop-virt",if=none,format=raw,id=d1,readonly=on -device virtio-blk-device,drive=d1)
LINUX_APPEND="console=ttyAMA0 rdinit=/bench/init quiet panic=-1"

fetch() { # url sha256 file
    if [ ! -f "$3" ]; then
        curl -fL --progress-bar -o "$3.part" "$1"
        mv "$3.part" "$3"
    fi
    echo "$2  $3" | shasum -a 256 -c --quiet
}

build_linux_guest() {
    local g=$TP/linux
    local f fresh=1
    for f in c/oscb.c c/oscnop.c scripts/oscompare-init.sh scripts/oscompare.sh; do [ "$g/initrd" -nt "$ROOT/$f" ] || fresh=; done
    [ -n "$fresh" ] && return
    fetch "$ALPINE_URL" "$ALPINE_SHA" "$TP/$ALPINE_ISO"
    fetch "$MUSL_URL" "$MUSL_SHA" "$TP/$MUSL.tar.gz"
    local musl=$TP/sysroot-$MUSL$OPT
    if [ ! -f "$musl/lib/libc.a" ]; then
        rm -rf "$TP/$MUSL" && tar xzf "$TP/$MUSL.tar.gz" -C "$TP"
        (cd "$TP/$MUSL" && ./configure --target=aarch64-linux-musl --prefix="$musl" --disable-shared \
            CC="$LLVM/clang" CFLAGS="--target=aarch64-linux-musl $OPT" AR="$LLVM/llvm-ar" RANLIB="$LLVM/llvm-ranlib" \
            >/dev/null && make -j3 install >/dev/null)
    fi
    rm -rf "$g" && mkdir -p "$g/pkg" "$g/root/bench" "$g/root/usr/sbin" "$g/root/usr/lib" "$g/root/etc"
    local apk name
    for apk in $APKS; do bsdtar -xf "$TP/$ALPINE_ISO" -C "$g" "apks/aarch64/$apk.apk"; done
    bsdtar -xf "$TP/$ALPINE_ISO" -C "$g" boot/vmlinuz-virt boot/initramfs-virt boot/modloop-virt
    for apk in $APKS; do bsdtar -xf "$g/apks/aarch64/$apk.apk" -C "$g/pkg"; done
    cp "$g/pkg/sbin/mke2fs" "$g/root/usr/sbin/"
    cp "$g/pkg/etc/mke2fs.conf" "$g/root/etc/"
    cp -P "$g"/pkg/usr/lib/*.so* "$g/root/usr/lib/"
    for name in oscb oscnop; do
        "$LLVM/clang" --target=aarch64-linux-musl --sysroot="$musl" $OPT -Wall -c "$ROOT/c/$name.c" -o "$g/$name.o"
        DYLD_LIBRARY_PATH=$SYSROOT/lib "$LLD" -static -s -o "$g/root/bench/$name" \
            "$musl/lib/crt1.o" "$musl/lib/crti.o" "$g/$name.o" "$musl/lib/libc.a" "$musl/lib/crtn.o"
    done
    cp "$ROOT/scripts/oscompare-init.sh" "$g/root/bench/init"
    chmod 755 "$g/root/bench/init"
    (cd "$g/root" && find . | LC_ALL=C sort | cpio -o -H newc -R 0:0 --quiet | gzip -9) >"$g/overlay.cpio.gz"
    # The kernel unpacks concatenated archives in order, so the overlay adds to Alpine's initramfs.
    cat "$g/boot/initramfs-virt" "$g/overlay.cpio.gz" >"$g/initrd"
}

build_macos() {
    mkdir -p "$TP/macos"
    xcrun clang $OPT -Wall -o "$TP/macos/oscb" "$ROOT/c/oscb.c"
    xcrun clang $OPT -o "$TP/macos/oscnop" "$ROOT/c/oscnop.c"
}

# The release kernel bundles oscb and oscnop built against MogOs's musl (c/Makefile).
build_mogos() {
    (cd "$ROOT" && CARGO_BUILD_JOBS=3 cargo build --release -p qemu-virt -q &&
        CARGO_BUILD_JOBS=3 cargo build --release -q -p mogfs --example mkfs --target aarch64-apple-darwin)
    rm -f "$TP/mogfs.img"
    "$ROOT/target/aarch64-apple-darwin/release/examples/mkfs" "$TP/mogfs.img" $((DISK_MIB * 256))
}

# A fully allocated image on both sides, so host block allocation costs the same.
fresh_disk() { # [image to copy in]
    dd if=/dev/zero of="$OUT/disk.img" bs=1m count=$DISK_MIB 2>/dev/null
    [ -z "${1:-}" ] || dd if="$1" of="$OUT/disk.img" bs=1m conv=notrunc 2>/dev/null
}

# QEMU with a 300 s alarm, so a hung guest fails the run instead of stalling it.
boot() { perl -e 'alarm 300; exec @ARGV' "${QEMU[@]}" "$@" </dev/null; }

# Boots MogOs into msh, types `command` at the prompt, then `exit`; prints the console output.
boot_msh() { # command
    python3 - "$1" "${QEMU[@]}" "${MOGOS[@]}" -append test=shell <<'EOF'
import signal, subprocess, sys
command, argv = sys.argv[1], sys.argv[2:]
qemu = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
out = b""
def timeout(*_):
    qemu.kill()
    sys.exit(f"msh: timed out\n{out.decode(errors='replace')}")
signal.signal(signal.SIGALRM, timeout)
signal.alarm(300)
for line in (command, "exit"):
    while out.count(b"msh> ") < (1 if line == command else 2):
        chunk = qemu.stdout.read1(4096)
        if not chunk:
            timeout()
        out += chunk
    qemu.stdin.write(line.encode() + b"\r")
    qemu.stdin.flush()
out += qemu.stdout.read()
qemu.wait()
sys.stdout.write(out.decode(errors="replace"))
EOF
}

mkdir -p "$TP" "$OUT"
rm -f "$OUT"/*.log
build_linux_guest
build_macos
build_mogos
{
    echo "date: $(date -u +%Y-%m-%dT%H:%MZ)"
    echo "host: $(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores, macOS $(sw_vers -productVersion)"
    echo "qemu: $(qemu-system-aarch64 --version | head -1)"
    echo "mogos: $(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet HEAD -- crates c || echo '+dirty'), release build"
    echo "linux: $ALPINE_ISO; oscb $OPT, static, $MUSL (MogOs: the same release with its syscall layer, soft-float)"
    echo "runs: $RUNS"
    echo "load before: $(sysctl -n vm.loadavg)"
} >"$OUT/meta.txt"

for run in $(seq "$RUNS"); do
    echo "run $run/$RUNS" >&2
    fresh_disk "$TP/mogfs.img"
    # A failed boot only loses its samples: the table shows each row's count when it is short.
    boot_msh "$MOGOS_SCRIPT" >"$OUT/mogos-$run.log" || true
    for test in $MOGOS_NATIVE; do
        fresh_disk "$TP/mogfs.img"
        boot "${MOGOS[@]}" -append "$test" >"$OUT/native-$run-$test.log" || true
    done
    fresh_disk
    boot "${LINUX[@]}" -append "$LINUX_APPEND" >"$OUT/linux-$run.log" || true
    fresh_disk
    boot "${LINUX[@]}" -append "$LINUX_APPEND mitigations=off" >"$OUT/linux-nomit-$run.log" || true
    rm -rf "$OUT/macos-dir" && mkdir "$OUT/macos-dir"
    for b in $BENCHES; do "$TP/macos/oscb" "$b" "$OUT/macos-dir" "$TP/macos/oscnop" || true; done >"$OUT/macos-$run.log"
done
echo "load after: $(sysctl -n vm.loadavg)" >>"$OUT/meta.txt"

python3 - "$OUT" "$RUNS" <<'EOF'
import glob, os, re, statistics, sys

out, runs = sys.argv[1], int(sys.argv[2])
# MogOs's native benchmark lines, mapped to the oscb rows that measure the same thing.
native = [
    (r"^syscall: (\d+) ns", "write0"),
    (r"^pipe: (\d+) ns", "pipe"),
    (r"^spawn: (\d+) ns", "spawn"),
    (r"^open\+write\+sync: (\d+) ns", "create+write+fsync"),
    (r"^open\+close: (\d+) ns", "open+close"),
    (r"^disk: 4 KiB write\+flush (\d+) MiB/s", "raw-write+flush-4k"),
    (r"^disk: 4 KiB read (\d+) MiB/s", "raw-read-4k"),
    (r"^disk: 256 KiB write\+flush (\d+) MiB/s", "raw-write+flush-256k"),
    (r"^disk: 256 KiB read (\d+) MiB/s", "raw-read-256k"),
]
rows = [
    ("getppid", "ns"), ("write0", "ns"), ("yield", "ns"), ("pipe", "ns"), ("spawn", "ns"),
    ("open+close", "ns"), ("create+write+fsync", "ns"), ("readdir1000", "ns"),
    ("file-write+fsync-256k", "MiB/s"), ("file-read-256k", "MiB/s"),
    ("raw-write+flush-4k", "MiB/s"), ("raw-read-4k", "MiB/s"),
    ("raw-write+flush-256k", "MiB/s"), ("raw-read-256k", "MiB/s"),
]
oses = ["mogos", "native", "linux", "linux-nomit", "macos"]
data = {os_: {} for os_ in oses}
errors, meta = {}, []
for path in sorted(glob.glob(os.path.join(out, "*.log"))):
    name = os.path.basename(path)
    os_ = "linux-nomit" if name.startswith("linux-nomit") else name.split("-")[0]
    for line in open(path, errors="replace"):
        line = line.strip()
        m = re.match(r"^oscb: (\S+) ([\d.]+) ", line)
        if m:
            data[os_].setdefault(m[1], []).append(float(m[2]))
        m = re.match(r"^oscb: error (\S+)", line)
        if m:
            errors.setdefault(f"{os_}: {m[1]}", 0)
            errors[f"{os_}: {m[1]}"] += 1
        if line.startswith("oscb-meta:") and name in ("linux-1.log", "linux-nomit-1.log"):
            meta.append(f"{name[:-6]}: {line[11:]}")
        if os_ == "native":
            for pattern, key in native:
                m = re.match(pattern, line)
                if m:
                    data[os_].setdefault(key, []).append(float(m[1]))

print(open(os.path.join(out, "meta.txt")).read().rstrip())
print("\n".join(meta))
print("errors (runs): " + (", ".join(f"{k} ({v})" for k, v in sorted(errors.items())) or "none"))
print()
def cell(values, unit):
    if not values:
        return "n/a"
    best = min(values) if unit == "ns" else max(values)
    short = "" if len(values) == runs else f" n={len(values)}"
    return f"{statistics.median(values):.1f} ({best:.1f}){short}"
print("| Benchmark | Unit | " + " | ".join(oses) + " | MogOs vs Linux |")
print("| --- | --- |" + " --- |" * (len(oses) + 1))
for key, unit in rows:
    cells = [cell(data[o].get(key), unit) for o in oses]
    # MogOs's getppid never traps (libc answers it), so it has no ratio; Linux's write0 crosses its tty layer, so
    # MogOs's write0 is compared with Linux's null syscall; the raw disk has only the native path.
    m = None if key == "getppid" else data["mogos"].get(key) or data["native"].get(key)
    l = data["linux"].get("getppid" if key == "write0" else key)
    ratio = "n/a"
    if m and l:
        r = statistics.median(l) / statistics.median(m) if unit == "ns" else statistics.median(m) / statistics.median(l)
        ratio = f"{r:.2f}x" + ("" if data["mogos"].get(key) else " (native)") + (" vs getppid" if key == "write0" else "")
    print(f"| {key} | {unit} | " + " | ".join(cells) + f" | {ratio} |")
EOF
