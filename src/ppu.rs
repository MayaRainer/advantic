use crate::{Bits, Interrupts, read_bytes};
use sdl2::VideoSubsystem;
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{Texture, TextureAccess, WindowCanvas};

pub const SCALE: u32 = 3;
pub const SCREEN_WIDTH: usize = 240;
pub const SCREEN_HEIGHT: u8 = 160;
const VBLANK_LINES: u8 = 68;
pub const TOTAL_LINES: u8 = SCREEN_HEIGHT + VBLANK_LINES;
const RENDER_CYCLES: u16 = 1004;
const HBLANK_CYCLES: u16 = 228;
pub const LINE_CYCLES: u16 = RENDER_CYCLES + HBLANK_CYCLES;

pub struct Ppu {
	canvas: WindowCanvas,
	pub line: u8,
	pub hblank: bool,
	wait_cycles: u16,
	pub palettes: [u8; 0x400],
	pub vram: Vec<u8>,
	registers: [u8; 0x60],
	pub oam: [u8; 0x400],
	reference_points: [i64; 4],
	texture: Texture,
}

const DISPSTAT: u32 = 0x4;
const VCOUNT: u32 = 0x5;

const PIXELS_PER_TILE: usize = 8;
const BYTES_PER_MAP_ENTRY: usize = 2;
const MAP_WIDTH: usize = 256;

fn transform_bg(x: usize, (scroll_y, scroll_x, scaling_data): (i64, i64, [i64; 4])) -> (usize, usize) {
	let x1 = x.cast_signed() as i64;
	let x = (scaling_data[0] * x1 + scroll_x) >> 8;
	let y = (scaling_data[2] * x1 + scroll_y) >> 8;
	let x = x.cast_unsigned() as usize;
	let y = y.cast_unsigned() as usize;
	(y, x)
}

type ObjectWindow<'a> = Option<&'a [u16; SCREEN_WIDTH]>;

impl Ppu {
	#[must_use]
	pub fn new(video: &VideoSubsystem) -> Self {
		let window = video
			.window("Advantic", SCREEN_WIDTH as u32 * SCALE, u32::from(SCREEN_HEIGHT) * SCALE)
			.position_centered()
			.build()
			.unwrap();
		let mut canvas = window.into_canvas().build().unwrap();
		#[allow(clippy::cast_precision_loss)]
		let scale = SCALE as f32;
		canvas.set_scale(scale, scale).unwrap();
		Self {
			texture: canvas
				.texture_creator()
				.create_texture(PixelFormatEnum::BGR555, TextureAccess::Static, SCREEN_WIDTH as u32, u32::from(SCREEN_HEIGHT))
				.unwrap(),
			canvas,
			line: 0,
			wait_cycles: 0,
			hblank: false,
			palettes: [0; _],
			oam: [0; _],
			reference_points: [0; _],
			registers: [0; _],
			vram: vec![0; 0x18000],
		}
	}

	fn get_window(&self, x: usize, object_window: ObjectWindow) -> u8 {
		let control = self.control();
		if control >> 13 == 0 {
			return 0b11111;
		}
		for i in 0..2 {
			let x = x as u8;
			if !control.bit(13 + (i as u16)) {
				continue;
			}
			let offset = 0x40 + i * 2;
			if self.registers[offset] > x
				&& self.registers[offset + 1] <= x
				&& self.registers[offset + 4] > self.line
				&& self.registers[offset + 5] <= self.line
			{
				return self.registers[0x48 + i];
			}
		}
		if let Some(object_window) = object_window
			&& object_window[x] != 0xffff
		{
			return self.registers[0x4b];
		}
		self.registers[0x4a]
	}

	fn pixel_from_tile(&self, tile_line: usize, tile_x: usize, palette: Option<u8>) -> Option<u8> {
		let palette_index = if let Some(palette) = palette {
			let pixel = (self.vram[tile_line + tile_x / 2] >> (tile_x % 2 * 4)) & 0b1111;
			if pixel == 0 {
				return None;
			}
			palette * 16 + pixel
		} else {
			let index = self.vram[tile_line + tile_x];
			if index == 0 {
				return None;
			}
			index
		};
		Some(palette_index)
	}

