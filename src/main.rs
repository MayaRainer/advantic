#![warn(clippy::pedantic)]
#![allow(
	clippy::too_many_lines,
	clippy::similar_names,
	clippy::cast_possible_truncation,
	clippy::missing_errors_doc,
	clippy::missing_panics_doc,
	clippy::struct_excessive_bools,
	clippy::too_many_arguments
)]

mod cpu;
mod ppu;
mod timer;

use crate::timer::Timers;
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use std::collections::HashSet;
use std::ops::Add;
use std::time::{Duration, Instant};
use crate::ppu::{LINE_CYCLES, TOTAL_LINES};

trait Bits {
	fn bit(&self, bit: Self) -> bool;
	fn set_bit(&mut self, bit: Self, set: bool);
}

macro_rules! bits {
	($t: ty) => {
		impl Bits for $t {
			fn bit(&self, bit: Self) -> bool {
				(self & (1 << bit)) != 0
			}

			fn set_bit(&mut self, bit: Self, set: bool) {
				let bit = 1 << bit;
				*self = if set { *self | bit } else { *self & !bit }
			}
		}
	};
}
bits!(u8);
bits!(u16);
bits!(u32);

struct Interrupts(u32);

impl Interrupts {
	fn interrupt(&mut self, kind: u32) {
		if self.0.bit(kind) {
			self.0.set_bit(kind + 16, true);
		}
	}

	fn write_register(&mut self, address: u32, value: u8) {
		let shift = (address - 0x200) * 8;
		match address {
			0x200..0x202 => self.0 = self.0 & !(0xff << shift) | (u32::from(value) << shift),
			_ => self.0 &= !(u32::from(value) << shift),
		}
	}
}

#[derive(Default, Clone, Copy)]
struct DmaChannel {
	pub index: usize,
	pub source: u32,
	pub target: u32,
	pub count: u32,
	pub handled: bool,
}

impl DmaChannel {
	fn register_offset(&self) -> usize {
		0xb0 + 0xc * self.index
	}

	fn load_count(&mut self, sys: &System) {
		let count = u32::from(u16::from_le_bytes(sys.io[self.register_offset() + 8..][..2].try_into().unwrap()));
		self.count = if self.index == 3 {
			if count == 0 { 0x10000 } else { count }
		} else {
			if count == 0 { 0x4000 } else { count & 0x3ff }
		};
	}

	fn dma(&mut self, sys: &mut System) -> u32 {
		let offset = self.register_offset();
		let mut control = u16::from_le_bytes(sys.io[offset + 0xa..][..2].try_into().unwrap());
		if control.bit(15) {
			fn get_increment(control: u16, shift: u16) -> i32 {
				let size = if control.bit(10) { 4 } else { 2 };
				match (control >> shift) & 0b11 {
					0 | 3 => size,
					1 => -size,
					2 => 0,
					_ => unreachable!(),
				}
			}

			let cond = match (control >> 12) & 0b11 {
				1 => sys.ppu.line == 160,
				2 => sys.ppu.hblank,
				3 => {
					assert_eq!(self.index, 3);
					self.index == 3 && sys.ppu.hblank
				}
				_ => true,
			};
			if !cond {
				self.handled = false;
				return 0;
			}
			if self.handled {
				return 0;
			}
			self.handled = true;
			assert!(!control.bit(11));
			let size = if control.bit(10) { 4 } else { 2 };
			let old_target = self.target;
			let source_increment = get_increment(control, 7);
			let target_increment = get_increment(control, 5);
			for _ in 0..self.count {
				sys.write(self.target, sys.read(self.source), size);
				self.source = self.source.wrapping_add_signed(source_increment);
				self.target = self.target.wrapping_add_signed(target_increment);
			}
			if control.bit(9) {
				self.load_count(sys);
				if (control >> 5) & 0b11 == 0b11 {
					self.target = old_target;
				}
			} else {
				control.set_bit(15, false);
				sys.io[offset + 0xb] = (control >> 8) as u8;
			}
			if control.bit(14) {
				sys.interrupts.interrupt((self.index + 8) as u32);
			}
			return self.count * 2 + 2;
		}
		0
	}
}

pub enum MemoryRegion {
	Bios,
	Rom,
	Ram,
	PPURegister,
	Vram,
	Palette,
	IO,
	Oam,
}

