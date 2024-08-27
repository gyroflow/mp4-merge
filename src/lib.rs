// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Write, Result };
use std::path::*;
use byteorder::{ BigEndian, LittleEndian, ReadBytesExt, WriteBytesExt };
use std::time::Instant;

mod desc_reader;
mod progress_stream;
mod writer;
mod insta360;
use progress_stream::*;

// We need to:
// - Merge mdat boxes
// - Sum         moov/mvhd/duration
// - Sum         moov/trak/tkhd/duration
// - Sum         moov/trak/mdia/mdhd/duration
// - Sum         moov/trak/edts/elst/segment duration
// - Merge lists moov/trak/mdia/minf/stbl/stts
// - Merge lists moov/trak/mdia/minf/stbl/stsz
// - Merge lists moov/trak/mdia/minf/stbl/stss
// - Merge lists moov/trak/mdia/minf/stbl/stco and co64
// - Rewrite stco to co64

const fn fourcc(s: &str) -> u32 {
    let s = s.as_bytes();
    (s[3] as u32) | ((s[2] as u32) << 8) | ((s[1] as u32) << 16) | ((s[0] as u32) << 24)
}
const fn has_children(typ: u32, is_read: bool) -> bool {
    typ == fourcc("moov") || typ == fourcc("trak") || typ == fourcc("edts") ||
    typ == fourcc("mdia") || typ == fourcc("minf") || typ == fourcc("stbl") ||
    (typ == fourcc("stsd") && is_read)
}
fn typ_to_str(typ: u32) -> String {
    match String::from_utf8(vec![(typ >> 24) as u8, (typ >> 16) as u8, (typ >> 8) as u8, typ as u8 ]) {
        Ok(x) => x,
        Err(_) => format!("{:08X}", typ)
    }
}

pub fn read_box<R: Read + Seek>(reader: &mut R) -> Result<(u32, u64, u64, i64)> {
    let pos = reader.stream_position()?;
    let size = reader.read_u32::<BigEndian>()?;
    let typ = reader.read_u32::<BigEndian>()?;
    if size == 1 {
        let largesize = reader.read_u64::<BigEndian>()?;
        Ok((typ, pos, largesize, 16))
    } else {
        Ok((typ, pos, size as u64, 8))
    }
}

pub fn join_files<P: AsRef<Path>, F: Fn(f64)>(files: &[P], output_file: &P, progress_cb: F) -> Result<()> {
    let mut open_files = Vec::with_capacity(files.len());
    for x in files {
        let f = std::fs::File::open(x)?;
        let size = f.metadata()?.len() as usize;
        open_files.push((f, size));
    }
    join_file_streams(&mut open_files, std::fs::File::create(output_file)?, progress_cb)
}

pub fn join_file_streams<F: Fn(f64), I: Read + Seek, O: Read + Write + Seek>(files: &mut [(I, usize)], output_file: O, progress_cb: F) -> Result<()> {
    // Get the merged description from all source files
    let mut desc = desc_reader::Desc::default();
    desc.moov_tracks.resize(10, Default::default());
    let mut total_size = 0;
    let num_files = files.len() as f64;
    let mut insta360_max_read = None;
    for (i, fs) in files.iter_mut().enumerate() {
        let filesize = fs.1;
        let mut fs = std::io::BufReader::with_capacity(16*1024, &mut fs.0);
        total_size += filesize;

        { // Find mdat first
            while let Ok((typ, offs, size, header_size)) = read_box(&mut fs) {
                let org_pos = fs.stream_position()?;
                if typ == fourcc("mdat") {
                    log::debug!("Reading {}, offset: {}, size: {size}, header_size: {header_size}", typ_to_str(typ), offs);
                    desc.mdat_position.push((None, org_pos, size - header_size as u64));
                    desc.mdat_final_position = org_pos;
                    break;
                }
                fs.seek(std::io::SeekFrom::Start(org_pos + size - header_size as u64))?;
            }

            if insta360_max_read.is_none() {
                fs.seek(std::io::SeekFrom::End(-40))?;
                let mut buf = vec![0u8; 40];
                fs.read_exact(&mut buf)?;
                // Check if it's Insta360
                if &buf[8..] == insta360::MAGIC {
                    insta360_max_read = Some(filesize as u64 - (&buf[..]).read_u32::<LittleEndian>()? as u64);
                }
            }

            fs.seek(std::io::SeekFrom::Start(0))?;
        }

        desc_reader::read_desc(&mut fs, &mut desc, 0, u64::MAX, i)?;

        if let Some(mdat) = desc.mdat_position.last_mut() {
            mdat.0 = Some(i);
            desc.mdat_offset += mdat.2;
            for t in &mut desc.moov_tracks {
                t.sample_offset = t.stsz_count;
                t.chunk_offset = t.stco.len() as u32;
            }
        }

        progress_cb(((i as f64 + 1.0) / num_files) * 0.1);
    }

    // Write it to the file
    let mut debounce = Instant::now();
    let f_out = ProgressStream::new(output_file, |total| {
        if (Instant::now() - debounce).as_millis() > 100 {
            progress_cb((0.1 + ((total as f64 / total_size as f64) * 0.9)).min(0.9999));
            debounce = Instant::now();
        }
    });
    let mut f_out = std::io::BufWriter::with_capacity(64*1024, f_out);

    writer::get_first(files).seek(std::io::SeekFrom::Start(0))?;
    writer::rewrite_from_desc(files, &mut f_out, &mut desc, 0, insta360_max_read.unwrap_or(u64::MAX))?;

    // Patch final mdat positions
    for track in &desc.moov_tracks {
        f_out.seek(std::io::SeekFrom::Start(track.co64_final_position))?;
        for x in &track.stco {
            f_out.write_u64::<BigEndian>(*x + desc.mdat_final_position)?;
        }
    }

    if insta360_max_read.is_some() {
        // Merge Insta360 metadata
        f_out.seek(std::io::SeekFrom::End(0))?;
        let offsets = insta360::get_insta360_offsets(files)?;
        insta360::merge_metadata(files, &offsets, f_out)?;
    }

    progress_cb(1.0);

    Ok(())
}

pub fn update_file_times(input_path: &PathBuf, output_path: &PathBuf) {
    if let Err(e) = || -> std::io::Result<()> {
        let org_time = filetime_creation::FileTime::from_creation_time(&std::fs::metadata(&input_path)?).ok_or(std::io::ErrorKind::Other)?;
        if cfg!(target_os = "windows") {
            ::log::debug!("Updating creation time of {} to {}", output_path.display(), org_time.to_string());
            filetime_creation::set_file_ctime(output_path, org_time)?;
        } else {
            ::log::debug!("Updating modification time of {} to {}", output_path.display(), org_time.to_string());
            filetime_creation::set_file_mtime(output_path, org_time)?;
        }
        Ok(())
    }() {
        ::log::warn!("Failed to update file times: {e:?}");
    }
}
