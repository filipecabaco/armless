//! What user mode answers to arguments a program gets wrong: each is an error the program can
//! see, as Linux gives it, and never a panic of the host (release builds abort on one).

mod prog;

use prog::*;

const EBADF: i64 = -9;
const EINVAL: i64 = -22;

/// Runs `p` on a machine of its own and checks that it passed its checks
fn passes(p: Prog) {
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn close_range_refuses_a_range_that_ends_before_it_starts_and_unknown_flags() {
    let mut p = Prog::default();
    // close_range(first, last, flags)
    p.sys(436, &[I(5), I(3), I(0)]).expect(EINVAL);
    p.sys(436, &[I(u32::MAX as u64), I(0), I(0)]).expect(EINVAL);
    p.sys(436, &[I(3), I(10), I(1)]).expect(EINVAL);
    // CLOSE_RANGE_UNSHARE and CLOSE_RANGE_CLOEXEC are known
    p.sys(436, &[I(3), I(10), I(2)]).expect(0);
    // pipe2: fds 3 and 4; CLOSE_RANGE_CLOEXEC marks them and leaves them open
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    p.sys(436, &[I(3), I(u32::MAX as u64), I(4)]).expect(0);
    p.sys(25, &[I(3), I(1)]).expect(1); // fcntl(F_GETFD): FD_CLOEXEC
    // A range past every descriptor closes none; one over them closes them
    p.sys(436, &[I(100), I(u32::MAX as u64), I(0)]).expect(0);
    p.sys(436, &[I(3), I(u32::MAX as u64), I(0)]).expect(0);
    p.sys(57, &[I(3)]).expect(EBADF);
    passes(p);
}

/// struct timespec { tv_sec, tv_nsec }
fn timespec(s: i64, ns: i64) -> Vec<u8> {
    [s.to_le_bytes(), ns.to_le_bytes()].concat()
}

/// A program that arms a 50 ms alarm (setitimer(ITIMER_REAL)) before the rest: SIGALRM ends it
/// (exit status 128 + 14) if it waits that long.
fn with_alarm() -> Prog {
    let mut p = Prog::default();
    // struct itimerval { interval, value } of timevals: once, in 50 ms
    p.put(0x80, &[0u64, 0, 0, 50_000].map(u64::to_le_bytes).concat());
    p.sys(103, &[I(0), D(0x80), I(0)]).expect(0);
    p
}

/// Runs `p`, which must end by its alarm
fn ends_by_alarm(p: Prog) {
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    assert!(matches!(status, armless_user::Status::Exited(142)), "{status:?}");
}

#[test]
fn a_sleep_of_negative_or_malformed_time_is_einval() {
    let mut p = Prog::default();
    p.put(0x100, &timespec(-1, 0));
    p.put(0x110, &timespec(0, 1_000_000_000));
    p.put(0x120, &timespec(0, -1));
    for at in [0x100, 0x110, 0x120] {
        p.sys(101, &[D(at), I(0)]).expect(EINVAL); // nanosleep
        p.sys(115, &[I(1), I(0), D(at), I(0)]).expect(EINVAL); // clock_nanosleep
        p.sys(73, &[I(0), I(0), D(at), I(0), I(8)]).expect(EINVAL); // ppoll
        p.sys(72, &[I(0), I(0), I(0), I(0), D(at), I(0)]).expect(EINVAL); // pselect6
    }
    // futex(FUTEX_WAIT) on a word that holds what it waits for
    p.sys(98, &[D(0x180), I(0), I(0), D(0x100)]).expect(EINVAL);
    p.sys(101, &[I(0xdead_0000), I(0)]).expect(-14); // EFAULT
    passes(p);
}

#[test]
fn a_sleep_too_long_to_say_waits_until_a_signal() {
    let huge = timespec(i64::MAX, 999_999_999);
    // nanosleep, clock_nanosleep
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(101, &[D(0x100), I(0)]);
    ends_by_alarm(p);
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(115, &[I(1), I(0), D(0x100), I(0)]);
    ends_by_alarm(p);
    // ppoll of nothing, pselect6 of nothing, a futex wait, epoll_pwait2
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(73, &[I(0), I(0), D(0x100), I(0), I(8)]);
    ends_by_alarm(p);
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(72, &[I(0), I(0), I(0), I(0), D(0x100), I(0)]);
    ends_by_alarm(p);
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(98, &[D(0x180), I(0), I(0), D(0x100)]);
    ends_by_alarm(p);
    let mut p = with_alarm();
    p.put(0x100, &huge);
    p.sys(20, &[I(0)]).expect(3); // epoll_create1
    p.sys(441, &[I(3), D(0x200), I(1), D(0x100), I(0), I(8)]);
    ends_by_alarm(p);
    // rt_sigtimedwait for a signal nothing sends (SIGUSR1)
    let mut p = with_alarm();
    p.put(0x100, &huge).put(0x140, &(1u64 << 9).to_le_bytes());
    p.sys(137, &[D(0x140), I(0), D(0x100), I(8)]);
    ends_by_alarm(p);
}

#[test]
fn a_socket_timeout_too_long_to_say_waits_until_a_signal() {
    let mut p = with_alarm();
    // socketpair(AF_UNIX, SOCK_STREAM), then SO_RCVTIMEO of the largest timeval and a read
    p.put(0x100, &[i64::MAX.to_le_bytes(), 0i64.to_le_bytes()].concat());
    p.sys(199, &[I(1), I(1), I(0), D(0x180)]).expect(0);
    p.sys(208, &[I(3), I(1), I(20), D(0x100), I(16)]).expect(0);
    p.sys(63, &[I(3), D(0x200), I(8)]);
    ends_by_alarm(p);
}

const AT_FDCWD: u64 = 0xffff_ff9c;
const EFAULT: i64 = -14;

#[test]
fn a_read_or_write_of_any_count_gives_what_fits_or_an_error() {
    let mut p = Prog::default();
    p.put(0x200, b"/proc/self/fdinfo/0\0").put(0x220, b"/dev/zero\0");
    p.put(0x230, b"/dev/urandom\0").put(0x240, b"/dev/null\0");
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).expect(3);
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(0), I(0)]).expect(4);
    p.sys(56, &[I(AT_FDCWD), D(0x230), I(0), I(0)]).expect(5);
    p.sys(56, &[I(AT_FDCWD), D(0x240), I(2), I(0)]).expect(6);
    // A /proc file's few bytes fit; /dev/zero's and /dev/urandom's endless ones don't
    p.sys(63, &[I(3), D(0x400), I(u64::MAX)]).expect_ok();
    p.sys(67, &[I(3), D(0x400), I(u64::MAX), I(0)]).expect_ok();
    for fd in [4, 5] {
        p.sys(63, &[I(fd), D(0x400), I(u64::MAX)]).expect(EFAULT);
        p.sys(63, &[I(fd), D(0x400), I(16)]).expect(16);
    }
    // getrandom: as much as its buffer takes, and no flags it doesn't know
    p.sys(278, &[D(0x400), I(u64::MAX), I(0)]).expect(EFAULT);
    p.sys(278, &[D(0x400), I(32), I(0x100)]).expect(EINVAL);
    p.sys(278, &[D(0x400), I(32), I(1)]).expect(32);
    // A write of more than the program has
    p.sys(64, &[I(6), D(0x400), I(u64::MAX)]).expect(EFAULT);
    p.sys(68, &[I(6), D(0x400), I(u64::MAX), I(0)]).expect(EFAULT);
    passes(p);
}

