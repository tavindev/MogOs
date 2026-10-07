/* Portable cross-OS benchmarks for scripts/oscompare.sh (docs/BENCHMARKS.md, "Cross-OS comparison"), the same source
 * on MogOs, Linux and macOS. Prints `oscb: <name> <value> <unit>` lines, or `oscb: error <name>`; times with
 * CNTVCT_EL0 read from user space (macOS: see ticks). No stdio: a static Linux musl lacks printf's long-double helpers.
 *   oscb <bench> <dir> <nop>   one of syscalls yield pipe spawn files readdir fileio; files in <dir>, <nop> the
 *                              spawn target (oscnop)
 *   oscb raw <device>          sequential raw-disk throughput, bypassing the page cache */
#include <dirent.h>
#include <fcntl.h>
#include <sched.h>
#include <spawn.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define TRIPS 100000
#define SPAWNS 1000
#define SYNCS 1000
#define SCANS 100
#define ENTRIES 1000
#define BLOCK 4096
#define BIG (256 * 1024)
#define DISK_BYTES (8 << 20)

static char buf[BIG] __attribute__((aligned(4096)));
static char *const no_env[] = {0};

#ifdef __APPLE__
#include <mach/mach_time.h>
/* macOS returns garbage for an EL0 `mrs cntvct_el0`; mach_absolute_time reads the same 24 MHz counter. */
static uint64_t ticks(void) { return mach_absolute_time(); }

static double seconds(uint64_t since)
{
	mach_timebase_info_data_t tb;
	mach_timebase_info(&tb);
	return (double)(ticks() - since) * tb.numer / tb.denom / 1e9;
}
#else
static uint64_t ticks(void)
{
	uint64_t t;
	__asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(t) : : "memory");
	return t;
}

static double seconds(uint64_t since)
{
	uint64_t freq;
	__asm__ volatile("mrs %0, cntfrq_el0" : "=r"(freq));
	return (double)(ticks() - since) / (double)freq;
}
#endif

static void put(const char *s) { write(1, s, strlen(s)); }

static void put_u64(uint64_t n)
{
	char d[20];
	int i = sizeof d;
	do d[--i] = '0' + n % 10; while ((n /= 10));
	write(1, d + i, sizeof d - i);
}

/* `value` with one decimal. */
static void report(const char *name, double value, const char *unit)
{
	uint64_t tenths = (uint64_t)(value * 10 + 0.5);
	put("oscb: ");
	put(name);
	put(" ");
	put_u64(tenths / 10);
	put(".");
	put_u64(tenths % 10);
	put(" ");
	put(unit);
	put("\n");
}

static void per_op(const char *name, uint64_t since, int ops) { report(name, seconds(since) * 1e9 / ops, "ns"); }

static void mib_s(const char *name, uint64_t since) { report(name, DISK_BYTES / 1048576.0 / seconds(since), "MiB/s"); }

static void fail(const char *name)
{
	put("oscb: error ");
	put(name);
	put("\n");
	_exit(1);
}

static void cloexec_pipe(int p[2])
{
	if (pipe(p) || fcntl(p[0], F_SETFD, FD_CLOEXEC) || fcntl(p[1], F_SETFD, FD_CLOEXEC)) fail("pipe");
}

/* Spawns `self <mode>` with `in` as its stdin and `out` as its stdout. */
static pid_t spawn_partner(const char *self, char *mode, int in, int out)
{
	posix_spawn_file_actions_t fa;
	char *argv[] = {(char *)self, mode, 0};
	pid_t pid;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_adddup2(&fa, in, 0);
	posix_spawn_file_actions_adddup2(&fa, out, 1);
	if (posix_spawn(&pid, self, &fa, 0, argv, no_env)) fail(mode);
	posix_spawn_file_actions_destroy(&fa);
	return pid;
}

static void reap(pid_t pid, const char *name)
{
	int status;
	if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status)) fail(name);
}

static void syscalls(void)
{
	uint64_t t = ticks();
	for (int i = 0; i < TRIPS; i++) getppid();
	per_op("getppid", t, TRIPS);
	t = ticks();
	for (int i = 0; i < TRIPS; i++)
		if (write(1, buf, 0)) fail("write0");
	per_op("write0", t, TRIPS);
}

static void yields(const char *self)
{
	int ready[2];
	if (sched_yield()) fail("yield");
	cloexec_pipe(ready);
	pid_t pid = spawn_partner(self, "yielder", 0, ready[1]);
	close(ready[1]);
	if (read(ready[0], buf, 1) != 1) fail("yield");
	uint64_t t = ticks();
	for (int i = 0; i < TRIPS; i++) sched_yield();
	per_op("yield", t, TRIPS);
	reap(pid, "yield");
}

static void pipes(const char *self)
{
	int to[2], from[2];
	cloexec_pipe(to);
	cloexec_pipe(from);
	pid_t pid = spawn_partner(self, "pong", to[0], from[1]);
	close(to[0]);
	close(from[1]);
	uint64_t t = ticks();
	for (int i = 0; i < TRIPS; i++)
		if (write(to[1], buf, 1) != 1 || read(from[0], buf, 1) != 1) fail("pipe");
	per_op("pipe", t, TRIPS);
	close(to[1]);
	reap(pid, "pipe");
}

