// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Write, Seek, Result, SeekFrom };
use byteorder::{ ReadBytesExt, WriteBytesExt, BigEndian };
use crate::{ fourcc, read_box, typ_to_str, desc_reader::Desc };

pub(crate) fn get_first<R: Read + Seek>(files: &mut [(R, usize)]) -> &mut R { files.get_mut(0).map(|x| &mut x.0).unwrap() }

pub fn rewrite_from_desc<R: Read + Seek, W: Write + Seek>(files: &mut [(R, usize)], output_file: &mut W, desc: &mut Desc, track: usize, max_read: u64) -> Result<u64> {
    let mut total_read_size = 0;
    let mut total_new_size = 0;
    let mut tl_track = track;
    while let Ok((typ, offs, size, header_size)) = read_box(get_first(files)) {
        if size == 0 || typ == 0 { break; }

        total_read_size += size;
        let mut new_size = size;
        if crate::has_children(typ, false) {
            let d = get_first(files);
            // Copy the header
            d.seek(SeekFrom::Current(-header_size))?;
            let out_pos = output_file.stream_position()?;
            std::io::copy(&mut d.take(header_size as u64), output_file)?;
            new_size = rewrite_from_desc(files, output_file, desc, tl_track, size - header_size as u64)?;
            new_size += header_size as u64;

            if typ == fourcc("trak") {
                tl_track += 1;
            }

            if new_size != size {
                log::debug!("Patching size from {size} to {new_size}");
                patch_bytes(output_file, out_pos, &(new_size as u32).to_be_bytes())?;
            }
        } else if typ == fourcc("mdat") {
            log::debug!("Merging mdat's, offset: {}, size: {size}", offs);

            output_file.write_all(&1u32.to_be_bytes())?;
            output_file.write_all(&fourcc("mdat").to_be_bytes())?;
            let pos = output_file.stream_position()?;
            output_file.write_all(&0u64.to_be_bytes())?;
            new_size = 16;

            desc.mdat_final_position = output_file.stream_position()?;

            // Merge all mdats
            for (file_index, mo, ms) in &desc.mdat_position {
                if let Some(file_index) = file_index {
                    if let Some(f) = files.get_mut(*file_index).map(|x| &mut x.0) {
                        let prev_pos = f.stream_position()?;
                        f.seek(SeekFrom::Start(*mo))?;
                        std::io::copy(&mut f.take(*ms), output_file)?;
                        f.seek(SeekFrom::Start(prev_pos))?;
                        new_size += ms;
                    }
                }
            }
            patch_bytes(output_file, pos, &new_size.to_be_bytes())?;

            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;

        } else if typ == fourcc("mvhd") || typ == fourcc("tkhd") || typ == fourcc("mdhd") || typ == fourcc("elst") {
            log::debug!("Writing {} with patched duration, offset: {}, size: {size}", typ_to_str(typ), offs);
            let d = get_first(files);

            let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);

            // Copy the original box
            d.seek(SeekFrom::Current(-header_size - 4))?;
            let pos = output_file.stream_position()? + header_size as u64 + 4;
            std::io::copy(&mut d.take(size), output_file)?;

            // Patch values
            if typ == fourcc("mvhd") {
                if v == 1 { patch_bytes(output_file, pos+8+8+4, &desc.moov_mvhd_duration.to_be_bytes())?; }
                else      { patch_bytes(output_file, pos+4+4+4, &(desc.moov_mvhd_duration as u32).to_be_bytes())?; }
            }
            if let Some(track_desc) = desc.moov_tracks.get(tl_track) {
                if typ == fourcc("tkhd") {
                    if v == 1 { patch_bytes(output_file, pos+8+8+8+4, &track_desc.tkhd_duration.to_be_bytes())?; }
                    else      { patch_bytes(output_file, pos+4+4+4+4, &(track_desc.tkhd_duration as u32).to_be_bytes())?; };
                }
                if typ == fourcc("mdhd") {
                    if v == 1 { patch_bytes(output_file, pos+8+8+4, &track_desc.mdhd_duration.to_be_bytes())?; }
                    else      { patch_bytes(output_file, pos+4+4+4, &(track_desc.mdhd_duration as u32).to_be_bytes())?; }
                }
                if typ == fourcc("elst") {
                    if v == 1 { patch_bytes(output_file, pos+4, &track_desc.elst_segment_duration.to_be_bytes())?; }
                    else      { patch_bytes(output_file, pos+4, &(track_desc.elst_segment_duration as u32).to_be_bytes())?; }
                }
            }

        } else if typ == fourcc("stts") || typ == fourcc("stsz") || typ == fourcc("stss") || typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("sdtp") || typ == fourcc("stsc") {
            log::debug!("Writing new {}, offset: {}, size: {size}", typ_to_str(typ), offs);

            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;

            let out_pos = output_file.stream_position()?;
            new_size = 12;
            output_file.write_all(&0u32.to_be_bytes())?;
            let new_typ = if typ == fourcc("stco") { fourcc("co64") } else { typ };
            output_file.write_all(&new_typ.to_be_bytes())?;
            output_file.write_all(&0u32.to_be_bytes())?; // flags

            let track_desc = desc.moov_tracks.get_mut(tl_track).unwrap();
            if typ == fourcc("stts") {
                let mut new_stts: Vec<(u32, u32)> = Vec::with_capacity(track_desc.stts.len());
                let mut prev_delta = None;
                for x in &track_desc.stts {
                    if let Some(prev_delta) = prev_delta {
                        if prev_delta == x.1 { new_stts.last_mut().unwrap().0 += x.0; continue; }
                    }
                    prev_delta = Some(x.1);
                    new_stts.push(*x);
                }
                output_file.write_u32::<BigEndian>(new_stts.len() as u32)?;
                new_size += 4;
                for (count, delta) in &new_stts {
                    output_file.write_u32::<BigEndian>(*count)?;
                    output_file.write_u32::<BigEndian>(*delta)?;
                    new_size += 8;
                }
            }
            if typ == fourcc("stsz") {
                output_file.write_u32::<BigEndian>(track_desc.stsz_sample_size)?; // sample_size
                output_file.write_u32::<BigEndian>(track_desc.stsz_count)?;
                new_size += 8;
                for x in &track_desc.stsz { output_file.write_u32::<BigEndian>(*x)?; new_size += 4; }
            }
            if typ == fourcc("stss") {
                output_file.write_u32::<BigEndian>(track_desc.stss.len() as u32)?;
                new_size += 4;
                for x in &track_desc.stss { output_file.write_u32::<BigEndian>(*x)?; new_size += 4; }
            }
            if typ == fourcc("stco") || typ == fourcc("co64") {
                output_file.write_u32::<BigEndian>(track_desc.stco.len() as u32)?;
                new_size += 4;
                track_desc.co64_final_position = output_file.stream_position()?;
                for x in &track_desc.stco {
                    output_file.write_u64::<BigEndian>(*x + desc.mdat_final_position)?;
                    new_size += 8;
                }
            }
            if typ == fourcc("sdtp") {
                for x in &track_desc.sdtp { output_file.write_u8(*x)?; new_size += 1; }
            }
            if typ == fourcc("stsc") {
                output_file.write_u32::<BigEndian>(track_desc.stsc.len() as u32)?;
                new_size += 4;
                for x in &track_desc.stsc {
                    output_file.write_u32::<BigEndian>(x.0)?;
                    output_file.write_u32::<BigEndian>(x.1)?;
                    output_file.write_u32::<BigEndian>(x.2)?;
                    new_size += 12;
                }
            }
            patch_bytes(output_file, out_pos, &(new_size as u32).to_be_bytes())?;
        } else {
            log::debug!("Writing original {}, offset: {}, size: {size}", typ_to_str(typ), offs);
            let d = get_first(files);

            // Copy without changes
            d.seek(SeekFrom::Current(-header_size))?;
            std::io::copy(&mut d.take(size), output_file)?;
        }
        total_new_size += new_size;
        if total_read_size >= max_read {
            break;
        }
    }
    Ok(total_new_size)
}

pub fn patch_bytes<W: Write + Seek>(writer: &mut W, position: u64, bytes: &[u8]) -> Result<()> {
    let new_pos = writer.stream_position()?;
    writer.seek(SeekFrom::Start(position))?;
    writer.write_all(bytes)?;
    writer.seek(SeekFrom::Start(new_pos))?;
    Ok(())
}