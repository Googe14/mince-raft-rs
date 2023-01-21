use std::{io::{Cursor, Read}, sync::{RwLock, Arc, RwLockReadGuard}, convert::TryInto};

use glam::{IVec2, IVec3};
use glium::{Display, VertexBuffer};
use log::debug;
use mcproto_rs::{v1_16_3::ChunkData, nbt};

use crate::{
    renderer::Vertex, network::read_varint, resources::{BlockState, BLOCKS},
};


use super::{WorldCoords, ChunkCoords, SectionCoords, ChunkLocation};

// Base 2 Log of number of state ids in the game
const MAX_BITS_PER_BLOCK: u64 = 15;
pub const SECTIONS_PER_CHUNK: usize = 16;
pub const MAX_SECTION: i32 = 15;
pub const MIN_SECTION: i32 = 0;
pub type BlockIndex = u16;
pub type ChunkArray = [BlockIndex; 4096];

#[derive(Debug)]
pub struct ChunkSection {
    pub y: i32,
    pub blocks: ChunkArray,
}

impl ChunkSection {
    pub fn new(y: i32, blocks: ChunkArray) -> ChunkSection {
        ChunkSection {
            y,
            blocks,
        }
    }

    /// Convert block coordinates from within a chunk to the chunk section
    pub fn map_from_chunk_coords(coords: &ChunkCoords) -> SectionCoords {
        IVec3::new(coords.x, coords.y.rem_euclid(16), coords.z)
    }

    /// Convert block coordinsate from within this chunk section to the entire chunk
    pub fn map_to_chunk_coords(&self, coords: &SectionCoords) -> ChunkCoords {
        IVec3::new(coords.x, self.y * 16 + coords.y, coords.z)
    }

    /// Get the block at the provided SectionCoords within this chunk section
    pub fn block_at(&self, coords: &SectionCoords) -> Option<&'static BlockState> {
        BLOCKS.get(&self.blocks[block_pos_to_index(coords)].into())
    }

    /// Get the chunk section index of the section containing the provided y level
    pub fn section_containing(y: i32) -> usize {
        y as usize / 16
    }

    pub fn section_at_index(index: usize) -> i32 {
        index as i32
    }

    pub fn index_of_section(section: i32) -> usize {
        section.try_into().unwrap()
    }
}

pub type WrappedChunkSection = Arc<RwLock<ChunkSection>>;
pub type VBO = VertexBuffer<Vertex>;
pub struct Chunk {
    pos: ChunkLocation,
    heightmap: [u16; 256],
    sections: [Option<(WrappedChunkSection, Option<VBO>)>; SECTIONS_PER_CHUNK],
}

impl Chunk {
    pub fn new(data: &ChunkData) -> Chunk {
        debug!("Processing chunk data");

        Chunk {
            pos: IVec2::new(data.position.x, data.position.z),

            heightmap: process_heightmap(data),
            sections: process_sections(data),
        }
    }

    /// Returns an option containing a reference to the request section of this chunk
    pub fn get_section(&self, y: usize) -> Option<RwLockReadGuard<ChunkSection>> {
        self.sections.get(y).unwrap_or(&None).as_ref().map(|(s,_)| s.read().unwrap())
    }

    pub fn get_section_vbo(&self, y: usize) -> Option<&VertexBuffer<Vertex>> {
        self.sections.get(y).unwrap_or(&None).as_ref().map(|(_,vbo)| vbo.as_ref()).unwrap_or(None)
    }

    pub fn get_section_containing(&self, y: i32) -> Option<RwLockReadGuard<ChunkSection>> {
        self.get_section(ChunkSection::section_containing(y))
    }

    pub fn get_coords(&self) -> &ChunkLocation {
        &self.pos
    }

    /// Converts a coordinates of a block from the world to the coordinates within the chunk
    pub fn map_from_world_coords(coords: &WorldCoords) -> ChunkCoords {
        IVec3::new(coords.x.rem_euclid(16), coords.y, coords.z.rem_euclid(16))
    }

    /// Converts a coordinates of a block within this chunk to a position in the world
    pub fn map_to_world_coords(&self, coords: &ChunkCoords) -> WorldCoords {
        assert!(coords.x >= 0 && coords.x < 16);
        assert!(coords.z >= 0 && coords.z < 16);
        IVec3::new(self.pos.x * 16 + coords.x, coords.y, self.pos.y * 16 + coords.z)
    }

    pub fn chunk_containing(coords: &WorldCoords) -> ChunkLocation {
        IVec2::new(coords.x.div_floor(16), coords.z.div_floor(16))
    }

    pub fn load_mesh(&mut self, dis: &Display, verts: Vec<Vertex>, section: usize) {
        self.sections.get_mut(section).map(|cs| cs.as_mut().map(|cs| cs.1 = Some(VertexBuffer::new(dis, &verts).unwrap())));
    }

