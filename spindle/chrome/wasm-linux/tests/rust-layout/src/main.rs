//! What layout.c prints, from Rust's libc crate: one `KIND NAME VALUE` line each.
use std::mem::{offset_of, size_of};

macro_rules! size {
    ($($name:ident),* $(,)?) => { $(println!("size {} {}", stringify!($name), size_of::<libc::$name>());)* };
}
macro_rules! at {
    ($struct:ident: $($field:ident $(as $label:literal)?),* $(,)?) => {
        $(println!("at {}.{} {}", stringify!($struct), at!(@label $field $($label)?), offset_of!(libc::$struct, $field));)*
    };
    (@label $field:ident) => { stringify!($field) };
    (@label $field:ident $label:literal) => { $label };
}
macro_rules! value {
    ($($name:ident),* $(,)?) => { $(println!("value {} {}", stringify!($name), libc::$name as i64);)* };
}

fn main() {
    println!("size long {}", size_of::<libc::c_long>());
    size!(
        size_t,
        time_t,
        off_t,
        ino_t,
        dev_t,
        nlink_t,
        blksize_t,
        blkcnt_t,
        clock_t,
        suseconds_t,
        mode_t,
        pid_t,
        uid_t,
        socklen_t,
        rlim_t,
        fsblkcnt_t,
        sigset_t,
        siginfo_t,
        fd_set,
        pthread_t,
        pthread_attr_t,
        pthread_mutex_t,
        pthread_cond_t,
        pthread_rwlock_t,
        stack_t
    );

    size!(stat);
    at!(stat: st_dev, st_ino, st_mode, st_nlink, st_uid, st_gid, st_rdev, st_size, st_blksize,
        st_blocks, st_atime, st_mtime, st_ctime);
    size!(timespec);
    at!(timespec: tv_nsec);
    size!(timeval);
    at!(timeval: tv_usec);
    size!(sigaction);
    at!(sigaction: sa_mask, sa_flags, sa_restorer);
    size!(dirent);
    at!(dirent: d_off, d_reclen, d_type, d_name);
    size!(pollfd, msghdr);
    at!(msghdr: msg_iov, msg_iovlen, msg_control, msg_controllen, msg_flags);
    size!(cmsghdr);
    at!(cmsghdr: cmsg_level);
    size!(
        sockaddr_un,
        sockaddr_in,
        sockaddr_in6,
        sockaddr_storage,
        termios
    );
    at!(termios: c_line, c_cc, __c_ispeed as "c_ispeed");
    size!(winsize, rlimit, utsname, epoll_event, iovec, statvfs);
    at!(statvfs: f_fsid, f_namemax);
    size!(flock);
    at!(flock: l_start, l_len, l_pid);
    size!(rusage);
    at!(rusage: ru_maxrss);
    size!(tm);
    at!(tm: tm_gmtoff, tm_zone);
    size!(itimerspec, ucred, passwd);
    at!(passwd: pw_dir);
    size!(addrinfo);
    at!(addrinfo: ai_addr, ai_next);
    at!(stack_t: ss_flags, ss_size);

    value!(
        O_CLOEXEC,
        O_DIRECTORY,
        O_NONBLOCK,
        O_NOFOLLOW,
        O_LARGEFILE,
        O_DIRECT,
        O_TMPFILE,
        O_PATH,
        F_GETFL,
        F_SETLK,
        F_DUPFD_CLOEXEC,
        F_OFD_SETLK,
        AT_FDCWD,
        AT_REMOVEDIR,
        SOCK_CLOEXEC,
        SOCK_NONBLOCK,
        SOCK_SEQPACKET,
        SOL_SOCKET,
        SO_PEERCRED,
        SO_RCVTIMEO,
        SO_REUSEADDR,
        SCM_RIGHTS,
        MSG_NOSIGNAL,
        EAGAIN,
        EDEADLK,
        ENOTSUP,
        ELOOP,
        ETIMEDOUT,
        EINPROGRESS,
        SIGCHLD,
        SIGBUS,
        SIGUSR1,
        SIGSYS
    );
    println!("value SIGRTMIN {}", libc::SIGRTMIN());
    value!(
        SA_RESTART,
        SA_SIGINFO,
        SA_ONSTACK,
        SA_NOCLDSTOP,
        SIG_BLOCK,
        TIOCGWINSZ,
        TIOCSCTTY,
        FIONBIO,
        TCSANOW,
        CLONE_NEWNS,
        CLONE_NEWPID,
        CLONE_VM,
        CLONE_VFORK,
        MS_BIND,
        MS_REC,
        MS_PRIVATE,
        MNT_DETACH,
        EPOLL_CLOEXEC,
        EPOLLIN
    );
    // C's is unsigned.
    println!("value EPOLLET {}", libc::EPOLLET as u32);
    value!(
        POLLIN,
        POLLRDHUP,
        S_IFMT,
        S_IFSOCK,
        RLIMIT_NOFILE,
        RLIMIT_AS,
        PR_SET_PDEATHSIG,
        PR_SET_NAME,
        CLOCK_MONOTONIC,
        CLOCK_BOOTTIME,
        WNOHANG,
        WEXITED,
        PTHREAD_STACK_MIN,
        _SC_PAGESIZE,
        _SC_NPROCESSORS_ONLN,
        PATH_MAX,
        RB_POWER_OFF,
        SIGSTKSZ,
        AF_UNIX,
        AF_INET6,
        IPPROTO_TCP
    );
    value!(
        SYS_read,
        SYS_openat,
        SYS_clone,
        SYS_getrandom,
        SYS_statx,
        SYS_pidfd_open,
        SYS_pidfd_send_signal,
        SYS_close_range,
        SYS_copy_file_range,
        SYS_epoll_pwait2,
        SYS_gettid,
        SYS_exit_group,
        SYS_rt_sigaction,
        SYS_io_uring_setup,
        SYS_memfd_create,
        SYS_faccessat2,
        SYS_futex,
        SYS_clock_gettime,
        SYS_clock_nanosleep,
        SYS_ppoll,
        SYS_pselect6,
        SYS_recvmmsg,
        SYS_utimensat,
        SYS_rt_sigtimedwait,
        SYS_timerfd_settime,
        SYS_clock_getres
    );
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    unsafe { libc::reboot(libc::RB_POWER_OFF) };
}
