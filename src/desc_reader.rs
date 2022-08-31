// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Result, SeekFrom };
use std::path::PathBuf;
use byteorder::{ ReadBytesExt, BigEndian };
use crate::{ fourcc, read_box, typ_to_str };

#[derive(Default, Clone, Debug)]
pub struct TrackDesc {
    pub tkhd_duration: u64,
    pub elst_segment_duration: u64,
    pub mdhd_duration: u64,
    pub stts: Vec<(u32, u32)>,
    pub stsz: Vec<u32>,
    pub stco: Vec<u64>,
    pub stss: Vec<u32>,
    pub sdtp: Vec<u8>,
    pub stss_offset: u32,
    pub stsz_sample_size: u32,
    pub stsz_count: u32,
}

#[derive(Default, Clone, Debug)]
pub struct Desc {
    pub mdat_position: Vec<(Option<PathBuf>, u64, u64)>, // file path, offset, size
    pub moov_mvhd_duration: u64,
    pub moov_tracks: Vec<TrackDesc>,
    pub mdat_offset: u64,
    pub mdat_final_position: u64,
}

pub fn read_desc<R: Read + Seek>(d: &mut R, desc: &mut Desc, track: usize, max_read: u64) -> Result<()> {
    let mut total_read_size = 0;
    let mut tl_track = track;
    while let Ok((typ, offs, size, header_size)) = read_box(d) {
        if size == 0 || typ == 0 { break; }
        total_read_size += size;
        if crate::has_children(typ) {
            read_desc(d, desc, tl_track, size - header_size as u64)?;

            if typ == fourcc("trak") {
                tl_track += 1;
            }
        } else {
            log::debug!("Reading {}, offset: {}, size: {size}, header_size: {header_size}", typ_to_str(typ), offs);
            let org_pos = d.stream_position()?;
            if typ == fourcc("mdat") {
                desc.mdat_position.push((None, org_pos, size - header_size as u64));
                desc.mdat_final_position = org_pos;
            }
            if typ == fourcc("mvhd") || typ == fourcc("tkhd") || typ == fourcc("mdhd") {
                let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);
                if typ == fourcc("mvhd") {
                    desc.moov_mvhd_duration += if v == 1 { d.seek(SeekFrom::Current(8+8+4))?; d.read_u64::<BigEndian>()? }
                                               else      { d.seek(SeekFrom::Current(4+4+4))?; d.read_u32::<BigEndian>()? as u64 };
                }
                if let Some(track_desc) = desc.moov_tracks.get_mut(tl_track) {
                    if typ == fourcc("tkhd") {
                        track_desc.tkhd_duration += if v == 1 { d.seek(SeekFrom::Current(8+8+4+4))?; d.read_u64::<BigEndian>()? }
                                                    else      { d.seek(SeekFrom::Current(4+4+4+4))?; d.read_u32::<BigEndian>()? as u64 };
                    }
                    if typ == fourcc("mdhd") {
                        track_desc.mdhd_duration += if v == 1 { d.seek(SeekFrom::Current(8+8+4))?; d.read_u64::<BigEndian>()? }
                                                    else      { d.seek(SeekFrom::Current(4+4+4))?; d.read_u32::<BigEndian>()? as u64 };
                    }
                }
            }
            if typ == fourcc("elst") || typ == fourcc("stts") || typ == fourcc("stsz") || typ == fourcc("stss") || typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("sdtp") {
                let track_desc = desc.moov_tracks.get_mut(tl_track).unwrap();
                let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);

                if typ == fourcc("elst") {
                    d.seek(SeekFrom::Current(4))?; // Skip fields
                    track_desc.elst_segment_duration += if v == 1 { d.read_u64::<BigEndian>()? } else { d.read_u32::<BigEndian>()? as u64 };
                }
                if typ == fourcc("stsz") {
                    track_desc.stsz_sample_size = d.read_u32::<BigEndian>()?;
                    let count = d.read_u32::<BigEndian>()?;
                    if track_desc.stsz_sample_size == 0 {
                        for _ in 0..count { track_desc.stsz.push(d.read_u32::<BigEndian>()?); }
                    }
                    track_desc.stsz_count += count;
                }
                if typ == fourcc("sdtp") {
                    let count = size - header_size as u64 - 4;
                    for _ in 0..count { track_desc.sdtp.push(d.read_u8()?); }
                }
                if typ == fourcc("stss") || typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("stts") {
                    let count = d.read_u32::<BigEndian>()?;
                    let current_file_mdat_position = desc.mdat_position.last().unwrap().1;
                    let mdat_offset = desc.mdat_offset as i64 - current_file_mdat_position as i64;
                    for _ in 0..count {
                        if typ == fourcc("stss") { track_desc.stss.push(d.read_u32::<BigEndian>()? + track_desc.stss_offset); }
                        if typ == fourcc("stco") { track_desc.stco.push((d.read_u32::<BigEndian>()? as i64 + mdat_offset) as u64); }
                        if typ == fourcc("co64") { track_desc.stco.push((d.read_u64::<BigEndian>()? as i64 + mdat_offset) as u64); }
                        if typ == fourcc("stts") { track_desc.stts.push((d.read_u32::<BigEndian>()?, d.read_u32::<BigEndian>()?)); }
                    }
                }
            }
            d.seek(SeekFrom::Start(org_pos + size - header_size as u64))?;
        }
        if total_read_size >= max_read {
            break;
        }
    }
    Ok(())
}