    pub fn block_at(&self, coords: &ChunkCoords) -> Option<&'static BlockState> {
        self.get_section(ChunkSection::section_containing(coords.y))
            .map(|s| s.block_at(&ChunkSection::map_from_chunk_coords(coords)))
            .unwrap_or(None)
    }

    /// Returns the y value of the highest block at the x/z position provided in this chunk
    pub fn get_highest_block(&self, coords: IVec2) -> i32 {
        self.heightmap[coords.y as usize * 16 + coords.x as usize] as i32
    }
}

/// Extracts the heightmap from chunk data
fn process_heightmap(data: &ChunkData) -> [u16; 256] {
    let mut map = [0u16; 256];

    if let nbt::Tag::Compound(heightmaps) = &data.heightmaps.root.payload {
        if heightmaps.len() != 2 {
            log::error!("Got unexpected number of heightmap compound elements, expected 2 got {}", heightmaps.len());
            return map;
        }

        for heightmap in heightmaps {
            if let nbt::NamedTag {
                name,
                payload: nbt::Tag::LongArray(longs),
            } = heightmap {
                if name != "MOTION_BLOCKING" {
                    continue;
                }

                let vals_per_long: usize = 7;
                for i in 0..256usize {
                    let long = 1 / vals_per_long;
                    let offset = (i % vals_per_long) * 9;

                    map[i] = ((longs[long] >> offset) & 0x1ff) as u16;
                }
            }
        }
    } else {
        log::error!("Didn't get heightmap compound expected from ChunkData");
        return map;
    }

    map
}

/// Builds a list of chunk sections from chunk data
fn process_sections(data: &ChunkData) -> [Option<(Arc<RwLock<ChunkSection>>, Option<VertexBuffer<Vertex>>)>; 16] {
    // Check bit mask for which chunk sections are present
    let mut chunk_sections_present = [false; SECTIONS_PER_CHUNK];
    for i in 0..16 {
        if data.primary_bit_mask.0 & 0b1 << i != 0 {
            chunk_sections_present[i] = true;
        }
    }

    const INIT: Option<(Arc<RwLock<ChunkSection>>, Option<VertexBuffer<Vertex>>)> = None;
    let mut sections = [INIT; SECTIONS_PER_CHUNK];

    // Decode data array
    let mut cur = Cursor::new(&*data.data);
    for i in 0..SECTIONS_PER_CHUNK {
        if !chunk_sections_present[i] {
            continue;
        }

        let mut buf = [0u8; 2];
        cur.read_exact(&mut buf).unwrap();
        let block_count = i16::from_ne_bytes(buf);

        let mut buf = [0u8; 1];
        cur.read_exact(&mut buf).unwrap();
        let mut bits_per_block = buf[0] as u64;

        if bits_per_block <= 4 {
            bits_per_block = 4;
        }
        if bits_per_block >= 9 {
            bits_per_block = MAX_BITS_PER_BLOCK;
        }

        let palette: Option<Vec<i32>>;

        // Construct palette or no palette
        if bits_per_block < 9 {
            let palette_len = read_varint(&mut cur).unwrap();
            log::debug!("Got chunk with pallete of {} elements.", palette_len);
            let mut palette_vec: Vec<i32> = Vec::new();

            for _ in 0..palette_len as usize {
                palette_vec.push(read_varint(&mut cur).unwrap());
            }
            palette = Some(palette_vec);
        } else {
            palette = None;
        }

        // Get long array of blocks
        let array_len = read_varint(&mut cur).unwrap();
        let mut array = Vec::new();

        for _ in 0..array_len as usize {
            let mut buf = [0u8; 8];
            cur.read_exact(&mut buf).unwrap();
            array.push(i64::from_be_bytes(buf));
        }

        // Bit mask depending on bits per block
        let mut mask = 0;
        for j in 0..bits_per_block {
            mask |= 1 << j;
        }
        let mask = mask;

        let blocks_per_long = 64 / bits_per_block;

        let mut blocks = [0u16; 4096];

        // Extract blocks
        for j in 0..4096 {
            let long = j / blocks_per_long;
            let start = (j % blocks_per_long) * bits_per_block;

            // Get block id / palette index from long
            let block = (array[long as usize] >> start) & mask;

            // Get block from palette
            match &palette {
                Some(pal) => {
                    blocks[j as usize] = pal[block as usize] as u16;
                }
                None => {
                    blocks[j as usize] = block as u16;
                }
            }
        }

        sections[i] = Some((Arc::new(RwLock::new(ChunkSection {
            y: i as i32,
            blocks,
        })), None));
    }
    sections
}

/// Converts a block position to an index within a chunk section array
pub fn block_pos_to_index(pos: &IVec3) -> usize {
    ((pos.y % 16) * 16 * 16 + pos.z * 16 + pos.x) as usize
}

/// Converts an index within a chunk section array to a 3d block pos
pub fn block_index_to_pos(i: usize) -> IVec3 {
    let x = i % 16;
    let y = i / (16 * 16);
    let z = (i / 16) % 16;

    IVec3::new(x as i32, y as i32, z as i32)
}
