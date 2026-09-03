use crate::{Bits, System};
use int_enum::IntEnum;

mod parser;
use parser::{
	DataProcessingCpsr, DataProcessingOpcode, HalfwordDataTransferMode, Indexing, Instruction, Offset, OffsetShift,
	register_index, spsr_index,
};

mod flags {
	pub const NEGATIVE: u32 = 31;
	pub const ZERO: u32 = 30;
	pub const CARRY: u32 = 29;
	pub const OVERFLOW: u32 = 28;
	pub const DISABLE_IRQ: u32 = 7;
	pub const THUMB: u32 = 5;
}

#[derive(Debug, Clone, Copy, IntEnum, PartialEq)]
#[repr(u8)]
enum CpuMode {
	User = 0b0000,
	Fiq = 0b0001,
	Irq = 0b0010,
	Supervisor = 0b0011,
	Abort = 0b0111,
	Undefined = 0b1011,
	System = 0b1111,
}

pub struct Cpu {
	registers: [u32; 31],
	cpsr: u32,
	spsr: [u32; 5],
	pipeline_size: u8,
	pub cycle: u32,
}

impl Cpu {
	const SP: u32 = 13;
	const LINK: u32 = 14;
	const PC: usize = 15;

	pub fn new() -> Self {
		let mut cpu = Self {
			registers: [0; _],
			cpsr: 1 << 4 | CpuMode::Supervisor as u32,
			spsr: [0; _],
			pipeline_size: 0,
			cycle: 0,
		};
		cpu.pipeline_load();
		cpu.pipeline_load();
		cpu
	}

	fn instruction_size(&self) -> u32 {
		if self.flag(flags::THUMB) { 2 } else { 4 }
	}

	fn pc(&self) -> u32 {
		self.registers[Self::PC]
	}

	fn set_pc(&mut self, value: u32) {
		self.registers[Self::PC] = value & !(self.instruction_size() - 1);
		self.pipeline_size = 0;
		self.cycle();
		self.cycle();
	}

	fn pipeline_load(&mut self) {
		if self.pipeline_size < 2 {
			self.pipeline_size += 1;
			self.registers[Self::PC] = self.pc().wrapping_add(self.instruction_size());
		}
	}

	fn cycle(&mut self) {
		self.cycle = self.cycle.wrapping_add(1);
		self.pipeline_load();
	}

	fn register(&self, register: usize) -> u32 {
		self.registers[register]
	}

	fn set_register(&mut self, register: usize, value: u32) {
		if register == Self::PC {
			self.set_pc(value);
		} else {
			self.registers[register] = value;
		}
	}

	fn flag(&self, flag: u32) -> bool {
		self.cpsr.bit(flag)
	}

	fn set_flag(&mut self, bit: u32, value: bool) {
		self.cpsr.set_bit(bit, value);
	}

	fn set_mode(&mut self, mode: CpuMode) {
		self.cpsr = self.cpsr & !0b1111 | mode as u32;
	}

	fn resolve_indexing(&mut self, index: Indexing, offset: u32) -> u32 {
		let address = self.register(index.base);
		let new_address = if index.subtract { address.wrapping_sub(offset) } else { address.wrapping_add(offset) };
		if index.write_back || !index.modify_first {
			self.set_register(index.base, new_address);
		}
		if index.modify_first { new_address } else { address }
	}

	fn resolve_offset(&mut self, offset: Offset, with_flags: bool) -> u32 {
		let (value, carry) = match offset {
			Offset::Immediate { value, carry } => (value, carry),
			Offset::Register(usize) => (self.register(usize), None),
			Offset::ShiftedRegister { register, mut shift_type, shift } => {
				let shift = match shift {
					OffsetShift::Register(register) => {
						let shift = self.register(register) & 0xff;
						if shift == 0 {
							shift_type = 0b00;
						}
						self.cycle();
						shift
					}
					OffsetShift::Immediate(value) => value,
				};

				let operand = self.register(register);
				match shift_type {
					0b00 if shift == 0 => (operand, None),
					0b00 => (
						operand.unbounded_shl(shift),
						Some(operand & 1u32.unbounded_shl(32u32.wrapping_sub(shift)) != 0),
					),
					0b01 => (operand.unbounded_shr(shift), Some(operand & 1u32.unbounded_shl(shift - 1) != 0)),
					0b10 => (
						operand.cast_signed().unbounded_shr(shift).cast_unsigned(),
						Some(operand.cast_signed().unbounded_shr(shift - 1) & 1 != 0),
					),
					0b11 if shift == 0 => {
						(operand >> 1 | u32::from(self.flag(flags::CARRY)) << 31, Some(operand & 1 != 0))
					}
					0b11 => (operand.rotate_right(shift), Some(operand & 1u32.unbounded_shl((shift - 1) % 32) != 0)),
					_ => unreachable!(),
				}
			}
		};
		if with_flags && let Some(carry) = carry {
			self.set_flag(flags::CARRY, carry);
		}
		value
	}

