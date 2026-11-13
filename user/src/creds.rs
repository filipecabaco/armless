//! A process's user and group ids (`Creds`), as Linux's setuid family changes them. A program
//! starts as root (uid and gid 0): root may take any ids; a process that isn't root may only move
//! among its real, effective and saved ones. su, runuser and pg_ctl's own su become another user
//! this way, and Postgres refuses to start as root, so getuid and geteuid have to say what was
//! set. The ids own the files a process makes; nothing here checks permissions by them.

use crate::{Process, Ret};

const EPERM: i64 = 1;
const EFAULT: i64 = 14;
/// -1: an id the call leaves as it is
const KEEP: u32 = u32::MAX;

/// The ids `ids` would be after `set` (real, effective, saved; KEEP leaves one), when `root` may
/// take any and anyone else only those it has. None: not allowed.
fn change(ids: [u32; 3], set: [u32; 3], root: bool) -> Option<[u32; 3]> {
    let mut out = ids;
    for (i, &v) in set.iter().enumerate() {
        if v == KEEP {
            continue;
        }
        if !root && !ids.contains(&v) {
            return None;
        }
        out[i] = v;
    }
    Some(out)
}

impl Process {
    /// setuid, setgid, setreuid, setregid, setresuid, setresgid, setfsuid, setfsgid, getresuid,
    /// getresgid (143 to 152)
    pub(crate) fn sys_ids(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let c = self.p.creds;
        let root = c.euid() == 0;
        let id = |x: u64| x as u32;
        let (uid, new) = match nr {
            // setuid(uid): root sets all three; anyone else only the effective, to its real or saved
            146 => {
                let u = id(a[0]);
                if u == KEEP {
                    return v(-(crate::files::EINVAL));
                }
                (true, if root { Some([u; 3]) } else if c.uid[0] == u || c.uid[2] == u {
                    Some([c.uid[0], u, c.uid[2]])
                } else {
                    None
                })
            },
            144 => {
                let g = id(a[0]);
                if g == KEEP {
                    return v(-(crate::files::EINVAL));
                }
                (false, if root { Some([g; 3]) } else if c.gid[0] == g || c.gid[2] == g {
                    Some([c.gid[0], g, c.gid[2]])
                } else {
                    None
                })
            },
            // setreuid(r, e), setregid(r, e): the saved one follows the effective when the real
            // changes or the effective becomes other than the old real
            145 | 143 => {
                let user = nr == 145;
                let ids = if user { c.uid } else { c.gid };
                let (r, e) = (id(a[0]), id(a[1]));
                let real_ok = r == KEEP || root || r == ids[0] || r == ids[1];
                let new = change(ids, [KEEP, e, KEEP], root)
                    .filter(|_| real_ok)
                    .map(|mut n| {
                        if r != KEEP {
                            n[0] = r;
                        }
                        if r != KEEP || e != KEEP && e != ids[0] {
                            n[2] = n[1];
                        }
                        n
                    });
                (user, new)
            },
            // setresuid(r, e, s), setresgid(r, e, s)
            147 | 149 => {
                let user = nr == 147;
                let ids = if user { c.uid } else { c.gid };
                (user, change(ids, [id(a[0]), id(a[1]), id(a[2])], root))
            },
            // setfsuid, setfsgid: the filesystem id is the effective one here; they say what it was
            151 => return v(c.euid() as i64),
            152 => return v(c.egid() as i64),
            // getresuid(r, e, s), getresgid(r, e, s)
            148 | 150 => {
                let ids = if nr == 148 { c.uid } else { c.gid };
                let ok = (0..3).all(|i| self.put_u32(a[i], ids[i]));
                return v(if ok { 0 } else { -EFAULT });
            },
            _ => return v(-(crate::files::EINVAL)),
        };
        let Some(new) = new else {
            return v(-EPERM);
        };
        if uid {
            self.p.creds.uid = new;
        } else {
            self.p.creds.gid = new;
        }
        v(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_takes_any_ids_and_others_only_their_own() {
        assert_eq!(change([0, 0, 0], [70, 70, 70], true), Some([70; 3]));
        assert_eq!(change([70, 70, 70], [0, KEEP, KEEP], false), None);
        assert_eq!(change([70, 0, 0], [KEEP, 70, KEEP], false), Some([70, 70, 0]));
        assert_eq!(change([70, 70, 0], [KEEP, 0, KEEP], false), Some([70, 0, 0]));
    }
}