#[must_use]
pub fn parse_address(address: u32) -> Option<(MemoryRegion, u32)> {
	Some(match address {
		0x0000_0000..0x0000_4000 => (MemoryRegion::Bios, address),
		0x0200_0000..0x0300_0000 => (MemoryRegion::Ram, address % 0x40000),
		0x0300_0000..0x0400_0000 => (MemoryRegion::Ram, (address % 0x8000) + 0x40000),
		0x0400_0000..0x0400_0060 => (MemoryRegion::PPURegister, address - 0x0400_0000),
		0x0400_0060..0x0400_03ff => (MemoryRegion::IO, address - 0x0400_0000),
		0x0500_0000..0x0600_0000 => (MemoryRegion::Palette, address % 0x400),
		0x0600_0000..0x0700_0000 => {
			let mut address = address % 0x20000;
			if address >= 0x18000 {
				address -= 0x8000;
			}
			(MemoryRegion::Vram, address)
		}
		0x0700_0000..0x0800_0000 => (MemoryRegion::Oam, address % 0x400),
		0x0800_0000..0x0A00_0000 => (MemoryRegion::Rom, address - 0x0800_0000),
		0x0A00_0000..0x0C00_0000 => (MemoryRegion::Rom, address - 0x0A00_0000),
		0x0C00_0000..0x0E00_0000 => (MemoryRegion::Rom, address - 0x0C00_0000),
		0x0E00_0000..0x0F00_0000 => (MemoryRegion::Ram, (address % 0x10000) + 0x48000),
		_ => return None,
	})
}

struct System {
	ram: Vec<u8>,
	rom: Vec<u8>,
	bios: Vec<u8>,
	ppu: ppu::Ppu,
	pressed_keys: HashSet<Keycode>,
	io: [u8; 0x400],
	cpu_paused: bool,
	interrupts: Interrupts,
	timer: Timers,
	dma: [DmaChannel; 4],
}

impl System {
	fn set_pressed_keys(&self, value: &mut u32, keys: &[Keycode], bit: u32) {
		value.set_bit(bit, !keys.iter().any(|key| self.pressed_keys.contains(key)));
	}
}

fn read_bytes(target: &[u8], index: u32) -> u32 {
	let index = index as usize;
	u32::from_le_bytes(target[index..=index + 3].try_into().unwrap())
}

impl System {
	fn read(&self, address: u32) -> u32 {
		let address = address & !3;
		let Some((region, address)) = parse_address(address) else {
			eprintln!("reading from unknown address: {address:08x}");
			return 0;
		};
		match region {
			MemoryRegion::Bios => read_bytes(&self.bios, address),
			MemoryRegion::Ram => read_bytes(&self.ram, address),
			MemoryRegion::PPURegister => self.ppu.read_register(address),
			MemoryRegion::Palette => read_bytes(&self.ppu.palettes, address),
			MemoryRegion::Vram => read_bytes(&self.ppu.vram, address),
			MemoryRegion::Rom => {
				if address >= self.rom.len() as u32 {
					return 0;
				}
				read_bytes(&self.rom, address)
			}
			MemoryRegion::Oam => read_bytes(&self.ppu.oam, address),
			MemoryRegion::IO => match address {
				0x100..0x110 => self.timer.read_register(address),
				0x130 => {
					let mut value = 0;
					self.set_pressed_keys(&mut value, &[Keycode::SPACE, Keycode::Z], 0);
					self.set_pressed_keys(&mut value, &[Keycode::LCTRL, Keycode::X], 1);
					self.set_pressed_keys(&mut value, &[Keycode::TAB], 2);
					self.set_pressed_keys(&mut value, &[Keycode::RETURN], 3);
					self.set_pressed_keys(&mut value, &[Keycode::RIGHT, Keycode::D], 4);
					self.set_pressed_keys(&mut value, &[Keycode::LEFT, Keycode::A], 5);
					self.set_pressed_keys(&mut value, &[Keycode::UP, Keycode::W], 6);
					self.set_pressed_keys(&mut value, &[Keycode::DOWN, Keycode::S], 7);
					self.set_pressed_keys(&mut value, &[Keycode::E], 8);
					self.set_pressed_keys(&mut value, &[Keycode::Q], 9);
					value
				}
				0x200 => self.interrupts.0,
				_ => read_bytes(&self.io, address),
			},
		}
	}