	fn read(&mut self, sys: &mut System, address: u32) -> u32 {
		self.cycle();
		sys.read(address)
	}

	fn write(&mut self, sys: &mut System, address: u32, register: usize, size: u8) {
		self.cycle();
		sys.write(address, self.register(register), size);
	}

	fn load_dynamic_width(&mut self, sys: &mut System, address: u32, size: u8) -> u32 {
		let value = self.read(sys, address).rotate_right(8 * (address % 4));
		value & 1u32.unbounded_shl(u32::from(size) * 8).wrapping_sub(1)
	}

	pub fn step(&mut self, sys: &mut System) {
		if !self.flag(flags::DISABLE_IRQ) && sys.read(0x0400_0208) != 0 {
			let interrupts = sys.interrupts.0;
			if interrupts & (interrupts >> 16) != 0 {
				self.spsr[spsr_index(CpuMode::Irq).unwrap()] = self.cpsr;
				self.set_register(
					register_index(CpuMode::Irq, Self::LINK),
					self.pc().wrapping_sub(if self.flag(flags::THUMB) { 0 } else { 4 }),
				);
				self.set_flag(flags::THUMB, false);
				self.set_mode(CpuMode::Irq);
				self.set_flag(flags::DISABLE_IRQ, true);
				self.set_pc(0x18);
				self.cycle();
				sys.cpu_paused = false;
			} else if sys.cpu_paused {
				self.cycle();
				return;
			}
		}

		let mode = CpuMode::try_from((self.cpsr & 0b1111) as u8).expect("invalid CPSR mode");
		let address = self.pc().wrapping_sub(u32::from(self.pipeline_size) * self.instruction_size());
		let opcode = self.load_dynamic_width(sys, address, self.instruction_size() as u8);
		let (cond, instruction) = if self.flag(flags::THUMB) {
			Instruction::parse_thumb(opcode, mode, self.pc())
		} else {
			Instruction::parse(opcode, mode)
		}
		.expect("invalid instruction");

		let condition = match cond >> 1 {
			0b000 => self.flag(flags::ZERO),
			0b001 => self.flag(flags::CARRY),
			0b010 => self.flag(flags::NEGATIVE),
			0b011 => self.flag(flags::OVERFLOW),
			0b100 => self.flag(flags::CARRY) && !self.flag(flags::ZERO),
			0b101 => self.flag(flags::NEGATIVE) == self.flag(flags::OVERFLOW),
			0b110 => !self.flag(flags::ZERO) && (self.flag(flags::NEGATIVE) == self.flag(flags::OVERFLOW)),
			_ => true,
		};
		self.pipeline_size = 1;
		if u8::from(condition) ^ (cond & 1) == 0 {
			self.pipeline_load();
			return;
		}

		match instruction {
			Instruction::Interrupt => {
				self.set_register(
					register_index(CpuMode::Supervisor, Self::LINK),
					self.pc().wrapping_sub(self.instruction_size()),
				);
				self.spsr[spsr_index(CpuMode::Supervisor).unwrap()] = self.cpsr;
				self.set_mode(CpuMode::Supervisor);
				self.set_flag(flags::THUMB, false);
				self.set_flag(flags::DISABLE_IRQ, true);
				self.set_pc(0x08);
			}
			Instruction::Branch { offset, link_register } => {
				let pc = self.pc();
				if self.flag(flags::THUMB)
					&& let Some(link_register) = link_register
				{
					self.set_pc(self.register(link_register).wrapping_add(offset.cast_unsigned()));
					self.set_register(link_register, pc - 1);
				} else {
					if let Some(link_register) = link_register {
						self.set_register(link_register, pc - 4);
					}
					self.set_pc(pc.wrapping_add_signed(offset));
				}
			}
			Instruction::LoadPsr { spsr, target } => {
				let value = if let Some(spsr) = spsr { self.spsr[spsr] } else { self.cpsr };
				self.set_register(target, value);
			}
			Instruction::StorePsr { spsr, source, mask } => {
				let value = self.resolve_offset(source, false);
				let psr = if let Some(spsr) = spsr {
					&mut self.spsr[spsr]
				} else {
					assert!(
						!mask.bit(flags::THUMB) || self.flag(flags::THUMB) == value.bit(flags::THUMB),
						"changing thumb mode during PSR transfer"
					);
					&mut self.cpsr
				};
				*psr = *psr & !mask | value & mask;
			}
			Instruction::BlockDataTransfer { load, registers, load_spsr, index } => {
				let mut address = self.register(index.base);

				let offset = if registers.is_empty() { 0x40 } else { registers.len() as u32 * 4 };
				let updated_base = if index.subtract {
					address = address.wrapping_sub(offset);
					address
				} else {
					address.wrapping_add(offset)
				};
				if index.modify_first != index.subtract {
					address = address.wrapping_add(4);
				}
				if load {
					self.cycle();
				}
				let registers = if registers.is_empty() { vec![Self::PC] } else { registers };
				for register in registers {
					if load {
						let value = self.read(sys, address);
						self.set_register(register, value);
					} else {
						self.write(sys, address, register, 4);
					}
					address = address.wrapping_add(4);
				}
				if index.write_back {
					self.set_register(index.base, updated_base);
				}
				if let Some(spsr) = load_spsr {
					self.cpsr = self.spsr[spsr];
				}
			}
			Instruction::SingleDataTransfer { load, index, target, offset, size } => {
				let offset = self.resolve_offset(offset, false);
				let address = self.resolve_indexing(index, offset);
				if load {
					self.cycle();
					let value = self.load_dynamic_width(sys, address, size);
					self.set_register(target, value);
				} else {
					self.write(sys, address, target, size);
				}
			}
			Instruction::HalfwordDataTransfer { mode, index, target, offset } => {
				let offset = self.resolve_offset(offset, false);
				let address = self.resolve_indexing(index, offset);
				assert!(
					address & 1 == 0 || matches!(mode, HalfwordDataTransferMode::LoadSignedByte),
					"bit 0 set for halfword load/store"
				);
				if let HalfwordDataTransferMode::StoreHalfword = mode {
					self.write(sys, address, target, 2);
				} else {
					self.cycle();
					let value = match mode {
						HalfwordDataTransferMode::LoadSignedByte => {
							i32::from(self.load_dynamic_width(sys, address, 1) as i8).cast_unsigned()
						}
						HalfwordDataTransferMode::LoadUnsignedHalfword => self.load_dynamic_width(sys, address, 2),
						HalfwordDataTransferMode::LoadSignedHalfword => {
							i32::from(self.load_dynamic_width(sys, address, 2) as i16).cast_unsigned()
						}
						HalfwordDataTransferMode::StoreHalfword => unreachable!(),
					};
					self.set_register(target, value);
				}
			}
			Instruction::BranchAndExchange { register } => {
				let target = self.register(register);
				self.set_flag(flags::THUMB, target.bit(0));
				self.set_pc(target);
			}
			Instruction::SingleDataSwap { source, target, base, size } => {
				let address = self.register(base);
				self.cycle();
				let value = self.load_dynamic_width(sys, address, size);
				self.write(sys, address, source, size);
				self.set_register(target, value);
			}
			Instruction::Multiply { operand1, operand2, accumulate, target, set_flags } => {
				let operand1 = self.register(operand1);
				let operand2 = self.register(operand2);
				let mut result = operand1.wrapping_mul(operand2);
				if let Some(accumulate) = accumulate {
					result = result.wrapping_add(self.register(accumulate));
					self.cycle();
				}
				if set_flags {
					self.set_flag(flags::ZERO, result == 0);
					self.set_flag(flags::NEGATIVE, result.bit(31));
				}
				self.set_register(target, result);
				let cycles = (4 - operand2.leading_ones().max(operand2.leading_zeros()) / 8).min(1);
				for _ in 0..cycles {
					self.cycle();
				}
			}
			Instruction::MultiplyLong {
				operand1,
				operand2,
				target_low,
				target_high,
				accumulate,
				signed,
				set_flags,
			} => {
				let operand1 = self.register(operand1);
				let operand2 = self.register(operand2);
				let accumulate = if accumulate {
					self.cycle();
					(u64::from(self.register(target_high)) << 32) | u64::from(self.register(target_low))
				} else {
					0
				};
				let (result, complexity) = if signed {
					let result = i64::from(operand1.cast_signed())
						.wrapping_mul(i64::from(operand2.cast_signed()))
						.wrapping_add(accumulate.cast_signed())
						.cast_unsigned();
					(result, operand2.leading_ones().max(operand2.leading_zeros()))
				} else {
					let result = (u64::from(operand1) * u64::from(operand2)).wrapping_add(accumulate);
					(result, operand2.leading_zeros())
				};
				let (low, high) = (result as u32, (result >> 32) as u32);
				if set_flags {
					self.set_flag(flags::ZERO, high == 0 && low == 0);
					self.set_flag(flags::NEGATIVE, high.bit(31));
				}
				self.set_register(target_low, low);
				self.set_register(target_high, high);
				let cycles = (4 - complexity / 8).min(1);
				for _ in 0..cycles {
					self.cycle();
				}
			}
			Instruction::DataProcessing { cpsr, opcode, operand1, operand2, target } => {
				let old_carry = self.flag(flags::CARRY);
				let set_flags = matches!(cpsr, DataProcessingCpsr::SetFlags);
				let operand = self.resolve_offset(operand2, set_flags);
				let input = self.register(operand1);

				let result;
				match opcode {
					DataProcessingOpcode::And | DataProcessingOpcode::TestAnd => result = input & operand,
					DataProcessingOpcode::Xor | DataProcessingOpcode::TestXor => result = input ^ operand,
					DataProcessingOpcode::Or => result = input | operand,
					DataProcessingOpcode::Move => result = operand,
					DataProcessingOpcode::BitClear => result = input & !operand,
					DataProcessingOpcode::MoveNot => result = !operand,
					_ => {
						let (op1, op2, mut carry) = match opcode {
							DataProcessingOpcode::Add | DataProcessingOpcode::TestAdd => (input, operand, false),
							DataProcessingOpcode::ReverseSubtract => (operand, !input, true),
							DataProcessingOpcode::Subtract | DataProcessingOpcode::TestSubtract => {
								(input, !operand, true)
							}
							DataProcessingOpcode::AddWithCarry => (input, operand, old_carry),
							DataProcessingOpcode::SubtractWithCarry => (input, !operand, old_carry),
							DataProcessingOpcode::ReverseSubtractWithCarry => (operand, !input, old_carry),
							_ => unreachable!(),
						};
						(result, carry) = op1.carrying_add(op2, carry);
						if set_flags {
							self.set_flag(flags::CARRY, carry);
							self.set_flag(flags::OVERFLOW, (op1 >> 31) == (op2 >> 31) && (op1 >> 31) != (result >> 31));
						}
					}
				}
				match cpsr {
					DataProcessingCpsr::SetFlags => {
						self.set_flag(flags::NEGATIVE, result.bit(31));
						self.set_flag(flags::ZERO, result == 0);
					}
					DataProcessingCpsr::LoadSpsr(spsr) => {
						self.cpsr = self.spsr[spsr];
					}
					DataProcessingCpsr::Unchanged => {}
				}
				if !matches!(
					opcode,
					DataProcessingOpcode::TestAdd
						| DataProcessingOpcode::TestAnd
						| DataProcessingOpcode::TestSubtract
						| DataProcessingOpcode::TestXor
				) {
					self.set_register(target, result);
				}
			}
		}
		self.pipeline_load();
	}
}
