//! The system calls (arm64 numbers), as Linux answers them: the dispatcher, one arm a call, and
//! the calls' groups, each in a file of its own.

mod diag;
mod fd;
mod file;
mod io;
mod ioctl;
mod misc;
pub(crate) mod nr;
mod path;
mod poll;

use armless::cpu::Cpu;
use armless::Host;

use crate::errno::{EBADF, ENOSYS};
use crate::proc::CloneArgs;
use crate::{page_up, Process, Ret};

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

/// The CPUs a program is told of with one CPU, so that a runtime sizing its threads by them
/// (Go's GOMAXPROCS, a JVM's GC threads) has some; with more, as many as there are.
const NCPUS_REPORTED: u64 = 4;

/// What a descriptor is ready for: (readable, writable, hung up)
type Ready = (bool, bool, bool);

impl Process {
    pub(crate) fn syscall<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        let nr = cpu.x[8];
        let a = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5]];
        *self.stats.syscalls.entry(nr).or_default() += 1;
        let tid = self.threads[cur].tid;
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match nr {
            // ---------------------------------------------------- reading and writing
            nr::READ => self.sys_read(&a),
            nr::PREAD64 => self.sys_pread64(&a),
            nr::READV | nr::PREADV | nr::PREADV2 => self.sys_readv(nr, &a),
            nr::WRITE => self.sys_write(&a),
            nr::PWRITE64 => self.sys_pwrite64(&a),
            nr::WRITEV | nr::PWRITEV | nr::PWRITEV2 => self.sys_writev(nr, &a),
            nr::SENDFILE => self.sys_sendfile(&a),
            nr::LSEEK => self.sys_lseek(&a),
            nr::CLOSE => v(if self.close_fd(fd0) { 0 } else { -EBADF }),
            nr::CLOSE_RANGE => self.sys_close_range(&a),

            // ---------------------------------------------------- paths and files
            nr::OPENAT => self.sys_openat(&a),
            nr::NEWFSTATAT => self.sys_newfstatat(&a),
            nr::FSTAT => self.sys_fstat(&a),
            nr::STATX => self.sys_statx(&a),
            nr::FACCESSAT | nr::FACCESSAT2 => self.sys_faccessat(nr, &a),
            nr::READLINKAT => self.sys_readlinkat(&a),
            nr::GETDENTS64 => self.sys_getdents64(&a),
            nr::GETCWD => self.sys_getcwd(&a),
            nr::CHDIR => self.sys_chdir(&a),
            nr::FCHDIR => self.sys_fchdir(&a),
            nr::MKDIRAT => self.sys_mkdirat(&a),
            nr::UNLINKAT => self.sys_unlinkat(&a),
            nr::SYMLINKAT => self.sys_symlinkat(&a),
            nr::LINKAT => self.sys_linkat(&a),
            nr::RENAMEAT | nr::RENAMEAT2 => self.sys_renameat(nr, &a),
            nr::FTRUNCATE => self.sys_ftruncate(&a),
            nr::FALLOCATE => self.sys_fallocate(&a),
            nr::TRUNCATE => self.sys_truncate(&a),
            nr::FCHMOD | nr::FCHMODAT => self.sys_fchmod(nr, &a),
            nr::FCHOWN | nr::FCHOWNAT => self.sys_fchown(nr, &a),
            nr::UTIMENSAT | nr::SYNC | nr::FSYNC | nr::FDATASYNC | nr::SYNCFS | nr::FADVISE64 => {
                v(0)
            },
            nr::FLOCK => self.flock(fd0, a[1]),
            nr::STATFS | nr::FSTATFS => self.sys_statfs(&a),
            nr::UMASK => self.sys_umask(&a),

            // ---------------------------------------------------- descriptors
            nr::DUP => self.sys_dup(&a),
            nr::DUP3 => self.sys_dup3(&a),
            nr::FCNTL => self.sys_fcntl(&a),
            nr::IOCTL => self.ioctl(fd0, a[1] as u32, a[2]),
            nr::PIPE2 => self.sys_pipe2(&a),
            // The interval timer and POSIX timers (timer.rs)
            nr::GETITIMER | nr::SETITIMER | nr::TIMER_CREATE..=nr::TIMER_DELETE => {
                self.sys_timer(nr, cur, &a)
            },
            // System V shared memory (shm.rs)
            nr::SHMGET => self.sys_shmget(a[0] as i32, a[1], a[2]),
            nr::SHMCTL => self.sys_shmctl(a[0] as i32, a[1], a[2]),
            nr::SHMAT => self.sys_shmat(a[0] as i32, a[1], a[2]),
            nr::SHMDT => self.sys_shmdt(a[0]),
            // timerfd.rs
            nr::TIMERFD_CREATE => self.sys_timerfd_create(a[0], a[1]),
            nr::TIMERFD_SETTIME => self.sys_timerfd_settime(a[0] as i32, a[1], a[2], a[3]),
            nr::TIMERFD_GETTIME => self.sys_timerfd_gettime(a[0] as i32, a[1]),
            // pidfd_open(pid, flags), pidfd_send_signal(pidfd, sig, info, flags), pidfd_getfd
            // (proc.rs)
            nr::PIDFD_OPEN => self.sys_pidfd_open(a[0] as i32 as i64, a[1]),
            nr::PIDFD_SEND_SIGNAL => self.sys_pidfd_send_signal(a[0] as i32, a[1] as u32, a[3]),
            nr::PIDFD_GETFD => self.sys_pidfd_getfd(a[0] as i32, a[1] as i32, a[2]),
            // signalfd4(fd, mask, sizemask, flags) (signal.rs)
            nr::SIGNALFD4 => self.sys_signalfd(a[0] as i32, a[1], a[2], a[3]),
            nr::EVENTFD2 => self.sys_eventfd2(&a),
            nr::EPOLL_CREATE1 => self.sys_epoll_create1(&a),
            nr::EPOLL_CTL => self.sys_epoll_ctl(&a),
            nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2 => self.sys_epoll_pwait(nr, &a),
            nr::PPOLL => self.ppoll(a[0], a[1], a[2]),
            nr::PSELECT6 => self.pselect(a[0], a[1], a[2], a[3], a[4]),
            // Sockets (net.rs)
            nr::SOCKET..=nr::RECVMSG | nr::ACCEPT4 | nr::RECVMMSG | nr::SENDMMSG => {
                self.net_syscall(nr, a)
            },

            // ---------------------------------------------------- memory
            nr::BRK => v(self.sys_brk(a[0])),
            nr::MMAP => self.sys_mmap(a[0], a[1], a[2], a[3], a[4] as i32, a[5]),
            nr::MUNMAP => v(self.sys_munmap(a[0], a[1])),
            // mremap(old, old_len, new_len, flags, new)
            nr::MREMAP => v(self.sys_mremap(a[0], page_up(a[1]), page_up(a[2]), a[3])),
            nr::MPROTECT => v(self.sys_mprotect(a[0], a[1], a[2])),
            nr::MSYNC
            | nr::MLOCK
            | nr::MUNLOCK
            | nr::MBIND
            | nr::SET_MEMPOLICY
            | nr::GET_MEMPOLICY => v(0),
            // DONTNEED and FREE give anonymous pages back
            nr::MADVISE => v(self.sys_madvise(a[0], a[1], a[2])),

            // ---------------------------------------------------- threads
            // A thread, or a process (fork, vfork)
            nr::CLONE => self.sys_clone(
                cpu,
                cur,
                CloneArgs {
                    flags: a[0] & !0xff,
                    exit_signal: (a[0] & 0xff) as u32,
                    sp: a[1],
                    ptid: a[2],
                    tls: a[3],
                    ctid: a[4],
                    pidfd: a[2],
                },
            ),
            nr::CLONE3 => self.sys_clone3(cpu, cur, a[0], a[1]),
            nr::FUTEX => self.sys_futex(&a),
            nr::SET_TID_ADDRESS => {
                self.threads[cur].clear_tid = a[0];
                v(tid)
            },
            nr::SET_ROBUST_LIST | nr::PRCTL | nr::MEMBARRIER | nr::SCHED_SETAFFINITY => v(0),
            // Scheduling parameters, policies and priorities (sched.rs)
            nr::SCHED_SETPARAM..=nr::SCHED_GETPARAM
            | nr::SCHED_GET_PRIORITY_MAX..=nr::SCHED_RR_GET_INTERVAL
            | nr::SETPRIORITY
            | nr::GETPRIORITY
            | nr::SCHED_SETATTR
            | nr::SCHED_GETATTR => self.sys_sched(nr, cur, &a),
            // glibc and others fall back when it is missing
            nr::RSEQ => v(-ENOSYS),
            nr::SCHED_YIELD => Ret::Yield,
            nr::SCHED_GETAFFINITY => self.sys_sched_getaffinity(&a),
            nr::GETCPU => self.sys_getcpu(&a),
            nr::EXIT => Ret::ThreadExit(a[0] as i32),
            nr::EXIT_GROUP => Ret::GroupExit(a[0] as i32),
            nr::WAIT4 => self.sys_wait4(&a),
            nr::WAITID => self.sys_waitid(&a),
            nr::EXECVE => self.sys_execve(cpu, cur, AT_FDCWD, [a[0], a[1], a[2], 0]),
            nr::EXECVEAT => self.sys_execve(cpu, cur, a[0] as i32, [a[1], a[2], a[3], a[4]]),

            // ---------------------------------------------------- ids
            nr::GETTID => v(tid),
            nr::GETPID => v(self.p.pid),
            nr::GETPPID => v(self.p.ppid),
            nr::GETUID => v(self.p.creds.uid[0] as i64),
            nr::GETEUID => v(self.p.creds.euid() as i64),
            nr::GETGID => v(self.p.creds.gid[0] as i64),
            nr::GETEGID => v(self.p.creds.egid() as i64),
            // The set*id calls, getresuid and getresgid (creds.rs)
            nr::SETREGID..=nr::SETFSGID => self.sys_ids(nr, &a),
            nr::GETPGID | nr::GETSID => self.sys_getpgid(nr, a[0] as i32 as i64),
            nr::SETSID => self.sys_setsid(),
            nr::SETPGID => self.sys_setpgid(a[0] as i32 as i64, a[1] as i32 as i64),
            // creds.rs
            nr::GETGROUPS => v(self.sys_getgroups(a[0] as i32, a[1])),
            nr::SETGROUPS => v(self.sys_setgroups(a[0], a[1])),
            // AIO (aio.rs)
            nr::IO_SETUP => v(self.sys_io_setup(a[0], a[1])),
            nr::IO_DESTROY => v(self.sys_io_destroy(a[0])),
            nr::IO_SUBMIT => self.sys_io_submit(a[0], a[1] as i64, a[2]),
            nr::IO_CANCEL => v(self.sys_io_cancel(a[0], a[1])),
            nr::IO_GETEVENTS | nr::IO_PGETEVENTS => {
                self.sys_io_getevents(a[0], a[1] as i64, a[2] as i64, a[3], a[4])
            },
            nr::CAPGET => self.sys_capget(&a),

            // ---------------------------------------------------- signals (signal.rs)
            nr::RT_SIGACTION => self.sys_sigaction(a[0], a[1], a[2], a[3]),
            nr::RT_SIGPROCMASK => self.sys_sigprocmask(cur, &a),
            nr::RT_SIGPENDING => self.sys_sigpending(cur, a[0]),
            nr::SIGALTSTACK => self.sys_sigaltstack(cur, a[0], a[1], cpu.sp_el0),
            // Go's SIGURG preemption among them
            nr::KILL..=nr::TGKILL => self.sys_kill(nr, &a),
            nr::RT_SIGRETURN => self.sigreturn(cpu, cur),
            nr::RT_SIGSUSPEND => self.sys_sigsuspend(cur, a[0], a[1]),
            nr::RT_SIGTIMEDWAIT => self.sys_sigtimedwait(cur, &a),

            // ---------------------------------------------------- time and the system
            nr::CLOCK_GETTIME => self.sys_clock_gettime(&a),
            nr::GETTIMEOFDAY => self.sys_gettimeofday(&a),
            nr::CLOCK_GETRES => self.sys_clock_getres(&a),
            nr::NANOSLEEP => self.sys_nanosleep(&a),
            nr::CLOCK_NANOSLEEP => self.sys_clock_nanosleep(&a),
            nr::GETRANDOM => self.sys_getrandom(&a),
            nr::PRLIMIT64 | nr::GETRLIMIT | nr::SETRLIMIT => self.sys_rlimit(nr, &a),
            nr::GETRUSAGE => self.sys_getrusage(&a),
            nr::TIMES => v((self.start.elapsed().as_millis() / 10) as i64),
            nr::SYSINFO => self.sys_sysinfo(&a),
            nr::UNAME => self.sys_uname(&a),
            _ => {
                *self.stats.unknown.entry(nr).or_default() += 1;
                v(-ENOSYS)
            },
        }
    }
}
