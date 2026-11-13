//! /proc/<pid>: each process's `cmdline`, `comm`, `stat` and `status`, as Linux gives them, for
//! every process there is (an ended one nobody waited for too), `self` and `thread-self` being the
//! one that asks. /proc itself lists them (getdents64) after what the filesystem's /proc
//! directory holds and `self`: busybox's ps, pgrep and pkill read /proc so, and so does a shell's
//! `for f in /proc/[0-9]*/cmdline`.
//!
//! A descriptor of /proc counts its position as any directory's does through the filesystem's
//! entries and `self`; from `PID_POS` on it is `PID_POS` plus the pid of the last process given,
//! so a listing taken in pieces goes on after that one whichever processes came or went since.

use armless::dev::fs::{Ino, ROOT};

use crate::files::{self, encode_dirents};
use crate::{Proc, Process};

/// Where the processes' entries start in a descriptor of /proc's position
const PID_POS: u64 = 1 << 32;
/// The inode numbers /proc's entries of its own have, above any of the filesystem's
const PID_INO: u64 = 1 << 48;
const DT_DIR: u8 = 4;
const DT_LNK: u8 = 10;
/// How long a comm is at most (TASK_COMM_LEN, less its NUL)
const COMM_LEN: usize = 15;

impl Process {
    /// The process a /proc path's first component names: `self` and `thread-self` the one that
    /// asks, a number one that is there.
    fn proc_named(&self, name: &[u8]) -> Option<&Proc> {
        match name {
            b"self" | b"thread-self" => Some(&self.p),
            _ if is_pid(name) => self.proc_ref(std::str::from_utf8(name).ok()?.parse().ok()?),
            _ => None,
        }
    }

    /// A file of /proc/<pid>/ made here (None for a path that isn't one)
    pub(crate) fn pid_file(&self, path: &[u8]) -> Option<Vec<u8>> {
        let rest = path.strip_prefix(b"/proc/")?;
        let slash = rest.iter().position(|&b| b == b'/')?;
        let p = self.proc_named(&rest[..slash])?;
        Some(match &rest[slash + 1..] {
            b"cmdline" => cmdline(p),
            b"comm" => comm(p).into_iter().chain(*b"\n").collect(),
            b"stat" => self.stat_line(p).into_bytes(),
            b"status" => self.status_text(p).into_bytes(),
            _ => return None,
        })
    }

    /// Whether `path` is /proc/<pid> (or self, or thread-self): Some(there is such a process),
    /// None for any other path
    pub(crate) fn pid_dir(&self, path: &[u8]) -> Option<bool> {
        let name = path.strip_prefix(b"/proc/")?;
        let name = name.strip_suffix(b"/").unwrap_or(name);
        let pidlike = matches!(name, b"self" | b"thread-self") || is_pid(name);
        pidlike.then(|| self.proc_named(name).is_some())
    }

    /// Whether `ino` is the filesystem's /proc, which getdents64 lists the processes in
    pub(crate) fn is_proc_dir(&self, ino: Ino) -> bool {
        self.fs(|fs| fs.lookup(ROOT, b"proc").ok() == Some(ino))
            .unwrap_or(false)
    }

    /// /proc made in the filesystem if it has none, for a program to open and list it
    pub(crate) fn make_proc_dir(&self) {
        self.fs(|fs| {
            if fs.lookup(ROOT, b"proc").is_err() {
                let _ = fs.create(ROOT, b"proc", armless::dev::fs::S_IFDIR | 0o555, 0, 0, 0);
            }
        });
    }

