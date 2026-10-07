#define __SYSCALL_LL_E(x) (x)
#define __SYSCALL_LL_O(x) (x)

/* MogOs: Linux syscall numbers go to a libc dispatcher over the native calls (src/mogos/syscall.c). */
long __mog_syscall(long, long, long, long, long, long, long);

static inline long __syscall0(long n)
{
	return __mog_syscall(n, 0, 0, 0, 0, 0, 0);
}

static inline long __syscall1(long n, long a)
{
	return __mog_syscall(n, a, 0, 0, 0, 0, 0);
}

static inline long __syscall2(long n, long a, long b)
{
	return __mog_syscall(n, a, b, 0, 0, 0, 0);
}

static inline long __syscall3(long n, long a, long b, long c)
{
	return __mog_syscall(n, a, b, c, 0, 0, 0);
}

static inline long __syscall4(long n, long a, long b, long c, long d)
{
	return __mog_syscall(n, a, b, c, d, 0, 0);
}

static inline long __syscall5(long n, long a, long b, long c, long d, long e)
{
	return __mog_syscall(n, a, b, c, d, e, 0);
}

static inline long __syscall6(long n, long a, long b, long c, long d, long e, long f)
{
	return __mog_syscall(n, a, b, c, d, e, f);
}

#define IPC_64 0
