//! JIT profiles: which code pages a machine compiled, so that another machine restored from the
//! same snapshot can compile them as soon as it starts instead of interpreting them until they
//! get hot. Profiles from several runs merge, so one learns what the runs of a snapshot
//! actually do (whatever they do) rather than what a build guessed they would.

/// A compiled page: its page key (physical address, EL and SPSel, as `Jit::key` without the
/// offset), a hash of its contents, how many runs compiled it, and its entry points (offsets).
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub key: u64,
    pub hash: u64,
    pub runs: u32,
    pub entries: Vec<u16>,
}

/// FNV-1a over the page, a word at a time.
pub fn hash(page: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &w in page.as_chunks::<8>().0 {
        h ^= u64::from_le_bytes(w);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const MAGIC: &[u8; 8] = b"ARMJITP2";

/// Merge `new` into `into`: a page seen again counts another run and gains its entry points.
/// The result is ordered by how many runs compiled each page, most first (the order to prewarm
/// in), and keeps at most `max` pages.
pub fn merge(into: &mut Vec<Page>, new: &[Page], max: usize) {
    for p in new {
        match into.iter_mut().find(|q| q.key == p.key && q.hash == p.hash) {
            Some(q) => {
                q.runs = q.runs.saturating_add(p.runs);
                q.entries.extend_from_slice(&p.entries);
                q.entries.sort_unstable();
                q.entries.dedup();
            },
            None => into.push(p.clone()),
        }
    }
    into.sort_by(|a, b| b.runs.cmp(&a.runs).then(a.key.cmp(&b.key)));
    into.truncate(max);
}

pub fn write(pages: &[Page]) -> Vec<u8> {
    let mut out = Vec::from(MAGIC.as_slice());
    for p in pages {
        out.extend_from_slice(&p.key.to_le_bytes());
        out.extend_from_slice(&p.hash.to_le_bytes());
        out.extend_from_slice(&p.runs.to_le_bytes());
        out.extend_from_slice(&(p.entries.len() as u16).to_le_bytes());
        for e in &p.entries {
            out.extend_from_slice(&e.to_le_bytes());
        }
    }
    out
}

pub fn read(data: &[u8]) -> Result<Vec<Page>, String> {
    let mut r = data
        .strip_prefix(MAGIC.as_slice())
        .ok_or("not a JIT profile")?;
    let mut take = |n: usize| -> Result<&[u8], String> {
        if r.len() < n {
            return Err("JIT profile: truncated".into());
        }
        let (a, b) = r.split_at(n);
        r = b;
        Ok(a)
    };
    let mut pages = Vec::new();
    while let Ok(key) = take(8) {
        let key = u64::from_le_bytes(key.try_into().unwrap());
        let hash = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let runs = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let n = u16::from_le_bytes(take(2)?.try_into().unwrap()) as usize;
        let entries = take(n * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        pages.push(Page {
            key,
            hash,
            runs,
            entries,
        });
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_counts_runs_and_round_trips() {
        let a = Page {
            key: 0x4000,
            hash: 7,
            runs: 1,
            entries: vec![4, 8],
        };
        let b = Page {
            key: 0x8000,
            hash: 9,
            runs: 1,
            entries: vec![0],
        };
        let mut all = vec![a.clone()];
        merge(
            &mut all,
            &[
                b.clone(),
                Page {
                    entries: vec![12],
                    ..a.clone()
                },
            ],
            16,
        );
        assert_eq!(
            all[0],
            Page {
                key: 0x4000,
                hash: 7,
                runs: 2,
                entries: vec![4, 8, 12]
            }
        );
        assert_eq!(all[1], b);
        assert_eq!(read(&write(&all)).unwrap(), all);
        merge(&mut all, &[], 1);
        assert_eq!(all.len(), 1);
    }
}
