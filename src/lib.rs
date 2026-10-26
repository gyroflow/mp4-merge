// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Write, Result, Error, ErrorKind };
use std::path::*;
use byteorder::{ BigEndian, LittleEndian, ReadBytesExt, WriteBytesExt };
use std::time::Instant;

mod desc_reader;
mod merge;
mod output;
mod progress_stream;
mod writer;
mod insta360;
use progress_stream::*;

// We need to:
// - Check that the files have the same tracks, with the same sample descriptions
// - Merge mdat boxes
// - Sum         moov/mvhd/duration
// - Sum         moov/trak/tkhd/duration
// - Sum         moov/trak/mdia/mdhd/duration, in one media timescale that holds every file's timing exactly
// - Merge lists moov/trak/edts/elst, each file's edits after the previous file's
// - Merge lists moov/trak/mdia/minf/stbl/stts and ctts, in that media timescale
// - Merge lists moov/trak/mdia/minf/stbl/stsz
// - Merge lists moov/trak/mdia/minf/stbl/stss, stps, sdtp and sbgp
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
    match String::from_utf8(vec![(typ >> 24) as u8, (typ >> 16) as u8, (typ >> 8) as u8, typ as u8 ]) {
        Ok(x) => x,
        Err(_) => format!("{:08X}", typ)
    }
}

/// Reads a box header: its type, position, size and header size. A size of 0 is a box up to the end of the file, the last
/// one (ISO/IEC 14496-12 §4.2), and is returned as 0. Any other size counts the header, so one smaller than its header,
/// or one whose end is past the largest stream position, is an error: every walk that goes from a box to its end moves
pub fn read_box<R: Read + Seek>(reader: &mut R) -> Result<(u32, u64, u64, i64)> {
    let pos = reader.stream_position()?;
    let size = reader.read_u32::<BigEndian>()?;
    let typ = reader.read_u32::<BigEndian>()?;
    let (size, header_size) = if size == 1 {
        (reader.read_u64::<BigEndian>()?, 16)
    } else if size == 0 {
        return Ok((typ, pos, 0, 8));
    } else {
        (size as u64, 8)
    };
    if let Some(e) = size_error(typ, pos, size, header_size) {
        return Err(e);
    }
    Ok((typ, pos, size, header_size as i64))
}

/// `read_box`'s rule for a size other than 0: it counts its header, and the box ends within the largest stream position
fn size_error(typ: u32, pos: u64, size: u64, header_size: u64) -> Option<Error> {
    (size < header_size || pos.checked_add(size).is_none_or(|end| end > i64::MAX as u64))
        .then(|| Error::new(ErrorKind::InvalidData, format!("the {} box at {pos} has an invalid size {size}", typ_to_str(typ))))
}

/// The end of a box that `read_box` read, when the file's boxes end at `end`: a box of size 0 runs to it. A box that runs
/// past it is an error
fn box_end(typ: u32, pos: u64, size: u64, end: u64) -> Result<u64> {
    let box_end = if size == 0 { end } else { pos + size };
    if box_end > end {
        return Err(Error::new(ErrorKind::InvalidData, format!("the {} box at {pos} has a size of {size}, which runs past the end at {end}", typ_to_str(typ))));
    }
    Ok(box_end)
}

/// A box's type, position, size (0: up to the end of the file), header size and end
type BoxHeader = (u32, u64, u64, i64, u64);

