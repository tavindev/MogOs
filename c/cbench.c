/* Times a syscall round trip through musl (a 0-byte write, the native `test=bench-syscall` call) and a
 * posix_spawn + waitpid of busybox `true`. */
#include <spawn.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
#include <sys/wait.h>

#define SYSCALLS 100000
#define SPAWNS 100

static long long now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

int main(void)
{
	long long start = now();
	for (int i = 0; i < SYSCALLS; i++) write(1, "", 0);
	printf("musl syscall: %lld ns/round-trip\n", (now() - start) / SYSCALLS);

	char *argv[] = { "true", 0 };
	start = now();
	for (int i = 0; i < SPAWNS; i++) {
		pid_t pid;
		int status;
		if (posix_spawn(&pid, "/bin/sh", 0, 0, argv, 0) || waitpid(pid, &status, 0) != pid || status) return 1;
	}
	printf("busybox spawn: %lld ns/round-trip\n", (now() - start) / SPAWNS);
	return 0;
}