	fn render_pixel(&self, rendered: &mut [u16], x: usize, colour: u16, layer: u8, object_window: ObjectWindow) {
		let window = self.get_window(x, object_window);
		assert!(!window.bit(5));
		if rendered[x] == 0xffff && window.bit(layer) {
			rendered[x] = colour;
		}
	}

	fn colour_from_palette(&self, offset: usize, index: u8) -> u16 {
		let palette_index = offset + usize::from(index) * 2;
		u16::from_le_bytes(self.palettes[palette_index..=palette_index + 1].try_into().unwrap())
	}

	fn render_objects(
		&self,
		objects: &mut Vec<([u8; 6], u8)>,
		max_priority: u8,
		object_window: ObjectWindow,
		rendered: &mut [u16; SCREEN_WIDTH],
	) {
		let control = self.control();
		let Some(end) = objects.iter().rposition(|(_, priority)| *priority <= max_priority) else { return };
		for (object, _) in objects.drain(0..=end) {
			let size = object[3] >> 6;
			let shape = object[1] >> 6;
			let (width, height) = if shape > 0 {
				let (larger, smaller) = match size {
					0 => (16, 8),
					1 => (32, 8),
					2 => (32, 16),
					3 => (64, 32),
					_ => unreachable!(),
				};
				if shape == 2 { (smaller, larger) } else { (larger, smaller) }
			} else {
				let size = (1 << size) * PIXELS_PER_TILE;
				(size, size)
			};
			let outer_height = if object[1].bit(1) { height * 2 } else { height };
			let outer_width = if object[1].bit(1) { width * 2 } else { width };
			let offset_per_tile_row = if control.bit(6) { width / PIXELS_PER_TILE } else { 0x20 };
			let object_y = usize::from(self.line.wrapping_sub(object[0]));
			if object_y >= outer_height {
				continue;
			}
			let object_y = if object[1].bit(4) { self.mosaic(object_y, 8) } else { object_y };
			assert_eq!((object[1] >> 2) & 0b11, 0, "obj mode");

			let x = usize::from(u16::from_le_bytes(object[2..4].try_into().unwrap()) & 0x1ff);
			let tile_number = usize::from(u16::from_le_bytes(object[4..6].try_into().unwrap()) & 0x3ff);
			let use_standard_palette = object[1].bit(5);
			let pixels_per_byte = if use_standard_palette { 1 } else { 2 };
			let tile_size = PIXELS_PER_TILE.pow(2) / pixels_per_byte;
			let scaling_data = if object[1].bit(0) {
				let scaling_index = usize::from((object[3] >> 1) & 0b11111);
				Some(std::array::from_fn::<_, 4, _>(|i| {
					i32::from(i16::from_le_bytes(self.oam[6 + scaling_index * 32 + i * 8..][..2].try_into().unwrap()))
				}))
			} else {
				None
			};

			for object_x in 0..outer_width {
				let x = (x + object_x) % 512;
				if x >= SCREEN_WIDTH {
					continue;
				}
				let object_x = if object[1].bit(4) { self.mosaic(object_x, 12) } else { object_x };
				#[allow(clippy::cast_possible_wrap)]
				let (object_y, object_x) = if let Some(scaling_data) = scaling_data {
					let x1 = object_x as i32 - (outer_width / 2) as i32;
					let y1 = object_y as i32 - (outer_height / 2) as i32;
					let x = ((scaling_data[0] * x1 + scaling_data[1] * y1) >> 8) + (width / 2) as i32;
					let y = ((scaling_data[2] * x1 + scaling_data[3] * y1) >> 8) + (height / 2) as i32;
					(y.cast_unsigned() as usize, x.cast_unsigned() as usize)
				} else {
					(
						if object[3].bit(5) { height - object_y - 1 } else { object_y },
						if object[3].bit(4) { width - object_x - 1 } else { object_x },
					)
				};
				if !(0..width).contains(&object_x) || !(0..height).contains(&object_y) {
					continue;
				}

				let tile_row_index = (tile_number + object_y / PIXELS_PER_TILE * offset_per_tile_row) % 1024;
				let tile_address = 0x10000 + tile_row_index * 32 + object_x / PIXELS_PER_TILE * tile_size;
				let tile_line = tile_address + (object_y % PIXELS_PER_TILE) * PIXELS_PER_TILE / pixels_per_byte;
				let tile_x = object_x % PIXELS_PER_TILE;
				let colour = self.pixel_from_tile(
					tile_line,
					tile_x,
					if use_standard_palette { None } else { Some(object[5] >> 4) },
				);
				if let Some(colour) = colour {
					self.render_pixel(rendered, x, self.colour_from_palette(0x200, colour), 4, object_window);
				}
			}
		}
	}

