//! What a stalled program waits on, for a host to see (and whether a wait is for the terminal).

use crate::files::Kind;
use crate::host::Instant;
use crate::Process;

use super::nr;


impl Process {
    /// Whether system call `nr` with `a`, parked to run again, waits on the terminal: a read of
    /// it, or a poll, select or epoll wait with it in its set (`Process::waiting_for_input`).
    pub(crate) fn waits_on_tty(&self, nr: u64, a: [u64; 5]) -> bool {
        let tty = |fd: i64| {
            i32::try_from(fd).is_ok_and(|fd| matches!(self.files.get(fd).map(|d| &d.kind), Some(Kind::Tty)))
        };
        match nr {
            nr::READ | nr::READV => tty(a[0] as i32 as i64),
            // its struct pollfd array (fd, events, revents)
            nr::PPOLL => (0..a[1].min(256)).any(|k| self.u32_at(a[0] + 8 * k).is_some_and(|fd| tty(fd as i32 as i64))),
            // the descriptors of its read set
            nr::PSELECT6 => {
                let n = a[0].min(1024);
                a[1] != 0
                    && self
                        .bytes_at(a[1], n.div_ceil(8) as usize)
                        .is_some_and(|b| (0..n).any(|fd| b[(fd / 8) as usize] & (1 << (fd % 8)) != 0 && tty(fd as i64)))
            },
            // what its instance watches
            nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2 => match self.files.get(a[0] as i32).map(|d| &d.kind) {
                Some(Kind::Epoll { watch }) => watch.keys().any(|&fd| tty(fd as i64)),
                _ => false,
            },
            _ => false,
        }
    }

