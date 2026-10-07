/* MogOs: the Linux syscalls musl makes, mapped onto the native calls (crates/kernel/src/syscall.rs); anything
 * unmapped is -ENOSYS. A C program starts with fixed handles: 0-2 stdin, stdout, stderr, 3 the root directory,
 * 4 the boot archive (programs to spawn), 5 the NetStack (TCP over IPv4 only); an absent one fails with EBADF or
 * EACCES on use. libc keeps the fd table (fd -> open file: handle, offset, path), the current directory (a path
 * below the root) and the pid table. */
#define _GNU_SOURCE
#include <dirent.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <netinet/in.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include "syscall.h"
#include "kstat.h"
#include "ksigaction.h"

enum { N_EXIT, N_IO, N_DUP, N_CLOSE, N_MAP, N_OPEN, N_SPAWN, N_PIPE, N_WAIT, N_KILL = 12, N_MKDIR, N_READDIR,
	N_SYNC, N_UNLINK, N_RENAME, N_SOCKET = 20, N_BIND, N_LISTEN, N_SUBMIT, N_IO_WAIT, N_SHUTDOWN };
enum { R_READ = 1, R_WRITE = 2, R_DUP = 8, R_TRANSFER = 16, R_EXEC = 32 };
enum { N_CREATE = 1, N_TRUNC = 2 };
enum { OP_RECEIVE, OP_SEND, OP_ACCEPT, OP_CONNECT };
enum { H_ROOT = 3, H_ARCHIVE = 4, H_NET = 5 };
/* Native limits: largest map, largest spawn argument buffer and count, a killed process's exit code. */
enum { MAX_MAP = 16 << 12, MAX_ARGS = 32, MAX_ARG_BYTES = 4096, KILLED = 256 };
/* Frames a spawned C program gets, halved while the parent's budget is short, down to what busybox needs to run
 * (its image, 128 KiB stack, tables and some heap). */
#define CHILD_BUDGET 1024
#define MIN_BUDGET 128
#define STACK (32 << 12)
#define PATH 256
#define FILES 32
#define FDS 32
#define CHILDREN 8

enum kind { FREE, TTY, NODE, DIRECTORY, PIPE, SOCKET };

struct file {
	long handle;
	long long off;
	int kind, refs, append;
	char path[PATH];
};

/* Everything a vfork child may change and its execve or _exit undoes. */
static struct state {
	struct file files[FILES];
	signed char fds[FDS];
	unsigned cloexec;
	char cwd[PATH];
} st, saved;

static struct kid {
	int pid, status, order;
	long handle; /* -1 once exited */
} kids[CHILDREN];
static int next_pid = 2, spawned;
/* Set in a vfork child; vfork.s reads it to refuse a nested vfork. */
int __mog_vfork_child;
#define child __mog_vfork_child
long __mog_vfork_jb[13];
_Noreturn void __mog_vfork_resume(long);

static long svc(long nr, long a, long b, long c, long d, long e, long f, long g)
{
	register long x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c, x3 __asm__("x3") = d;
	register long x4 __asm__("x4") = e, x5 __asm__("x5") = f, x6 __asm__("x6") = g, x8 __asm__("x8") = nr;
	__asm__ __volatile__("svc 0" : "+r"(x0), "+r"(x1) : "r"(x2), "r"(x3), "r"(x4), "r"(x5), "r"(x6), "r"(x8)
		: "memory");
	return x0;
}
#define svc1(n, a) svc(n, a, 0, 0, 0, 0, 0, 0)
#define nclose(h) svc1(N_CLOSE, h)

/* A duplicate with as many of the rights a child can use as `h` has (dup only narrows, and libc does not know). */
static long ndup(long h)
{
	static const long rights[] = { R_READ | R_WRITE | R_DUP | R_TRANSFER, R_READ | R_EXEC | R_DUP | R_TRANSFER,
		R_WRITE | R_DUP | R_TRANSFER, R_READ | R_DUP | R_TRANSFER, R_TRANSFER };
	long r = -EBADF;
	for (int i = 0; i < 5 && r < 0; i++) r = svc(N_DUP, h, rights[i], 0, 0, 0, 0, 0);
	return r;
}

/* `p` below the root ("" is the root itself). */
static long nopen(const char *p, int flags)
{
	if (!*p) return flags ? -EISDIR : ndup(H_ROOT);
	return svc(N_OPEN, H_ROOT, (long)p, strlen(p), flags, 0, 0, 0);
}

/* A readdir of nothing: a file is ENOTDIR, a directory 0 or EINVAL (an entry that does not fit). */
static int isdir(long h)
{
	long r = svc(N_READDIR, h, 0, 0, 0, 0, 0, 0);
	return r >= 0 || r == -EINVAL;
}

