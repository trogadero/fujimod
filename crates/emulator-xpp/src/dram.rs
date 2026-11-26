//! X-Processor Pro DRAM controller words the running firmware uses.
//!
//! X-T20 2.11 0xc8f09c reads bits 8..10 of +0xec, the device's refresh-rate
//! level (taken from SDRAM mode register 4), and rewrites the low half of the
//! refresh words +0x9c and +0xa0: 0xc30 below level 4, 0x618 at level 4. The
//! level is a declared input; the other +0xec bits read 0. +0x9c and +0xa0
//! read back their last write and start at 0, since the boot code that
//! programs them before the kernel is not modeled.
use fujimod_emulator_devices::{Cx, Device};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Register window size in bytes.
pub const WINDOW: u64 = 0x1000;
const REFRESH: [u64; 2] = [0x9c, 0xa0];
const STATUS: u64 = 0xec;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    refresh: [u32; 2],
    words: std::collections::BTreeMap<u64, u32>,
    due: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    training_due: Option<u64>,
}
pub struct Controller {
    base: u64,
    refresh_level: u8,
    state: State,
    settle_ticks: Option<u64>,
    training_result: Option<u32>,
}
impl Controller {
    pub fn new(base: u64, refresh_level: u8) -> Result<Self, String> {
        if base & 0xfff != 0 || refresh_level > 7 {
            return Err("Invalid X-Processor Pro DRAM controller base or refresh level".into());
        }
        Ok(Self {
            base,
            refresh_level,
            state: State::default(),
            settle_ticks: None,
            training_result: None,
        })
    }
    /// Permit the controller's cold initialization sequence.
    pub fn with_startup(mut self, ticks: u64) -> Result<Self, String> {
        if ticks == 0 {
            return Err("DRAM startup clock must be positive".into());
        }
        self.settle_ticks = Some(ticks);
        Ok(self)
    }
    /// Declared training outcome for a cold-boot laboratory board. The X-Pro2
    /// stage at NOR+0x16c requests training with +0xe4 bit24 and polls +0xe8
    /// for0x1f (complete) or0x1b (retry). This is not a measured chip latency.
    pub fn with_training_result(mut self, result: Option<u32>) -> Result<Self, String> {
        if result.is_some_and(|r| !matches!(r, 0x1b | 0x1f))
            || result.is_some() && self.settle_ticks.is_none() {
            return Err("Invalid declared DRAM training result".into());
        }
        self.training_result = result;
        Ok(self)
    }
    fn startup_word(&self, offset: u64) -> bool {
        self.settle_ticks.is_some()
            && offset & 3 == 0
            && (offset < 0x2e8 || (0x400..0x718).contains(&offset))
    }
}
impl Device for Controller {
    fn name(&self) -> &'static str {
        "xpp-dram-controller"
    }
    fn read(&mut self, cx: &mut Cx<'_>, address: u64, size: u8) -> u64 {
        let offset = address.wrapping_sub(self.base);
        if size != 4 {
            cx.unmodeled();
            return 0;
        }
        u64::from(match offset {
            STATUS => u32::from(self.refresh_level) << 8,
            o => match REFRESH.iter().position(|r| *r == o) {
                Some(n) => self.state.refresh[n],
                None if self.startup_word(o) => self.state.words.get(&o).copied().unwrap_or(0),
                None => {
                    cx.unmodeled();
                    0
                }
            },
        })
    }
    fn write(&mut self, cx: &mut Cx<'_>, address: u64, size: u8, value: u64) {
        let offset = address.wrapping_sub(self.base);
        match REFRESH.iter().position(|r| *r == offset) {
            Some(n) if size == 4 && value <= u32::MAX.into() => {
                self.state.refresh[n] = value as u32
            }
            _ if size == 4 && value <= u32::MAX.into() && self.startup_word(offset) => {
                let before = self.state.words.get(&offset).copied().unwrap_or(0);
                self.state.words.insert(
                    offset,
                    if offset == 0x14c {
                        value as u32 & !8 | before & 8
                    } else {
                        value as u32
                    },
                );
                if offset == 0 && value & 1 != 0 {
                    self.state.due = cx.now().checked_add(self.settle_ticks.unwrap());
                    if self.state.due.is_none() {
                        cx.fail("DRAM startup deadline overflow");
                    }
                }
                if offset == 0xe4 && value & (1 << 24) != 0 && self.training_result.is_some() {
                    self.state.words.insert(0xe8, 0);
                    self.state.training_due = cx.now().checked_add(self.settle_ticks.unwrap());
                    if self.state.training_due.is_none() { cx.fail("DRAM training deadline overflow"); }
                }
            }
            _ => cx.unmodeled(),
        }
    }
    fn advance(&mut self, cx: &mut Cx<'_>) {
        if self.state.due.is_some_and(|at| at <= cx.now()) {
            self.state.due = None;
            *self.state.words.entry(0x14c).or_default() |= 8;
        }
        if self.state.training_due.is_some_and(|at| at <= cx.now()) {
            self.state.training_due = None;
            self.state.words.insert(0xe8, self.training_result.unwrap());
        }
    }
    fn next_deadline(&self, _: u64) -> Option<u64> {
        self.state.due.into_iter().chain(self.state.training_due).min()
    }
    fn save(&self) -> Value {
        serde_json::to_value(&self.state).unwrap_or(Value::Null)
    }
    fn restore(&mut self, value: &Value) -> Result<(), String> {
        self.state = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        Ok(())
    }
    fujimod_emulator_devices::checkpoint_state!(state: State, Self::check);
}
impl Controller {
    /// Programmed words exist only where the startup law allows them, and a
    /// pending startup needs a controller that performs one.
    fn check(&self, state: &State) -> Result<(), String> {
        if state.words.keys().any(|&o| !self.startup_word(o))
            || state.due.is_some() && self.settle_ticks.is_none()
            || state.training_due.is_some() && self.training_result.is_none()
        {
            return Err("Invalid X-Processor Pro DRAM controller checkpoint".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fujimod_emulator_devices::test_host::{TestHost, read, write};

    const BASE: u64 = 0xfff0e000;

    #[test]
    fn declared_training_follows_request_and_clock_and_survives_restore() {
        for result in [0x1b, 0x1f] {
            let mut dram = Controller::new(BASE, 2).unwrap().with_startup(48).unwrap()
                .with_training_result(Some(result)).unwrap();
            let mut host = TestHost::new();
            assert_eq!(read(&mut dram, &mut host, BASE + 0xe8), 0);
            write(&mut dram, &mut host, BASE + 0xe4, 1 << 24);
            assert_eq!(dram.next_deadline(0), Some(48));
            let pending = dram.save();
            host.now = 47;
            dram.advance(&mut host.cx());
            assert_eq!(read(&mut dram, &mut host, BASE + 0xe8), 0);
            host.now = 48;
            dram.advance(&mut host.cx());
            assert_eq!(read(&mut dram, &mut host, BASE + 0xe8), result as u64);
            dram.restore(&pending).unwrap();
            dram.advance(&mut host.cx());
            assert_eq!(read(&mut dram, &mut host, BASE + 0xe8), result as u64);
        }
        assert!(Controller::new(BASE, 2).unwrap().with_training_result(Some(0x1f)).is_err());
        assert!(Controller::new(BASE, 2).unwrap().with_startup(48).unwrap()
            .with_training_result(Some(0xff)).is_err());
    }

    #[test]
    fn refresh_level_reads_in_bits_8_to_10_and_refresh_words_read_back() {
        let mut dram = Controller::new(BASE, 2).unwrap();
        let mut h = TestHost::new();
        assert_eq!(read(&mut dram, &mut h, BASE + 0xec) >> 8 & 7, 2);
        // 0xc8f190: keep the high half, refresh interval 0xc30 in the low half.
        write(&mut dram, &mut h, BASE + 0x9c, 0xc30);
        write(&mut dram, &mut h, BASE + 0xa0, 0x1_0c30);
        let saved = dram.save();
        write(&mut dram, &mut h, BASE + 0xa0, 0);
        dram.restore(&saved).unwrap();
        assert_eq!(read(&mut dram, &mut h, BASE + 0x9c), 0xc30);
        assert_eq!(read(&mut dram, &mut h, BASE + 0xa0), 0x1_0c30);
        let mut cx = h.cx();
        dram.read(&mut cx, BASE + 0x10, 4);
        assert!(cx.is_unmodeled());
        assert!(Controller::new(BASE, 8).is_err());
    }
}
