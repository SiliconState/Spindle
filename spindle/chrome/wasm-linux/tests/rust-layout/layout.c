// The layouts and constants of musl for Spindle's wasm Linux, one `NAME VALUE` line
// each: src/main.rs prints the same names from Rust's libc crate (as patched by
// build-rust.sh), and chrome/tests/wasm_linux_rust.mjs runs both and compares.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <pwd.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/reboot.h>
#include <sys/resource.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#define SIZE(T) printf("size %s %zu\n", #T, sizeof(struct T))
#define TYPE(T) printf("size %s %zu\n", #T, sizeof(T))
#define AT(T, F, NAME) printf("at %s.%s %zu\n", #T, NAME, offsetof(struct T, F))
#define TAT(T, F) printf("at %s.%s %zu\n", #T, #F, offsetof(T, F))
#define VALUE(C) printf("value %s %lld\n", #C, (long long)(C))
// Rust's libc names a call that takes times by its old name, and means the 64-bit-time
// one (its timespec has a 64-bit tv_sec): SYS_futex is SYS_futex_time64.
#define TIME64(C, T) printf("value %s %lld\n", #C, (long long)(T))

int main(void) {
    TYPE(long); TYPE(size_t); TYPE(time_t); TYPE(off_t); TYPE(ino_t); TYPE(dev_t);
    TYPE(nlink_t); TYPE(blksize_t); TYPE(blkcnt_t); TYPE(clock_t); TYPE(suseconds_t);
    TYPE(mode_t); TYPE(pid_t); TYPE(uid_t); TYPE(socklen_t); TYPE(rlim_t); TYPE(fsblkcnt_t);
    TYPE(sigset_t); TYPE(siginfo_t); TYPE(fd_set); TYPE(pthread_t); TYPE(pthread_attr_t);
    TYPE(pthread_mutex_t); TYPE(pthread_cond_t); TYPE(pthread_rwlock_t); TYPE(stack_t);

    SIZE(stat);
    AT(stat, st_dev, "st_dev"); AT(stat, st_ino, "st_ino"); AT(stat, st_mode, "st_mode");
    AT(stat, st_nlink, "st_nlink"); AT(stat, st_uid, "st_uid"); AT(stat, st_gid, "st_gid");
    AT(stat, st_rdev, "st_rdev"); AT(stat, st_size, "st_size"); AT(stat, st_blksize, "st_blksize");
    AT(stat, st_blocks, "st_blocks"); AT(stat, st_atim, "st_atime"); AT(stat, st_mtim, "st_mtime");
    AT(stat, st_ctim, "st_ctime");
    SIZE(timespec); AT(timespec, tv_nsec, "tv_nsec");
    SIZE(timeval); AT(timeval, tv_usec, "tv_usec");
    SIZE(sigaction); AT(sigaction, sa_mask, "sa_mask"); AT(sigaction, sa_flags, "sa_flags");
    AT(sigaction, sa_restorer, "sa_restorer");
    SIZE(dirent); AT(dirent, d_off, "d_off"); AT(dirent, d_reclen, "d_reclen");
    AT(dirent, d_type, "d_type"); AT(dirent, d_name, "d_name");
    SIZE(pollfd);
    SIZE(msghdr); AT(msghdr, msg_iov, "msg_iov"); AT(msghdr, msg_iovlen, "msg_iovlen");
    AT(msghdr, msg_control, "msg_control"); AT(msghdr, msg_controllen, "msg_controllen");
    AT(msghdr, msg_flags, "msg_flags");
    SIZE(cmsghdr); AT(cmsghdr, cmsg_level, "cmsg_level");
    SIZE(sockaddr_un); SIZE(sockaddr_in); SIZE(sockaddr_in6); SIZE(sockaddr_storage);
    SIZE(termios); AT(termios, c_line, "c_line"); AT(termios, c_cc, "c_cc");
    AT(termios, __c_ispeed, "c_ispeed");
    SIZE(winsize); SIZE(rlimit); SIZE(utsname); SIZE(epoll_event); SIZE(iovec);
    SIZE(statvfs); AT(statvfs, f_fsid, "f_fsid"); AT(statvfs, f_namemax, "f_namemax");
    SIZE(flock); AT(flock, l_start, "l_start"); AT(flock, l_len, "l_len"); AT(flock, l_pid, "l_pid");
    SIZE(rusage); AT(rusage, ru_maxrss, "ru_maxrss");
    SIZE(tm); AT(tm, tm_gmtoff, "tm_gmtoff"); AT(tm, tm_zone, "tm_zone");
    SIZE(itimerspec); SIZE(ucred); SIZE(passwd); AT(passwd, pw_dir, "pw_dir");
    SIZE(addrinfo); AT(addrinfo, ai_addr, "ai_addr"); AT(addrinfo, ai_next, "ai_next");
    TAT(stack_t, ss_flags); TAT(stack_t, ss_size);

    VALUE(O_CLOEXEC); VALUE(O_DIRECTORY); VALUE(O_NONBLOCK); VALUE(O_NOFOLLOW); VALUE(O_LARGEFILE);
    VALUE(O_DIRECT); VALUE(O_TMPFILE); VALUE(O_PATH); VALUE(F_GETFL); VALUE(F_SETLK);
    VALUE(F_DUPFD_CLOEXEC); VALUE(F_OFD_SETLK); VALUE(AT_FDCWD); VALUE(AT_REMOVEDIR);
    VALUE(SOCK_CLOEXEC); VALUE(SOCK_NONBLOCK); VALUE(SOCK_SEQPACKET); VALUE(SOL_SOCKET);
    VALUE(SO_PEERCRED); VALUE(SO_RCVTIMEO); VALUE(SO_REUSEADDR); VALUE(SCM_RIGHTS);
    VALUE(MSG_NOSIGNAL); VALUE(EAGAIN); VALUE(EDEADLK); VALUE(ENOTSUP); VALUE(ELOOP);
    VALUE(ETIMEDOUT); VALUE(EINPROGRESS); VALUE(SIGCHLD); VALUE(SIGBUS); VALUE(SIGUSR1);
    VALUE(SIGSYS); VALUE(SIGRTMIN); VALUE(SA_RESTART); VALUE(SA_SIGINFO); VALUE(SA_ONSTACK);
    VALUE(SA_NOCLDSTOP); VALUE(SIG_BLOCK); VALUE(TIOCGWINSZ); VALUE(TIOCSCTTY); VALUE(FIONBIO);
    VALUE(TCSANOW); VALUE(CLONE_NEWNS); VALUE(CLONE_NEWPID); VALUE(CLONE_VM); VALUE(CLONE_VFORK);
    VALUE(MS_BIND); VALUE(MS_REC); VALUE(MS_PRIVATE); VALUE(MNT_DETACH);
    VALUE(EPOLL_CLOEXEC); VALUE(EPOLLIN); VALUE(EPOLLET); VALUE(POLLIN); VALUE(POLLRDHUP);
    VALUE(S_IFMT); VALUE(S_IFSOCK); VALUE(RLIMIT_NOFILE); VALUE(RLIMIT_AS); VALUE(PR_SET_PDEATHSIG);
    VALUE(PR_SET_NAME); VALUE(CLOCK_MONOTONIC); VALUE(CLOCK_BOOTTIME); VALUE(WNOHANG); VALUE(WEXITED);
    VALUE(PTHREAD_STACK_MIN); VALUE(_SC_PAGESIZE); VALUE(_SC_NPROCESSORS_ONLN); VALUE(PATH_MAX);
    VALUE(RB_POWER_OFF); VALUE(SIGSTKSZ); VALUE(AF_UNIX); VALUE(AF_INET6); VALUE(IPPROTO_TCP);
    VALUE(SYS_read); VALUE(SYS_openat); VALUE(SYS_clone);
    VALUE(SYS_getrandom); VALUE(SYS_statx); VALUE(SYS_pidfd_open); VALUE(SYS_pidfd_send_signal);
    VALUE(SYS_close_range); VALUE(SYS_copy_file_range); VALUE(SYS_epoll_pwait2); VALUE(SYS_gettid);
    VALUE(SYS_exit_group); VALUE(SYS_rt_sigaction); VALUE(SYS_io_uring_setup);
    VALUE(SYS_memfd_create); VALUE(SYS_faccessat2);
    TIME64(SYS_futex, SYS_futex_time64); TIME64(SYS_clock_gettime, SYS_clock_gettime64);
    TIME64(SYS_clock_nanosleep, SYS_clock_nanosleep_time64); TIME64(SYS_ppoll, SYS_ppoll_time64);
    TIME64(SYS_pselect6, SYS_pselect6_time64); TIME64(SYS_recvmmsg, SYS_recvmmsg_time64);
    TIME64(SYS_utimensat, SYS_utimensat_time64);
    TIME64(SYS_rt_sigtimedwait, SYS_rt_sigtimedwait_time64);
    TIME64(SYS_timerfd_settime, SYS_timerfd_settime64); TIME64(SYS_clock_getres, SYS_clock_getres_time64);
    fflush(stdout);
    reboot(RB_POWER_OFF);
}