/* No stat call: the size is the first offset a one-byte read returns nothing at (files are under 64 KiB). */
static long long size_of(long h)
{
	char c;
	long long lo = 0, hi = 1 << 16;
	while (lo < hi) {
		long long mid = (lo + hi) / 2;
		long r = svc(N_IO, h, 0, (long)&c, 1, mid, 0, 0);
		if (r < 0) return r;
		if (r) lo = mid + 1;
		else hi = mid;
	}
	return lo;
}

static struct file *fd_file(int fd)
{
	return (unsigned)fd < FDS && st.fds[fd] >= 0 ? &st.files[st.fds[fd]] : 0;
}

static int inherited(int i)
{
	return child && saved.files[i].refs && saved.files[i].handle == st.files[i].handle;
}

static void unref(int fd)
{
	int i = st.fds[fd];
	st.fds[fd] = -1;
	st.cloexec &= ~(1u << fd);
	if (--st.files[i].refs) return;
	if (!inherited(i)) nclose(st.files[i].handle);
	st.files[i].kind = FREE;
}

static int install(int fd, int i, int cloexec)
{
	if (st.fds[fd] >= 0) unref(fd);
	st.fds[fd] = i;
	st.files[i].refs++;
	if (cloexec) st.cloexec |= 1u << fd;
	return fd;
}

static int free_fd(int min)
{
	for (int fd = min; fd < FDS; fd++)
		if (st.fds[fd] < 0) return fd;
	return -EMFILE;
}

static int new_file(long h, int kind, const char *path)
{
	for (int i = 0; i < FILES; i++)
		if (st.files[i].kind == FREE) {
			st.files[i] = (struct file){ .handle = h, .kind = kind };
			strcpy(st.files[i].path, path);
			return i;
		}
	return -ENFILE;
}

/* `path` relative to `dirfd` as a path below the root in `out`: `.`, `..` (stopping at the root) and repeated `/`
 * resolved here, since the kernel takes names only. */
static int resolve(int dirfd, const char *path, char *out)
{
	const char *base = "";
	size_t len = 0;
	if (!*path) return -ENOENT;
	if (*path != '/') {
		struct file *f = dirfd == AT_FDCWD ? 0 : fd_file(dirfd);
		if (dirfd == AT_FDCWD) base = st.cwd;
		else if (!f) return -EBADF;
		else if (f->kind != DIRECTORY) return -ENOTDIR;
		else base = f->path;
	}
	for (int part = 0; part < 2; part++) {
		for (const char *s = part ? path : base; *s;) {
			const char *e = strchrnul(s, '/');
			size_t n = e - s;
			if (n == 2 && s[0] == '.' && s[1] == '.') {
				while (len && out[len - 1] != '/') len--;
				if (len) len--;
			} else if (n && !(n == 1 && *s == '.')) {
				if (len + 1 + n >= PATH) return -ENAMETOOLONG;
				if (len) out[len++] = '/';
				memcpy(out + len, s, n);
				len += n;
			}
			s = *e ? e + 1 : e;
		}
	}
	out[len] = 0;
	return 0;
}

static long do_openat(int dirfd, const char *path, int flags)
{
	char p[PATH];
	int r = resolve(dirfd, path, p), fd = free_fd(0), i;
	if (r) return r;
	if (fd < 0) return fd;
	if ((flags & (O_CREAT | O_EXCL)) == (O_CREAT | O_EXCL)) {
		long h = nopen(p, 0);
		if (h >= 0) return nclose(h), -EEXIST;
	}
	long h = nopen(p, (flags & O_CREAT ? N_CREATE : 0) | (flags & O_TRUNC ? N_TRUNC : 0));
	if (h < 0) return h;
	int kind = isdir(h) ? DIRECTORY : NODE;
	if (kind != DIRECTORY && flags & O_DIRECTORY) r = -ENOTDIR;
	if (kind == DIRECTORY && (flags & O_ACCMODE) != O_RDONLY) r = -EISDIR;
	if (!r && (i = new_file(h, kind, p)) < 0) r = i;
	if (r) return nclose(h), r;
	st.files[i].append = !!(flags & O_APPEND);
	return install(fd, i, flags & O_CLOEXEC);
}

/* A socket op, waited for at once: with one thread, it is the only one in flight. */
static long sock_op(long h, long op, long ptr, long len)
{
	long r = svc(N_SUBMIT, h, op, ptr, len, 0, 0, 0);
	return r < 0 ? r : svc(N_IO_WAIT, 0, 0, 0, 0, 0, 0, 0);
}

