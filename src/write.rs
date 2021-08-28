use std::{
    cmp,
    collections::HashMap,
    io::{self, Write},
};

use bitstream_io::{BitWrite, BitWriter};
use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use flate2::{write::ZlibEncoder, Compression};
use thiserror::Error;

use crate::{
    ext::write::*,
    save::{BrickColor, SaveData, Size},
    MAGIC_BYTES, SAVE_VERSION,
};

/// A write error.
#[derive(Error, Debug)]
pub enum WriteError {
    #[error("generic io error")]
    IoError(#[from] io::Error),
}

/// A save writer, which writes its `data` to its `writer` (a `Write`).
pub struct SaveWriter<W: Write> {
    writer: W,
    data: SaveData,
}

impl<W: Write> SaveWriter<W> {
    pub fn new(writer: W, data: SaveData) -> SaveWriter<W> {
        SaveWriter { writer, data }
    }

    /// Writes the magic bytes and the save version, effectively "header 0".
    fn write_header0(&mut self) -> Result<(), WriteError> {
        self.writer.write_all(&MAGIC_BYTES)?;
        self.writer.write_u16::<LittleEndian>(SAVE_VERSION)?;
        self.writer
            .write_i32::<LittleEndian>(self.data.game_version)?;
        Ok(())
    }

    /// Writes the first header.
    pub fn write_header1(&mut self) -> Result<(), WriteError> {
        self.write_header0()?;

        // this Vec<u8> will store the bytes to the header, and eventually
        // will be compressed when necessary
        let mut w: Vec<u8> = vec![];
        w.write_string(self.data.header1.map.clone())?;
        w.write_string(self.data.header1.author.name.clone())?;
        w.write_string(self.data.header1.description.clone())?;
        w.write_uuid(self.data.header1.author.id)?;

        // if the host is None, then we assume it to be the
        // same as the author. can safely write the same value
        let host = self
            .data
            .header1
            .host
            .clone()
            .unwrap_or_else(|| self.data.header1.author.clone());
        w.write_string(host.name)?;
        w.write_uuid(host.id)?;

        w.write_all(&self.data.header1.save_time)?;
        w.write_i32::<LittleEndian>(self.data.bricks.len() as i32)?;

        write_compressed(&mut self.writer, w)?;

        Ok(())
    }

    /// Writes the second header.
    pub fn write_header2(&mut self) -> Result<(), WriteError> {
        // see above for compression methods
        let mut w: Vec<u8> = vec![];
        w.write_array(self.data.header2.mods.clone(), |writer, string| {
            writer.write_string(string)
        })?;
        w.write_array(self.data.header2.brick_assets.clone(), |writer, string| {
            writer.write_string(string)
        })?;
        w.write_array(self.data.header2.colors.clone(), |writer, color| {
            writer.write_color_bgra(color)
        })?;
        w.write_array(self.data.header2.materials.clone(), |writer, string| {
            writer.write_string(string)
        })?;

        w.write_array(
            self.data.header2.brick_owners.clone(),
            |writer, brick_owner| -> io::Result<()> {
                writer.write_uuid(brick_owner.id)?;
                writer.write_string(brick_owner.name)?;
                writer.write_i32::<LittleEndian>(brick_owner.bricks as i32)?;
                Ok(())
            },
        )?;

        w.write_array(
            self.data.header2.physical_materials.clone(),
            |writer, string| writer.write_string(string),
        )?;

        write_compressed(&mut self.writer, w)?;

        Ok(())
    }

    /// Writes the preview.
    pub fn write_preview(&mut self) -> Result<(), WriteError> {
        let preview = self.data.preview.clone();
        let preview_type = preview.type_byte();
        self.writer.write_u8(preview_type)?;
        match preview_type {
            0 => (),
            _ => {
                let bytes = preview.unwrap();
                self.writer.write_i32::<LittleEndian>(bytes.len() as i32)?;
                self.writer.write_all(&bytes)?
            }
        }
        Ok(())
    }

