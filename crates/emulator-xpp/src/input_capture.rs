//! Idle XPP input capture, following the bounded XP5 idle model.
//! X-Pro2 5.11 0xb2a340 reads status at +0xc of 0x20-byte channels;
//! 0xb2a3a4 acknowledges bits 0/16 and 0xb29d00 disables via +0.
//! 0xb2a2c0 reads/modifies the bank interrupt mask at +0xa0, bits 0..7.
//! Channels 0..4 occupy one bank and 5..7 a second bank, selected by the
//! caller's explicit wiring. No external pulses are supplied in this model.
//! Active capture, unsupported words and malformed accesses refuse.
//! An explicit experimental sequence variant exposes bit 0 and the declared
//! bank IRQ when DMA writes the stock 0x00070000 buffer terminator. Its timing
//! and the relation between the retained bank mask and completion are not
//! physically validated; the idle variant never generates this completion.
use fujimod_emulator_devices::{Cx, Device};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct SequenceState {
    pending: u16,
    status: Vec<u32>,
}

/// DMA view of count preload words. X-Pro2's program at 0xe79140 (generated
/// in RAM) stores words from 0xf45aa4 to channel 2 +4. Values are retained;
/// active capture and pulse generation remain unsupported.
pub struct CounterSource {
    base: u64,
    channels: u64,
    counts: Arc<Mutex<Vec<u32>>>,
    sequence: Option<Arc<Mutex<SequenceState>>>,
}
impl CounterSource {
    fn index(&self, address: u64) -> usize {
        ((address - self.base) / 0x20) as usize
    }
}
impl fujimod_emulator_ip_arm::pl330::Source for CounterSource {
    fn contains(&self, address: u64, len: usize) -> bool {
        address
            .checked_sub(self.base)
            .is_some_and(|offset| len == 4 && offset / 0x20 < self.channels && offset % 0x20 == 4)
    }
    fn read(&self, address: u64, len: usize) -> Result<Vec<u8>, String> {
        if !self.contains(address, len) {
            return Err("unsupported input capture DMA read".into());
        }
        Ok(
            self.counts.lock().map_err(|_| "capture counts poisoned")?[self.index(address)]
                .to_le_bytes()
                .to_vec(),
        )
    }
    fn writable(&self) -> bool {
        true
    }
    fn write(&self, address: u64, bytes: &[u8]) -> Result<(), String> {
        if !self.contains(address, bytes.len()) {
            return Err("unsupported input capture DMA store".into());
        }
        let value = u32::from_le_bytes(bytes.try_into().map_err(|_| "capture preload width")?);
        self.counts.lock().map_err(|_| "capture counts poisoned")?[self.index(address)] = value;
        // Explicit experimental completion contract: the stock writer ends
        // its command buffer with 0x00070000. No external capture is invented.
        if value == 0x00070000 {
            if let Some(sequence) = &self.sequence {
                sequence.lock().map_err(|_| "capture sequence poisoned")?.pending |= 1 << self.index(address);
            }
        }
        Ok(())
    }
}

