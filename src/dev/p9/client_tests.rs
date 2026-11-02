//! 9P2000.L as a Linux client speaks it: whole sessions against the server, and the queue
//! side (requests parked for blobs, flushed, answered on arrival, carried through a snapshot).

use std::sync::Arc;

use super::*;

const NOW: u64 = 1_700_000_000;

fn io() -> Io {
    Io::new(Arc::new(crate::smp::Machine::new(1 << 20, 1)), || NOW)
}

fn msg(ty: u8, tag: u16, body: impl FnOnce(&mut W)) -> Vec<u8> {
    let mut b = W::new();
    body(&mut b);
    let mut w = W::new();
    w.u32(7 + b.0.len() as u32);
    w.u8(ty);
    w.u16(tag);
    w.0.extend_from_slice(&b.0);
    w.0
}

fn s(w: &mut W, v: &str) {
    put_str(w, v.as_bytes());
}

struct Client {
    p: P9,
    io: Io,
}

impl Client {
    fn new() -> Client {
        Client {
            p: P9::new("share", Fs::empty()),
            io: io(),
        }
    }

    /// The reply's body, or the errno of an Rlerror.
    fn call(&mut self, ty: u8, body: impl FnOnce(&mut W)) -> Result<Vec<u8>, u32> {
        let r = self
            .p
            .handle(&self.io, &msg(ty, 9, body))
            .expect("nothing waits for a blob");
        assert_eq!(
            u32::from_le_bytes(r[..4].try_into().unwrap()) as usize,
            r.len(),
            "size field"
        );
        assert_eq!(u16::from_le_bytes([r[5], r[6]]), 9, "tag echoed");
        if r[4] == TLERROR + 1 {
            return Err(u32::from_le_bytes(r[7..11].try_into().unwrap()));
        }
        assert_eq!(r[4], ty + 1);
        Ok(r[7..].to_vec())
    }

    fn ok(&mut self, ty: u8, body: impl FnOnce(&mut W)) -> Vec<u8> {
        self.call(ty, body)
            .unwrap_or_else(|e| panic!("type {ty}: errno {e}"))
    }

    fn version(&mut self, msize: u32, v: &str) -> (u32, String) {
        let r = self.ok(TVERSION, |w| {
            w.u32(msize);
            s(w, v);
        });
        let mut r = R(&r);
        (
            r.u32().unwrap(),
            String::from_utf8(take_str(&mut r)).unwrap(),
        )
    }

    fn attach(&mut self, fid: u32, uid: u32) -> Vec<u8> {
        self.ok(TATTACH, |w| {
            w.u32(fid);
            w.u32(!0);
            s(w, "user");
            s(w, "");
            w.u32(uid);
        })
    }

    /// The qids walked (as many as matched).
    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<usize, u32> {
        let r = self.call(TWALK, |w| {
            w.u32(fid);
            w.u32(newfid);
            w.u16(names.len() as u16);
            for n in names {
                s(w, n);
            }
        })?;
        let n = u16::from_le_bytes([r[0], r[1]]) as usize;
        assert_eq!(r.len(), 2 + 13 * n);
        Ok(n)
    }

    fn create(&mut self, fid: u32, name: &str, mode: u32, gid: u32) -> Result<Vec<u8>, u32> {
        self.call(TLCREATE, |w| {
            w.u32(fid);
            s(w, name);
            w.u32(2); // O_RDWR
            w.u32(mode);
            w.u32(gid);
        })
    }

    fn mkdir(&mut self, fid: u32, name: &str) {
        self.ok(TMKDIR, |w| {
            w.u32(fid);
            s(w, name);
            w.u32(0o755);
            w.u32(0);
        });
    }