	fn write_byte(&mut self, address: u32, value: u8) {
		let Some((region, address)) = parse_address(address) else {
			eprintln!("writing to unknown address: {address:08x}");
			return;
		};
		match region {
			MemoryRegion::Ram => self.ram[address as usize] = value,
			MemoryRegion::Palette => self.ppu.palettes[address as usize] = value,
			MemoryRegion::Vram => self.ppu.vram[address as usize] = value,
			MemoryRegion::Bios => panic!("Attempt to write to BIOS"),
			MemoryRegion::Rom => panic!("Attempt to write to ROM"),
			MemoryRegion::Oam => self.ppu.oam[address as usize] = value,
			MemoryRegion::PPURegister => self.ppu.write_register(address, value),
			MemoryRegion::IO => match address {
				0xbb | 0xc7 | 0xd3 | 0xdf => {
					let index = ((address - 0xbb) / 0xc) as usize;
					let channel = &mut self.dma[index];
					let registers = &self.io[channel.register_offset()..];
					if value.bit(7) && !registers[0xb].bit(7) {
						let mut channel = *channel;
						channel.source = u32::from_le_bytes(registers[0..4].try_into().unwrap()) & 0xfff_ffff;
						channel.target = u32::from_le_bytes(registers[4..8].try_into().unwrap()) & 0xfff_ffff;
						channel.load_count(self);
						channel.handled = false;
						self.dma[index] = channel;
					}
					self.io[address as usize] = value;
				}
				0x100..0x110 => self.timer.write_register(address, value),
				0x200..0x204 => self.interrupts.write_register(address, value),
				0x301 => self.cpu_paused = true,
				_ => {
					self.io[address as usize] = value;
				}
			},
		}
	}

	fn write(&mut self, address: u32, value: u32, size: u8) {
		let address = address & !(u32::from(size) - 1);
		for i in 0..size {
			self.write_byte(address + u32::from(i), (value >> (i * 8)) as u8);
		}
	}
}

const FRAME_CYCLES: u32 = LINE_CYCLES as u32 * TOTAL_LINES as u32;
const CYCLES_PER_SECOND: u32 = 2u32.pow(24);
const SYNC_CYCLE_DURATION: u32 = (Duration::from_secs(1).as_nanos() as u32) / (CYCLES_PER_SECOND / FRAME_CYCLES);

fn main() {
	let path = std::env::args().nth(1).expect("No path given");
	let bios = std::fs::read("gba_bios.bin").expect("BIOS file not found");
	let rom = std::fs::read(path).expect("ROM file not found");
	let mut cpu = cpu::Cpu::new();
	let sdl_context = sdl2::init().unwrap();
	let ppu = ppu::Ppu::new(&sdl_context.video().unwrap());
	let mut sys = System {
		bios,
		rom,
		ram: vec![0; 0x58000],
		pressed_keys: HashSet::new(),
		io: [0; _],
		ppu,
		cpu_paused: false,
		interrupts: Interrupts(0),
		timer: Timers::default(),
		dma: std::array::from_fn(|index| DmaChannel { index, ..Default::default() }),
	};
	let mut time = Instant::now();
	let mut event_pump = sdl_context.event_pump().unwrap();
	'running: loop {
		let mut cycle = cpu.cycle;
		cpu.step(&mut sys);
		loop {
			if cycle == cpu.cycle {
				break;
			}
			sys.ppu.step(&mut sys.interrupts);
			sys.timer.step(&mut sys.interrupts, cycle);
			for (i, mut channel) in sys.dma.into_iter().enumerate() {
				cpu.cycle = cpu.cycle.wrapping_add(channel.dma(&mut sys));
				sys.dma[i] = channel;
			}
			if cycle.is_multiple_of(FRAME_CYCLES) {
				for event in event_pump.poll_iter() {
					match event {
						Event::Quit { .. } => {
							break 'running;
						}
						Event::KeyDown { keycode: Some(keycode), .. } => {
							sys.pressed_keys.insert(keycode);
							sys.interrupts.interrupt(12);
						}
						Event::KeyUp { keycode: Some(keycode), .. } => {
							sys.pressed_keys.remove(&keycode);
						}
						_ => {}
					}
				}
				let sleep_until = time.add(Duration::new(0, SYNC_CYCLE_DURATION));
				let duration = sleep_until.duration_since(Instant::now());
				assert!(!duration.is_zero(), "Lag");
				std::thread::sleep(duration);
				time = Instant::now();
			}
			cycle = cycle.wrapping_add(1);
		}
	}
}
