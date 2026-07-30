use crate::cpu::{Cpu, CpuMode, bit};
use int_enum::IntEnum;

pub fn register_index(mode: CpuMode, register: u32) -> usize {
	let register = (register & 0b1111) as usize;
	match register {
		8..13 if matches!(mode, CpuMode::Fiq) => register + 8,
		13..15 => {
			let offset = match mode {
				CpuMode::User | CpuMode::System => 0,
				CpuMode::Fiq => 8,
				CpuMode::Supervisor => 10,
				CpuMode::Abort => 12,
				CpuMode::Irq => 14,
				CpuMode::Undefined => 16,
			};
			register + offset
		}
		_ => register,
	}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Indexing {
	pub write_back: bool,
	pub subtract: bool,
	pub modify_first: bool,
	pub base: usize,
}

impl Indexing {
	fn parse(mode: CpuMode, instruction: u32) -> Result<Self, &'static str> {
		let parsed = Self {
			write_back: bit(instruction, 21),
			subtract: !bit(instruction, 23),
			modify_first: bit(instruction, 24),
			base: register_index(mode, instruction >> 16),
		};
		if parsed.base == Cpu::PC && (parsed.write_back || !parsed.modify_first) {
			return Err("write back set with R15 as base");
		}
		Ok(parsed)
	}
}

#[derive(Clone, Copy, Debug)]
pub enum OffsetShift {
	Immediate(u32),
	Register(usize),
}

impl OffsetShift {
	fn parse_immediate(value: u32, shift_type: u8) -> OffsetShift {
		let shift = (value) & 0b11111;
		OffsetShift::Immediate(if shift == 0 && (0b01..0b11).contains(&shift_type) { 32 } else { shift })
	}
}

#[derive(Clone, Copy, Debug)]
pub enum Offset {
	Immediate { value: u32, carry: Option<bool> },
	Register(usize),
	ShiftedRegister { register: usize, shift_type: u8, shift: OffsetShift },
}

impl Offset {
	fn parse_shifted_register(instruction: u32, mode: CpuMode) -> Self {
		let shift_type = ((instruction >> 5) & 0b11) as u8;
		Self::ShiftedRegister {
			register: register_index(mode, instruction),
			shift_type,
			shift: if bit(instruction, 4) {
				OffsetShift::Register(register_index(mode, instruction >> 8))
			} else {
				OffsetShift::parse_immediate(instruction >> 7, shift_type)
			},
		}
	}

	fn parse_data_processing(instruction: u32, mode: CpuMode) -> Self {
		if bit(instruction, 25) {
			let rotate = ((instruction >> 8) & 0b1111) * 2;
			let value = (instruction & 0xff).rotate_right(rotate);
			Self::Immediate { value, carry: if rotate != 0 { Some(bit(value, 31)) } else { None } }
		} else {
			Self::parse_shifted_register(instruction, mode)
		}
	}
}

#[derive(Clone, Copy, Debug)]
pub enum HalfwordDataTransferMode {
	StoreHalfword,
	LoadUnsignedHalfword,
	LoadSignedHalfword,
	LoadSignedByte,
}

#[derive(Clone, Copy, Debug, IntEnum, PartialEq)]
#[repr(u8)]
pub enum DataProcessingOpcode {
	And,
	Xor,
	Subtract,
	ReverseSubtract,
	Add,
	AddWithCarry,
	SubtractWithCarry,
	ReverseSubtractWithCarry,
	TestAnd,
	TestXor,
	TestSubtract,
	TestAdd,
	Or,
	Move,
	BitClear,
	MoveNot,
}

#[derive(Clone, Copy, Debug)]
pub enum DataProcessingCpsr {
	Unchanged,
	SetFlags,
	LoadSpsr(usize),
}

impl DataProcessingOpcode {
	fn parse(instruction: u32) -> Self {
		Self::try_from((instruction & 0b1111) as u8).unwrap()
	}
}