    fn open(&mut self, fid: u32, flags: u32) -> Result<Vec<u8>, u32> {
        self.call(TLOPEN, |w| {
            w.u32(fid);
            w.u32(flags);
        })
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> u32 {
        let r = self.ok(TWRITE, |w| {
            w.u32(fid);
            w.u64(offset);
            w.u32(data.len() as u32);
            w.0.extend_from_slice(data);
        });
        u32::from_le_bytes(r[..4].try_into().unwrap())
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, u32> {
        let r = self.call(TREAD, |w| {
            w.u32(fid);
            w.u64(offset);
            w.u32(count);
        })?;
        let n = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
        assert_eq!(r.len(), 4 + n);
        Ok(r[4..].to_vec())
    }

    fn getattr(&mut self, fid: u32) -> Result<Attr, u32> {
        let r = self.call(TGETATTR, |w| {
            w.u32(fid);
            w.u64(0x7ff);
        })?;
        assert_eq!(r.len(), 8 + 13 + 12 + 5 * 8 + 4 * 16 + 16);
        let mut r = R(&r);
        assert_eq!(r.u64(), Some(0x7ff));
        let qid_type = r.u8().unwrap();
        r.take(12).unwrap();
        let (mode, uid, gid) = (r.u32().unwrap(), r.u32().unwrap(), r.u32().unwrap());
        let (nlink, rdev, size, _blksize, blocks) = (
            r.u64().unwrap(),
            r.u64().unwrap(),
            r.u64().unwrap(),
            r.u64().unwrap(),
            r.u64().unwrap(),
        );
        let mut t = || (r.u64().unwrap(), r.u64().unwrap());
        let (atime, mtime, ctime) = (t(), t(), t());
        Ok(Attr {
            qid_type,
            mode,
            uid,
            gid,
            nlink,
            rdev,
            size,
            blocks,
            atime,
            mtime,
            ctime,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        fid: u32,
        valid: u32,
        mode: u32,
        uid: u32,
        gid: u32,
        size: u64,
        at: u64,
        mt: u64,
    ) -> Result<(), u32> {
        self.call(TSETATTR, |w| {
            w.u32(fid);
            w.u32(valid);
            w.u32(mode);
            w.u32(uid);
            w.u32(gid);
            w.u64(size);
            w.u64(at);
            w.u64(5);
            w.u64(mt);
            w.u64(6);
        })
        .map(|r| assert!(r.is_empty()))
    }

    /// (name, offset to continue from) for one Treaddir.
    fn readdir(&mut self, fid: u32, offset: u64, count: u32) -> Vec<(String, u64)> {
        let r = self.ok(TREADDIR, |w| {
            w.u32(fid);
            w.u64(offset);
            w.u32(count);
        });
        let n = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
        assert_eq!(r.len(), 4 + n);
        assert!(n <= count as usize, "{n} bytes for a count of {count}");
        let mut r = R(&r[4..]);
        let mut out = Vec::new();
        while r.take(13).is_some() {
            let off = r.u64().unwrap();
            r.u8().unwrap();
            out.push((String::from_utf8(take_str(&mut r)).unwrap(), off));
        }
        out
    }

    fn unlink(&mut self, dir: u32, name: &str, flags: u32) -> Result<Vec<u8>, u32> {
        self.call(TUNLINKAT, |w| {
            w.u32(dir);
            s(w, name);
            w.u32(flags);
        })
    }
}

#[derive(Debug)]
struct Attr {
    qid_type: u8,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    rdev: u64,
    size: u64,
    blocks: u64,
    atime: (u64, u64),
    mtime: (u64, u64),
    ctime: (u64, u64),
}

fn take_str(r: &mut R) -> Vec<u8> {
    let n = r.u16().unwrap() as usize;
    r.take(n).unwrap().to_vec()
}

#[test]
fn version_sets_the_message_size_and_starts_the_session_over() {
    let mut c = Client::new();
    assert_eq!(
        c.version(1 << 30, "9P2000.L"),
        (MAX_MSIZE, "9P2000.L".into())
    );
    assert_eq!(c.version(100, "9P2000.L"), (4096, "9P2000.L".into()));
    assert_eq!(c.version(8192, "9P2000.u").1, "unknown");
    c.attach(1, 0);
    assert!(c.walk(1, 2, &[]).is_ok());
    c.version(8192, "9P2000.L");
    assert_eq!(
        c.walk(1, 2, &[]),
        Err(fs::EBADF),
        "fids from before a Tversion are gone"
    );
    // The mount tag, as config space spells it
    let tag: Vec<u8> = (0..8).map(|i| c.p.config_read(i)).collect();
    assert_eq!(tag, b"\x05\x00share\x00");
}

#[test]
fn files_written_read_back_and_owned_by_who_attached() {
    let mut c = Client::new();
    c.version(4096, "9P2000.L");
    let root = c.attach(1, 1000);
    assert_eq!(
        (root[0], u64::from_le_bytes(root[5..13].try_into().unwrap())),
        (0x80, fs::ROOT)
    );
    c.walk(1, 2, &[]).unwrap();
    let r = c.create(2, "f", fs::S_IFDIR | 0o640, 50).unwrap();
    assert_eq!(r[0], 0, "a file whatever the mode's type says");
    assert_eq!(
        u32::from_le_bytes(r[13..17].try_into().unwrap()),
        4096 - 24,
        "iounit"
    );
    assert_eq!(c.write(2, 0, b"hello world"), 11);
    assert_eq!(c.read(2, 6, 100).unwrap(), b"world");
    assert_eq!(c.read(2, 50, 10).unwrap(), b"");
    let a = c.getattr(2).unwrap();
    assert_eq!(
        (a.mode, a.uid, a.gid, a.nlink, a.size, a.blocks),
        (fs::S_IFREG | 0o640, 1000, 50, 1, 11, 1)
    );
    assert_eq!((a.mtime, a.ctime), ((NOW, 0), (NOW, 0)));
    // A read is no bigger than the message size allows
    c.write(2, 0, &[7; 6000]);
    assert_eq!(c.read(2, 0, 6000).unwrap().len(), 4096 - 11);
    // Opened again with O_TRUNC: empty
    c.walk(1, 3, &["f"]).unwrap();
    c.open(3, O_TRUNC).unwrap();
    assert_eq!(c.getattr(3).unwrap().size, 0);
    // The same name again
    c.walk(1, 4, &[]).unwrap();
    assert_eq!(c.create(4, "f", 0o644, 0).unwrap_err(), fs::EEXIST);
    // Directories are not read as files, nor truncated on open
    c.mkdir(1, "d");
    c.walk(1, 5, &["d"]).unwrap();
    c.open(5, O_TRUNC).unwrap();
    assert_eq!(c.read(5, 0, 10), Err(fs::EISDIR));
    // Clunked fids are gone; clunking one twice is an error
    c.ok(TCLUNK, |w| w.u32(3));
    assert_eq!(c.getattr(3).unwrap_err(), fs::EBADF);
    assert_eq!(c.call(TCLUNK, |w| w.u32(3)), Err(fs::EBADF));
}

#[test]
fn walks_bind_the_new_fid_only_when_every_name_was_found() {
    let mut c = Client::new();
    c.attach(1, 0);
    c.mkdir(1, "a");
    c.walk(1, 2, &["a"]).unwrap();
    c.mkdir(2, "b");
    assert_eq!(c.walk(1, 3, &["a", "b"]), Ok(2));
    assert_eq!(c.getattr(3).unwrap().qid_type, 0x80);
    assert_eq!(
        c.walk(1, 4, &["a", "nope", "b"]),
        Ok(1),
        "the qids up to the miss"
    );
    assert_eq!(c.getattr(4).unwrap_err(), fs::EBADF, "and no fid");
    assert_eq!(
        c.walk(1, 4, &["nope"]),
        Err(fs::ENOENT),
        "a miss on the first is an error"
    );
    assert_eq!(c.walk(3, 4, &[".."]), Ok(1));
    assert_eq!(c.walk(9, 4, &[]), Err(fs::EBADF));
    // Walking onto an existing fid rebinds it
    c.walk(1, 3, &[]).unwrap();
    assert_eq!(c.getattr(3).unwrap().mode, fs::S_IFDIR | 0o755);
}

#[test]
fn directories_list_a_page_at_a_time_from_where_the_last_left_off() {
    let mut c = Client::new();
    c.attach(1, 0);
    let names: Vec<String> = (0..40)
        .map(|i| format!("file-with-a-long-name-{i:02}"))
        .collect();
    for n in &names {
        c.walk(1, 2, &[]).unwrap();
        c.create(2, n, 0o644, 0).unwrap();
    }
    let mut seen = Vec::new();
    let mut offset = 0;
    for _ in 0..100 {
        let page = c.readdir(1, offset, 300);
        if page.is_empty() {
            break;
        }
        assert!(page.len() < 42, "a page holds what fits in the count");
        offset = page.last().unwrap().1;
        seen.extend(page.into_iter().map(|(n, _)| n));
    }
    let mut want = vec![".".to_string(), "..".to_string()];
    want.extend(names);
    assert_eq!(seen, want, "every entry once, in order");
    // A count larger than a message holds is cut to it
    c.version(4096, "9P2000.L");
    c.attach(1, 0);
    let all: usize = c
        .readdir(1, 0, 1 << 20)
        .iter()
        .map(|(n, _)| 24 + n.len())
        .sum();
    assert!(all <= 4096 - 11, "{all}");
}

#[test]
fn links_renames_and_removals() {
    let mut c = Client::new();
    c.attach(1, 0);
    c.mkdir(1, "d");
    c.walk(1, 2, &["d"]).unwrap();
    c.walk(2, 3, &[]).unwrap();
    c.create(3, "f", 0o600, 0).unwrap();
    c.write(3, 0, b"x");
    // Symlinks
    c.ok(TSYMLINK, |w| {
        w.u32(1);
        s(w, "l");
        s(w, "d/f");
        w.u32(0);
    });
    c.walk(1, 4, &["l"]).unwrap();
    let r = c.ok(TREADLINK, |w| w.u32(4));
    assert_eq!(take_str(&mut R(&r)), b"d/f");
    assert_eq!(c.getattr(4).unwrap().mode, fs::S_IFLNK | 0o777);
    assert_eq!(
        c.call(TREADLINK, |w| w.u32(3)),
        Err(fs::EINVAL),
        "not a symlink"
    );
    // Hard links share the inode
    c.ok(TLINK, |w| {
        w.u32(1);
        w.u32(3);
        s(w, "hard");
    });
    assert_eq!(c.getattr(3).unwrap().nlink, 2);
    c.walk(1, 5, &["hard"]).unwrap();
    assert_eq!(c.read(5, 0, 10).unwrap(), b"x");
    // Rename across directories
    c.ok(TRENAMEAT, |w| {
        w.u32(1);
        s(w, "hard");
        w.u32(2);
        s(w, "moved");
    });
    assert_eq!(c.walk(1, 6, &["hard"]), Err(fs::ENOENT));
    assert_eq!(c.walk(1, 6, &["d", "moved"]), Ok(2));
    assert_eq!(
        c.call(TRENAME, |w| w.u32(1)),
        Err(fs::EOPNOTSUPP),
        "Trenameat instead"
    );
    // Removal: files, then the directory once empty
    assert_eq!(c.unlink(1, "d", AT_REMOVEDIR), Err(fs::ENOTEMPTY));
    c.unlink(2, "f", 0).unwrap();
    assert_eq!(c.getattr(3).unwrap().nlink, 1);
    c.unlink(2, "moved", 0).unwrap();
    assert_eq!(
        c.read(3, 0, 10).unwrap(),
        b"x",
        "an open fid keeps its file"
    );
    c.unlink(1, "d", AT_REMOVEDIR).unwrap();
    assert_eq!(c.unlink(1, "d", AT_REMOVEDIR), Err(fs::ENOENT));
    // Tremove is not done (Linux uses Tunlinkat), but the fid is clunked as 9P says
    assert_eq!(c.call(TREMOVE, |w| w.u32(4)), Err(fs::EOPNOTSUPP));
    assert_eq!(c.getattr(4).unwrap_err(), fs::EBADF);
    // Device nodes keep their numbers
    c.ok(TMKNOD, |w| {
        w.u32(1);
        s(w, "null");
        w.u32(0o020666);
        w.u32(1);
        w.u32(3);
        w.u32(0);
    });
    c.walk(1, 7, &["null"]).unwrap();
    let a = c.getattr(7).unwrap();
    assert_eq!((a.mode, a.rdev), (0o020666, 1 << 8 | 3));
}

#[test]
fn setattr_changes_only_what_it_says_is_valid() {
    let mut c = Client::new();
    c.attach(1, 0);
    c.walk(1, 2, &[]).unwrap();
    c.create(2, "f", 0o644, 0).unwrap();
    c.write(2, 0, b"0123456789");
    c.setattr(2, 1, 0o4711 | fs::S_IFDIR, 7, 8, 0, 0, 0)
        .unwrap();
    let a = c.getattr(2).unwrap();
    assert_eq!(
        (a.mode, a.uid, a.gid),
        (fs::S_IFREG | 0o4711, 0, 0),
        "the type is not the client's to change"
    );
    c.setattr(2, 2 | 4, 0, 7, 8, 0, 0, 0).unwrap();
    let a = c.getattr(2).unwrap();
    assert_eq!((a.mode & 0o7777, a.uid, a.gid), (0o4711, 7, 8));
    c.setattr(2, 8, 0, 0, 0, 4, 0, 0).unwrap();
    assert_eq!(c.read(2, 0, 100).unwrap(), b"0123");
    c.setattr(2, 16 | 128 | 32, 0, 0, 0, 0, 111, 222).unwrap();
    let a = c.getattr(2).unwrap();
    assert_eq!(
        (a.atime, a.mtime, a.ctime),
        ((111, 5), (NOW, 0), (NOW, 0)),
        "atime given, mtime now"
    );
    c.setattr(2, 32 | 256, 0, 0, 0, 0, 0, 333).unwrap();
    assert_eq!(c.getattr(2).unwrap().mtime, (333, 6));
    assert_eq!(c.setattr(1, 8, 0, 0, 0, 0, 0, 0), Err(fs::EISDIR));
    assert_eq!(c.setattr(9, 1, 0, 0, 0, 0, 0, 0), Err(fs::EBADF));
}

#[test]
fn the_rest_of_what_linux_asks_is_answered_as_it_expects() {
    let mut c = Client::new();
    c.attach(1, 0);
    let r = c.ok(TSTATFS, |w| w.u32(1));
    assert_eq!(r.len(), 4 + 4 + 6 * 8 + 4);
    assert_eq!(
        &r[..8],
        &[0x97, 0x19, 0x02, 0x01, 0, 0x10, 0, 0],
        "V9FS_MAGIC, 4 KiB blocks"
    );
    assert_eq!(c.ok(TFSYNC, |w| w.u32(1)), b"");
    assert_eq!(c.ok(TLOCK, |w| w.u32(1)), [0], "locks always succeed");
    let r = c.ok(TGETLOCK, |w| {
        w.u32(1);
        w.u8(1);
        w.u64(10);
        w.u64(20);
        w.u32(42);
        s(w, "host");
    });
    let mut r = R(&r);
    assert_eq!(
        (r.u8(), r.u64(), r.u64(), r.u32()),
        (Some(2), Some(10), Some(20), Some(42)),
        "unlocked"
    );
    assert_eq!(take_str(&mut r), b"host");
    assert_eq!(c.call(TXATTRWALK, |w| w.u32(1)), Err(fs::ENODATA));
    assert_eq!(c.call(TXATTRWALK, |w| w.u32(9)), Err(fs::EBADF));
    assert_eq!(c.call(TXATTRCREATE, |w| w.u32(1)), Err(fs::EOPNOTSUPP));
    assert_eq!(c.call(TFLUSH, |w| w.u16(3)), Ok(vec![]), "nothing to flush");
    assert_eq!(c.call(200, |_| {}), Err(fs::EOPNOTSUPP));
    // Short and truncated messages
    assert_eq!(c.call(TGETATTR, |w| w.u16(1)), Err(fs::EINVAL));
    assert_eq!(
        c.call(TWALK, |w| {
            w.u32(1);
            w.u32(2);
            w.u16(1);
            w.u16(10);
            w.0.extend_from_slice(b"abc");
        }),
        Err(fs::EINVAL)
    );
    assert_eq!(c.p.handle(&c.io, &[1, 2, 3]), Ok(vec![]));
    for bad in [
        &b""[..],
        &b"\x00"[..],
        &b"name/with/slash"[..],
        &[b'x'; 256][..],
    ] {
        c.walk(1, 2, &[]).unwrap();
        let r = c.call(TLCREATE, |w| {
            w.u32(2);
            put_str(w, bad);
            w.u32(0);
            w.u32(0o644);
            w.u32(0);
        });
        assert!(r.is_err() || bad == b"\x00", "{bad:?}");
    }
}

#[test]
fn fids_and_their_owners_survive_a_snapshot() {
    let mut c = Client::new();
    c.version(16384, "9P2000.L");
    c.attach(1, 1000);
    c.walk(1, 2, &[]).unwrap();
    let mut w = W::new();
    c.p.save(&mut w);
    let mut owners = W::new();
    c.p.save_owners(&mut owners);
    for with_owners in [true, false] {
        let mut back = Client::new();
        back.p.load(&mut R(&w.0)).unwrap();
        if with_owners {
            back.p.load_owners(&mut R(&owners.0)).unwrap();
        }
        back.create(2, "f", 0o644, 0).unwrap();
        assert_eq!(
            back.getattr(2).unwrap().uid,
            if with_owners { 1000 } else { 0 }
        );
        assert_eq!(back.read(2, 0, 1 << 20).unwrap(), b"");
        assert_eq!(back.p.msize, 16384);
    }
    // A snapshot cut short does not load
    assert!(Client::new()
        .p
        .load(&mut R(&w.0[..w.0.len() - 1]))
        .is_none());
}

/// A 9P queue in guest RAM: descriptors at 0x1000, available ring at 0x2000, used at 0x3000.
struct Ring {
    io: Io,
    t: Transport,
    offered: u16,
}

impl Ring {
    fn new() -> Ring {
        let mut t = Transport::new(40, P9::FEATURES, &[128]);
        let q = &mut t.queues[0];
        (q.desc, q.avail, q.used, q.ready) = (0x1000, 0x2000, 0x3000, true);
        Ring {
            io: io(),
            t,
            offered: 0,
        }
    }

    fn desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let d = 0x1000 + 16 * i as u64;
        self.io.write(d, &addr.to_le_bytes());
        self.io.write_u32(d + 8, len);
        self.io.write_u16(d + 12, flags);
        self.io.write_u16(d + 14, next);
    }

    /// Offer a request with a reply buffer; returns its head.
    fn offer(&mut self, p: &mut P9, request: &[u8]) -> u16 {
        let head = 2 * self.offered;
        let at = 0x10000 + 0x2000 * self.offered as u64;
        self.io.write(at, request);
        self.desc(head, at, request.len() as u32, 1, head + 1);
        self.desc(head + 1, at + 0x1000, 0x1000, 2, 0);
        self.io
            .write_u16(0x2000 + 4 + 2 * self.offered as u64, head);
        self.offered += 1;
        self.io.write_u16(0x2002, self.offered);
        p.notify(&self.io, &mut self.t, 0);
        head
    }

    /// Replies given back so far: (head, reply type, tag, body).
    fn used(&self) -> Vec<(u16, u8, u16, Vec<u8>)> {
        (0..self.io.read_u16(0x3002) as u64)
            .map(|k| {
                let head = self.io.read_u32(0x3004 + 8 * k) as u16;
                let len = self.io.read_u32(0x3008 + 8 * k) as usize;
                let mut r = vec![0; len];
                self.io
                    .read(0x10000 + 0x1000 * head as u64 + 0x1000, &mut r);
                (
                    head,
                    r[4],
                    u16::from_le_bytes([r[5], r[6]]),
                    r[7..].to_vec(),
                )
            })
            .collect()
    }

    fn blobs(&self) -> Vec<String> {
        self.io
            .out
            .drain()
            .into_iter()
            .filter_map(|e| match e {
                Event::Blob(b) => Some(b),
                _ => None,
            })
            .collect()
    }
}

fn read_msg(tag: u16, fid: u32) -> Vec<u8> {
    msg(TREAD, tag, |w| {
        w.u32(fid);
        w.u64(0);
        w.u32(64);
    })
}

#[test]
fn reads_wait_for_their_blob_and_can_be_flushed_while_they_do() {
    let json = br#"{"version":3,"fsroot":[["f",3,0,33188,0,0,"blob-a"]]}"#;
    let mut p = P9::new("t", Fs::from_json(json).unwrap());
    let mut ring = Ring::new();
    let f = p.fs.lookup_path("/f").unwrap();
    p.set_fid(7, f, 0);
    let first = ring.offer(&mut p, &read_msg(1, 7));
    let second = ring.offer(&mut p, &read_msg(2, 7));
    assert!(ring.used().is_empty(), "both wait");
    assert_eq!(ring.blobs(), ["blob-a"], "asked for once");
    assert_eq!(p.wanted_blobs(), ["blob-a"]);
    // The guest gives up on the first: it is answered (interrupted), then the flush
    let flush = ring.offer(&mut p, &msg(TFLUSH, 3, |w| w.u16(1)));
    let used = ring.used();
    assert_eq!(used.len(), 2);
    assert_eq!(
        (used[0].0, used[0].1, used[0].2, &used[0].3[..]),
        (first, TLERROR + 1, 1, &4u32.to_le_bytes()[..])
    );
    assert_eq!((used[1].0, used[1].1, used[1].2), (flush, TFLUSH + 1, 3));
    // A snapshot keeps the one still waiting, and asks for its blob again
    let mut w = W::new();
    p.save(&mut w);
    let mut back = P9::new("t", Fs::empty());
    back.load(&mut R(&w.0)).unwrap();
    assert_eq!(back.wanted_blobs(), ["blob-a"]);
    // The blob arrives: the second is answered with its data
    for p in [&mut p, &mut back] {
        p.fs.provide_blob("blob-a", b"xyz".to_vec()).unwrap();
    }
    p.blob_arrived(&ring.io, &mut ring.t, "blob-a");
    let used = ring.used();
    assert_eq!(used.len(), 3);
    assert_eq!((used[2].0, used[2].1, used[2].2), (second, TREAD + 1, 2));
    assert_eq!(&used[2].3[4..], b"xyz");
    assert!(p.wanted_blobs().is_empty());
    // The restored server answers it too, into the same buffers
    let mut ring2 = Ring {
        io: io(),
        t: Transport::new(40, P9::FEATURES, &[128]),
        offered: 0,
    };
    let q = &mut ring2.t.queues[0];
    (q.desc, q.avail, q.used, q.ready) = (0x1000, 0x2000, 0x3000, true);
    back.blob_arrived(&ring2.io, &mut ring2.t, "blob-a");
    assert_eq!(ring2.io.read_u16(0x3002), 1);
    assert_eq!(ring2.io.read_u32(0x3004) as u16, second);
    // A blob that cannot be had is an error, not a wait forever
    let json = br#"{"version":3,"fsroot":[["g",3,0,33188,0,0,"blob-b"]]}"#;
    let mut p = P9::new("t", Fs::from_json(json).unwrap());
    let mut ring = Ring::new();
    let g = p.fs.lookup_path("/g").unwrap();
    p.set_fid(1, g, 0);
    ring.offer(&mut p, &read_msg(5, 1));
    p.fs.blob_failed("blob-b");
    p.blob_arrived(&ring.io, &mut ring.t, "blob-b");
    let used = ring.used();
    assert_eq!(
        (used[0].1, &used[0].3[..]),
        (TLERROR + 1, &fs::EIO.to_le_bytes()[..])
    );
    // Reset lets go of fids and of what waited
    ring.offer(&mut p, &read_msg(6, 1));
    p.reset();
    assert!(p.wanted_blobs().is_empty());
    assert!(p.fid(1).is_err());
}
