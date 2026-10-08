#!/bin/sh
# Usage: scripts/bench.sh <test> <rounds> [<kernel> [<base kernel>]]
# Boots the kernel (default: the release build, built first) <rounds> times under hvf with -append test=<test>, each
# boot on a fresh 1024-block MogFS image, and prints the median and min of every `bench <name>: <ns> ns`,
# `<name>: [<what>] <ns> ns/round-trip` and `boot: <us> us` line. With a base kernel, each round boots both, the order
# alternating, and prints the base, the new and the delta of each; `SLOWER` marks a row whose median and min both rose.
# <test> may carry more bootargs; QEMU_ARGS adds QEMU arguments (a NIC: `-netdev user,id=n0 -device virtio-net-device,netdev=n0`).
# Usage: scripts/bench.sh host <rounds> <base commit> <package> [<criterion args>]
# Host criterion benches of <package>, the working tree against <base commit> (a temporary worktree): each round
# runs both, the order alternating, then prints criterion's change estimate and confidence interval for every row; the
# end prints each row's median, min and max change over the rounds.
set -eu
[ $# -ge 2 ] || { sed -n '2,11s/^# //p' "$0"; exit 2; }
root=$(cd "$(dirname "$0")/.." && pwd)

# One benchmark at a time on this machine, across worktrees (bench.lock in the common git dir); waits its turn.
if [ -z "${BENCH_LOCKED:-}" ]; then
    lock=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)/bench.lock
    lockf -kst 0 "$lock" true || echo "bench.sh: waiting for another benchmark to finish ($lock)" >&2
    BENCH_LOCKED=1 exec lockf -k "$lock" "$root/scripts/bench.sh" "$@"
fi