	fn bg_control(&self, layer: u16) -> u16 {
		u16::from_le_bytes(self.registers[8 + (layer as usize) * 2..][..2].try_into().unwrap())
	}

	fn load_reference_point(&mut self, index: usize) {
		let register = &self.registers[0x28 + 0x10 * (index / 2) + 4 * (index % 2)..];
		self.reference_points[index] = i64::from(i32::from_le_bytes(register[..4].try_into().unwrap()) << 4) >> 4;
	}

	fn bg_scaling_data(&mut self, layer: u16) -> (i64, i64, [i64; 4]) {
		let reference_points = &mut self.reference_points[usize::from(layer - 2) * 2..];
		let x = reference_points[0];
		let y = reference_points[1];
		let registers = &self.registers[usize::from(layer) * 0x10..];
		let data = std::array::from_fn::<_, 4, _>(|i| {
			i64::from(i16::from_le_bytes(registers[i * 2..][..2].try_into().unwrap()))
		});
		reference_points[0] += data[1];
		reference_points[1] += data[3];
		(y, x, data)
	}

	fn objects(&self) -> impl Iterator<Item = ([u8; 6], u8)> {
		let mode = self.control() & 0b111;
		self.oam
			.chunks(8)
			.filter(move |data| {
				let tile_number = usize::from(u16::from_le_bytes(data[4..6].try_into().unwrap()) & 0x3ff);
				(data[1] & 0b11) != 0b10 && mode < 3 || tile_number >= 512
			})
			.map(|data| (data[0..6].try_into().unwrap(), (data[5] >> 2) & 0b11))
	}

	fn control(&self) -> u16 {
		u16::from_le_bytes(self.registers[0..2].try_into().unwrap())
	}

	fn mosaic(&self, value: usize, shift: u8) -> usize {
		value - (value % usize::from((self.registers[0x4c + usize::from(shift / 8)] >> (shift % 8) & 0xf) + 1))
	}

