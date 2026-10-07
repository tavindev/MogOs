/* `test=sockets`: a TCP echo over 127.0.0.1 on musl's BSD sockets. `tcpecho s` serves one connection on port 8,
 * echoing what one read gets; `tcpecho c` connects (retrying while refused: the server may not listen yet), sends
 * "hello" and prints the echo. */
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(8), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
	char buf[64];
	int one = 1, s = socket(AF_INET, SOCK_STREAM, 0);
	if (s < 0) return 1;
	if (argc > 1 && argv[1][0] == 's') {
		if (setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) || bind(s, (void *)&a, sizeof a) ||
		    listen(s, 1))
			return 2;
		int c = accept(s, 0, 0);
		ssize_t n = c < 0 ? -1 : read(c, buf, sizeof buf);
		if (n <= 0 || write(c, buf, n) != n) return 3;
		close(c);
		close(s);
		printf("tcpecho: served %zd bytes\n", n);
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