static void spawns(const char *nop)
{
	char *argv[] = {(char *)nop, 0};
	uint64_t t = ticks();
	for (int i = 0; i < SPAWNS; i++) {
		pid_t pid;
		if (posix_spawn(&pid, nop, 0, 0, argv, no_env)) fail("spawn");
		reap(pid, "spawn");
	}
	per_op("spawn", t, SPAWNS);
}

static void files(void)
{
	uint64_t t = ticks();
	for (int i = 0; i < SYNCS; i++) {
		int fd = open("f", O_WRONLY | O_CREAT | O_TRUNC, 0644);
		if (fd < 0 || write(fd, buf, 100) != 100 || fsync(fd)) fail("create+write+fsync");
		close(fd);
	}
	per_op("create+write+fsync", t, SYNCS);
	t = ticks();
	for (int i = 0; i < TRIPS; i++) {
		int fd = open("f", O_RDONLY);
		if (fd < 0) fail("open+close");
		close(fd);
	}
	per_op("open+close", t, TRIPS);
}

static void readdirs(void)
{
	char name[] = "d/0000";
	if (mkdir("d", 0755)) fail("readdir1000");
	for (int i = 0; i < ENTRIES; i++) {
		for (int j = 5, n = i; j > 1; j--, n /= 10) name[j] = '0' + n % 10;
		int fd = open(name, O_WRONLY | O_CREAT, 0644);
		if (fd < 0) fail("readdir1000");
		close(fd);
	}
	uint64_t t = ticks();
	for (int i = 0; i < SCANS; i++) {
		DIR *d = opendir("d");
		int n = 0;
		if (!d) fail("readdir1000");
		while (readdir(d)) n++;
		closedir(d);
		if (n < ENTRIES) fail("readdir1000");
	}
	per_op("readdir1000", t, SCANS);
}

/* Sequential 8 MiB through a file: written then fsynced, then read back warm (whatever the OS caches). */
static void file_io(void)
{
	int fd = open("seq", O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (fd < 0) fail("file-write+fsync-256k");
	uint64_t t = ticks();
	for (int n = 0; n < DISK_BYTES; n += BIG)
		if (write(fd, buf, BIG) != BIG) fail("file-write+fsync-256k");
	if (fsync(fd)) fail("file-write+fsync-256k");
	mib_s("file-write+fsync-256k", t);
	close(fd);
	fd = open("seq", O_RDONLY);
	t = ticks();
	for (int n = 0; n < DISK_BYTES; n += BIG)
		if (read(fd, buf, BIG) != BIG) fail("file-read-256k");
	mib_s("file-read-256k", t);
	close(fd);
}

/* As MogOs's `test=bench-disk`: 8 MiB written then flushed, then read, one request in flight, 4 KiB then 256 KiB. */
static void raw(const char *device)
{
#ifdef O_DIRECT
	int fd = open(device, O_RDWR | O_DIRECT);
	if (fd < 0) fail("raw");
	for (int size = BLOCK; size <= BIG; size *= BIG / BLOCK) {
		uint64_t t = ticks();
		for (off_t n = 0; n < DISK_BYTES; n += size)
			if (pwrite(fd, buf, size, n) != size) fail("raw-write");
		if (fsync(fd)) fail("raw-write");
		mib_s(size == BLOCK ? "raw-write+flush-4k" : "raw-write+flush-256k", t);
		t = ticks();
		for (off_t n = 0; n < DISK_BYTES; n += size)
			if (pread(fd, buf, size, n) != size) fail("raw-read");
		mib_s(size == BLOCK ? "raw-read-4k" : "raw-read-256k", t);
	}
#else
	(void)device;
	fail("raw");
#endif
}

int main(int argc, char **argv)
{
	const char *bench = argc > 1 ? argv[1] : "";
	memset(buf, 0x5a, sizeof buf);
	if (argc == 2 && !strcmp(bench, "pong")) {
		while (read(0, buf, 1) == 1) write(1, buf, 1);
		return 0;
	}
	if (argc == 2 && !strcmp(bench, "yielder")) {
		write(1, buf, 1);
		for (int i = 0; i < TRIPS; i++) sched_yield();
		return 0;
	}
	if (argc == 3 && !strcmp(bench, "raw")) raw(argv[2]);
	else if (argc != 4 || chdir(argv[2])) fail("usage: oscb <bench> <dir> <nop> | oscb raw <device>");
	else if (!strcmp(bench, "syscalls")) syscalls();
	else if (!strcmp(bench, "yield")) yields(argv[0]);
	else if (!strcmp(bench, "pipe")) pipes(argv[0]);
	else if (!strcmp(bench, "spawn")) spawns(argv[3]);
	else if (!strcmp(bench, "files")) files();
	else if (!strcmp(bench, "readdir")) readdirs();
	else if (!strcmp(bench, "fileio")) file_io();
	else fail(bench);
	return 0;
}