if [ "$1" = host ]; then
    [ $# -ge 4 ] || { sed -n '2,11s/^# //p' "$0"; exit 2; }
    rounds=$2 base=$3 package=$4
    shift 4
    # The base outside $root, so it does not also load $root's .cargo/config.toml.
    tmp=$(mktemp -d)
    src="$tmp/base" log="$tmp/log" changes="$tmp/changes"
    : >"$changes"
    git -C "$root" worktree prune
    git -C "$root" worktree add -q --detach "$src" "$base"
    trap 'git -C "$root" worktree remove --force "$src"; rm -rf "$tmp"' EXIT
    # Fresh and shared by both trees: the compare step sees both and no row from an earlier run.
    export CRITERION_HOME="$tmp/criterion"
    # bench <tree> <target dir> <criterion args>: output to $log, shown on failure; runs in <tree> for its config.
    bench() {
        tree=$1 dir=$2
        shift 2
        (cd "$tree" && CARGO_TARGET_DIR=$dir cargo bench -q --target aarch64-apple-darwin -p "$package" --bench '*' \
            -- "$@") >"$log" 2>&1 || { cat "$log" >&2; exit 1; }
    }
    # Not "base" or "new": criterion keeps its own last run under those names.
    i=0
    while [ "$i" -lt "$rounds" ]; do
        if [ $((i % 2)) -eq 0 ]; then
            bench "$src" "$root/target/bench-base" --save-baseline before "$@"
            bench "$root" "$root/target" --save-baseline after "$@"
        else
            bench "$root" "$root/target" --save-baseline after "$@"
            bench "$src" "$root/target/bench-base" --save-baseline before "$@"
        fi
        i=$((i + 1))
        echo "round $i, load $(sysctl -n vm.loadavg | cut -d' ' -f2):"
        bench "$root" "$root/target" --load-baseline after --baseline before "$@"
        grep -vE '^(Benchmarking|Found|  [0-9])|^$' "$log"
        # `<row> <tab> <change %>`; a long row name stands on its own line above `time:`, and with a throughput
        # `change:` stands alone above its `time:` estimates.
        sed 's/−/-/g' "$log" | awk '/^[^ ]/ { name = $0; sub(/ *time:.*/, "", name) }
            /change:/ && NF == 1 { pending = 1; next }
            /change:/ || (pending && /time:/) { gsub(/[][%]/, ""); print name "\t" $3; pending = 0 }' >>"$changes"
    done
    [ -s "$changes" ] || { echo "bench.sh: no criterion rows in $package" >&2; exit 1; }
    awk -F'\t' '
    {
        if (!(($1) in count)) names[++names_len] = $1
        values[$1, ++count[$1]] = $2
    }
    END {
        printf "%-48s %8s %8s %8s %6s\n", "row (after vs before)", "median", "min", "max", "rounds"
        for (k = 1; k <= names_len; k++) {
            key = names[k]; n = count[key]
            for (i = 1; i <= n; i++) a[i] = values[key, i]
            for (i = 2; i <= n; i++) { v = a[i]; for (j = i - 1; j > 0 && a[j] > v; j--) a[j + 1] = a[j]; a[j + 1] = v }
            printf "%-48s %+7.1f%% %+7.1f%% %+7.1f%% %6d\n", key, n % 2 ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2, \
                a[1], a[n], n
        }
    }' "$changes"
    exit
fi

test=$1 rounds=$2 new=${3:-} base=${4:-}
if [ -z "$new" ]; then
    cargo build -q --release --manifest-path "$root/Cargo.toml" -p qemu-virt
    new=$root/target/aarch64-unknown-none-softfloat/release/mog_os
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cargo run -q --manifest-path "$root/Cargo.toml" -p mogfs --example mkfs --target aarch64-apple-darwin -- \
    "$tmp/clean.img" 1024 >/dev/null

# boot <label> <kernel>: appends `<label> <tab> <name> <tab> <value>` per measured line to $tmp/results.
boot() {
    cp "$tmp/clean.img" "$tmp/disk.img"
    qemu-system-aarch64 -M virt,gic-version=3 -accel hvf -cpu cortex-a72 -m 128M -global virtio-mmio.force-legacy=false \
        -global virtio-mmio.ioeventfd=off -nographic -kernel "$2" \
        -drive file="$tmp/disk.img",if=none,format=raw,id=d0 -device virtio-blk-device,drive=d0 ${QEMU_ARGS:-} \
        -append "test=$test" </dev/null | tr -d '\r' >"$tmp/out"
    sed -n -e "s/^bench \(.*\): \([0-9.]*\) ns$/$1	\1	\2/p" \
        -e "s/^\([^:]*\): \([^ ]* \)\{0,1\}\([0-9.]*\) ns\/round-trip$/$1	\1 \2	\3/p" \
        -e "s/^boot: \([0-9]*\) us$/$1	boot (us)	\1/p" "$tmp/out" | sed 's/ 	/	/' >"$tmp/rows"
    # Every boot prints `boot:`, so a run with nothing else measured nothing.
    if grep -qE '^(panic|fault):' "$tmp/out" || ! grep -qv '	boot (us)	' "$tmp/rows"; then
        cat "$tmp/out" >&2
        echo "bench.sh: $2 failed" >&2
        exit 1
    fi
    cat "$tmp/rows" >>"$tmp/results"
}

i=0
while [ "$i" -lt "$rounds" ]; do
    if [ -z "$base" ]; then
        boot new "$new"
    elif [ $((i % 2)) -eq 0 ]; then
        boot base "$base"
        boot new "$new"
    else
        boot new "$new"
        boot base "$base"
    fi
    i=$((i + 1))
done

awk -F'\t' -v ab="${base:+1}" '
function stats(key,    n, i, j, v, a) {
    n = count[key]
    for (i = 1; i <= n; i++) a[i] = values[key, i]
    for (i = 2; i <= n; i++) { v = a[i]; for (j = i - 1; j > 0 && a[j] > v; j--) a[j + 1] = a[j]; a[j + 1] = v }
    median = n % 2 ? a[(n + 1) / 2] : (a[n / 2] + a[n / 2 + 1]) / 2
    min = a[1]
}
{
    if (!(($2) in seen)) { seen[$2] = 1; names[++names_len] = $2 }
    values[$1 SUBSEP $2, ++count[$1 SUBSEP $2]] = $3
}
END {
    if (!ab) {
        printf "%-16s %12s %12s %6s\n", "bench", "median ns", "min ns", "boots"
        for (k = 1; k <= names_len; k++) {
            stats("new" SUBSEP names[k])
            printf "%-16s %12.1f %12.1f %6d\n", names[k], median, min, count["new" SUBSEP names[k]]
        }
        exit
    }
    printf "%-16s %12s %12s %8s %12s %12s %8s\n", "bench", "base median", "new median", "delta", "base min", "new min", "delta"
    for (k = 1; k <= names_len; k++) {
        if (!count["base" SUBSEP names[k]] || !count["new" SUBSEP names[k]]) {
            printf "%-16s  only in the %s kernel\n", names[k], count["new" SUBSEP names[k]] ? "new" : "base"
            continue
        }
        stats("base" SUBSEP names[k]); bm = median; bn = min
        stats("new" SUBSEP names[k]); nm = median; nn = min
        printf "%-16s %12.1f %12.1f %+7.1f%% %12.1f %12.1f %+7.1f%%%s\n", names[k], bm, nm, (nm - bm) * 100 / bm, \
            bn, nn, (nn - bn) * 100 / bn, (nm > bm && nn > bn) ? "  SLOWER" : ""
    }
}' "$tmp/results"
