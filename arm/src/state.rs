//! CPU state snapshot for v86-style `save_state` / `initial_state`.
//!
//! Only CPU-side state is serialised here (registers, system registers, GIC,
//! RTC, timer). RAM is saved by the existing v86 state code, which reads it
//! directly out of wasm memory, so it is not duplicated in this blob.

use crate::board::Host;
use crate::cpu::{Cpu, SysRegs};

const MAGIC: &[u8; 4] = b"ARML";
const VERSION: u32 = 1;

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

macro_rules! sys_fields {
    ($m:ident) => {
        $m!(sctlr actlr cpacr ttbr0 ttbr1 tcr spsr elr esr far par mair amair vbar contextidr
            tpidr_el1 tpidr_el0 tpidrro_el0 cntkctl cntv_ctl cntv_cval cntp_ctl cntp_cval mdscr
            csselr fpcr fpsr afsr0 afsr1)
    };
}

impl<H: Host> Cpu<H> {
    pub fn save_cpu_state(&mut self) -> Vec<u8> {
        let counter = self.counter();
        let mut w = W(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.u32(VERSION);
        w.u64(self.mem.len() as u64);
        for r in self.x {
            w.u64(r);
        }
        w.u64(self.pc);
        w.u64(self.sp_el0);
        w.u64(self.sp_el1);
        w.u64(self.pstate());
        w.u64(self.insn_count);
        w.u64(counter);
        let s = &self.sys;
        macro_rules! save {
            ($($f:ident)*) => { $( w.u64(s.$f); )* };
        }
        sys_fields!(save);
        w.u32(s.other.len() as u32);
        for (&k, &v) in &s.other {
            w.u32(k);
            w.u64(v);
        }
        w.u8(self.excl.is_some() as u8);
        w.u64(self.excl.unwrap_or(0));
        self.gic.save(&mut w.0);
        w.u64(self.rtc.offset as u64);
        w.u32(self.rtc.match_value);
        w.u8(self.rtc.enabled as u8 | (self.rtc.irq_mask as u8) << 1 | (self.rtc.raw_irq as u8) << 2);
        w.0
    }

    pub fn load_cpu_state(&mut self, data: &[u8]) -> Result<(), String> {
        self.try_load(data).ok_or_else(|| "corrupt or truncated CPU state".to_string())?
    }

    fn try_load(&mut self, data: &[u8]) -> Option<Result<(), String>> {
        let mut r = R(data);
        if r.take(4)? != MAGIC {
            return Some(Err("not an armless CPU state".into()));
        }
        let version = r.u32()?;
        if version != VERSION {
            return Some(Err(format!("unsupported CPU state version {version}")));
        }
        let mem = r.u64()?;
        if mem != self.mem.len() as u64 {
            return Some(Err(format!(
                "state was saved with {} MiB of RAM but this machine has {} MiB",
                mem >> 20,
                self.mem.len() >> 20
            )));
        }
        for i in 0..31 {
            self.x[i] = r.u64()?;
        }
        self.pc = r.u64()?;
        self.sp_el0 = r.u64()?;
        self.sp_el1 = r.u64()?;
        let pstate = r.u64()?;
        self.insn_count = r.u64()?;
        let counter = r.u64()?;
        let mut s = SysRegs::default();
        macro_rules! load {
            ($($f:ident)*) => { $( s.$f = r.u64()?; )* };
        }
        sys_fields!(load);
        let n = r.u32()?;
        for _ in 0..n {
            let k = r.u32()?;
            let v = r.u64()?;
            s.other.insert(k, v);
        }
        self.sys = s;
        let has_excl = r.u8()? != 0;
        let excl = r.u64()?;
        self.excl = if has_excl { Some(excl) } else { None };
        let mut rest = r.0;
        self.gic.load(&mut rest)?;
        let mut r = R(rest);
        self.rtc.offset = r.u64()? as i64;
        self.rtc.match_value = r.u32()?;
        let f = r.u8()?;
        self.rtc.enabled = f & 1 != 0;
        self.rtc.irq_mask = f & 2 != 0;
        self.rtc.raw_irq = f & 4 != 0;

        // Restore PSTATE (EL, SPSel, DAIF, NZCV).
        self.set_nzcv(pstate);
        self.daif = (pstate as u32) & (0xf << 6);
        match pstate & 0xf {
            0 => self.el = 0,
            4 => {
                self.el = 1;
                self.spsel = false
            }
            _ => {
                self.el = 1;
                self.spsel = true
            }
        }
        // The guest's counter continues from where it was saved.
        self.timer_ns_base = self.host.now_ns();
        self.timer_cnt_base = counter;
        self.tlb_flush();
        self.irq_dirty = true;
        Some(Ok(()))
    }
}