#[derive(Clone, Debug)]
pub enum Instruction {
	Interrupt,
	Branch {
		link_register: Option<usize>,
		offset: i32,
	},
	BlockDataTransfer {
		load: bool,
		registers: Vec<usize>,
		index: Indexing,
		load_spsr: Option<usize>,
	},
	SingleDataTransfer {
		load: bool,
		index: Indexing,
		target: usize,
		offset: Offset,
		byte: bool,
	},
	HalfwordDataTransfer {
		mode: HalfwordDataTransferMode,
		index: Indexing,
		target: usize,
		offset: Offset,
	},
	BranchAndExchange {
		register: usize,
	},
	SingleDataSwap {
		source: usize,
		target: usize,
		base: usize,
		byte: bool,
	},
	MultiplyLong {
		operand1: usize,
		operand2: usize,
		target_low: usize,
		target_high: usize,
		accumulate: bool,
		signed: bool,
		set_flags: bool,
	},
	Multiply {
		operand1: usize,
		operand2: usize,
		accumulate: Option<usize>,
		target: usize,
		set_flags: bool,
	},
	LoadPsr {
		spsr: Option<usize>,
		target: usize,
	},
	StorePsr {
		spsr: Option<usize>,
		source: Offset,
		mask: u32,
	},
	DataProcessing {
		cpsr: DataProcessingCpsr,
		opcode: DataProcessingOpcode,
		operand1: usize,
		operand2: Offset,
		target: usize,
	},
}

fn thumb_register(instruction: u32, bit: u32, mode: CpuMode) -> usize {
	register_index(mode, (instruction >> bit) & 0b111)
}

fn parse_register_list(mode: CpuMode, instruction: u32, index: &Indexing) -> Result<Vec<usize>, &'static str> {
	let mut registers: Vec<usize> = Vec::new();
	for i in 0..16 {
		if bit(instruction, i) {
			let register = register_index(mode, i);
			if index.write_back && register == index.base {
				return Err("base included in Rlist with write-back enabled");
			}
			registers.push(register);
		}
	}
	if registers.is_empty() {
		return Err("Rlist empty");
	}
	Ok(registers)
}

pub fn spsr_index(mode: CpuMode) -> Result<usize, &'static str> {
	Ok(match mode {
		CpuMode::Fiq => 0,
		CpuMode::Supervisor => 1,
		CpuMode::Abort => 2,
		CpuMode::Irq => 3,
		CpuMode::Undefined => 4,
		_ => return Err("spsr accessed in user mode"),
	})
}