	fn render_line(&mut self) {
		let control = self.control();
		if control.bit(7) {
			return;
		}
		let mode = control & 0b111;

		let object_window = if control.bit(15) {
			let mut window = [0xffffu16; SCREEN_WIDTH];
			let mut objects: Vec<_> = self.objects().filter(|(data, _)| (data[1] >> 2) & 0b11 == 0b10).collect();
			self.render_objects(&mut objects, 4, None, &mut window);
			Some(window)
		} else {
			None
		};
		let mut objects = if control.bit(12) {
			self.objects().filter(|(data, _)| (data[1] >> 2) & 0b11 != 0b10).collect()
		} else {
			vec![]
		};
		let mut rendered = [0xffffu16; SCREEN_WIDTH];
		objects.sort_by(|(_, prio1), (_, prio2)| prio1.cmp(prio2));
		match mode {
			0..=2 => {
				let mut layers = Vec::new();
				for layer in 0..4 {
					if control.bit(8 + layer)
						&& (layer == 2 || (0..=1).contains(&layer) && mode != 2 || layer == 3 && mode == 2)
					{
						layers.push((layer, (self.bg_control(layer) & 0b11) as u8));
					}
				}
				layers.sort_by(|(_, a_prio), (_, b_prio)| a_prio.cmp(b_prio));

				for (layer, priority) in layers {
					self.render_objects(&mut objects, priority, object_window.as_ref(), &mut rendered);
					let control = self.bg_control(layer);
					let y = usize::from(self.line);
					let y = if control.bit(6) { self.mosaic(y, 0) } else { y };
					let map_offset = 0x800 * usize::from((control >> 8) & 0b11111);
					let tile_offset = 0x4000 * usize::from((control >> 2) & 0b11);

					let scaling_data =
						if mode == 2 || mode == 1 && layer == 2 { Some(self.bg_scaling_data(layer)) } else { None };

					for x in 0..SCREEN_WIDTH {
						let bg_x = if control.bit(6) { self.mosaic(x, 4) } else { x };
						let (tile_index, tile_y, tile_x, palette) = if let Some(scaling_data) = scaling_data {
							let size = 16 << (control >> 14);
							let size_in_pixels = size * PIXELS_PER_TILE;
							let (mut bg_y, mut bg_x) = transform_bg(bg_x, scaling_data);
							if control.bit(13) {
								bg_y %= size_in_pixels;
								bg_x %= size_in_pixels;
							} else if !(0..size_in_pixels).contains(&bg_x) || !(0..size_in_pixels).contains(&bg_y) {
								continue;
							}
							let map_index = map_offset + (bg_y / PIXELS_PER_TILE) * size + bg_x / PIXELS_PER_TILE;
							let tile_index = usize::from(self.vram[map_index]);
							(tile_index, bg_y % PIXELS_PER_TILE, bg_x % PIXELS_PER_TILE, None)
						} else {
							fn pixel_to_map_index(pixel: usize) -> usize {
								(pixel % MAP_WIDTH) / PIXELS_PER_TILE
							}
							fn get_scroll(registers: &[u8]) -> usize {
								usize::from(u16::from_le_bytes(registers[..2].try_into().unwrap()) & 0x1ff)
							}
							let scroll = &self.registers[0x10 + (layer as usize) * 4..];
							let bg_x = bg_x + get_scroll(scroll);
							let bg_y = y + get_scroll(&scroll[2..]);

							let mut area_index = 0;
							if control.bit(14) && bg_x % 512 >= MAP_WIDTH {
								area_index += 1;
							}
							if control.bit(15) && bg_y % 512 >= MAP_WIDTH {
								area_index += if control.bit(15) { 2 } else { 1 };
							}

							let area_offset =
								map_offset + area_index * (MAP_WIDTH / PIXELS_PER_TILE).pow(2) * BYTES_PER_MAP_ENTRY;
							let area_line_index = area_offset
								+ pixel_to_map_index(bg_y) * MAP_WIDTH / PIXELS_PER_TILE * BYTES_PER_MAP_ENTRY;
							let map_entry_index = area_line_index + pixel_to_map_index(bg_x) * BYTES_PER_MAP_ENTRY;
							let map_entry = u16::from_le_bytes(
								self.vram[map_entry_index..][..BYTES_PER_MAP_ENTRY].try_into().unwrap(),
							);
							let tile_y = bg_y % PIXELS_PER_TILE;
							let tile_x = bg_x % PIXELS_PER_TILE;
							(
								usize::from(map_entry) & 0x3ff,
								if map_entry.bit(11) { 7 - tile_y } else { tile_y },
								if map_entry.bit(10) { 7 - tile_x } else { tile_x },
								if control.bit(7) { None } else { Some((map_entry >> 12) as u8) },
							)
						};
						let pixels_per_byte = if palette.is_none() { 1 } else { 2 };
						let tile_address = tile_offset + tile_index * PIXELS_PER_TILE.pow(2) / pixels_per_byte;
						let tile_line = tile_address + tile_y * PIXELS_PER_TILE / pixels_per_byte;
						if tile_line >= 0x10000 {
							// do not render tiles in object VRAM
							continue;
						}

						let colour = self.pixel_from_tile(tile_line, tile_x, palette);
						if let Some(colour) = colour {
							self.render_pixel(
								&mut rendered,
								x,
								self.colour_from_palette(0, colour),
								layer as u8,
								object_window.as_ref(),
							);
						}
					}
				}
			}
			3..=5 => {
				if control.bit(10) {
					let priority = (self.bg_control(2) & 0b11) as u8;
					self.render_objects(&mut objects, priority, object_window.as_ref(), &mut rendered);

					let offset = if mode > 3 && control.bit(4) { 0xa000 } else { 0 };
					let scaling_data = self.bg_scaling_data(2);
					let width = if mode == 5 { 160 } else { SCREEN_WIDTH };
					let height = usize::from(if mode == 5 { 128 } else { SCREEN_HEIGHT });
					for x in 0..SCREEN_WIDTH {
						let (bg_y, bg_x) = transform_bg(x, scaling_data);
						if !(0..width).contains(&bg_x) || !(0..height).contains(&bg_y) {
							continue;
						}
						let index = bg_y * width + bg_x;
						let pixel = if mode == 4 {
							self.colour_from_palette(0, self.vram[offset + index])
						} else {
							u16::from_le_bytes(self.vram[offset + index * 2..][..2].try_into().unwrap())
						};
						self.render_pixel(&mut rendered, x, pixel, 2, object_window.as_ref());
					}
				}
			}
			_ => unimplemented!(),
		}
		self.render_objects(&mut objects, 4, object_window.as_ref(), &mut rendered);

		let default_colour = self.colour_from_palette(0, 0);
		let pixels: Vec<u8> = rendered
			.into_iter()
			.flat_map(|colour| (if colour == 0xffff { default_colour } else { colour }).to_le_bytes())
			.collect();
		self.texture.update(Rect::new(0, i32::from(self.line), SCREEN_WIDTH as u32, 1), &pixels, SCREEN_WIDTH * 2).unwrap();
	}