/// struct iovec array of (offset of the data, length)
fn iovecs(v: &[(u32, u64)]) -> Vec<u8> {
    v.iter()
        .flat_map(|&(at, len)| [(DATA + at as u64).to_le_bytes(), len.to_le_bytes()])
        .flatten()
        .collect()
}

#[test]
fn an_iovec_array_whose_lengths_overflow_is_refused() {
    let mut p = Prog::default();
    p.put(0x220, b"/dev/zero\0");
    p.sys(56, &[I(AT_FDCWD), D(0x220), I(2), I(0)]).expect(3);
    // A length past what a ssize_t holds: EINVAL, as Linux
    p.put(0x100, &iovecs(&[(0x400, 16), (0x400, 1 << 63)]));
    p.sys(65, &[I(3), D(0x100), I(2)]).expect(EINVAL); // readv
    p.sys(66, &[I(3), D(0x100), I(2)]).expect(EINVAL); // writev
    // Lengths whose sum overflows: up to what a read gives, then what fits
    p.put(0x140, &iovecs(&[(0x400, 16), (0x400, i64::MAX as u64), (0x400, i64::MAX as u64)]));
    p.sys(65, &[I(3), D(0x140), I(3)]).expect_valid();
    p.sys(66, &[I(3), D(0x140), I(3)]).expect_valid();
    // More than UIO_MAXIOV of them
    p.sys(65, &[I(3), D(0x140), I(1025)]).expect(EINVAL);
    // One that fits, then one past the program's memory: the bytes that fit
    p.put(0x180, &iovecs(&[(0x400, 16), (0x400, 1 << 40)]));
    p.sys(65, &[I(3), D(0x180), I(2)]).expect(16);
    passes(p);
}

