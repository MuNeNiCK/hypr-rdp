use anyhow::{Context, Result};
use ironrdp_core::{Encode, WriteCursor};
use ironrdp_graphics::color_conversion;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::codecs::rfx::{
    Block, ChannelsPdu, CodecChannel, CodecVersionsPdu, ContextPdu, EntropyAlgorithm,
    FrameBeginPdu, FrameEndPdu, OperatingMode, Quant, RegionPdu, RfxChannel, RfxRectangle,
    SyncPdu, Tile, TileSetPdu,
};

const RLGR_BUF_SIZE: usize = 64 * 64 * 2;

/// RemoteFX encoder for EGFX transport.
pub struct RfxEncoder {
    width: u32,
    height: u32,
    frame_index: u32,
    quant: Quant,
    y_buf: [i16; 64 * 64],
    cb_buf: [i16; 64 * 64],
    cr_buf: [i16; 64 * 64],
    rlgr_y: Vec<u8>,
    rlgr_cb: Vec<u8>,
    rlgr_cr: Vec<u8>,
}

impl RfxEncoder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame_index: 0,
            quant: Quant::default(),
            y_buf: [0i16; 64 * 64],
            cb_buf: [0i16; 64 * 64],
            cr_buf: [0i16; 64 * 64],
            rlgr_y: vec![0u8; RLGR_BUF_SIZE],
            rlgr_cb: vec![0u8; RLGR_BUF_SIZE],
            rlgr_cr: vec![0u8; RLGR_BUF_SIZE],
        }
    }

    /// Encode a BGRA frame into an RFX message sequence for WireToSurface1Pdu.
    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        let tiles_x = (self.width + 63) / 64;
        let tiles_y = (self.height + 63) / 64;

        struct EncodedTile {
            x: u16,
            y: u16,
            y_len: usize,
            cb_len: usize,
            data: Vec<u8>,
        }

        let mut encoded_tiles = Vec::with_capacity((tiles_x * tiles_y) as usize);

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let tile_x = tx * 64;
                let tile_y = ty * 64;
                let tile_w = (self.width - tile_x).min(64);
                let tile_h = (self.height - tile_y).min(64);

                let tile_offset = (tile_y as usize) * stride + (tile_x as usize) * 4;
                let tile_slice = &bgra[tile_offset..];

                color_conversion::to_64x64_ycbcr_tile(
                    tile_slice, tile_w, tile_h, stride as u32,
                    PixelFormat::BgrX32,
                    &mut self.y_buf, &mut self.cb_buf, &mut self.cr_buf,
                ).context("YCbCr conversion")?;

                let y_len = ironrdp_graphics::rfx_encode_component(
                    &mut self.y_buf, &mut self.rlgr_y, &self.quant, EntropyAlgorithm::Rlgr3,
                ).context("RLGR Y")?;
                let cb_len = ironrdp_graphics::rfx_encode_component(
                    &mut self.cb_buf, &mut self.rlgr_cb, &self.quant, EntropyAlgorithm::Rlgr3,
                ).context("RLGR Cb")?;
                let cr_len = ironrdp_graphics::rfx_encode_component(
                    &mut self.cr_buf, &mut self.rlgr_cr, &self.quant, EntropyAlgorithm::Rlgr3,
                ).context("RLGR Cr")?;

                let mut data = Vec::with_capacity(y_len + cb_len + cr_len);
                data.extend_from_slice(&self.rlgr_y[..y_len]);
                data.extend_from_slice(&self.rlgr_cb[..cb_len]);
                data.extend_from_slice(&self.rlgr_cr[..cr_len]);

                encoded_tiles.push(EncodedTile {
                    x: (tile_x / 64) as u16,
                    y: (tile_y / 64) as u16,
                    y_len, cb_len, data,
                });
            }
        }

        let tiles: Vec<Tile<'_>> = encoded_tiles.iter().map(|t| {
            Tile {
                y_quant_index: 0,
                cb_quant_index: 0,
                cr_quant_index: 0,
                x: t.x,
                y: t.y,
                y_data: &t.data[..t.y_len],
                cb_data: &t.data[t.y_len..t.y_len + t.cb_len],
                cr_data: &t.data[t.y_len + t.cb_len..],
            }
        }).collect();

        let region_rects = vec![RfxRectangle {
            x: 0, y: 0,
            width: self.width as u16,
            height: self.height as u16,
        }];

        let mut all_blocks: Vec<Block<'_>> = Vec::new();
        if self.frame_index == 0 {
            all_blocks.push(Block::Sync(SyncPdu));
            all_blocks.push(Block::CodecVersions(CodecVersionsPdu));
            all_blocks.push(Block::Channels(ChannelsPdu(vec![RfxChannel {
                width: self.width as i16,
                height: self.height as i16,
            }])));
        }
        all_blocks.push(Block::CodecChannel(CodecChannel::Context(ContextPdu {
            flags: OperatingMode::empty(),
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
        })));
        all_blocks.push(Block::CodecChannel(CodecChannel::FrameBegin(FrameBeginPdu {
            index: self.frame_index,
            number_of_regions: 1,
        })));
        all_blocks.push(Block::CodecChannel(CodecChannel::Region(RegionPdu {
            rectangles: region_rects,
        })));
        all_blocks.push(Block::CodecChannel(CodecChannel::TileSet(TileSetPdu {
            entropy_algorithm: EntropyAlgorithm::Rlgr3,
            quants: vec![self.quant.clone()],
            tiles,
        })));
        all_blocks.push(Block::CodecChannel(CodecChannel::FrameEnd(FrameEndPdu)));

        self.frame_index += 1;

        let total_size: usize = all_blocks.iter().map(|b| b.size()).sum();
        let mut buf = vec![0u8; total_size];
        let mut cursor = WriteCursor::new(&mut buf);
        for block in &all_blocks {
            block.encode(&mut cursor).context("RFX block encode")?;
        }

        Ok(buf)
    }
}