	pub(crate) fn step(&mut self, interrupts: &mut Interrupts) {
		let dispstat = self.registers[DISPSTAT as usize];
		if self.wait_cycles > 0 {
			self.wait_cycles -= 1;
			return;
		}
		if self.hblank || self.line >= SCREEN_HEIGHT {
			match self.line {
				SCREEN_HEIGHT => {
					for i in 0..4 {
						self.load_reference_point(i);
					}
					if dispstat.bit(3) {
						interrupts.interrupt(0);
					}
				}
				TOTAL_LINES => {
					self.line = 0xff;
					self.canvas.copy(&self.texture, None, None).unwrap();
					self.canvas.present();
				}
				_ => {}
			}
			self.line = self.line.wrapping_add(1);
			if dispstat.bit(5) && self.line == self.registers[VCOUNT as usize] {
				interrupts.interrupt(2);
			}
			self.wait_cycles = if self.hblank { RENDER_CYCLES } else { LINE_CYCLES };
			self.hblank = false;
		} else if self.line < SCREEN_HEIGHT {
			self.hblank = true;
			self.render_line();
			self.wait_cycles = HBLANK_CYCLES;
			if dispstat.bit(4) {
				interrupts.interrupt(1);
			}
		}
	}

	pub fn read_register(&self, address: u32) -> u32 {
		match address {
			DISPSTAT => {
				let mut dispstat = u32::from(u16::from_le_bytes(self.registers[4..6].try_into().unwrap()));
				dispstat |= u32::from(self.line) << 16;
				dispstat.set_bit(0, self.line >= SCREEN_HEIGHT);
				dispstat.set_bit(1, self.hblank);
				dispstat.set_bit(2, self.line == self.registers[VCOUNT as usize]);
				dispstat
			}
			_ => {
				read_bytes(&self.registers, address)
			}
		}
	}

	pub fn write_register(&mut self, address: u32, value: u8) {
		self.registers[address as usize] = value;
		if matches!(address, 0x28..0x2f | 0x38..0x3f) {
			let address = address - 0x28;
			self.load_reference_point(((address % 8) / 4 + (address / 0x10) * 2) as usize);
		}
	}
}
