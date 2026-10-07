#!/bin/sh
# The Linux guest's init (rdinit=/bench/init, over Alpine's initramfs): loads the virtio-blk and ext4 modules, runs
# the raw-disk benchmark on the benchmark disk, formats it ext4, runs every other benchmark on it, powers off.
/bin/busybox --install -s
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
modprobe -a virtio_mmio virtio_blk squashfs
# ext4 is in Alpine's modloop (squashfs), attached read-only as a second disk; modprobe -d expects <dir>/lib/modules.
mkdir -p /.modloop /tmp/ml/lib /mnt
for disk in vda vdb; do mount -t squashfs -o ro /dev/$disk /.modloop 2>/dev/null && break; done
[ $disk = vda ] && disk=vdb || disk=vda
ln -s /.modloop/modules /tmp/ml/lib/modules
modprobe -d /tmp/ml ext4
umount /.modloop
# Frees the RAM the initramfs's modules hold before anything is measured.
rm -rf /usr/lib/modules
echo "oscb-meta: kernel $(uname -r)"
echo "oscb-meta: cmdline $(cat /proc/cmdline)"
for f in /sys/devices/system/cpu/vulnerabilities/*; do echo "oscb-meta: vuln ${f##*/}: $(cat "$f")"; done
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo | sed 's/^/oscb-meta: /'
/bench/oscb raw /dev/$disk
mke2fs -q -F -t ext4 -b 4096 -E lazy_itable_init=0,lazy_journal_init=0 /dev/$disk
mount -t ext4 /dev/$disk /mnt
echo "oscb-meta: mount $(grep ' /mnt ' /proc/mounts)"
for b in syscalls yield pipe spawn files readdir fileio; do /bench/oscb $b /mnt /bench/oscnop; done
umount /mnt
poweroff -f