    /// Writes the bricks and components.
    pub fn write_bricks(&mut self) -> Result<(), WriteError> {
        let mut vec = vec![];
        let mut bits = BitWriter::endian(&mut vec, bitstream_io::LittleEndian);

        let asset_name_count = cmp::max(self.data.header2.brick_assets.len(), 2);
        let material_count = cmp::max(self.data.header2.materials.len(), 2);
        let physical_material_count = cmp::max(self.data.header2.physical_materials.len(), 2);
        let color_count = cmp::max(self.data.header2.colors.len(), 2);
        let brick_count = self.data.bricks.len();

        let mut component_bricks: HashMap<String, Vec<u32>> = HashMap::new();

        for (i, brick) in self.data.bricks.clone().into_iter().enumerate() {
            bits.byte_align()?;

            // write asset name index: <asset_name_index: u32; N>
            bits.write_uint(brick.asset_name_index, asset_name_count as u32)?;

            // write brick size:
            // <procedural?: bit>[x: uint_packed][y: uint_packed][z: uint_packed]
            match brick.size {
                Size::Procedural(x, y, z) => {
                    bits.write_bit(true)?;
                    bits.write_uint_packed(x)?;
                    bits.write_uint_packed(y)?;
                    bits.write_uint_packed(z)?;
                }
                Size::Empty => bits.write_bit(false)?,
            }

            // write position:
            // <x: int_packed><y: int_packed><z: int_packed>
            bits.write_int_packed(brick.position.0)?;
            bits.write_int_packed(brick.position.1)?;
            bits.write_int_packed(brick.position.2)?;

            // write orientation: <orientation: uint; 24>
            let orientation = ((brick.direction as u32) << 2) | (brick.rotation as u32);
            bits.write_uint(orientation, 24)?;

            // write collision bits:
            // <player: bit><weapon: bit><interaction: bit><tool: bit>
            bits.write_bit(brick.collision.player)?;
            bits.write_bit(brick.collision.weapon)?;
            bits.write_bit(brick.collision.interaction)?;
            bits.write_bit(brick.collision.tool)?;

            // write visibility: <visibility: bit>
            bits.write_bit(brick.visibility)?;

            // write material index: <material_index: u32; N>
            bits.write_uint(brick.material_index, material_count as u32)?;

            // write physical index: <physical_index: u32; N>
            bits.write_uint(brick.physical_index, physical_material_count as u32)?;

            // write material intensity: <material_intensity: u32; 11>
            bits.write_uint(brick.material_intensity, 11)?;

            // write color:
            // <unique?: bit 0><index: uint; N> OR
            // <unique?: bit 1><r: byte><g: byte><b: byte>
            match brick.color {
                BrickColor::Index(ind) => {
                    bits.write_bit(false)?;
                    bits.write_uint(ind, color_count as u32)?;
                }
                BrickColor::Unique(color) => {
                    bits.write_bit(true)?;
                    let bytes = [color.r, color.g, color.b];
                    bits.write_bytes(&bytes)?;
                }
            }

            // write owner index: <owner_index: uint packed>
            bits.write_uint_packed(brick.owner_index)?;

            for (key, _) in brick.components {
                match component_bricks.get_mut(&key) {
                    Some(vec) => vec.push(i as u32),
                    None => {
                        component_bricks.insert(key, vec![i as u32]);
                    }
                }
            }
        }

        bits.byte_align()?;

        write_compressed(&mut self.writer, vec)?;

        let mut vec: Vec<u8> = vec![];
        vec.write_i32::<LittleEndian>(self.data.components.len() as i32)?;

        for (name, component) in self.data.components.clone().into_iter() {
            vec.write_string(name.clone())?;

            let mut bits = BitWriter::endian(vec, bitstream_io::LittleEndian);
            let mut version_bytes = [0u8; 4];
            LittleEndian::write_i32(&mut version_bytes, component.version);

            // write version
            bits.write_bytes(&version_bytes)?;

            // write brick indices
            bits.write_array(&component_bricks[&name], |writer, &i| {
                writer.write_uint(i, cmp::max(brick_count as u32, 2))
            })?;

            // write properties
            let properties = component
                .properties
                .clone()
                .into_iter()
                .collect::<Vec<(String, String)>>();
            bits.write_array(&properties, |writer, (key, val)| -> io::Result<()> {
                writer.write_string(key.clone())?;
                writer.write_string(val.clone())?;
                Ok(())
            })?;

            // read brick indices
            for &i in component_bricks[&name].iter() {
                for (p, _) in component
                    .properties
                    .clone()
                    .into_iter()
                    .collect::<Vec<(String, String)>>()
                {
                    let brick = &self.data.bricks[i as usize];
                    bits.write_unreal(brick.components[&name][&p].clone())?;
                }
            }

            bits.byte_align()?;
            vec = bits.into_writer();
        }

        write_compressed(&mut self.writer, vec)?;

        Ok(())
    }

    /// Writes the full save.
    pub fn write(&mut self) -> Result<(), WriteError> {
        self.write_header1()?;
        self.write_header2()?;
        self.write_preview()?;
        self.write_bricks()?;
        Ok(())
    }
}

/// Write a `Vec<u8>` out to a `Write`, following the BRS spec for compression.
fn write_compressed(writer: &mut impl Write, vec: Vec<u8>) -> io::Result<()> {
    let compressed = ZlibEncoder::new(vec.clone(), Compression::default()).finish()?;

    writer.write_i32::<LittleEndian>(vec.len() as i32)?;

    if compressed.len() < vec.len() {
        // compressed is smaller, write (unc_size: i32, c_size: i32, bytes)
        writer.write_i32::<LittleEndian>(compressed.len() as i32)?;
        writer.write_all(&compressed[..])?;
    } else {
        // write uncompressed (unc_size: i32, c_size: i32 = 0, bytes)
        writer.write_i32::<LittleEndian>(0)?;
        writer.write_all(&vec[..])?;
    }

    Ok(())
}
