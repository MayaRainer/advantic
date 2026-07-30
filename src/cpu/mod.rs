use crate::{MemoryBus, bit, set_bit};
use int_enum::IntEnum;

mod parser;
use crate::cpu::parser::{DataProcessingCpsr, register_index, spsr_index};
use parser::{DataProcessingOpcode, HalfwordDataTransferMode, Indexing, Instruction, Offset, OffsetShift};

mod flags {
	pub const NEGATIVE: u32 = 31;
	pub const ZERO: u32 = 30;
	pub const CARRY: u32 = 29;
	pub const OVERFLOW: u32 = 28;
	pub const DISABLE_IRQ: u32 = 7;
	pub const THUMB: u32 = 5;
}

#[derive(Debug, Clone, Copy, IntEnum)]
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

fn load_dynamic_width(mem: &mut dyn MemoryBus, address: u32, byte: bool) -> u32 {
	let value = mem.read(address);
	if byte { value & 0xff } else { value.rotate_right(8 * (address % 4)) }
}

pub struct Cpu {
	registers: [u32; 31],
	cpsr: u32,
	spsr: [u32; 5],
	skip_test: bool,
	pipeline_size: u8,
	pub cycle: u16,
}

impl Cpu {
	const SP: u32 = 13;
	const LINK: u32 = 14;
	const PC: usize = 15;

	pub fn new() -> Self {
		let mut registers = [0; _];
		registers[Self::PC] = 0x0800_0008;
		registers[register_index(CpuMode::User, Self::SP)] = 0x0300_7F00;
		registers[register_index(CpuMode::Irq, Self::SP)] = 0x0300_7FA0;
		registers[register_index(CpuMode::Supervisor, Self::SP)] = 0x0300_7FE0;
		Self {
			registers,
			cpsr: 1 << 4 | CpuMode::System as u32,
			spsr: [0; _],
			pipeline_size: 2,
			skip_test: false,
			cycle: 0,
		}
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
	}

	fn cycle(&mut self) {
		self.cycle = self.cycle.wrapping_add(1);
		if self.pipeline_size < 2 {
			self.pipeline_size += 1;
			self.registers[Self::PC] = self.pc().wrapping_add(self.instruction_size());
		}
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
		bit(self.cpsr, flag)
	}