static struct file *socket_file(int fd)
{
	struct file *f = fd_file(fd);
	return f && f->kind == SOCKET ? f : 0;
}

static long do_socket(int domain, int type, int protocol)
{
	if (domain != AF_INET) return -EAFNOSUPPORT;
	if ((type & 0xff) != SOCK_STREAM || protocol && protocol != IPPROTO_TCP) return -EPROTONOSUPPORT;
	int fd = free_fd(0), i;
	if (fd < 0) return fd;
	long h = svc1(N_SOCKET, H_NET);
	if (h < 0) return h;
	if ((i = new_file(h, SOCKET, "")) < 0) return nclose(h), i;
	return install(fd, i, type & SOCK_CLOEXEC);
}

/* The port of an AF_INET address. */
static long port_of(const struct sockaddr_in *a, socklen_t len)
{
	if (len < sizeof *a || a->sin_family != AF_INET) return -EINVAL;
	return ntohs(a->sin_port);
}

static long do_accept(int fd, struct sockaddr_in *addr, socklen_t *len, int flags)
{
	struct file *f = socket_file(fd);
	int new = free_fd(0), i;
	if (!f) return fd_file(fd) ? -ENOTSOCK : -EBADF;
	if (new < 0) return new;
	long h = sock_op(f->handle, OP_ACCEPT, 0, 0);
	if (h < 0) return h;
	if ((i = new_file(h, SOCKET, "")) < 0) return nclose(h), i;
	/* The kernel does not report the peer's address. */
	if (addr && len && *len >= sizeof *addr) *addr = (struct sockaddr_in){ .sin_family = AF_INET };
	return install(new, i, flags & SOCK_CLOEXEC);
}

static long do_connect(int fd, const struct sockaddr_in *a, socklen_t len)
{
	struct file *f = socket_file(fd);
	long port = port_of(a, len);
	if (!f) return fd_file(fd) ? -ENOTSOCK : -EBADF;
	return port < 0 ? port : sock_op(f->handle, OP_CONNECT, ntohl(a->sin_addr.s_addr), port);
}

static long rw(int fd, char *buf, size_t n, int write)
{
	struct file *f = fd_file(fd);
	if (!f) return -EBADF;
	if (f->kind == DIRECTORY) return -EISDIR;
	if (f->kind == SOCKET) return sock_op(f->handle, write ? OP_SEND : OP_RECEIVE, (long)buf, n);
	if (f->kind != NODE) return svc(N_IO, f->handle, write, (long)buf, n, 0, 0, 0);
	if (write && f->append && (f->off = size_of(f->handle)) < 0) return f->off;
	long r = svc(N_IO, f->handle, write, (long)buf, n, f->off, 0, 0);
	if (r > 0) f->off += r;
	return r;
}

static long rwv(int fd, const struct iovec *iov, int count, int write)
{
	long total = 0;
	for (int i = 0; i < count; i++) {
		long r = rw(fd, iov[i].iov_base, iov[i].iov_len, write);
		if (r < 0) return total ? total : r;
		total += r;
		if ((size_t)r < iov[i].iov_len || !write && fd_file(fd)->kind != NODE) break;
	}
	return total;
}

static long do_lseek(int fd, long long off, int whence)
{
	struct file *f = fd_file(fd);
	if (!f) return -EBADF;
	if (f->kind != NODE && f->kind != DIRECTORY) return -ESPIPE;
	long long base = whence == SEEK_SET ? 0 : whence == SEEK_CUR ? f->off : whence == SEEK_END ? size_of(f->handle) : -1;
	if (whence == SEEK_END && base < 0) return base;
	if (base < 0 || base + off < 0) return -EINVAL;
	return f->off = base + off;
}

/* The native `name\n` (`name/\n` for a directory) entries as `struct dirent`s; the offset counts entries. */
static long do_getdents(int fd, char *buf, size_t len)
{
	struct file *f = fd_file(fd);
	char names[1024];
	if (!f) return -EBADF;
	if (f->kind != DIRECTORY) return -ENOTDIR;
	long n = svc(N_READDIR, f->handle, (long)names, sizeof names, f->off, 0, 0, 0);
	if (n < 0) return n;
	size_t pos = 0;
	long count = 0;
	for (char *s = names, *nl; s < names + n; s = nl + 1) {
		nl = memchr(s, '\n', names + n - s);
		size_t namelen = nl - s, dir = namelen && s[namelen - 1] == '/';
		namelen -= dir;
		size_t rec = (offsetof(struct dirent, d_name) + namelen + 8) & ~7ul;
		if (pos + rec > len) break;
		struct dirent *d = (void *)(buf + pos);
		d->d_ino = d->d_off = f->off + ++count;
		d->d_reclen = rec;
		d->d_type = dir ? DT_DIR : DT_REG;
		memcpy(d->d_name, s, namelen);
		d->d_name[namelen] = 0;
		pos += rec;
	}
	if (n && !count) return -EINVAL;
	f->off += count;
	return pos;
}

