/* `test=sockets`: a TCP echo over 127.0.0.1 on musl's BSD sockets. `tcpecho s` serves one connection on port 8,
 * echoing what one read gets; `tcpecho c` connects (retrying while refused: the server may not listen yet), sends
 * "hello" and prints the echo. `tcpecho p` spawns `tcpecho n`, which prints what `socket` gets: a child inherits no
 * network. */
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(8), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
	char buf[64];
	if (argc > 1 && argv[1][0] == 'p') {
		char *args[] = { argv[0], "n", 0 };
		pid_t pid;
		int status;
		return posix_spawn(&pid, argv[0], 0, 0, args, 0) || waitpid(pid, &status, 0) != pid || status;
	}
	int one = 1, s = socket(AF_INET, SOCK_STREAM, 0);
	if (argc > 1 && argv[1][0] == 'n') {
		printf("tcpecho: child socket: %s\n", s < 0 && errno == EBADF ? "EBADF" : "not EBADF");
		return 0;
	}
	if (s < 0) return 1;
	if (argc > 1 && argv[1][0] == 's') {
		if (setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) || bind(s, (void *)&a, sizeof a) ||
		    listen(s, 1))
			return 2;
		struct sockaddr_in peer;
		socklen_t len = sizeof peer;
		int c = accept(s, (void *)&peer, &len);
		ssize_t n = c < 0 ? -1 : read(c, buf, sizeof buf);
		if (n <= 0 || write(c, buf, n) != n) return 3;
		close(c);
		close(s);
		printf("tcpecho: served %zd bytes to %s, port %s\n", n, inet_ntoa(peer.sin_addr),
		       len == sizeof peer && peer.sin_port ? "set" : "missing");
		return 0;
	}
	for (int tries = 0; connect(s, (void *)&a, sizeof a); tries++) {
		if (errno != ECONNREFUSED || tries == 100) return 4;
		close(s);
		if ((s = socket(AF_INET, SOCK_STREAM, 0)) < 0) return 5;
	}
	if (write(s, "hello", 5) != 5) return 6;
	ssize_t n = read(s, buf, sizeof buf);
	if (n <= 0) return 7;
	printf("tcpecho: %.*s\n", (int)n, buf);
	return 0;
}
