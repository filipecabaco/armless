//! The process state's encoding: little-endian u64s and length-prefixed bytes.

pub(crate) struct W(pub Vec<u8>);

impl W {
    pub fn u(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    pub fn b(&mut self, d: &[u8]) {
        self.u(d.len() as u64);
        self.0.extend_from_slice(d);
    }
}

pub(crate) struct R<'a>(pub &'a [u8]);

impl<'a> R<'a> {
    pub fn u(&mut self) -> Result<u64, String> {
        let (v, rest) = self
            .0
            .split_first_chunk::<8>()
            .ok_or("truncated process state")?;
        self.0 = rest;
        Ok(u64::from_le_bytes(*v))
    }

    pub fn b(&mut self) -> Result<&'a [u8], String> {
        let n = self.u()? as usize;
        let (v, rest) = self
            .0
            .split_at_checked(n)
            .ok_or("truncated process state")?;
        self.0 = rest;
        Ok(v)
    }
}