    /// getdents64 of /proc (`ino`) from `pos`: its entries in the filesystem, `self`, then a
    /// directory for each process
    pub(crate) fn proc_dirents(
        &self,
        ino: Ino,
        pos: u64,
        max: usize,
    ) -> Result<(Vec<u8>, u64), i64> {
        let own = self
            .fs(|fs| -> Result<Vec<_>, i64> {
                let entries = fs.list(ino).map_err(|_| -files::ENOTDIR)?;
                let n = entries.len() as u64;
                let mut own: Vec<_> = entries
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, ino))| {
                        (
                            ino,
                            i as u64 + 1,
                            fs.get(ino).map_or(0, |i| files::dtype(i.mode)),
                            name,
                        )
                    })
                    .collect();
                own.push((PID_INO, n + 1, DT_LNK, b"self".to_vec()));
                Ok(own)
            })
            .ok_or(-files::EBADF)??;
        let mut pids: Vec<i64> = self.proc_list().map(|p| p.pid).collect();
        pids.sort_unstable();
        let procs = pids.into_iter().map(|pid| {
            (
                PID_INO + pid as u64,
                PID_POS + pid as u64,
                DT_DIR,
                pid.to_string().into_bytes(),
            )
        });
        let after = own.into_iter().chain(procs).filter(|e| e.1 > pos);
        encode_dirents(after, pos, max)
    }

    /// R for a process that runs or may (the one that asks, or one with a thread that isn't
    /// parked in a call), S for one whose threads all wait, Z for one that ended
    fn state(&self, p: &Proc) -> (char, &'static str) {
        if p.zombie.is_some() {
            return ('Z', "zombie");
        }
        let runnable = self
            .threads
            .iter()
            .any(|t| t.pid == p.pid && !t.exited && t.wait.is_none());
        if p.pid == self.p.pid || runnable {
            ('R', "running")
        } else {
            ('S', "sleeping")
        }
    }

    /// Its threads that haven't ended (one for a process that has, as Linux says)
    fn thread_count(&self, p: &Proc) -> usize {
        let n = self
            .threads
            .iter()
            .filter(|t| t.pid == p.pid && !t.exited)
            .count();
        n.max(1)
    }

    /// /proc/<pid>/stat: its 52 fields, zero where nothing is known (times, faults, memory)
    fn stat_line(&self, p: &Proc) -> String {
        let (state, _) = self.state(p);
        let comm = String::from_utf8_lossy(&comm(p)).into_owned();
        let threads = self.thread_count(p);
        // pid (comm) state ppid pgrp session tty_nr tpgid flags, minflt to cstime (8), priority
        // nice num_threads itrealvalue starttime vsize rss
        let mut s = format!(
            "{} ({comm}) {state} {} {} {} 0 -1 0 {}20 0 {threads} 0 0 0 0",
            p.pid,
            p.ppid,
            p.pgid,
            p.sid,
            "0 ".repeat(8)
        );
        // rsslim to cnswap (13), exit_signal, then processor to exit_code (14)
        s += &" 0".repeat(13);
        s += &format!(" {}", p.exit_signal);
        s += &" 0".repeat(14);
        s.push('\n');
        s
    }

    /// /proc/<pid>/status: the lines busybox and procps read
    fn status_text(&self, p: &Proc) -> String {
        let (state, word) = self.state(p);
        let ids = |v: [u32; 3]| format!("{}\t{}\t{}\t{}", v[0], v[1], v[2], v[1]);
        let groups: String = p.groups.iter().map(|g| format!("{g} ")).collect();
        format!(
            "Name:\t{}\nUmask:\t{:04o}\nState:\t{state} ({word})\nTgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\nGid:\t{}\nFDSize:\t64\nGroups:\t{groups}\nThreads:\t{}\n",
            String::from_utf8_lossy(&comm(p)),
            p.umask,
            p.ppid,
            ids(p.creds.uid),
            ids(p.creds.gid),
            self.thread_count(p),
            pid = p.pid,
        )
    }
}

/// Whether a name is a number, as a process's directory is named
fn is_pid(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(u8::is_ascii_digit)
}

/// /proc/<pid>/cmdline: its arguments, each with its NUL; nothing once it has ended, and its
/// executable's path when no arguments were kept (a state saved before they were)
fn cmdline(p: &Proc) -> Vec<u8> {
    if p.zombie.is_some() {
        Vec::new()
    } else if p.cmdline.is_empty() {
        p.exe.iter().copied().chain([0]).collect()
    } else {
        p.cmdline.clone()
    }
}

/// Its name: the last component of argv[0] (of the executable's path if it has none), cut to
/// 15 bytes. (`sleep` for busybox's applet, which Linux too names by what was run.)
fn comm(p: &Proc) -> Vec<u8> {
    comm_of(&p.cmdline, &p.exe)
}

fn comm_of(cmdline: &[u8], exe: &[u8]) -> Vec<u8> {
    let arg0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    let path = if arg0.is_empty() { exe } else { arg0 };
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or_default();
    base[..base.len().min(COMM_LEN)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_is_argv0s_last_component_cut_to_15_bytes() {
        assert_eq!(comm_of(b"sleep\x004167\0", b"/bin/busybox"), b"sleep");
        assert_eq!(comm_of(b"/usr/bin/prog\0x\0", b"/x"), b"prog");
        assert_eq!(
            comm_of(b"", b"/usr/lib/postgresql/bin/postgres"),
            b"postgres"
        );
        assert_eq!(
            comm_of(b"/a/a-very-long-program-name\0", b""),
            b"a-very-long-pro"
        );
    }
}