    /// Every thread parked in a call, for a host to see what a stalled program waits on: its
    /// process and thread, the call and its wait, and for a read, poll, select or epoll wait the
    /// descriptors in it, each what it is and what it is ready for (readable, writable, hung up).
    pub fn describe_waits(&mut self) -> String {
        let kind = |k: &Kind| match k {
            Kind::Tty => "tty",
            Kind::PipeRead(_) => "pipe<",
            Kind::PipeWrite(_) => "pipe>",
            Kind::Sock(_) => "sock",
            Kind::File { .. } => "file",
            Kind::Event { .. } => "eventfd",
            Kind::Epoll { .. } => "epoll",
            Kind::TimerFd(_) => "timerfd",
            Kind::Signal { .. } => "signalfd",
            Kind::Pidfd { .. } => "pidfd",
            _ => "other",
        };
        let before = self.p.pid;
        let waiting: Vec<(i64, i64, String, [u64; 6])> = self
            .threads
            .iter()
            .filter(|t| !t.exited)
            .map(|t| {
                let x = &t.ctx.x;
                let Some((w, d)) = t.wait.as_ref() else {
                    return (t.pid, t.tid, "runnable".to_string(), [u64::MAX, 0, 0, 0, 0, 0]);
                };
                let left = d.map(|d| format!(" {:.1}s left", d.saturating_duration_since(Instant::now()).as_secs_f64()));
                (t.pid, t.tid, format!("{w:?}{}", left.unwrap_or_default()), [x[8], x[0], x[1], x[2], x[3], x[4]])
            })
            .collect();
        let mut out = String::new();
        for (pid, tid, wait, [nr, a0, a1, a2, _, _]) in waiting {
            self.switch_to(pid);
            let fds: Vec<i32> = match nr {
                nr::READ | nr::READV | nr::RECVFROM | nr::RECVMSG | nr::ACCEPT | nr::ACCEPT4 => vec![a0 as i32],
                nr::PPOLL => (0..a1.min(64)).filter_map(|k| self.u32_at(a0 + 8 * k).map(|f| f as i32)).collect(),
                nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2 => match self.files.get(a0 as i32).map(|d| &d.kind) {
                    Some(Kind::Epoll { watch }) => watch.keys().copied().collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            // An epoll wait's watches: the events asked for (EPOLLET 1<<31, EPOLLONESHOT 1<<30)
            let flags: std::collections::BTreeMap<i32, u32> = match (nr, self.files.get(a0 as i32).map(|d| &d.kind)) {
                (nr::EPOLL_PWAIT | nr::EPOLL_PWAIT2, Some(Kind::Epoll { watch })) => watch.iter().map(|(&f, w)| (f, w.events)).collect(),
                _ => Default::default(),
            };
            let said: Vec<String> = fds
                .iter()
                .map(|&fd| {
                    let k = self.files.get(fd).map_or("-".to_string(), |d| match d.kind {
                        // Whose, and whether it is there, ended or gone
                        Kind::Pidfd { pid } => format!(
                            "pidfd({pid} {})",
                            match self.proc_ref(pid) {
                                None => "gone",
                                Some(p) if p.zombie.is_some() => "ended",
                                Some(_) if self.threads.iter().any(|t| t.pid == pid && !t.exited) => "running",
                                Some(_) => "no threads",
                            }
                        ),
                        Kind::Sock(id) => self.files.net.brief(id),
                        ref other => kind(other).to_string(),
                    });
                    let r = self.ready(fd).map_or(String::new(), |(r, w, h)| {
                        format!(" {}{}{}", if r { "r" } else { "" }, if w { "w" } else { "" }, if h { "h" } else { "" })
                    });
                    let ev = flags.get(&fd).map_or(String::new(), |e| format!(" ev={e:#x}"));
                    format!("{fd}:{k}{r}{ev}")
                })
                .collect();
            // A futex wait: the value it waits while the word holds, and what the word holds now
            // (not that value: a wake it should have had)
            let said = if nr == nr::RT_SIGSUSPEND {
                // sigsuspend: what it lets through, what is pending, what SIGCHLD does, and the
                // process's children (an ended one not waited for yet: "z")
                let t = self.threads.iter().find(|t| t.tid == tid);
                let mask = t.map_or(0, |t| t.sig.mask);
                let pend: Vec<u32> = t.map_or(Vec::new(), |t| t.sig.pending.keys().copied().collect());
                let ppend: Vec<u32> = self.p.sig.pending.keys().copied().collect();
                let chld = format!("{:?}", self.p.sig.disposition(17));
                let kids: Vec<String> = self
                    .proc_list()
                    .filter(|q| q.ppid == pid)
                    .map(|q| format!("{}{}", q.pid, if q.zombie.is_some() { "z" } else { "" }))
                    .collect();
                vec![format!("mask {mask:#x} pending {pend:?} process pending {ppend:?} SIGCHLD {chld} children {kids:?}")]
            } else if nr == nr::FUTEX && matches!(a1 & 0x7f, 0 | 9) {
                let now = self.u32_at(a0);
                let stale = now.is_some_and(|v| v != a2 as u32);
                vec![format!("word {now:?} waits while {}{}", a2 as u32, if stale { " STALE" } else { "" })]
            } else {
                said
            };
            let exe = String::from_utf8_lossy(&self.p.exe).into_owned();
            out += &format!("{pid}/{tid} {exe} sys {nr} {wait} [{}]\n", said.join(" "));
        }
        // Every process's sockets, by descriptor
        let pids: Vec<i64> = self.proc_list().filter(|p| p.zombie.is_none()).map(|p| p.pid).collect();
        for pid in pids {
            self.switch_to(pid);
            let socks: Vec<String> = self
                .files
                .fds
                .iter()
                .filter_map(|(&fd, &(d, _))| match self.files.descs.get(&d).map(|d| &d.kind) {
                    Some(Kind::Sock(id)) => Some(format!("{fd}:{}", self.files.net.brief(*id))),
                    _ => None,
                })
                .collect();
            if !socks.is_empty() {
                let exe = String::from_utf8_lossy(&self.p.exe).into_owned();
                out += &format!("sockets of {pid} {exe}: {}\n", socks.join(" "));
            }
        }
        self.switch_to(before);
        out
    }
}