/// The next box of a list of boxes that ends at `parent_end` (its container's end, or where the file's boxes end), in a
/// file whose boxes end at `file_end`: its type, position, size, header size and end, by the rules of `read_box` and
/// `box_end`. `Ok(Err(e))` when what's there isn't a box: fewer bytes than a header, a size that can't be one, or a box
/// that runs past `parent_end`. Only bytes before `parent_end` are read, so any other error is the stream's
fn next_box<R: Read + Seek>(reader: &mut R, file_end: u64, parent_end: u64) -> Result<std::result::Result<BoxHeader, Error>> {
    let pos = reader.stream_position()?;
    let left = parent_end.saturating_sub(pos);
    let not_a_box = |msg: String| Ok(Err(Error::new(ErrorKind::InvalidData, msg)));
    if left < 8 {
        return not_a_box(format!("the {left} bytes at {pos} aren't a box"));
    }
    let size = reader.read_u32::<BigEndian>()?;
    let typ = reader.read_u32::<BigEndian>()?;
    let (size, header_size) = match size {
        0 => (0, 8),
        1 if left < 16 => return not_a_box(format!("the {} box at {pos} has a 64-bit size, which doesn't fit in the {left} bytes left", typ_to_str(typ))),
        1 => (reader.read_u64::<BigEndian>()?, 16),
        size => (size as u64, 8)
    };
    // A 32-bit size of 0 is a box up to the end of the file
    if header_size == 16 || size != 0 {
        if let Some(e) = size_error(typ, pos, size, header_size) { return Ok(Err(e)); }
    }
    let end = match box_end(typ, pos, size, file_end) {
        Ok(end) => end,
        Err(e) => return Ok(Err(e))
    };
    if end > parent_end {
        return not_a_box(format!("the {} box at {pos} runs past the end of its parent, at {parent_end}", typ_to_str(typ)));
    }
    Ok(Ok((typ, pos, size, header_size as i64, end)))
}

/// The next `len` bytes of the `typ` box at `pos`, after its header. The box is within the file, so a stream that ends
/// first is an error
fn read_body<R: Read>(reader: &mut R, len: u64, typ: u32, pos: u64) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    usize::try_from(len).ok().and_then(|len| body.try_reserve_exact(len).ok())
        .ok_or_else(|| Error::new(ErrorKind::OutOfMemory, format!("the {} box at {pos} doesn't fit in memory", typ_to_str(typ))))?;
    reader.take(len).read_to_end(&mut body)?;
    if (body.len() as u64) < len {
        return Err(Error::new(ErrorKind::UnexpectedEof, format!("the file ends inside the {} box at {pos}", typ_to_str(typ))));
    }
    Ok(body)
}

/// Copies the next `len` bytes of `reader`, all of which are in its file, to `writer`. A stream that ends first is an error
fn copy_exact<R: Read, W: Write>(reader: &mut R, len: u64, writer: &mut W) -> Result<()> {
    let copied = std::io::copy(&mut reader.take(len), writer)?;
    if copied < len {
        return Err(Error::new(ErrorKind::UnexpectedEof, format!("a file ended {} bytes before the end of what was copied from it", len - copied)));
    }
    Ok(())
}

/// `e`, saying which file it is about
fn in_file(i: usize, e: Error) -> Error {
    Error::new(e.kind(), format!("file {}: {e}", i + 1))
}

/// The error for a merge of no files, which has no first file to take the boxes from
fn no_files() -> Error {
    Error::new(ErrorKind::InvalidInput, "There are no files to merge")
}

/// Merges the `files` into `output_file`.
///
/// Every check comes first: an output that is one of the inputs (by file identity, so also another spelling of its path, a
/// letter case variant, or a link to it) is refused, and so are files that can't be merged. Only then is the output
/// created, under a temporary name in its directory, which it leaves for its own once it is complete and on disk: a merge
/// that fails never leaves a truncated or partial file under that name. An existing file there is replaced, as before.
/// Where only that last rename fails (another program opened the file there meanwhile), the complete output is kept under
/// its temporary name, and the error says where.
pub fn join_files<P: AsRef<Path>, F: Fn(f64)>(files: &[P], output_file: &P, progress_cb: F) -> Result<()> {
    if files.is_empty() {
        return Err(no_files());
    }
    let mut open_files = Vec::with_capacity(files.len());
    for x in files {
        let f = std::fs::File::open(x).map_err(|e| Error::new(e.kind(), format!("opening {}: {e}", x.as_ref().display())))?;
        let size = f.metadata()?.len() as usize;
        open_files.push((f, size));
    }
    let destination = output::Destination::check(output_file.as_ref(), files, &open_files)?;
    let output = join_file_streams_with(&mut open_files, || destination.create(), |p| progress_cb(p.min(0.9999)))?;
    output.commit()?;
    progress_cb(1.0);
    Ok(())
}

