// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Result, SeekFrom };
use byteorder::{ ReadBytesExt, BigEndian };
use crate::{ fourcc, read_box, typ_to_str };

#[derive(Default, Clone, Debug)]
pub struct TrackDesc {
    pub tkhd_duration: u64,
    pub elst_segment_duration: u64,
    pub mdhd_timescale: u32,
    pub mdhd_duration: u64,
    pub stts: Vec<(u32, u32)>,
    pub stsz: Vec<u32>,
    pub stco: Vec<u64>,
    pub stss: Vec<u32>,
    pub sdtp: Vec<u8>,
    pub sample_offset: u32,
    pub chunk_offset: u32,
    pub stsz_sample_size: u32,
    pub stsz_count: u32,
    pub stsc: Vec<(u32, u32, u32)>, // first_chunk, samples_per_chunk, sample_description_index
    pub co64_final_position: u64,
    pub skip: bool,
}

#[derive(Default, Clone, Debug)]
pub struct Desc {
    pub mdat_position: Vec<(Option<usize>, u64, u64)>, // file path, offset, size
    pub mvhd_timescale_per_file: Vec<u32>,
    pub moov_mvhd_timescale: u32,
    pub moov_mvhd_duration: u64,
    pub moov_tracks: Vec<TrackDesc>,
    pub mdat_offset: u64,
    pub mdat_final_position: u64,
}

pub fn read_desc<R: Read + Seek>(d: &mut R, desc: &mut Desc, track: usize, max_read: u64, file_index: usize) -> Result<()> {
    let mut tl_track = track;
    let start_offs = d.stream_position()?;
    desc.mvhd_timescale_per_file.push(0);
    while let Ok((typ, offs, size, header_size)) = read_box(d) {
        if size == 0 || typ == 0 { continue; }
        if crate::has_children(typ, true) {
            read_desc(d, desc, tl_track, size - header_size as u64, file_index)?;

            if typ == fourcc("trak") {
                tl_track += 1;
            }
        } else {
            log::debug!("Reading {}, offset: {}, size: {size}, header_size: {header_size}", typ_to_str(typ), offs);
            let org_pos = d.stream_position()?;
            // if typ == fourcc("mdat") {
            //     desc.mdat_position.push((None, org_pos, size - header_size as u64));
            //     desc.mdat_final_position = org_pos;
            // }
            if typ == fourcc("mvhd") || typ == fourcc("tkhd") || typ == fourcc("mdhd") {
                let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);
                if typ == fourcc("mvhd") {
                    let timescale = if v == 1 { d.seek(SeekFrom::Current(8+8))?; d.read_u32::<BigEndian>()? }
                                    else      { d.seek(SeekFrom::Current(4+4))?; d.read_u32::<BigEndian>()? };
                    let duration = if v == 1 { d.read_u64::<BigEndian>()? }
                                   else      { d.read_u32::<BigEndian>()? as u64 };
                    if desc.moov_mvhd_timescale == 0 {
                        desc.moov_mvhd_timescale = timescale;
                    }
                    desc.mvhd_timescale_per_file[file_index] = timescale;
                    desc.moov_mvhd_duration += ((duration as f64 / timescale as f64) * desc.moov_mvhd_timescale as f64).ceil() as u64;
                }
                if let Some(track_desc) = desc.moov_tracks.get_mut(tl_track) {
                    if typ == fourcc("tkhd") {
                        let duration = if v == 1 { d.seek(SeekFrom::Current(8+8+4+4))?; d.read_u64::<BigEndian>()? }
                                       else      { d.seek(SeekFrom::Current(4+4+4+4))?; d.read_u32::<BigEndian>()? as u64 };
                        track_desc.tkhd_duration += ((duration as f64 / *desc.mvhd_timescale_per_file.get(file_index).ok_or(std::io::Error::other("Invalid index"))? as f64) * desc.moov_mvhd_timescale as f64).ceil() as u64;
                    }
                    if typ == fourcc("mdhd") {
                        let timescale = if v == 1 { d.seek(SeekFrom::Current(8+8))?; d.read_u32::<BigEndian>()? }
                                        else      { d.seek(SeekFrom::Current(4+4))?; d.read_u32::<BigEndian>()? };
                        let duration = if v == 1 { d.read_u64::<BigEndian>()? }
                                       else      { d.read_u32::<BigEndian>()? as u64 };
                        if track_desc.mdhd_timescale == 0 {
                            track_desc.mdhd_timescale = timescale;
                        }
                        let add_duration = ((duration as f64 / timescale as f64) * track_desc.mdhd_timescale as f64).ceil() as u64;
                        track_desc.mdhd_duration += add_duration;
                    }
                }
            }
            if typ == fourcc("elst") || typ == fourcc("stts") || typ == fourcc("stsz") || typ == fourcc("stss") ||
               typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("sdtp") || typ == fourcc("stsc") {
                let track_desc = desc.moov_tracks.get_mut(tl_track).unwrap();
                if !(track_desc.skip && file_index > 0) {
                    let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);

                    if typ == fourcc("elst") {
                        let entry_count = d.read_u32::<BigEndian>()?;
                        for _ in 0..entry_count {
                            let segment_duration = if v == 1 { d.read_u64::<BigEndian>()? } else { d.read_u32::<BigEndian>()? as u64 };
                            let media_time       = if v == 1 { d.read_i64::<BigEndian>()? } else { d.read_i32::<BigEndian>()? as i64 };
                            d.seek(SeekFrom::Current(4))?; // Skip Media rate
                            if media_time != -1 {
                                track_desc.elst_segment_duration += segment_duration;
                            }
                        }
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
                    if typ == fourcc("stss") || typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("stts") || typ == fourcc("stsc") {
                        let count = d.read_u32::<BigEndian>()?;
                        let current_file_mdat_position = desc.mdat_position.last().unwrap().1;
                        let mdat_offset = desc.mdat_offset as i64 - current_file_mdat_position as i64;
                        for _ in 0..count {
                            if typ == fourcc("stss") { track_desc.stss.push(d.read_u32::<BigEndian>()? + track_desc.sample_offset); }
                            if typ == fourcc("stco") { track_desc.stco.push((d.read_u32::<BigEndian>()? as i64 + mdat_offset) as u64); }
                            if typ == fourcc("co64") { track_desc.stco.push((d.read_u64::<BigEndian>()? as i64 + mdat_offset) as u64); }
                            if typ == fourcc("stts") { track_desc.stts.push((d.read_u32::<BigEndian>()?, d.read_u32::<BigEndian>()?)); }
                            if typ == fourcc("stsc") { track_desc.stsc.push((
                                d.read_u32::<BigEndian>()? + track_desc.chunk_offset,
                                d.read_u32::<BigEndian>()?,
                                d.read_u32::<BigEndian>()?
                            )); }
                        }
                    }
                }
            }
            if typ == fourcc("tmcd") {
                // Timecode shouldn't be merged
                let track_desc = desc.moov_tracks.get_mut(tl_track).unwrap();
                track_desc.skip = true;
            }
            d.seek(SeekFrom::Start(org_pos + size - header_size as u64))?;
        }
        if d.stream_position()? - start_offs >= max_read {
            break;
        }
    }
    Ok(())
}