static unsigned long hash(const char *s)
{
	unsigned long h = 14695981039346656037ul;
	while (*s) h = (h ^ (unsigned char)*s++) * 1099511628211ul;
	return h | 1;
}

/* Kind from the open file or a probe; size from probes; the path's hash as inode (MogFS has no hard links). */
static long do_fstatat(int dirfd, const char *path, struct kstat *k, int flag)
{
	char p[PATH];
	struct file *f = 0;
	long h;
	int kind;
	if (!*path && flag & AT_EMPTY_PATH) {
		if (!(f = fd_file(dirfd))) return -EBADF;
		h = f->handle, kind = f->kind;
		strcpy(p, f->path);
	} else {
		int r = resolve(dirfd, path, p);
		if (r) return r;
		if ((h = nopen(p, 0)) < 0) return h;
		kind = isdir(h) ? DIRECTORY : NODE;
	}
	*k = (struct kstat){ .st_dev = 1, .st_nlink = 1, .st_blksize = 4096 };
	k->st_ino = hash(p) + (kind == TTY || kind == PIPE ? dirfd : 0);
	k->st_mode = kind == DIRECTORY ? S_IFDIR | 0755 : kind == NODE ? S_IFREG | 0644 : kind == TTY ? S_IFCHR | 0620 :
		kind == SOCKET ? S_IFSOCK | 0777 : S_IFIFO | 0600;
	if (kind == NODE) k->st_size = size_of(h);
	if (!f) nclose(h);
	if (k->st_size < 0) return k->st_size;
	k->st_blocks = (k->st_size + 511) / 512;
	return 0;
}

static long do_unlinkat(int dirfd, const char *path, int flag)
{
	char p[PATH];
	int r = resolve(dirfd, path, p);
	if (r) return r;
	long h = nopen(p, 0);
	if (h < 0) return h;
	int dir = isdir(h);
	nclose(h);
	if (!*p) return -EBUSY;
	if (dir != !!(flag & AT_REMOVEDIR)) return dir ? -EISDIR : -ENOTDIR;
	return svc(N_UNLINK, H_ROOT, (long)p, strlen(p), 0, 0, 0, 0);
}

/* The kernel never replaces a name; POSIX replaces a file, so remove it first (not atomic; a directory target
 * stays EEXIST, a move below itself fails before anything is removed). */
static long do_renameat(int olddir, const char *old, int newdir, const char *new)
{
	char a[PATH], b[PATH];
	int r = resolve(olddir, old, a);
	if (r || (r = resolve(newdir, new, b))) return r;
	long rn = svc(N_RENAME, H_ROOT, (long)a, strlen(a), H_ROOT, (long)b, strlen(b), 0);
	if (rn != -EEXIST || !strcmp(a, b) || !strncmp(b, a, strlen(a)) && b[strlen(a)] == '/') return rn;
	long h = nopen(b, 0);
	if (h < 0) return h;
	int dir = isdir(h);
	nclose(h);
	if (dir) return -EEXIST;
	if ((rn = svc(N_UNLINK, H_ROOT, (long)b, strlen(b), 0, 0, 0, 0))) return rn;
	return svc(N_RENAME, H_ROOT, (long)a, strlen(a), H_ROOT, (long)b, strlen(b), 0);
}

static long do_dup3(int old, int new, int flags, int min)
{
	if (!fd_file(old)) return -EBADF;
	if (min < 0) return -EINVAL;
	if (new == -1 && (new = free_fd(min)) < 0) return new;
	if ((unsigned)new >= FDS) return -EBADF;
	if (new == old) return -EINVAL;
	return install(new, st.fds[old], flags & O_CLOEXEC);
}

static long do_fcntl(int fd, int cmd, long arg)
{
	struct file *f = fd_file(fd);
	if (!f) return -EBADF;
	switch (cmd) {
	case F_DUPFD: return do_dup3(fd, -1, 0, arg);
	case F_DUPFD_CLOEXEC: return do_dup3(fd, -1, O_CLOEXEC, arg);
	case F_GETFD: return st.cloexec >> fd & 1;
	case F_SETFD:
		st.cloexec = (st.cloexec & ~(1u << fd)) | (arg & FD_CLOEXEC) << fd;
		return 0;
	case F_GETFL: return O_RDWR | (f->append ? O_APPEND : 0);
	case F_SETFL:
		f->append = !!(arg & O_APPEND);
		return 0;
	}
	return -EINVAL;
}