	fn set_flag(&mut self, bit: u32, value: bool) {
		self.cpsr = set_bit(self.cpsr, bit, value);
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

	pub fn step(&mut self, mem: &mut dyn MemoryBus) {
		let mode = CpuMode::try_from((self.cpsr & 0b1111) as u8).expect("invalid CPSR mode");
		let opcode = mem.read(self.pc().wrapping_sub(u32::from(self.pipeline_size) * self.instruction_size()));
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
			self.cycle();
			return;
		}

		match instruction {
			Instruction::Interrupt => {
				self.set_register(
					register_index(CpuMode::Supervisor, Self::LINK),
					self.pc().wrapping_sub(self.instruction_size()),
				);
				self.spsr[spsr_index(CpuMode::Supervisor).unwrap()] = self.cpsr;
				self.cpsr = self.cpsr & !0b1111 | CpuMode::Supervisor as u32;
				self.set_flag(flags::THUMB, false);
				self.set_flag(flags::DISABLE_IRQ, true);
				self.set_pc(0x08);
			}
			Instruction::Branch { offset, link_register } => {
				let pc = self.pc();
				if self.flag(flags::THUMB)
					&& let Some(link_register) = link_register
				{
					self.set_pc(self.register(link_register) + offset.cast_unsigned());
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
						!bit(mask, flags::THUMB) || self.flag(flags::THUMB) == bit(value, flags::THUMB),
						"changing thumb mode during PSR transfer"
					);
					&mut self.cpsr
				};
				*psr = *psr & !mask | value & mask;
			}
			Instruction::BlockDataTransfer { load, registers, load_spsr, index } => {
				let mut address = self.register(index.base);

				let offset = registers.len() as u32 * 4;
				let updated_base = if index.subtract {
					address = address.wrapping_sub(offset);
					address
				} else {
					address.wrapping_add(offset)
				};
				if index.modify_first != index.subtract {
					address = address.wrapping_add(4);
				}
				self.cycle();
				for register in registers {
					if load {
						let value = mem.read(address);
						self.set_register(register, value);
					} else {
						mem.write(address, self.register(register), 4);
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
			Instruction::SingleDataTransfer { load, index, target, offset, byte } => {
				let offset = self.resolve_offset(offset, false);
				let address = self.resolve_indexing(index, offset);
				self.cycle();
				if load {
					self.set_register(target, load_dynamic_width(mem, address, byte));
				} else {
					mem.write(address, self.register(target), if byte { 1 } else { 4 });
				}
			}
			Instruction::HalfwordDataTransfer { mode, index, target, offset } => {
				let offset = self.resolve_offset(offset, false);
				let address = self.resolve_indexing(index, offset);
				self.cycle();
				if address & 1 != 0 && !matches!(mode, HalfwordDataTransferMode::LoadSignedByte) {
					self.skip_test = true;
					//panic!("bit 0 set for halfword load/store");
				}
				if let HalfwordDataTransferMode::StoreHalfword = mode {
					let value = self.register(target);
					mem.write(address, value, 2);
				} else {
					let value = match mode {
						HalfwordDataTransferMode::LoadSignedByte => i32::from(mem.read(address) as i8).cast_unsigned(),
						HalfwordDataTransferMode::LoadUnsignedHalfword => u32::from(mem.read(address) as u16),
						HalfwordDataTransferMode::LoadSignedHalfword => {
							i32::from(mem.read(address) as i16).cast_unsigned()
						}
						HalfwordDataTransferMode::StoreHalfword => unreachable!(),
					};
					self.set_register(target, value);
				}
			}
			Instruction::BranchAndExchange { register } => {
				let target = self.register(register);
				self.set_flag(flags::THUMB, bit(target, 0));
				self.set_pc(target);
			}
			Instruction::SingleDataSwap { source, target, base, byte } => {
				let address = self.register(base);
				let value = load_dynamic_width(mem, address, byte);
				mem.write(address, self.register(source), if byte { 1 } else { 4 });
				self.set_register(target, value);
			}
			Instruction::Multiply { operand1, operand2, accumulate, target, set_flags } => {
				let mut result = self.register(operand1).wrapping_mul(self.register(operand2));
				if let Some(accumulate) = accumulate {
					result = result.wrapping_add(self.register(accumulate));
				}
				if set_flags {
					self.set_flag(flags::ZERO, result == 0);
					self.set_flag(flags::NEGATIVE, bit(result, 31));
				}
				self.set_register(target, result);
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
					(u64::from(self.register(target_high)) << 32) | u64::from(self.register(target_low))
				} else {
					0
				};
				let result = if signed {
					i64::from(operand1.cast_signed())
						.wrapping_mul(i64::from(operand2.cast_signed()))
						.wrapping_add(accumulate.cast_signed())
						.cast_unsigned()
				} else {
					(u64::from(operand1) * u64::from(operand2)).wrapping_add(accumulate)
				};
				let (low, high) = (result as u32, (result >> 32) as u32);
				if set_flags {
					self.set_flag(flags::ZERO, high == 0 && low == 0);
					self.set_flag(flags::NEGATIVE, bit(high, 31));
				}
				self.set_register(target_low, low);
				self.set_register(target_high, high);
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
				if !matches!(
					opcode,
					DataProcessingOpcode::TestAdd
						| DataProcessingOpcode::TestAnd
						| DataProcessingOpcode::TestSubtract
						| DataProcessingOpcode::TestXor
				) {
					self.set_register(target, result);
				}
				match cpsr {
					DataProcessingCpsr::SetFlags => {
						self.set_flag(flags::NEGATIVE, bit(result, 31));
						self.set_flag(flags::ZERO, result == 0);
					}
					DataProcessingCpsr::LoadSpsr(spsr) => self.cpsr = self.spsr[spsr],
					DataProcessingCpsr::Unchanged => {}
				}
			}
		}
		self.cycle();
	}
}

#[test]
fn test() {
	use std::cell::RefCell;
	use std::collections::{HashMap, VecDeque};

	#[derive(Debug)]
	enum Transaction {
		Read { address: u32, value: u32 },
		Write { address: u32, value: u32 },
	}
	struct TestMemoryBus {
		transactions: RefCell<VecDeque<Transaction>>,
	}
	impl MemoryBus for TestMemoryBus {
		fn read(&self, address: u32) -> u32 {
			let transaction = self.transactions.borrow_mut().pop_front().unwrap();
			if let Transaction::Read { address: a, value } = transaction {
				assert_eq!(address, a, "read address");
				value
			} else {
				panic!("expected read {address}")
			}
		}

		fn write(&mut self, address: u32, value: u32, size: u8) {
			let value = value & u32::MAX >> ((4 - size) * 8);
			let transaction = self.transactions.borrow_mut().pop_front().unwrap();
			if let Transaction::Write { address: a, value: v } = transaction {
				assert_eq!(address, a, "write address");
				assert_eq!(value, v, "write value {address}");
			} else {
				panic!("expected write {address}")
			}
		}
	}
	struct TestData {
		data: Vec<u8>,
		offset: usize,
	}
	impl TestData {
		fn next_u32(&mut self) -> u32 {
			let data = u32::from_le_bytes(self.data[self.offset..(self.offset + 4)].try_into().unwrap());
			self.offset += 4;
			data
		}
	}

	for (file_index, file) in std::fs::read_dir("v1").unwrap().enumerate() {
		let mut errors = HashMap::<String, u16>::new();
		let file = file.unwrap();
		println!("{}", file.path().display());
		let data = std::fs::read(file.path()).unwrap();
		let mut data = TestData { data, offset: 4 };
		let count = data.next_u32();

		for index in 0..count {
			let end = data.offset + data.next_u32() as usize;
			data.offset += 8;

			let registers = std::array::from_fn(|_| data.next_u32());
			let cpsr = data.next_u32();
			let spsr = std::array::from_fn(|_| data.next_u32());
			data.offset += 4 * 3 + 8;

			let mut final_registers: [_; 31] = std::array::from_fn(|_| data.next_u32());
			let mut final_cpsr = data.next_u32();
			let final_spsr: [_; 5] = std::array::from_fn(|_| data.next_u32());
			data.offset += 4 * 3 + 8;
			let mut cpu = Cpu { registers, cpsr, spsr, pipeline_size: 2, skip_test: false, cycle: 0 };
			let mode = CpuMode::try_from((cpsr & 0b1111) as u8).expect("invalid CPSR mode");

			let mut transactions: VecDeque<_> = (0..data.next_u32())
				.filter_map(|_| {
					let kind = data.next_u32();
					let _size = data.next_u32();
					let address = data.next_u32();
					let value = data.next_u32();
					data.offset += 8;
					if kind == 0 {
						return None;
					}
					Some(if kind == 2 {
						Transaction::Write { address, value }
					} else {
						Transaction::Read { address, value }
					})
				})
				.collect();
			data.offset += 8;
			let opcode = data.next_u32();
			let address = data.next_u32();
			data.offset = end;

			let (shifted_opcode, instruction) = if cpu.flag(flags::THUMB) {
				let opcode = opcode << if address.is_multiple_of(4) { 0 } else { 16 };
				(opcode, Instruction::parse_thumb(opcode, mode, address))
			} else {
				(opcode, Instruction::parse(opcode, mode))
			};
			transactions.push_front(Transaction::Read { address, value: shifted_opcode });
			let instruction = match instruction {
				Ok(parsed) => parsed,
				Err(err) => {
					*errors.entry(err.to_string()).or_default() += 1;
					continue;
				}
			};

			if matches!(instruction.1, Instruction::Multiply { .. } | Instruction::MultiplyLong { .. }) {
				final_cpsr = set_bit(final_cpsr, flags::CARRY, bit(cpsr, flags::CARRY));
			}

			if let (_, Instruction::StorePsr { spsr, source, .. }) = instruction
				&& let Offset::ShiftedRegister { register, .. } = source
				&& spsr.is_none()
			{
				let mask = 1 << 4;
				cpu.registers[register] |= final_cpsr & mask;
				final_registers[register] |= final_cpsr & mask;

				cpu.registers[register] = set_bit(cpu.registers[register], flags::THUMB, bit(cpsr, flags::THUMB));
				final_registers[register] = set_bit(final_registers[register], flags::THUMB, bit(cpsr, flags::THUMB));
				final_cpsr = set_bit(final_cpsr, flags::THUMB, bit(cpsr, flags::THUMB));
			}

			if file_index == 17 {
				dbg!(index);
				if cpu.flag(flags::THUMB) {
					println!("instruction {opcode:016b}");
				} else {
					println!("instruction {opcode:032b}");
				}
				dbg!(instruction);
				println!("cpsr {cpsr:032b} {final_cpsr:032b}");
			}
			let mut mem = TestMemoryBus { transactions: RefCell::new(transactions) };
			cpu.step(&mut mem);

			if cpu.skip_test {
				continue;
			}
			assert!(mem.transactions.take().is_empty());
			for (i, value) in cpu.registers.iter().enumerate() {
				let mut expected = final_registers[i];
				if i == Cpu::PC {
					expected &= !(cpu.instruction_size() - 1);
				}
				assert_eq!(*value, expected, "registers[{i}] {file_index} {index}");
			}
			assert_eq!(cpu.cpsr, final_cpsr, "cpsr {:032b} {:032b} {}", cpu.cpsr, final_cpsr, index);
			for (i, value) in cpu.spsr.iter().enumerate() {
				assert_eq!(*value, final_spsr[i], "spsr[{i}]");
			}
		}
		for (error, count) in errors {
			eprintln!("{error}: {count}");
		}
	}
}
