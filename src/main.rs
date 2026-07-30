#![warn(clippy::pedantic)]
#![allow(
	clippy::too_many_lines,
	clippy::similar_names,
	clippy::cast_possible_truncation,
	clippy::missing_errors_doc,
	clippy::missing_panics_doc,
	clippy::struct_excessive_bools
)]

mod cpu;

trait MemoryBus {
	fn read(&self, address: u32) -> u32;
	fn write(&mut self, address: u32, value: u32, size: u8);
}

#[inline]
fn bit(value: u32, bit: u32) -> bool {
	(value & (1 << bit)) != 0
}

fn set_bit(value: u32, bit: u32, set: bool) -> u32 {
	let bit = 1 << bit;
	if set { value | bit } else { value & !bit }
}

struct System {
	ram: Vec<u8>,
	rom: Vec<u8>,
}

enum MemoryRegion {
	Rom,
	Ram,
}

fn parse_memory_address(address: u32, alignment: u8) -> Option<(MemoryRegion, u32)> {
	let address = address & !(u32::from(alignment) - 1);
	Some(match address {
		0x0200_0000..0x0204_0000 => (MemoryRegion::Ram, address - 0x0200_0000),
		0x0300_0000..0x0300_8000 => (MemoryRegion::Ram, address - 0x0300_0000 + 0x40000),
		0x0800_0000..0x0A00_0000 => (MemoryRegion::Rom, address - 0x0800_0000),
		_ => return None
	})
}

fn read_bytes(target: &[u8], index: u32) -> u32 {
	let index = index as usize;
	u32::from_le_bytes(target[index..=index + 3].try_into().unwrap())
}

fn write_bytes(target: &mut [u8], index: u32, value: u32, size: u8) {
	for i in 0..size {
		target[(index + u32::from(i)) as usize] = (value >> (i * 8)) as u8;
	}
}

impl MemoryBus for System {
	fn read(&self, address: u32) -> u32 {
		let Some((region, address)) = parse_memory_address(address, 4) else {
			eprintln!("reading from unknown address: {address:08x}");
			return 0
		};
		match region {
			MemoryRegion::Ram => read_bytes(&self.ram, address),
			MemoryRegion::Rom => read_bytes(&self.rom, address),
		}
	}

	fn write(&mut self, address: u32, value: u32, size: u8) {
		let Some((region, address)) = parse_memory_address(address, size) else {
			eprintln!("writing to unknown address: {address:08x}");
			return;
		};
		match region {
			MemoryRegion::Ram => write_bytes(&mut self.ram, address, value, size),
			MemoryRegion::Rom => panic!("Attempt to write to ROM"),
		}
	}
}

fn main() {
	let path = std::env::args().nth(1).expect("No path given");
	let rom = std::fs::read(path).expect("ROM file not found");
	let mut cpu = cpu::Cpu::new();
	let mut sys = System { rom, ram: vec![0; 0x48000] };
	loop {
		let mut cycle = cpu.cycle;
		cpu.step(&mut sys);
		loop {
			if cycle == cpu.cycle {
				break;
			}
			if cycle == 0 {
				std::thread::sleep(std::time::Duration::from_millis(100));
			}
			cycle = cycle.wrapping_add(1);
		}
	}
}