static long do_chdir(int fd, const char *path)
{
	char p[PATH];
	int r = path ? resolve(AT_FDCWD, path, p) : 0;
	if (r) return r;
	if (!path) {
		struct file *f = fd_file(fd);
		if (!f) return -EBADF;
		if (f->kind != DIRECTORY) return -ENOTDIR;
		strcpy(p, f->path);
	} else {
		long h = nopen(p, 0);
		if (h < 0) return h;
		int dir = isdir(h);
		nclose(h);
		if (!dir) return -ENOTDIR;
	}
	strcpy(st.cwd, p);
	return 0;
}

/* Anonymous memory from `map`, 64 KiB per call; the kernel places maps one after another, so they join up. */
static long do_mmap(long addr, size_t len, int flags)
{
	if (addr || flags & MAP_FIXED) return -ENOMEM;
	if (!(flags & MAP_ANONYMOUS)) return -ENODEV;
	if (!len) return -EINVAL;
	long start = 0, next = 0;
	for (len = (len + 4095) & ~4095ul; len;) {
		size_t n = len < MAX_MAP ? len : MAX_MAP;
		long a = svc1(N_MAP, n);
		if (a < 0) return a;
		if (start && a != next) return -ENOMEM;
		if (!start) start = a;
		next = a + n;
		len -= n;
	}
	return start;
}

static int add_child(long handle, int status)
{
	for (int i = 0; i < CHILDREN; i++)
		if (!kids[i].pid) {
			kids[i] = (struct kid){ next_pid, status, ++spawned, handle };
			next_pid = next_pid == 30000 ? 2 : next_pid + 1;
			return kids[i].pid;
		}
	if (handle >= 0) svc1(N_KILL, handle), nclose(handle);
	return -EAGAIN;
}

static size_t put(char *buf, size_t at, const char *s)
{
	size_t n = strlen(s) + 1;
	if (at + n > MAX_ARG_BYTES) return 0;
	memcpy(buf + at, s, n);
	return at + n;
}

/* Spawns the boot archive's C program (`sh`, `hello`, `cbench`, `oscb`, `oscnop`; the native ones expect other
 * handles) named by `path`'s last component with the stdio fds, the root and the archive. The arguments start with
 * "<argc> <stdin> <stdout> <stderr> /<cwd>", each stdio fd `t` (console), `p` (pipe), `d`, or `f<offset>` (`a` if
 * appending) for a file, then argv, then as much of envp as fits. */
static long spawn(const char *path, char *const argv[], char *const envp[])
{
	char args[MAX_ARG_BYTES], head[PATH + 96], io[3][24];
	int argc = 0, count = 1;
	while (argv[argc]) argc++;
	if (argc + 1 > MAX_ARGS) return -E2BIG;
	const char *name = strrchr(path, '/');
	name = name ? name + 1 : path;
	if (strcmp(name, "sh") && strcmp(name, "hello") && strcmp(name, "cbench") && strcmp(name, "oscb") &&
	    strcmp(name, "oscnop") && strcmp(name, "tcpecho"))
		return -ENOENT;
	for (int fd = 0; fd < 3; fd++) {
		struct file *f = fd_file(fd);
		int kind = f ? f->kind : TTY;
		if (kind == NODE) snprintf(io[fd], sizeof io[fd], "%c%lld", f->append ? 'a' : 'f', f->off);
		else snprintf(io[fd], sizeof io[fd], "%c", kind == PIPE ? 'p' : kind == DIRECTORY ? 'd' : 't');
	}
	snprintf(head, sizeof head, "%d %s %s %s /%s", argc, io[0], io[1], io[2], st.cwd);
	size_t len = put(args, 0, head);
	for (int i = 0; i < argc; i++, count++)
		if (!(len = put(args, len, argv[i]))) return -E2BIG;
	for (int i = 0; envp && envp[i] && count < MAX_ARGS; i++, count++) {
		size_t next = put(args, len, envp[i]);
		if (!next) break;
		len = next;
	}
	long exe = svc(N_OPEN, H_ARCHIVE, (long)name, strlen(name), 0, 0, 0, 0);
	if (exe < 0) return exe;
	/* Every slot but the NetStack: a child never inherits network access (least privilege). */
	long handles[H_NET], p = -ENOMEM;
	int got = 0, n = H_NET;
	for (; got < n; got++) {
		struct file *f = got < 3 && !(st.cloexec >> got & 1) ? fd_file(got) : 0;
		long dup = got < 3 ? (f ? ndup(f->handle) : -EBADF) : ndup(got);
		/* An absent slot gets a handle with no rights, so the later ones keep their values. */
		if (dup < 0) dup = svc(N_DUP, exe, R_TRANSFER, 0, 0, 0, 0, 0);
		if (dup < 0) {
			p = dup;
			break;
		}
		handles[got] = dup;
	}
	for (long budget = CHILD_BUDGET; got == n && p == -ENOMEM && budget >= MIN_BUDGET; budget /= 2)
		p = svc(N_SPAWN, exe, (long)handles, n, budget, -1, (long)args, len);
	if (p < 0)
		while (got--) nclose(handles[got]);
	nclose(exe);
	return p;
}

