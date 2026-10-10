//! Machine state snapshot for v86-style `save_state` / `initial_state`.
//!
//! Everything but RAM is serialised here: the shared state (generic timer,
//! GIC, RTC) and each CPU (registers, system registers, power state). RAM is
//! saved by the existing v86 state code, which reads it directly out of wasm
//! memory, so it is not duplicated in this blob.
//!
//! Layout (version 2): magic, version, RAM size, CPU count, counter, GIC,
//! RTC, then one block per CPU.

use std::sync::atomic::Ordering;

use crate::board::Host;
use crate::cpu::{Cpu, SysRegs};
use crate::smp;

const MAGIC: &[u8; 4] = b"ARML";
const VERSION: u32 = 2;

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
    /// Snapshot of a single-CPU machine.
    pub fn save_cpu_state(&mut self) -> Vec<u8> {
        assert_eq!(self.machine.ncpus, 1, "use save_machine_state");
        Self::save_machine_state(&mut [self])
    }

    /// Snapshot of the machine whose CPUs (all of them, in order, stopped) are `cpus`.
    pub fn save_machine_state(cpus: &mut [&mut Cpu<H>]) -> Vec<u8> {
        let counter = cpus[0].counter();
        let m = cpus[0].machine.clone();
        let mut w = W(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.u32(VERSION);
        w.u64(m.mem.len() as u64);
        w.u32(m.ncpus as u32);
        w.u64(counter);
        m.gic.lock().save(&mut w.0);
        {
            let rtc = m.rtc.lock();
            w.u64(rtc.offset as u64);
            w.u32(rtc.match_value);
            w.u8(rtc.enabled as u8 | (rtc.irq_mask as u8) << 1 | (rtc.raw_irq as u8) << 2);
        }
        for cpu in cpus.iter() {
            cpu.save_one(&mut w);
        }
        w.0
    }

    fn save_one(&self, w: &mut W) {
        let me = &self.machine.cpus[self.index];
        w.u32(me.power.load(Ordering::SeqCst));
        w.u64(me.entry.load(Ordering::SeqCst));
        w.u64(me.context.load(Ordering::SeqCst));
        for r in self.x {
            w.u64(r);
        }
        for q in self.vreg {
            w.u64(q as u64);
            w.u64((q >> 64) as u64);
        }
        w.u64(self.pc);
        w.u64(self.sp_el0);
        w.u64(self.sp_el1);
        w.u64(self.pstate());
        w.u64(self.insn_count);
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
        w.u8(self.excl().is_some() as u8);
        w.u64(self.excl().unwrap_or(0));
        w.u64(self.excl_value);
        w.u64(self.excl_value2);
    }

    /// Restore a single-CPU machine.
    pub fn load_cpu_state(&mut self, data: &[u8]) -> Result<(), String> {
        Self::load_machine_state(&mut [self], data)
    }

    /// Restore the machine whose CPUs (all of them, in order, stopped) are `cpus`.
    pub fn load_machine_state(cpus: &mut [&mut Cpu<H>], data: &[u8]) -> Result<(), String> {
        Self::try_load(cpus, data).ok_or_else(|| "corrupt or truncated CPU state".to_string())?
    }

    fn try_load(cpus: &mut [&mut Cpu<H>], data: &[u8]) -> Option<Result<(), String>> {
        let m = cpus[0].machine.clone();
        let mut r = R(data);
        if r.take(4)? != MAGIC {
            return Some(Err("not an armless CPU state".into()));
        }
        let version = r.u32()?;
        if version != VERSION {
            return Some(Err(format!("unsupported CPU state version {version}")));
        }
        let mem = r.u64()?;
        if mem != m.mem.len() as u64 {
            return Some(Err(format!(
                "state was saved with {} MiB of RAM but this machine has {} MiB",
                mem >> 20,
                m.mem.len() >> 20
            )));
        }
        let ncpus = r.u32()? as usize;
        if ncpus != m.ncpus || ncpus != cpus.len() {
            return Some(Err(format!(
                "state was saved with {ncpus} CPUs but this machine has {}",
                m.ncpus
            )));
        }
        let counter = r.u64()?;
        let mut rest = r.0;
        m.gic.lock().load(&mut rest)?;
        let mut r = R(rest);
        {
            let mut rtc = m.rtc.lock();
            rtc.offset = r.u64()? as i64;
            rtc.match_value = r.u32()?;
            let f = r.u8()?;
            rtc.enabled = f & 1 != 0;
            rtc.irq_mask = f & 2 != 0;
            rtc.raw_irq = f & 4 != 0;
        }
        for cpu in cpus.iter_mut() {
            cpu.load_one(&mut r)?;
        }
        // The guest's counter continues from where it was saved.
        m.timer_ns_base
            .store(cpus[0].host.now_ns(), Ordering::SeqCst);
        m.timer_cnt_base.store(counter, Ordering::SeqCst);
        m.stop.store(smp::STOP_NONE, Ordering::SeqCst);
        Some(Ok(()))
    }

    fn load_one(&mut self, r: &mut R) -> Option<()> {
        let me = &self.machine.cpus[self.index];
        me.power.store(r.u32()?, Ordering::SeqCst);
        me.entry.store(r.u64()?, Ordering::SeqCst);
        me.context.store(r.u64()?, Ordering::SeqCst);
        for i in 0..31 {
            self.x[i] = r.u64()?;
        }
        for i in 0..32 {
            let lo = r.u64()? as u128;
            let hi = r.u64()? as u128;
            self.vreg[i] = lo | hi << 64;
        }
        self.pc = r.u64()?;
        self.sp_el0 = r.u64()?;
        self.sp_el1 = r.u64()?;
        let pstate = r.u64()?;
        self.insn_count = r.u64()?;
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
        self.set_excl(if has_excl { Some(excl) } else { None });
        self.excl_value = r.u64()?;
        self.excl_value2 = r.u64()?;

        // Restore PSTATE (EL, SPSel, DAIF, NZCV).
        self.set_nzcv(pstate);
        self.daif = (pstate as u32) & (0xf << 6);
        match pstate & 0xf {
            0 => self.el = 0,
            4 => {
                self.el = 1;
                self.spsel = false
            },
            _ => {
                self.el = 1;
                self.spsel = true
            },
        }
        self.tlb_flush();
        // RAM was replaced: no translated block can be trusted.
        self.jit.flush_all();
        self.jit_entry = true;
        self.tlb_sync_gen = 0;
        self.machine.cpus[self.index].tlb_seen.store(
            self.machine.tlb_gen.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.set_irq_dirty();
        Some(())
    }
}