pub struct InputCapture {
    base: u64,
    channels: u64,
    interrupt_mask: u8,
    sequence_end_irq: Option<u32>,
    counts: Arc<Mutex<Vec<u32>>>,
    sequence: Option<Arc<Mutex<SequenceState>>>,
}
impl InputCapture {
    pub fn new(base: u64, channels: u64) -> Result<Self, String> {
        if base & 0xfff != 0 || !(1..=16).contains(&channels) {
            return Err("invalid XPP input-capture wiring".into());
        }
        Ok(Self {
            base,
            channels,
            interrupt_mask: 0,
            sequence_end_irq: None,
            counts: Arc::new(Mutex::new(vec![0; channels as usize])),
            sequence: None,
        })
    }
    /// Experimental output-sequence end, inferred from stock bcad04 and
    /// b28ee4. IRQ wiring is supplied by the graph; physical timing is unknown.
    pub fn with_sequence_end_irq(mut self, irq: u32) -> Result<Self, String> {
        if !(32..512).contains(&irq) { return Err("invalid sequence IRQ".into()); }
        self.sequence_end_irq = Some(irq);
        self.sequence = Some(Arc::new(Mutex::new(SequenceState { pending: 0, status: vec![0; self.channels as usize] })));
        Ok(self)
    }
    fn update_irq(&self, cx: &mut Cx<'_>) {
        if let (Some(irq), Some(sequence)) = (self.sequence_end_irq, &self.sequence) {
            cx.set_irq(irq, sequence.lock().unwrap().status.iter().any(|value| *value != 0));
        }
    }
    pub fn counter_source(&self) -> CounterSource {
        CounterSource {
            base: self.base,
            channels: self.channels,
            counts: self.counts.clone(),
            sequence: self.sequence.clone(),
        }
    }
    fn offset(&self, address: u64, size: u8) -> Option<u64> {
        let offset = address.checked_sub(self.base)?;
        (size == 4 && offset / 0x20 < self.channels).then_some(offset % 0x20)
    }
}
impl Device for InputCapture {
    fn name(&self) -> &'static str {
        if self.sequence_end_irq.is_some() { "xpp-input-capture-sequence" } else { "xpp-input-capture-idle" }
    }
    fn read(&mut self, cx: &mut Cx<'_>, address: u64, size: u8) -> u64 {
        if address == self.base + 0xa0 && size == 4 {
            return self.interrupt_mask.into();
        }
        if self.offset(address, size) == Some(4) {
            return self.counts.lock().unwrap()[((address - self.base) / 0x20) as usize].into();
        }
        if self.offset(address, size) == Some(0xc) {
            if let Some(sequence) = &self.sequence {
                return sequence.lock().unwrap().status[((address - self.base) / 0x20) as usize].into();
            }
        }
        if !matches!(self.offset(address, size), Some(0 | 4 | 0xc | 0x10)) {
            cx.unmodeled();
        }
        0
    }
    fn write(&mut self, cx: &mut Cx<'_>, address: u64, size: u8, value: u64) {
        if address == self.base + 0xa0 && size == 4 && value <= 0xff {
            self.interrupt_mask = value as u8;
            return;
        }
        match self.offset(address, size) {
            Some(0) if value == 0 => {
                if let Some(sequence) = &self.sequence {
                    let channel = ((address - self.base) / 0x20) as usize;
                    let mut state = sequence.lock().unwrap();
                    state.pending &= !(1 << channel);
                    state.status[channel] = 0;
                }
                self.update_irq(cx);
            }
            Some(0xc) if value & !0x10001 == 0 => {
                if let Some(sequence) = &self.sequence {
                    sequence.lock().unwrap().status[((address - self.base) / 0x20) as usize] &= !(value as u32);
                }
                self.update_irq(cx);
            }
            _ => cx.unmodeled(),
        }
    }
    fn advance(&mut self, cx: &mut Cx<'_>) {
        if let Some(sequence) = &self.sequence {
            let mut state = sequence.lock().unwrap();
            let pending = state.pending;
            state.pending = 0;
            for channel in 0..self.channels as usize {
                if pending & (1 << channel) != 0 { state.status[channel] |= 1; }
            }
        }
        self.update_irq(cx);
    }
    fn next_deadline(&self, now: u64) -> Option<u64> {
        self.sequence.as_ref().and_then(|state| (state.lock().unwrap().pending != 0).then_some(now))
    }
    fn pollable(&self, address: u64, size: u8) -> bool {
        matches!(self.offset(address, size), Some(0 | 4 | 0xc | 0x10))
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!({"base":self.base,"channels":self.channels,"interrupt_mask":self.interrupt_mask,"counts":self.counts.lock().unwrap().clone(),"sequence_end_irq":self.sequence_end_irq,"sequence":self.sequence.as_ref().map(|s|s.lock().unwrap().clone())})
    }
    fn checkpoint(&self) -> Result<fujimod_emulator_devices::checkpoint::Checkpoint, String> {
        fujimod_emulator_devices::checkpoint::Checkpoint::encode(
            self.name(),
            &(
                self.base,
                self.channels,
                self.interrupt_mask,
                self.counts.lock().unwrap().clone(),
                self.sequence_end_irq,
                self.sequence.as_ref().map(|s| s.lock().unwrap().clone()),
            ),
        )
    }
    fn fingerprint(&self) -> Result<serde_json::Value, String> {
        Ok(self.checkpoint()?.fingerprint())
    }
    fn validate_checkpoint(
        &self,
        saved: &fujimod_emulator_devices::checkpoint::Checkpoint,
    ) -> Result<(), String> {
        let (base, channels, _, counts, irq, sequence): (u64, u64, u8, Vec<u32>, Option<u32>, Option<SequenceState>) = saved.decode(self.name())?;
        if (base, channels) != (self.base, self.channels)
            || counts.len() != self.channels as usize
            || irq != self.sequence_end_irq
            || sequence.as_ref().map(|s| s.status.len()) != self.sequence.as_ref().map(|_| self.channels as usize)
            || sequence.as_ref().is_some_and(|s| s.pending as u32 >= (1u32 << self.channels) || s.status.iter().any(|v| v & !1 != 0))
            || !saved.children().is_empty()
        {
            return Err("input capture checkpoint wiring differs".into());
        }
        Ok(())
    }
    fn restore_checkpoint(
        &mut self,
        saved: &fujimod_emulator_devices::checkpoint::Checkpoint,
    ) -> Result<(), String> {
        self.validate_checkpoint(saved)?;
        let (_, _, mask, counts, _, sequence): (u64, u64, u8, Vec<u32>, Option<u32>, Option<SequenceState>) = saved.decode(self.name())?;
        self.interrupt_mask = mask;
        *self.counts.lock().unwrap() = counts;
        if let (Some(current), Some(saved)) = (&self.sequence, sequence) { *current.lock().unwrap() = saved; }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fujimod_emulator_devices::test_host::recording::RecordingHost;
    #[test]
    fn explicit_sequence_end_irq_survives_checkpoint_and_acknowledges() {
        use fujimod_emulator_ip_arm::pl330::Source;
        let mut model = InputCapture::new(0xfff2f000, 5).unwrap().with_sequence_end_irq(90).unwrap();
        let source = model.counter_source();
        let mut host = RecordingHost::default();
        source.write(0xfff2f044, &0x2c590001u32.to_le_bytes()).unwrap();
        assert_eq!(model.next_deadline(100), None);
        source.write(0xfff2f044, &0x00070000u32.to_le_bytes()).unwrap();
        assert_eq!(model.next_deadline(100), Some(100));
        assert!(host.irqs.is_empty());
        let pending = model.checkpoint().unwrap();
        let pending_fingerprint = model.fingerprint().unwrap();
        model.advance(&mut Cx::new(&mut host, 0, 0, 0));
        assert_eq!(model.read(&mut Cx::new(&mut host, 0, 0, 0), 0xfff2f04c, 4), 1);
        assert_eq!(host.irqs.last(), Some(&(90, true)));
        assert_eq!(model.next_deadline(100), None);
        model.write(&mut Cx::new(&mut host, 0, 0, 0), 0xfff2f04c, 4, 1);
        assert_eq!(host.irqs.last(), Some(&(90, false)));
        assert_ne!(model.fingerprint().unwrap(), pending_fingerprint);
        model.restore_checkpoint(&pending).unwrap();
        assert_eq!(model.fingerprint().unwrap(), pending_fingerprint);
        assert_eq!(model.next_deadline(100), Some(100));
        model.advance(&mut Cx::new(&mut host, 0, 0, 0));
        assert_eq!(host.irqs.last(), Some(&(90, true)));
        assert!(InputCapture::new(0xfff2f000, 5).unwrap().validate_checkpoint(&pending).is_err());
        let mut idle = InputCapture::new(0xfff2f000, 5).unwrap();
        idle.counter_source().write(0xfff2f044, &0x00070000u32.to_le_bytes()).unwrap();
        assert_eq!(idle.next_deadline(100), None);
        assert_eq!(idle.read(&mut Cx::new(&mut host, 0, 0, 0), 0xfff2f04c, 4), 0);
    }
    #[test]
    fn dma_preloads_only_counter_words_and_checkpoint_restores_shared_state() {
        use fujimod_emulator_ip_arm::pl330::Source;
        let mut d = InputCapture::new(0xfff2f000, 5).unwrap();
        let source = d.counter_source();
        for channel in 0..5 {
            assert_eq!(
                source.read(0xfff2f004 + channel * 0x20, 4).unwrap(),
                vec![0; 4]
            );
        }
        assert!(source.read(0xfff2f044, 8).is_err());
        assert!(source.read(0xfff2f04c, 4).is_err());
        assert!(source.read(0xfff2f0a4, 4).is_err());
        let saved = d.checkpoint().unwrap();
        assert!(source.writable());
        source.write(0xfff2f044, &[1, 2, 3, 4]).unwrap();
        let mut h = RecordingHost::default();
        assert_eq!(
            d.read(&mut Cx::new(&mut h, 0, 0, 0), 0xfff2f044, 4),
            0x04030201
        );
        d.restore_checkpoint(&saved).unwrap();
        assert_eq!(source.read(0xfff2f044, 4).unwrap(), vec![0; 4]);
        assert!(source.write(0xfff2f04c, &[1; 4]).is_err());
    }
    #[test]
    fn idle_banks_acknowledge_without_inventing_samples_and_refuse_active_capture() {
        let mut h = RecordingHost::default();
        for (base, channels) in [(0xfff2f000, 5), (0xfff30000, 3)] {
            let mut d = InputCapture::new(base, channels).unwrap();
            for ch in 0..channels {
                let mut cx = Cx::new(&mut h, 0, 0, 0);
                let at = base + ch * 0x20;
                d.write(&mut cx, at, 4, 0);
                d.write(&mut cx, at + 0xc, 4, 0x10001);
                assert_eq!(d.read(&mut cx, at + 0xc, 4), 0);
                assert_eq!(cx.refusal(), None);
            }
            for (at, width, value) in [
                (base, 4, 1),
                (base + 0xc, 4, 2),
                (base, 2, 0),
                (base + channels * 0x20 + 4, 4, 0),
            ] {
                let mut cx = Cx::new(&mut h, 0, 0, 0);
                d.write(&mut cx, at, width, value);
                assert!(cx.is_unmodeled());
            }
            assert!(!d.pollable(base - 4, 4));
            assert!(!d.pollable(base + 8, 4));
            let mut cx = Cx::new(&mut h, 0, 0, 0);
            d.write(&mut cx, base + 0xa0, 4, 0x81);
            assert_eq!(d.read(&mut cx, base + 0xa0, 4), 0x81);
            assert_eq!(cx.refusal(), None);
            let saved = d.checkpoint().unwrap();
            d.interrupt_mask = 0;
            d.restore_checkpoint(&saved).unwrap();
            assert_eq!(d.interrupt_mask, 0x81);
            assert!(
                InputCapture::new(base, channels + 1)
                    .unwrap()
                    .restore_checkpoint(&saved)
                    .is_err()
            );
            assert_eq!(d.next_deadline(0), None);
        }
        assert!(h.events.is_empty());
        assert!(h.irqs.is_empty());
        assert!(InputCapture::new(1, 5).is_err());
        assert!(InputCapture::new(0xfff2f000, 0).is_err());
    }
}