impl Instruction {
	pub fn parse(instruction: u32, mode: CpuMode) -> Result<(u8, Self), &'static str> {
		let parsed = match (instruction >> 25) & 0b111 {
			0b111 if bit(instruction, 24) => Self::Interrupt,
			0b110 | 0b111 => return Err("coprocessor"),
			0b101 => Self::Branch {
				offset: ((instruction & 0x00ff_ffff) << 8).cast_signed() >> 6,
				link_register: if bit(instruction, 24) { Some(register_index(mode, Cpu::LINK)) } else { None },
			},
			0b100 => {
				let load = bit(instruction, 20);
				let mut load_spsr = None;
				let index = Indexing::parse(mode, instruction)?;
				let mut mode = mode;
				if bit(instruction, 22) {
					if load && bit(instruction, Cpu::PC as u32) {
						load_spsr = Some(spsr_index(mode)?);
					} else {
						mode = CpuMode::User;
						if index.write_back {
							return Err("write-back during user bank transfer");
						}
					}
				}
				if index.base == Cpu::PC {
					return Err("R15 used in base for block data transfer");
				}

				Self::BlockDataTransfer {
					load,
					registers: parse_register_list(mode, instruction, &index)?,
					index,
					load_spsr,
				}
			}
			0b011 if bit(instruction, 4) => return Err("undefined"),
			0b010 | 0b011 => {
				let index = Indexing::parse(mode, instruction)?;
				let target = register_index(mode, instruction >> 12);
				if target == index.base && (index.write_back || !index.modify_first) {
					return Err("Rn = Rm with write-back in single data transfer");
				}
				Self::SingleDataTransfer {
					load: bit(instruction, 20),
					index,
					target,
					byte: bit(instruction, 22),
					offset: if bit(instruction, 25) {
						Offset::parse_shifted_register(instruction, mode)
					} else {
						Offset::Immediate { value: instruction & 0xfff, carry: None }
					},
				}
			}
			_ => {
				if !bit(instruction, 25) && (instruction >> 4) & 0b1001 == 0b1001 {
					if instruction & (0b11 << 5) != 0 {
						let index = Indexing::parse(mode, instruction)?;
						let target = register_index(mode, instruction >> 12);
						if target == index.base && (index.write_back || !index.modify_first) {
							return Err("Rn = Rm with write-back in halfword data transfer");
						}
						Self::HalfwordDataTransfer {
							mode: if bit(instruction, 20) {
								match (instruction >> 5) & 0b11 {
									0b01 => HalfwordDataTransferMode::LoadUnsignedHalfword,
									0b10 => HalfwordDataTransferMode::LoadSignedByte,
									0b11 => HalfwordDataTransferMode::LoadSignedHalfword,
									_ => unreachable!(),
								}
							} else {
								HalfwordDataTransferMode::StoreHalfword
							},
							offset: if bit(instruction, 22) {
								Offset::Immediate { value: (instruction >> 4) & 0xf0 | instruction & 0xf, carry: None }
							} else {
								Offset::Register(register_index(mode, instruction))
							},
							target,
							index,
						}
					} else {
						let operand1 = register_index(mode, instruction);
						let operand2 = register_index(mode, instruction >> 8);
						let operand3 = register_index(mode, instruction >> 12);
						let operand4 = register_index(mode, instruction >> 16);
						if [operand1, operand2, operand3, operand4].contains(&Cpu::PC) {
							return Err("R15 used as operand for MUL/MULL/SWP");
						}
						match (instruction >> 23) & 0b11 {
							0b00 => Self::Multiply {
								operand1,
								operand2,
								accumulate: if bit(instruction, 21) { Some(operand3) } else { None },
								target: operand4,
								set_flags: bit(instruction, 20),
							},
							0b01 => Self::MultiplyLong {
								operand1,
								operand2,
								accumulate: bit(instruction, 21),
								signed: bit(instruction, 22),
								target_low: operand3,
								target_high: operand4,
								set_flags: bit(instruction, 20),
							},
							0b10 => Self::SingleDataSwap {
								source: operand1,
								target: operand3,
								base: operand4,
								byte: bit(instruction, 22),
							},
							_ => return Err("invalid instruction"),
						}
					}
				} else if (instruction >> 20) & 0b11_1111 == 0b01_0010 && (instruction >> 8) & 0b1111 == 0b1111 {
					let register = register_index(mode, instruction);
					if register == Cpu::PC {
						return Err("R15 used as operand for BX");
					}
					Self::BranchAndExchange { register }
				} else if !bit(instruction, 20) && instruction >> 23 & 0b11 == 0b10 {
					let spsr = if bit(instruction, 22) { Some(spsr_index(mode)?) } else { None };
					if bit(instruction, 21) {
						let mut fields = (instruction >> 16) & 0b1111;
						if matches!(mode, CpuMode::User) {
							fields &= 0b1000;
						}
						let source = Offset::parse_data_processing(instruction, mode);
						if let Offset::ShiftedRegister { register, .. } = source
							&& register == Cpu::PC
						{
							return Err("R15 used as operand for PSR transfer");
						}
						Self::StorePsr {
							spsr,
							mask: (0..4).fold(0, |acc, i| if bit(fields, i) { acc | 0xff << (i * 8) } else { acc }),
							source,
						}
					} else {
						let target = register_index(mode, instruction >> 12);
						if target == Cpu::PC {
							return Err("R15 used as operand for PSR transfer");
						}
						Self::LoadPsr { spsr, target }
					}
				} else {
					let target = register_index(mode, instruction >> 12);
					let set_flags = bit(instruction, 20);
					Self::DataProcessing {
						operand1: register_index(mode, instruction >> 16),
						operand2: Offset::parse_data_processing(instruction, mode),
						target,
						cpsr: if set_flags {
							if target == Cpu::PC {
								DataProcessingCpsr::LoadSpsr(spsr_index(mode)?)
							} else {
								DataProcessingCpsr::SetFlags
							}
						} else {
							DataProcessingCpsr::Unchanged
						},
						opcode: DataProcessingOpcode::parse(instruction >> 21),
					}
				}
			}
		};
		Ok(((instruction >> 28) as u8, parsed))
	}

	pub fn parse_thumb(instruction: u32, mode: CpuMode, address: u32) -> Result<(u8, Self), &'static str> {
		let instruction = instruction >> if address.is_multiple_of(4) { 0 } else { 16 };
		let mut cond = 0b1110;
		let instruction = match (instruction >> 12) & 0b1111 {
			0b1111 => {
				if bit(instruction, 11) {
					Instruction::Branch {
						offset: (instruction & 0x7ff).cast_signed() << 1,
						link_register: Some(register_index(mode, Cpu::LINK)),
					}
				} else {
					Self::DataProcessing {
						cpsr: DataProcessingCpsr::Unchanged,
						operand1: Cpu::PC,
						operand2: Offset::Immediate {
							value: (((instruction & 0x7ff) << 21).cast_signed() >> 9).cast_unsigned(),
							carry: None,
						},
						opcode: DataProcessingOpcode::Add,
						target: register_index(mode, Cpu::LINK),
					}
				}
			}
			0b1110 => {
				Instruction::Branch { offset: ((instruction & 0x7ff) << 21).cast_signed() >> 20, link_register: None }
			}
			0b1101 => {
				if instruction >> 8 & 0b1111 == 0b1111 {
					Self::Interrupt
				} else {
					cond = ((instruction >> 8) & 0b1111) as u8;
					Self::Branch {
						offset: i32::from(((instruction & 0xff) as u8).cast_signed()) << 1,
						link_register: None,
					}
				}
			}
			0b1100 => {
				let index =
					Indexing { base: thumb_register(instruction, 8, mode), write_back: true, ..Indexing::default() };
				Self::BlockDataTransfer {
					registers: parse_register_list(mode, instruction & 0xff, &index)?,
					load: bit(instruction, 11),
					index,
					load_spsr: None,
				}
			}
			0b1011 => {
				if bit(instruction, 10) {
					let load = bit(instruction, 11);
					let index = Indexing {
						base: register_index(mode, Cpu::SP),
						subtract: !load,
						modify_first: !load,
						write_back: true,
					};
					let mut registers = parse_register_list(mode, instruction & 0xff, &index)?;
					if bit(instruction, 8) {
						registers.push(if load { Cpu::PC } else { register_index(mode, Cpu::LINK) });
					}
					Self::BlockDataTransfer { registers, load, index, load_spsr: None }
				} else {
					let sp = register_index(mode, Cpu::SP);
					Self::DataProcessing {
						cpsr: DataProcessingCpsr::Unchanged,
						target: sp,
						opcode: if bit(instruction, 7) {
							DataProcessingOpcode::Subtract
						} else {
							DataProcessingOpcode::Add
						},
						operand1: sp,
						operand2: Offset::Immediate { value: (instruction & 0b111_1111) << 2, carry: None },
					}
				}
			}
			0b1010 => {
				let use_sp = bit(instruction, 11);
				Self::DataProcessing {
					cpsr: DataProcessingCpsr::Unchanged,
					target: thumb_register(instruction, 8, mode),
					opcode: DataProcessingOpcode::Add,
					operand1: register_index(mode, if use_sp { Cpu::SP } else { Cpu::PC as u32 }),
					operand2: Offset::Immediate {
						value: ((instruction & 0xff) << 2).wrapping_sub(if use_sp { 0 } else { address & 0b10 }),
						carry: None,
					},
				}
			}
			0b1001 => Self::SingleDataTransfer {
				target: thumb_register(instruction, 8, mode),
				index: Indexing { base: register_index(mode, Cpu::SP), modify_first: true, ..Indexing::default() },
				offset: Offset::Immediate { value: (instruction & 0xff) << 2, carry: None },
				load: bit(instruction, 11),
				byte: false,
			},
			0b1000 => Self::HalfwordDataTransfer {
				target: thumb_register(instruction, 0, mode),
				index: Indexing {
					base: thumb_register(instruction, 3, mode),
					modify_first: true,
					..Indexing::default()
				},
				mode: if bit(instruction, 11) {
					HalfwordDataTransferMode::LoadUnsignedHalfword
				} else {
					HalfwordDataTransferMode::StoreHalfword
				},
				offset: Offset::Immediate { value: (instruction >> 5) & 0b11_1110, carry: None },
			},
			0b0110..=0b0111 => {
				let byte = bit(instruction, 12);
				let offset = (instruction >> 6) & 0b11111;
				Self::SingleDataTransfer {
					target: thumb_register(instruction, 0, mode),
					index: Indexing {
						base: thumb_register(instruction, 3, mode),
						modify_first: true,
						..Indexing::default()
					},
					offset: Offset::Immediate { value: offset << if byte { 0 } else { 2 }, carry: None },
					load: bit(instruction, 11),
					byte,
				}
			}
			0b0101 => {
				let index =
					Indexing { base: thumb_register(instruction, 3, mode), modify_first: true, ..Indexing::default() };
				let target = thumb_register(instruction, 0, mode);
				let offset = Offset::Register(thumb_register(instruction, 6, mode));
				if bit(instruction, 9) {
					Self::HalfwordDataTransfer {
						target,
						index,
						mode: match (instruction >> 10) & 0b11 {
							0b00 => HalfwordDataTransferMode::StoreHalfword,
							0b01 => HalfwordDataTransferMode::LoadSignedByte,
							0b10 => HalfwordDataTransferMode::LoadUnsignedHalfword,
							0b11 => HalfwordDataTransferMode::LoadSignedHalfword,
							_ => unreachable!(),
						},
						offset,
					}
				} else {
					Self::SingleDataTransfer {
						target,
						index,
						offset,
						load: bit(instruction, 11),
						byte: bit(instruction, 10),
					}
				}
			}
			0b0100 => match (instruction >> 10) & 0b11 {
				0b10..=0b11 => Self::SingleDataTransfer {
					target: thumb_register(instruction, 8, mode),
					index: Indexing {
						base: register_index(mode, Cpu::PC as u32),
						modify_first: true,
						..Indexing::default()
					},
					offset: Offset::Immediate {
						value: ((instruction & 0xff) << 2).wrapping_sub(address & 0b10),
						carry: None,
					},
					load: true,
					byte: false,
				},
				0b01 => {
					let operation = (instruction >> 8) & 0b11;
					let source = register_index(mode, instruction >> 3);
					let target = register_index(mode, (instruction >> 4) & 0b1000 | instruction & 0b111);
					if operation == 0b11 {
						Self::BranchAndExchange { register: source }
					} else {
						let opcode = match operation {
							0b00 => DataProcessingOpcode::Add,
							0b01 => DataProcessingOpcode::TestSubtract,
							0b10 => DataProcessingOpcode::Move,
							_ => unreachable!(),
						};
						Self::DataProcessing {
							cpsr: if opcode == DataProcessingOpcode::TestSubtract {
								DataProcessingCpsr::SetFlags
							} else {
								DataProcessingCpsr::Unchanged
							},
							target,
							opcode,
							operand1: target,
							operand2: Offset::Register(source),
						}
					}
				}
				0b00 => {
					let opcode = instruction >> 6 & 0b1111;
					let source = thumb_register(instruction, 3, mode);
					let target = thumb_register(instruction, 0, mode);
					match opcode {
						0b0000 | 0b0001 | 0b0101..=0b0110 | 0b1000 | 0b1010..=0b1100 | 0b1110 | 0b1111 => {
							Self::DataProcessing {
								cpsr: DataProcessingCpsr::SetFlags,
								target,
								opcode: DataProcessingOpcode::parse(opcode),
								operand1: target,
								operand2: Offset::Register(source),
							}
						}
						0b0010..=0b0100 | 0b0111 => Self::DataProcessing {
							cpsr: DataProcessingCpsr::SetFlags,
							target,
							opcode: DataProcessingOpcode::Move,
							operand1: 0,
							operand2: Offset::ShiftedRegister {
								register: target,
								shift_type: if opcode == 0b0111 { 0b11 } else { opcode - 0b0010 } as u8,
								shift: OffsetShift::Register(source),
							},
						},
						0b1001 => Self::DataProcessing {
							cpsr: DataProcessingCpsr::SetFlags,
							target,
							opcode: DataProcessingOpcode::ReverseSubtract,
							operand1: source,
							operand2: Offset::Immediate { value: 0, carry: None },
						},
						0b1101 => Self::Multiply {
							operand1: target,
							target,
							operand2: source,
							set_flags: true,
							accumulate: None,
						},
						_ => unreachable!(),
					}
				}
				_ => unreachable!(),
			},
			0b0010..=0b0011 => {
				let target = thumb_register(instruction, 8, mode);
				Self::DataProcessing {
					cpsr: DataProcessingCpsr::SetFlags,
					target,
					opcode: match (instruction >> 11) & 0b11 {
						0b00 => DataProcessingOpcode::Move,
						0b01 => DataProcessingOpcode::TestSubtract,
						0b10 => DataProcessingOpcode::Add,
						0b11 => DataProcessingOpcode::Subtract,
						_ => unreachable!(),
					},
					operand1: target,
					operand2: Offset::Immediate { value: instruction & 0xff, carry: None },
				}
			}
			_ => {
				if (instruction >> 11) & 0b11 == 0b11 {
					Self::DataProcessing {
						cpsr: DataProcessingCpsr::SetFlags,
						target: thumb_register(instruction, 0, mode),
						opcode: if bit(instruction, 9) {
							DataProcessingOpcode::Subtract
						} else {
							DataProcessingOpcode::Add
						},
						operand1: thumb_register(instruction, 3, mode),
						operand2: if bit(instruction, 10) {
							Offset::Immediate { value: (instruction >> 6) & 0b111, carry: None }
						} else {
							Offset::Register(thumb_register(instruction, 6, mode))
						},
					}
				} else {
					let shift_type = ((instruction >> 11) & 0b11) as u8;
					Self::DataProcessing {
						cpsr: DataProcessingCpsr::SetFlags,
						target: thumb_register(instruction, 0, mode),
						opcode: DataProcessingOpcode::Move,
						operand1: 0,
						operand2: Offset::ShiftedRegister {
							register: thumb_register(instruction, 3, mode),
							shift_type,
							shift: OffsetShift::parse_immediate(instruction >> 6, shift_type),
						},
					}
				}
			}
		};
		Ok((cond, instruction))
	}
}
