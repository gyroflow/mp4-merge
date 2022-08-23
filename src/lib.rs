// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Result };
use std::path::{ Path, PathBuf };
use byteorder::{ ReadBytesExt, BigEndian };
use std::time::Instant;

mod desc_reader;
mod progress_stream;
mod writer;
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
const fn has_children(typ: u32) -> bool {
    typ == fourcc("moov") || typ == fourcc("trak") || typ == fourcc("edts") ||
    typ == fourcc("mdia") || typ == fourcc("minf") || typ == fourcc("stbl")
}
fn typ_to_str(typ: u32) -> String {
    unsafe { String::from_utf8_unchecked(vec![(typ >> 24) as u8, (typ >> 16) as u8, (typ >> 8) as u8, typ as u8 ]) }
}

pub fn read_box<R: Read + Seek>(reader: &mut R) -> Result<(u32, u64, u64, i64)> {
    let pos = reader.stream_position()?;
    let size = reader.read_u32::<BigEndian>()?;
    let typ = reader.read_u32::<BigEndian>()?;
    if size == 1 {
        let largesize = reader.read_u64::<BigEndian>()?;
        Ok((typ, pos, largesize - 8, 16))
    } else {
        Ok((typ, pos, size as u64, 8))
    }
}

pub fn join_files<P: AsRef<Path> + AsRef<std::ffi::OsStr>, F: Fn(f64)>(files: &[P], output_file: P, progress_cb: F) -> Result<()> {
    // Get the merged description from all source files
    let mut desc = desc_reader::Desc::default();
    desc.moov_tracks.resize(10, Default::default());
    let mut total_size = 0;
    for (i, path) in files.iter().enumerate() {
        let mut fs = std::fs::File::open(path)?;
        total_size += fs.metadata()?.len();

        desc_reader::read_desc(&mut fs, &mut desc, 0, u64::MAX)?;

        if let Some(mdat) = desc.mdat_position.last_mut() {
            mdat.0 = Some(PathBuf::from(path));
            desc.mdat_offset += mdat.2;
            for t in &mut desc.moov_tracks {
                t.stss_offset = t.stsz_count;
            }
        }

        progress_cb(((i as f64 + 1.0) / files.len() as f64) * 0.2);
    }

    // Write it to the file
    let mut f1 = std::fs::File::open(&files[0])?;
    let f_out = std::fs::File::create(output_file)?;
    let mut debounce = Instant::now();
    let mut f_out = ProgressStream::new(f_out, |total| {
        if (Instant::now() - debounce).as_millis() > 20 {
            progress_cb((0.2 + ((total as f64 / total_size as f64) * 0.8)).min(0.9999));
            debounce = Instant::now();
        }
    });
    writer::rewrite_from_desc(&mut f1, &mut f_out, &mut desc, 0, u64::MAX).unwrap();
    progress_cb(1.0);

    Ok(())
}