/* Not nested: vfork.s answers EAGAIN in a child, before it overwrites the saved registers. */
int __mog_vfork_enter(void)
{
	saved = st;
	child = 1;
	return 0;
}

/* Ends child mode: closes what the child opened, restores the parent's fds and returns `vfork` the child's pid. */
static _Noreturn void vfork_leave(long handle, int status)
{
	for (int i = 0; i < FILES; i++)
		if (st.files[i].kind != FREE && !inherited(i)) nclose(st.files[i].handle);
	st = saved;
	child = 0;
	int pid = add_child(handle, status);
	if (pid < 0) errno = -pid, pid = -1;
	__mog_vfork_resume(pid);
}

static _Noreturn void do_exit(int code)
{
	if (child) vfork_leave(-1, code);
	svc1(N_EXIT, code);
	for (;;);
}

/* No exec: a native spawn; outside a vfork child the process then waits and exits with the child's code. */
static long do_execve(const char *path, char *const argv[], char *const envp[])
{
	long p = spawn(path, argv, envp);
	if (p < 0) return p;
	if (child) vfork_leave(p, 0);
	long code = svc1(N_WAIT, p);
	nclose(p);
	do_exit(code == KILLED ? 128 + SIGKILL : code);
}

static long do_wait4(int pid, int *status, int options)
{
	struct kid *k = 0;
	/* No native wait-for-any: an exited pseudo-child first, else the newest child (the foreground one). */
	for (int i = 0; i < CHILDREN; i++) {
		struct kid *c = &kids[i];
		if (c->pid && (pid == -1 || c->pid == pid) &&
		    (!k || (c->handle < 0) > (k->handle < 0) || (c->handle < 0) == (k->handle < 0) && c->order > k->order))
			k = c;
	}
	if (!k) return -ECHILD;
	if (k->handle >= 0) {
		if (options & WNOHANG) return 0;
		long code = svc1(N_WAIT, k->handle);
		if (code < 0) return code;
		nclose(k->handle);
		k->status = code;
	}
	if (status) *status = k->status == KILLED ? SIGKILL : (k->status & 0xff) << 8;
	pid = k->pid;
	k->pid = 0;
	return pid;
}

/* SIGKILL, SIGTERM, SIGINT, SIGQUIT and SIGHUP end a child through its process handle (or the caller, through
 * exit); signal 0 checks the pid; others are ignored. */
static long do_kill(int pid, int sig)
{
	int fatal = sig == SIGKILL || sig == SIGTERM || sig == SIGINT || sig == SIGQUIT || sig == SIGHUP;
	if (pid == 1 && fatal) do_exit(128 + sig);
	if (pid == 1) return 0;
	for (int i = 0; i < CHILDREN; i++)
		if (kids[i].pid == pid && pid > 0)
			return fatal && kids[i].handle >= 0 ? svc1(N_KILL, kids[i].handle) : 0;
	return pid > 0 ? -ESRCH : 0;
}

static long do_pipe2(int *out, int flags)
{
	register long x0 __asm__("x0"), x1 __asm__("x1"), x8 __asm__("x8") = N_PIPE;
	__asm__ __volatile__("svc 0" : "=r"(x0), "=r"(x1) : "r"(x8) : "memory");
	if (x0 < 0) return x0;
	long h[2] = { x0, x1 };
	int fd[2];
	for (int k = 0; k < 2; k++) {
		int i = (fd[k] = free_fd(0)) < 0 ? fd[k] : new_file(h[k], PIPE, "");
		if (i < 0) {
			if (k) unref(fd[0]);
			else nclose(h[0]);
			nclose(h[1]);
			return i;
		}
		install(fd[k], i, flags & O_CLOEXEC);
	}
	out[0] = fd[0], out[1] = fd[1];
	return 0;
}