#[test]
fn a_send_of_any_length_gives_an_error_not_a_crash() {
    let mut p = Prog::default();
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0); // socketpair: 3 and 4
    p.sys(206, &[I(3), D(0x400), I(u64::MAX), I(0), I(0), I(0)]).expect(EFAULT); // sendto
    p.sys(207, &[I(4), D(0x400), I(u64::MAX), I(0x40), I(0), I(0)]).expect_valid(); // recvfrom
    p.sys(208, &[I(3), I(1), I(2), D(0x400), I(u64::MAX)]).expect_valid(); // setsockopt
    passes(p);
}

/// RLIMIT_NOFILE as user mode reports and keeps it
const NOFILE: u64 = 4096;
const EMFILE: i64 = -24;

#[test]
fn dup3_and_f_dupfd_refuse_descriptors_out_of_range() {
    let mut p = Prog::default();
    // dup3 to a negative descriptor, one past the limit, with flags it doesn't know, or to itself
    p.sys(24, &[I(1), I(u64::MAX), I(0)]).expect(EBADF);
    p.sys(24, &[I(1), I(0xffff_ffff), I(0)]).expect(EBADF);
    p.sys(24, &[I(1), I(NOFILE), I(0)]).expect(EBADF);
    p.sys(24, &[I(1), I(10), I(1)]).expect(EINVAL);
    p.sys(24, &[I(1), I(1), I(0)]).expect(EINVAL);
    p.sys(24, &[I(1), I(NOFILE - 1), I(0o2000000)]).expect(NOFILE as i64 - 1);
    // fcntl(F_DUPFD, F_DUPFD_CLOEXEC) from a negative descriptor or one past the limit
    for cmd in [0, 1030] {
        p.sys(25, &[I(1), I(cmd), I(0xffff_fffb)]).expect(EINVAL);
        p.sys(25, &[I(1), I(cmd), I(u64::MAX)]).expect(EINVAL);
        p.sys(25, &[I(1), I(cmd), I(NOFILE)]).expect(EINVAL);
    }
    p.sys(25, &[I(1), I(0), I(5)]).expect(5);
    // The last one free is taken; then there are none above it
    p.sys(25, &[I(1), I(0), I(NOFILE - 2)]).expect(NOFILE as i64 - 2);
    p.sys(25, &[I(1), I(0), I(NOFILE - 2)]).expect(EMFILE);
    passes(p);
}

#[test]
fn a_process_gets_emfile_past_its_descriptor_limit_which_getrlimit_reports() {
    let mut p = Prog::default();
    // getrlimit(RLIMIT_NOFILE) and prlimit64 say the same: soft and hard
    p.sys(163, &[I(7), D(0x100)]).expect(0);
    p.sys(261, &[I(0), I(7), I(0), D(0x110)]).expect(0);
    for at in [0x100, 0x108, 0x110, 0x118] {
        p.arg(0, X(at));
        p.expect_wide(NOFILE);
    }
    // Asking for more than there is: EPERM
    p.put(0x120, &[NOFILE * 2, NOFILE * 2].map(u64::to_le_bytes).concat());
    p.sys(164, &[I(7), D(0x120)]).expect(-1);
    // Fill the table with dups of stdout: each call gives one until none is left
    p.sys(24, &[I(1), I(NOFILE - 1), I(0)]).expect(NOFILE as i64 - 1);
    // for (;;) if (dup(1) < 0) break;
    let top = p.code.len();
    p.sys(23, &[I(1)]);
    let back = top as i64 - p.code.len() as i64;
    p.code.push(0xb6f8_0000 | ((back as u32) & 0x3fff) << 5); // tbz x0, #63, top
    p.expect(EMFILE);
    // pipe2 and openat say so too
    p.sys(59, &[D(0x100), I(0)]).expect(EMFILE);
    p.put(0x200, b"/dev/null\0");
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).expect(EMFILE);
    // One closed: one can be had
    p.sys(57, &[I(100)]).expect(0);
    p.sys(56, &[I(AT_FDCWD), D(0x200), I(0), I(0)]).expect(100);
    passes(p);
}

