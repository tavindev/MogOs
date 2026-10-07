/* MogOs: vfork's child mode applies the file actions to its copy of the fd table and execve spawns natively; a
 * failure before or at exec is reported through `ec`, which the child shares with the parent. Attributes other
 * than the exec function (signals, process groups, ids) do not apply: there are none to set. */
#define _GNU_SOURCE
#include <spawn.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/wait.h>
#include "fdop.h"

int posix_spawn(pid_t *restrict res, const char *restrict path,
	const posix_spawn_file_actions_t *fa,
	const posix_spawnattr_t *restrict attr,
	char *const argv[restrict], char *const envp[restrict])
{
	int (*exec)(const char *, char *const *, char *const *) =
		attr && attr->__fn ? (int (*)())attr->__fn : execve;
	volatile int ec = 0;
	pid_t pid = vfork();
	if (!pid) {
		struct fdop *op = fa ? fa->__actions : 0;
		int fd;
		while (op && op->next) op = op->next;
		for (; op && !ec; op = op->prev) {
			switch (op->cmd) {
			case FDOP_CLOSE:
				close(op->fd);
				break;
			case FDOP_DUP2:
				if (dup2(op->srcfd, op->fd) < 0) ec = errno;
				break;
			case FDOP_OPEN:
				if ((fd = open(op->path, op->oflag, op->mode)) < 0) ec = errno;
				else if (fd != op->fd && (dup2(fd, op->fd) < 0 || close(fd))) ec = errno;
				break;
			case FDOP_CHDIR:
				if (chdir(op->path)) ec = errno;
				break;
			case FDOP_FCHDIR:
				if (fchdir(op->fd)) ec = errno;
				break;
			}
		}
		if (!ec) exec(path, argv, envp);
		if (!ec) ec = errno;
		_exit(127);
	}
	if (pid < 0) return errno;
	if (ec) {
		waitpid(pid, &(int){0}, 0);
		return ec;
	}
	if (res) *res = pid;
	return 0;
}