static long do_clock_gettime(struct timespec *ts)
{
	unsigned long count, freq;
	__asm__ __volatile__("isb; mrs %0, cntvct_el0; mrs %1, cntfrq_el0" : "=r"(count), "=r"(freq));
	ts->tv_sec = count / freq;
	ts->tv_nsec = (count % freq) * 1000000000 / freq;
	return 0;
}

long __mog_syscall(long n, long a, long b, long c, long d, long e, long f)
{
	switch (n) {
	case SYS_read: return rw(a, (char *)b, c, 0);
	case SYS_write: return rw(a, (char *)b, c, 1);
	case SYS_readv: return rwv(a, (void *)b, c, 0);
	case SYS_writev: return rwv(a, (void *)b, c, 1);
	case SYS_pread64:
	case SYS_pwrite64: {
		struct file *file = fd_file(a);
		if (!file) return -EBADF;
		return svc(N_IO, file->handle, n == SYS_pwrite64, b, c, d, 0, 0);
	}
	case SYS_lseek: return do_lseek(a, b, c);
	case SYS_openat: return do_openat(a, (char *)b, c);
	case SYS_close:
		if (!fd_file(a)) return -EBADF;
		unref(a);
		return 0;
	case SYS_getdents64: return do_getdents(a, (char *)b, c);
	case SYS_newfstatat: return do_fstatat(a, (char *)b, (void *)c, d);
	case SYS_fstat: return do_fstatat(a, "", (void *)b, AT_EMPTY_PATH);
	case SYS_mkdirat: {
		char p[PATH];
		int r = resolve(a, (char *)b, p);
		if (r) return r;
		return *p ? svc(N_MKDIR, H_ROOT, (long)p, strlen(p), 0, 0, 0, 0) : -EEXIST;
	}
	case SYS_unlinkat: return do_unlinkat(a, (char *)b, c);
	case SYS_renameat:
	case SYS_renameat2: return do_renameat(a, (char *)b, c, (char *)d);
	case SYS_faccessat:
	case SYS_faccessat2: {
		char p[PATH];
		int r = resolve(a, (char *)b, p);
		long h = r ? r : nopen(p, 0);
		return h < 0 ? h : nclose(h);
	}
	case SYS_getcwd:
		if (strlen(st.cwd) + 2 > (size_t)b) return -ERANGE;
		*(char *)a = '/';
		strcpy((char *)a + 1, st.cwd);
		return strlen(st.cwd) + 2;
	case SYS_chdir: return do_chdir(-1, (char *)a);
	case SYS_fchdir: return do_chdir(a, 0);
	case SYS_dup: return do_dup3(a, -1, 0, 0);
	case SYS_dup3: return b < 0 ? -EBADF : do_dup3(a, b, c, 0);
	case SYS_fcntl: return do_fcntl(a, b, c);
	case SYS_pipe2: return do_pipe2((int *)a, b);
	case SYS_ioctl: {
		struct file *file = fd_file(a);
		if (!file) return -EBADF;
		if (b != TIOCGWINSZ || file->kind != TTY) return -ENOTTY;
		*(struct winsize *)c = (struct winsize){ .ws_row = 24, .ws_col = 80 };
		return 0;
	}
	case SYS_sync: return svc1(N_SYNC, H_ROOT);
	case SYS_fsync:
	case SYS_fdatasync: {
		struct file *file = fd_file(a);
		if (!file) return -EBADF;
		return file->kind == NODE || file->kind == DIRECTORY ? svc1(N_SYNC, file->handle) : -EINVAL;
	}
	case SYS_exit:
	case SYS_exit_group: do_exit(a);
	case SYS_execve: return do_execve((char *)a, (void *)b, (void *)c);
	case SYS_wait4: return do_wait4(a, (int *)b, c);
	case SYS_kill: return do_kill(a, b);
	case SYS_tkill: return do_kill(1, b);
	case SYS_tgkill: return do_kill(1, c);
	case SYS_getpid: return child ? next_pid : 1;
	case SYS_gettid:
	case SYS_set_tid_address: return 1;
	case SYS_getppid:
	case SYS_getuid:
	case SYS_geteuid:
	case SYS_getgid:
	case SYS_getegid:
	case SYS_setpgid:
	case SYS_sigaltstack: return 0;
	case SYS_getpgid:
	case SYS_setsid: return 1;
	case SYS_umask: return 022;
	case SYS_rt_sigaction:
		if (c) memset((void *)c, 0, sizeof(struct k_sigaction));
		return 0;
	case SYS_rt_sigprocmask:
		if (c) memset((void *)c, 0, d);
		return 0;
	case SYS_brk: return 0;
	case SYS_mmap: return do_mmap(a, b, d);
	case SYS_munmap:
	case SYS_mprotect:
	case SYS_madvise: return 0;
	case SYS_clock_gettime: return do_clock_gettime((void *)b);
	case SYS_socket: return do_socket(a, b, c);
	case SYS_bind: {
		struct file *file = socket_file(a);
		long port = port_of((void *)b, c);
		if (!file) return fd_file(a) ? -ENOTSOCK : -EBADF;
		/* The kernel takes any (0) or 127.0.0.1, which listens on loopback only. */
		return port < 0 ? port : svc(N_BIND, file->handle, port, ntohl(((struct sockaddr_in *)b)->sin_addr.s_addr), 0, 0, 0, 0);
	}
	case SYS_listen:
	case SYS_shutdown: {
		struct file *file = socket_file(a);
		if (!file) return fd_file(a) ? -ENOTSOCK : -EBADF;
		/* SHUT_RD alone has nothing to do: received data is simply not read. */
		if (n == SYS_shutdown && b == SHUT_RD) return 0;
		return svc1(n == SYS_listen ? N_LISTEN : N_SHUTDOWN, file->handle);
	}
	case SYS_accept: return do_accept(a, (void *)b, (void *)c, 0);
	case SYS_accept4: return do_accept(a, (void *)b, (void *)c, d);
	case SYS_connect: return do_connect(a, (void *)b, c);
	case SYS_sendto:
	case SYS_recvfrom:
		if (!socket_file(a)) return fd_file(a) ? -ENOTSOCK : -EBADF;
		return rw(a, (char *)b, c, n == SYS_sendto);
	case SYS_setsockopt:
		/* Ports are free to rebind once closed, so SO_REUSEADDR is what the kernel already does. */
		if (!socket_file(a)) return fd_file(a) ? -ENOTSOCK : -EBADF;
		return b == SOL_SOCKET && c == SO_REUSEADDR ? 0 : -ENOPROTOOPT;
	case SYS_uname:
		*(struct utsname *)a = (struct utsname){ "MogOs", "mogos", "0.4", "", "aarch64" };
		return 0;
	}
	return -ENOSYS;
}