#[test]
fn munmap_and_madvise_refuse_a_start_off_a_page_and_lengths_past_the_address_space() {
    let mut p = Prog::default();
    // Each would take the program's own code with it, rounded as it was
    p.sys(215, &[I(1), I(0xdead_0000)]).expect(EINVAL); // munmap
    p.sys(215, &[D(0), I(u64::MAX)]).expect(EINVAL);
    p.sys(233, &[I(1), I(0xdead_0000), I(4)]).expect(EINVAL); // madvise(MADV_DONTNEED)
    p.sys(233, &[D(0), I(u64::MAX), I(4)]).expect(EINVAL);
    passes(p);
}

/// A struct cmsghdr (len, level, type) and its data
fn cmsg(len: u64, level: u32, ty: u32, data: &[u8]) -> Vec<u8> {
    let mut b = len.to_le_bytes().to_vec();
    b.extend_from_slice(&level.to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(data);
    b
}

#[test]
fn sendmsg_refuses_control_data_whose_lengths_overflow_or_run_short() {
    const ENOBUFS: i64 = -105;
    let mut p = Prog::default();
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0); // socketpair: 3 and 4
    p.put(0x300, b"x");
    // A second header whose length wraps the sum of where it starts and its length to 0 (it
    // would pass the check against controllen, then go back to the start, for ever)
    let mut ctl = cmsg(16, 41, 0, &[]);
    ctl.extend_from_slice(&cmsg(0u64.wrapping_sub(16), 41, 0, &[]));
    p.msg(0x400, 0x300, 1, &ctl, 32);
    p.sys(211, &[I(3), D(0x400), I(0)]).expect(EINVAL);
    // One shorter than its own header
    p.msg(0x500, 0x300, 1, &cmsg(8, 41, 0, &[]), 16);
    p.sys(211, &[I(3), D(0x500), I(0)]).expect(EINVAL);
    // More control data than a socket takes (optmem_max): ENOBUFS, before any of it is read
    p.msg(0x600, 0x300, 1, &[], 16);
    p.put(0x600 + 40, &(1u64 << 30).to_le_bytes());
    p.sys(211, &[I(3), D(0x600), I(0)]).expect(ENOBUFS);
    passes(p);
}

#[test]
fn scm_rights_takes_at_most_scm_max_fd_descriptors() {
    // SOL_SOCKET, SCM_RIGHTS: 253 descriptors (all fd 0) go; 254 are EINVAL before any is read,
    // as Linux checks (the last, not open, would be EBADF)
    let mut p = Prog::default();
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0);
    p.put(0x300, b"x");
    let rights = |n: usize| cmsg(16 + 4 * n as u64, 1, 1, &vec![0; 4 * n]);
    let mut over = rights(254);
    over[16 + 4 * 253..].copy_from_slice(&999u32.to_le_bytes());
    p.msg(0x400, 0x300, 1, &over, 16 + 4 * 254);
    p.sys(211, &[I(3), D(0x400), I(0)]).expect(EINVAL);
    p.msg(0x900, 0x300, 1, &rights(253), 16 + 4 * 253);
    p.sys(211, &[I(3), D(0x900), I(0)]).expect(1);
    passes(p);
}

#[test]
fn shmat_refuses_an_address_out_of_the_address_space_or_over_a_mapping() {
    const SHM_RND: u64 = 0o20000;
    const SHM_REMAP: u64 = 0o40000;
    let mut p = Prog::default();
    // shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600)
    p.sys(194, &[I(0), I(4096), I(0o1600)]).expect(0);
    // Its end wraps; far past the address space; at its end; rounded down to page 0
    p.sys(196, &[I(0), I(0xffff_ffff_ffff_f000), I(0)]).expect(EINVAL);
    p.sys(196, &[I(0), I(1 << 60), I(0)]).expect(EINVAL);
    p.sys(196, &[I(0), I(1 << 39), I(0)]).expect(EINVAL);
    p.sys(196, &[I(0), I(0x1234), I(SHM_RND)]).expect(EINVAL);
    // Over a mapping: only with SHM_REMAP, which replaces it
    p.sys(222, &[I(0), I(4096), I(3), I(0x22), I(u64::MAX), I(0)]).save(0x100);
    p.sys(196, &[I(0), X(0x100), I(0)]).expect(EINVAL);
    p.sys(196, &[I(0), X(0x100), I(SHM_REMAP)]).expect_ok();
    passes(p);
}
