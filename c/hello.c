/* A C program on MogOs' musl: prints through stdio, gets ENOSYS for an unmapped syscall and for fork, exits 42. */
#include <errno.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>

int main(int argc, char **argv)
{
	printf("hello from musl: %s, %d args\n", argv[0], argc);
	long r = syscall(999);
	printf("syscall 999: %s\n", r == -1 && errno == ENOSYS ? "ENOSYS" : "unexpected");
	printf("fork: %s\n", fork() == -1 && errno == ENOSYS ? "ENOSYS" : "unexpected");
	return 42;
}