/// Merges the `files` (each a stream and its size) into the stream `output_file`.
pub fn join_file_streams<F: Fn(f64), I: Read + Seek, O: Read + Write + Seek>(files: &mut [(I, usize)], output_file: O, progress_cb: F) -> Result<()> {
    join_file_streams_with(files, || Ok(output_file), progress_cb).map(|_| ())
}

/// [`join_file_streams`], with the output created by `create_output` only once every file has been read and checked, so a
/// merge that is refused never creates or touches it. Returns the output, completely written and flushed.
pub fn join_file_streams_with<F: Fn(f64), I: Read + Seek, O: Read + Write + Seek, C: FnOnce() -> Result<O>>(files: &mut [(I, usize)], create_output: C, progress_cb: F) -> Result<O> {
    if files.is_empty() {
        return Err(no_files());
    }
    // Get the merged description from all source files
    let mut desc = desc_reader::Desc::default();
    let mut total_size = 0;
    let num_files = files.len() as f64;
    let mut insta360_max_read = None;
    for (i, fs) in files.iter_mut().enumerate() {
        let filesize = fs.1;
        let mut fs = std::io::BufReader::with_capacity(16*1024, &mut fs.0);
        total_size += filesize;
        // Where the boxes end: at the end of the file, or where Insta360's metadata starts
        let mut boxes_end = filesize as u64;

        {
            fs.seek(std::io::SeekFrom::End(-40))?;
            let mut buf = vec![0u8; 40];
            fs.read_exact(&mut buf)?;
            // Check if it's Insta360
            let insta360 = &buf[8..] == insta360::MAGIC;
            if insta360 {
                boxes_end = boxes_end.checked_sub((&buf[..]).read_u32::<LittleEndian>()? as u64).ok_or_else(|| in_file(i, Error::new(ErrorKind::InvalidData, "Insta360 metadata larger than the file")))?;
            }
            if i == 0 {
                insta360_max_read = insta360.then_some(boxes_end);
            } else if insta360_max_read.is_some() != insta360 {
                return Err(Error::new(ErrorKind::InvalidInput, format!("These files can't be merged: file {} {} Insta360 metadata and file 1 {}",
                    i + 1, if insta360 { "has" } else { "has no" }, if insta360 { "doesn't" } else { "does" })));
            }

            fs.seek(std::io::SeekFrom::Start(0))?;
        }

        { // Find mdat first
            loop {
                let (typ, offs, size, header_size) = match read_box(&mut fs) {
                    Ok(b) => b,
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(in_file(i, e))
                };
                let org_pos = fs.stream_position()?;
                let end = box_end(typ, offs, size, boxes_end).map_err(|e| in_file(i, e))?;
                if typ == fourcc("mdat") {
                    log::debug!("Reading {}, offset: {}, size: {size}, header_size: {header_size}", typ_to_str(typ), offs);
                    desc.mdat_position.push((None, org_pos, end - org_pos));
                    break;
                }
                if end == boxes_end { break; }
                fs.seek(std::io::SeekFrom::Start(end))?;
            }

            fs.seek(std::io::SeekFrom::Start(0))?;
        }

        if desc.mdat_position.len() != i + 1 {
            return Err(Error::new(ErrorKind::InvalidData, format!("file {} has no mdat box", i + 1)));
        }

        // The Insta360 metadata after the boxes isn't read as boxes
        if desc.parts.len() <= i { desc.parts.resize(i + 1, Default::default()); }
        desc.parts[i].end = boxes_end;
        desc_reader::read_desc(&mut fs, &mut desc, 0, boxes_end, i, 0)?;
        match desc.parts[i].moov {
            1 if desc.parts[i].tracks.is_empty() => return Err(Error::new(ErrorKind::InvalidData, format!("file {} has no tracks", i + 1))),
            1 => {}
            0 => return Err(Error::new(ErrorKind::InvalidData, format!("file {} has no moov box: it may be a recording that wasn't finished", i + 1))),
            _ => return Err(Error::new(ErrorKind::InvalidData, format!("file {} has more than one moov box", i + 1)))
        }
        // Its media data: the top-level mdat its samples are in. A file may have more than one (ISO/IEC 14496-12 §8.1.1),
        // and only that one is merged, so a file with samples in another one too is refused (`merge::within_mdat`)
        let part = &mut desc.parts[i];
        let holds = |&(at, size): &(u64, u64)| part.tracks.iter().flat_map(|t| &t.stco).all(|&o| o >= at && o - at <= size);
        let (_, first_at, first_size) = desc.mdat_position[i];
        let (mdat_at, mdat_size) = part.mdats.iter().copied().find(holds).unwrap_or((first_at, first_size));
        // The writer writes the first file's boxes up to where they end, the merged mdat in place of its mdat
        if part.walk_end < mdat_at + mdat_size {
            return Err(Error::new(ErrorKind::InvalidData, format!("file {}: its boxes end at {}, before its mdat box", i + 1, part.walk_end)));
        }
        for (at, _) in part.mdats.iter().filter(|m| m.0 != mdat_at) {
            log::warn!("File {} has another mdat box, with its data at {at}: only the one its samples are in is merged", i + 1);
        }
        // The chunk offsets in the merged mdat, where this file's data starts at `mdat_offset`. One before the file's mdat is
        // outside every one: u64::MAX
        for offset in part.tracks.iter_mut().flat_map(|t| &mut t.stco) {
            *offset = offset.checked_sub(mdat_at).and_then(|o| o.checked_add(desc.mdat_offset)).unwrap_or(u64::MAX);
        }
        part.mdat = (desc.mdat_offset, mdat_size);
        desc.mdat_position[i] = (Some(i), mdat_at, mdat_size);
        desc.mdat_offset += mdat_size;

        progress_cb(((i as f64 + 1.0) / num_files) * 0.1);
    }

    // Check the files and merge their descriptions
    merge::merge(&mut desc)?;

    // Write it to the file, which only now exists
    let mut output_file = create_output()?;
    let mut debounce = Instant::now();
    let f_out = ProgressStream::new(&mut output_file, |total| {
        if (Instant::now() - debounce).as_millis() > 100 {
            progress_cb((0.1 + ((total as f64 / total_size as f64) * 0.9)).min(0.9999));
            debounce = Instant::now();
        }
    });
    let mut f_out = std::io::BufWriter::with_capacity(64*1024, f_out);

    // The first file's boxes, up to where the reader's walk of them ended. Every error ends the merge
    writer::get_first(files).seek(std::io::SeekFrom::Start(0))?;
    let walk_end = desc.parts[0].walk_end;
    writer::rewrite_from_desc(files, &mut f_out, &mut desc, 0, walk_end, 0)?;

    // Patch final mdat positions
    let mdat_final_position = desc.mdat_final_position.ok_or_else(|| Error::new(ErrorKind::InvalidData, "file 1: its mdat box wasn't written"))?;
    for (t, track) in desc.moov_tracks.iter().enumerate() {
        let Some(co64_final_position) = track.co64_final_position else {
            if track.stco.is_empty() { continue; }
            return Err(Error::new(ErrorKind::InvalidData, format!("file 1: track {} has no chunk offset table (stco or co64) to write the merged track's chunks in", t + 1)));
        };
        f_out.seek(std::io::SeekFrom::Start(co64_final_position))?;
        for x in &track.stco {
            f_out.write_u64::<BigEndian>(*x + mdat_final_position)?;
        }
    }

    if insta360_max_read.is_some() {
        // Merge Insta360 metadata
        f_out.seek(std::io::SeekFrom::End(0))?;
        let offsets = insta360::get_insta360_offsets(files)?;
        insta360::merge_metadata(files, &offsets, &mut f_out)?;
    }
    // Explicitly: dropping the buffer would flush its last bytes and ignore an error
    f_out.flush()?;
    drop(f_out);

    progress_cb(1.0);

    Ok(output_file)
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
