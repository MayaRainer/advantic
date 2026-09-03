use crate::{Bits, Interrupts};

#[derive(Default)]
struct Timer {
	registers: [u8; 4],
	current: u16,
}

#[derive(Default)]
pub struct Timers {
	timers: [Timer; 4],
}

fn timer_index(address: u32) -> usize {
	((address - 0x100) / 4) as usize
}

impl Timers {
	pub fn step(&mut self, interrupts: &mut Interrupts, cycle: u32) {
		let mut overflow;
		for (i, timer) in self.timers.iter_mut().enumerate() {
			let control = timer.registers[2];
			let divider = match control & 0b11 {
				0b00 => 1,
				0b01 => 64,
				0b10 => 256,
				0b11 => 1024,
				_ => unreachable!(),
			};
			if control.bit(7) && cycle.is_multiple_of(divider) {
				timer.current = timer.current.wrapping_add(1);
				overflow = timer.current == 0;
				if overflow {
					timer.current = u16::from_le_bytes(timer.registers[0..2].try_into().unwrap());
					if control.bit(6) {
						interrupts.interrupt(3 + i as u32);
					}
				}
			}
		}
	}

	pub fn read_register(&self, address: u32) -> u32 {
		let timer = &self.timers[timer_index(address)];
		u32::from_le_bytes(timer.registers) & 0xffff_0000 | u32::from(timer.current)
	}

	pub fn write_register(&mut self, address: u32, value: u8) {
		let timer = &mut self.timers[timer_index(address)];
		let index = (address % 4) as usize;
		if index == 3 && !timer.registers[index].bit(6) && value.bit(6) {
			timer.current = u16::from_le_bytes(timer.registers[0..2].try_into().unwrap());
		}
		timer.registers[index] = value;
	}
}