static long long digits(char *s, char **end)
{
	long long n = 0;
	for (; *s >= '0' && *s <= '9'; s++) n = n * 10 + *s - '0';
	*end = s;
	return n;
}

/* From `_start` (x0-x2 as the kernel passed them): maps the stack and lays out argc, argv, envp and an auxv with
 * AT_PAGESZ on it for `_start_c`. Strings starting with `spawn`'s header (msh writes "<argc> /<cwd>": stdio on the
 * console) are a C parent's: argv, then envp; others are all argv, from the root, stdio on the console. */
long *__mog_start(long count, char *args, long len)
{
	char *strings[MAX_ARGS], *s = args;
	int argc = count;
	for (int i = 0; i < FDS; i++) st.fds[i] = -1;
	for (int fd = 0; fd < 3; fd++) install(fd, new_file(fd, TTY, ""), 0);
	for (int i = 0; i < count && i < MAX_ARGS; i++, s += strlen(s) + 1) strings[i] = s;
	char **argv = strings, *end;
	if (count && *strings[0] >= '0' && *strings[0] <= '9') {
		/* By hand: libc functions may set errno, which needs the thread pointer `_start_c` sets up later. */
		long n = digits(strings[0], &end);
		for (int fd = 0; fd < 3 && end[0] == ' ' && end[1] != '/'; fd++) {
			struct file *f = &st.files[st.fds[fd]];
			f->kind = end[1] == 'p' ? PIPE : end[1] == 'd' ? DIRECTORY : end[1] == 't' ? TTY : NODE;
			f->append = end[1] == 'a';
			f->off = digits(end + 2, &end);
		}
		if (end[0] == ' ' && end[1] == '/' && n < count) {
			resolve(AT_FDCWD, end + 1, st.cwd);
			argc = n, argv++, count--;
		}
	}
	long stack = do_mmap(0, STACK, MAP_ANONYMOUS);
	if (stack < 0) svc1(N_EXIT, 127);
	long *p = (long *)((stack + STACK - (count + 7) * sizeof(long)) & ~15ul);
	p[0] = argc;
	for (int i = 0; i < count; i++) p[1 + i + (i >= argc)] = (long)argv[i];
	p[1 + argc] = 0;
	long *aux = p + count + 2;
	aux[0] = 0;
	aux[1] = AT_PAGESZ, aux[2] = 4096, aux[3] = AT_NULL, aux[4] = 0;
	return p;
}
