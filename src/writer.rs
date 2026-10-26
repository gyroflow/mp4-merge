// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Write, Seek, Result, SeekFrom, ErrorKind };
use byteorder::{ ReadBytesExt, WriteBytesExt, BigEndian };
use crate::{ fourcc, next_box, read_body, copy_exact, in_file, typ_to_str, desc_reader::{ compact_to_group, Desc, Edit, TrackDesc } };

pub(crate) fn get_first<R: Read + Seek>(files: &mut [(R, usize)]) -> &mut R { files.get_mut(0).map(|x| &mut x.0).unwrap() }

/// `parent` is the type of the box whose children are written, 0 for the top level, and `max_read` their size: at the top
/// level, where the reader's walk of the first file's boxes ended. The reader has checked every box walked here, so any
/// error ends the merge
pub fn rewrite_from_desc<R: Read + Seek, W: Write + Seek>(files: &mut [(R, usize)], output_file: &mut W, desc: &mut Desc, track: usize, max_read: u64, parent: u32) -> Result<u64> {
    let mut total_read_size = 0;
    let mut total_new_size = 0;
    let mut tl_track = track;
    let mut sample_group = 0; // The `sbgp` boxes of a `stbl`, in order
    let mut compact_group = 0; // The `csgm` boxes the merge read, in order
    let file_end = desc.parts.first().map_or(u64::MAX, |p| p.end);
    let parent_end = get_first(files).stream_position()?.saturating_add(max_read);
    while total_read_size < max_read {
        if max_read - total_read_size < 8 {
            // QuickTime's 32-bit 0 that ends a list of atoms, which the reader has checked. Writers may leave it out
            get_first(files).seek(SeekFrom::Current((max_read - total_read_size) as i64))?;
            break;
        }
        // A box of size 0 runs to the end of the file
        let (typ, offs, _, header_size, end) = next_box(get_first(files), file_end, parent_end)??;
        let size = end - offs;

        total_read_size += size;
        let mut new_size = size;
        if crate::has_children(typ) {
            let d = get_first(files);
            // Copy the header
            d.seek(SeekFrom::Current(-header_size))?;
            let out_pos = output_file.stream_position()?;
            copy_exact(d, header_size as u64, output_file).map_err(|e| in_file(0, e))?;
            new_size = rewrite_from_desc(files, output_file, desc, tl_track, size - header_size as u64, typ)?;
            new_size += header_size as u64;

            if typ == fourcc("trak") {
                tl_track += 1;
            }

            if new_size != size {
                log::debug!("Patching size from {size} to {new_size}");
                if header_size == 16 { patch_bytes(output_file, out_pos + 8, &new_size.to_be_bytes())?; }
                else                 { patch_bytes(output_file, out_pos, &(new_size as u32).to_be_bytes())?; }
            }
        } else if typ == fourcc("udta") && parent == fourcc("moov") && size >= header_size as u64 {
            // mp4-trim's segment table (`moov/udta/MTRM`) maps the frames of the file it's in to the recordings they were cut from.
            // The merged file has the frames of all the files, on a timeline rebuilt here, so the first file's table would describe
            // frames it doesn't have: it's left out, and the rest of `udta` is written as it is
            let body = read_body(get_first(files), size - header_size as u64, typ, offs).map_err(|e| in_file(0, e))?;
            let kept = without_segment_table(&body);
            if kept.is_some() {
                log::debug!("Writing {} without the segment table, offset: {}, size: {size}", typ_to_str(typ), offs);
            } else {
                log::debug!("Writing original {}, offset: {}, size: {size}", typ_to_str(typ), offs);
            }
            match kept {
                // It held only the table
                Some(kept) if kept.is_empty() => new_size = 0,
                kept => {
                    let body = kept.unwrap_or(body);
                    new_size = header_size as u64 + body.len() as u64;
                    if header_size == 16 {
                        output_file.write_all(&1u32.to_be_bytes())?;
                        output_file.write_all(&typ.to_be_bytes())?;
                        output_file.write_all(&new_size.to_be_bytes())?;
                    } else {
                        output_file.write_all(&(new_size as u32).to_be_bytes())?;
                        output_file.write_all(&typ.to_be_bytes())?;
                    }
                    output_file.write_all(&body)?;
                }
            }
        } else if typ == fourcc("mdat") && parent == 0 && desc.mdat_position.first().is_some_and(|m| m.1 == offs + header_size as u64) {
            log::debug!("Merging mdat's, offset: {}, size: {size}", offs);

            output_file.write_all(&1u32.to_be_bytes())?;
            output_file.write_all(&fourcc("mdat").to_be_bytes())?;
            let pos = output_file.stream_position()?;
            output_file.write_all(&0u64.to_be_bytes())?;
            new_size = 16;

            desc.mdat_final_position = Some(output_file.stream_position()?);

            // Merge all mdats
            for (file_index, mo, ms) in &desc.mdat_position {
                if let Some(file_index) = file_index {
                    if let Some(f) = files.get_mut(*file_index).map(|x| &mut x.0) {
                        let prev_pos = f.stream_position()?;
                        f.seek(SeekFrom::Start(*mo))?;
                        copy_exact(f, *ms, output_file).map_err(|e| in_file(*file_index, e))?;
                        f.seek(SeekFrom::Start(prev_pos))?;
                        new_size += ms;
                    }
                }
            }
            // Then the silence the merge puts into PCM tracks (`merge::Filler`)
            for &(len, byte) in &desc.mdat_filler {
                let run = [byte; 64 * 1024];
                let mut left = len;
                while left > 0 {
                    let n = left.min(run.len() as u64);
                    output_file.write_all(&run[..n as usize])?;
                    left -= n;
                }
                new_size += len;
            }
            patch_bytes(output_file, pos, &new_size.to_be_bytes())?;

            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;

        } else if typ == fourcc("mdat") && parent == 0 {
            // Another mdat of the first file. Only the one its samples are in is merged, and every track's samples are in
            // that one (`merge::within_mdat`), so nothing uses this one: it's left out
            log::debug!("Leaving out another mdat, offset: {}, size: {size}", offs);
            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;
            new_size = 0;

        } else if (typ == fourcc("mvhd") && parent == fourcc("moov")) || (typ == fourcc("tkhd") && parent == fourcc("trak")) || (typ == fourcc("mdhd") && parent == fourcc("mdia")) {
            log::debug!("Writing {} with patched duration, offset: {}, size: {size}", typ_to_str(typ), offs);
            let mut body = read_body(get_first(files), size - header_size as u64, typ, offs).map_err(|e| in_file(0, e))?;

            // Patch values
            if typ == fourcc("mvhd") {
                set_duration(&mut body, typ, desc.moov_mvhd_duration)?;
            }
            if let Some(track_desc) = desc.moov_tracks.get(tl_track) {
                if typ == fourcc("tkhd") {
                    set_duration(&mut body, typ, track_desc.tkhd_duration)?;
                }
                if typ == fourcc("mdhd") {
                    let at = if body.first() == Some(&1) { 4+8+8 } else { 4+4+4 };
                    body.get_mut(at..at + 4).ok_or_else(|| too_short(typ))?.copy_from_slice(&track_desc.mdhd_timescale.to_be_bytes());
                    set_duration(&mut body, typ, track_desc.mdhd_duration)?;
                }
            }
            new_size = write_box(output_file, typ, header_size, &body)?;

            if typ == fourcc("tkhd") {
                // When the track has an edts, the edit list goes in it (below)
                let has_edts = desc.parts.first().and_then(|p| p.tracks.get(tl_track)).is_some_and(|t| t.has_edts);
                if let Some(edits) = desc.moov_tracks.get(tl_track).filter(|t| t.insert_edts && !has_edts).and_then(|t| t.elst.as_ref()) {
                    // The first file's track has no edit list, and the merged one needs one. It follows `tkhd`
                    log::debug!("Writing new edts");
                    new_size += write_box(output_file, fourcc("edts"), 8, &box_bytes(fourcc("elst"), &elst_body(0, 0, edits)))?;
                }
            }

        } else if typ == fourcc("elst") && (parent == fourcc("edts") || parent == fourcc("trak")) {
            // Where the reader reads it: a RunCam camera's is in the trak
            log::debug!("Writing new {}, offset: {}, size: {size}", typ_to_str(typ), offs);
            let d = get_first(files);
            let (v, flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);
            match desc.moov_tracks.get(tl_track).and_then(|t| t.elst.as_ref()) {
                Some(edits) => {
                    d.seek(SeekFrom::Current(size as i64 - header_size - 4))?;
                    new_size = write_box(output_file, typ, header_size, &elst_body(v, flags, edits))?;
                }
                None => {
                    d.seek(SeekFrom::Current(-header_size - 4))?;
                    copy_exact(d, size, output_file).map_err(|e| in_file(0, e))?;
                }
            }

        } else if typ == fourcc("stsd") && parent == fourcc("stbl") && desc.moov_tracks.get(tl_track).is_some_and(|t| t.stsd.is_some()) {
            log::debug!("Writing new {} with merged decoder configurations, offset: {}, size: {size}", typ_to_str(typ), offs);
            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;
            new_size = write_box(output_file, typ, header_size, desc.moov_tracks[tl_track].stsd.as_deref().unwrap())?;

        } else if parent == fourcc("stbl") && desc.moov_tracks.get(tl_track).is_some() && (
                  typ == fourcc("stts") || typ == fourcc("ctts") || typ == fourcc("stsz") || typ == fourcc("stz2") || typ == fourcc("stss") || typ == fourcc("stps") ||
                  typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("sdtp") || typ == fourcc("stsc") || typ == fourcc("sbgp") || typ == fourcc("cslg")) {
            log::debug!("Writing new {}, offset: {}, size: {size}", typ_to_str(typ), offs);

            let mut original = Vec::new();
            if typ == fourcc("cslg") { original = read_body(get_first(files), size - header_size as u64, typ, offs).map_err(|e| in_file(0, e))?; }
            else                     { get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?; }

            // Before the merged mdat is written, its position isn't known: lib.rs writes the chunk offsets again then
            let mdat_final_position = desc.mdat_final_position.unwrap_or_default();
            let track_desc = desc.moov_tracks.get_mut(tl_track).unwrap();
            if typ == fourcc("cslg") && track_desc.cslg.is_none() {
                new_size = write_box(output_file, typ, header_size, &original)?;
            } else {
                new_size = write_table(output_file, typ, track_desc, mdat_final_position, &original, sample_group)?;
            }
            if typ == fourcc("sbgp") { sample_group += 1; }
        } else if typ == fourcc("csgm") && parent == fourcc("stbl") && desc.moov_tracks.get(tl_track).is_some_and(|t| compact_group < t.compact_groups.len()) {
            // Apple's compact mapping: the merged one where the merge read the first file's, which the reader decides alike
            let body = read_body(get_first(files), size - header_size as u64, typ, offs).map_err(|e| in_file(0, e))?;
            if compact_to_group(&body).is_some() {
                log::debug!("Writing new {}, offset: {}, size: {size}", typ_to_str(typ), offs);
                let merged = desc.moov_tracks[tl_track].compact_groups[compact_group].to_box()
                    .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "a merged csgm doesn't fit its fields"))?;
                compact_group += 1;
                output_file.write_all(&merged)?;
                new_size = merged.len() as u64;
            } else {
                log::debug!("Writing original {}, offset: {}, size: {size}", typ_to_str(typ), offs);
                new_size = write_box(output_file, typ, header_size, &body)?;
            }
        } else if typ == fourcc("mvex") && parent == fourcc("moov") {
            // The merged file has no movie fragments, which is all a Movie Extends Box is about (ISO/IEC 14496-12 §8.8.1):
            // its fragments' defaults (trex), and their whole duration (mehd), which would be the first file's
            log::debug!("Leaving out {}, offset: {}, size: {size}", typ_to_str(typ), offs);
            get_first(files).seek(SeekFrom::Current(size as i64 - header_size))?;
            new_size = 0;
        } else {
            log::debug!("Writing original {}, offset: {}, size: {size}", typ_to_str(typ), offs);
            let d = get_first(files);

            // Copy without changes
            d.seek(SeekFrom::Current(-header_size))?;
            copy_exact(d, size, output_file).map_err(|e| in_file(0, e))?;
        }
        total_new_size += new_size;
    }
    if parent == fourcc("edts") {
        if let Some(edits) = desc.moov_tracks.get(tl_track).filter(|t| t.insert_edts).and_then(|t| t.elst.as_ref()) {
            // The first file's track has an edts without an edit list (ISO/IEC 14496-12 §8.6.5), and the merged one needs one
            log::debug!("Writing new elst in the edts");
            total_new_size += write_box(output_file, fourcc("elst"), 8, &elst_body(0, 0, edits))?;
        }
    }
    if parent == fourcc("stbl") {
        let mdat_final_position = desc.mdat_final_position.unwrap_or_default();
        if let Some(track_desc) = desc.moov_tracks.get_mut(tl_track) {
            // The merged track has tables the first file's doesn't: another file has composition offsets, non-sync
            // samples or partial sync samples
            for (typ, has) in [(fourcc("ctts"), track_desc.ctts.is_some()), (fourcc("stss"), track_desc.stss.is_some()), (fourcc("stps"), track_desc.stps.is_some())] {
                if has && !track_desc.written.contains(&typ) {
                    log::debug!("Writing new {} after the tables of the first file", typ_to_str(typ));
                    total_new_size += write_table(output_file, typ, track_desc, mdat_final_position, &[], 0)?;
                }
            }
        }
    }
    Ok(total_new_size)
}

/// Writes the merged `typ` table of a track (`stco` as `co64`, `stz2` as `stsz`), and returns its size. `original` is the
/// first file's `cslg` after its header, and `sample_group` the index of an `sbgp` among the first file's
fn write_table<W: Write + Seek>(output_file: &mut W, typ: u32, track_desc: &mut TrackDesc, mdat_final_position: u64, original: &[u8], sample_group: usize) -> Result<u64> {
    let out_pos = output_file.stream_position()?;
    let mut new_size = 12;
    output_file.write_all(&0u32.to_be_bytes())?;
    let new_typ = if typ == fourcc("stco") { fourcc("co64") } else if typ == fourcc("stz2") { fourcc("stsz") } else { typ };
    output_file.write_all(&new_typ.to_be_bytes())?;
    let group = track_desc.sample_groups.get(sample_group).filter(|_| typ == fourcc("sbgp"));
    let cslg = track_desc.cslg.filter(|_| typ == fourcc("cslg"));
    // Version 0 of `cslg` has 32-bit values
    let cslg_version = match cslg {
        Some(values) if original.first() == Some(&1) || values.iter().any(|v| i32::try_from(*v).is_err()) => 1,
        _ => 0
    };
    let version_flags = if typ == fourcc("ctts") { (track_desc.ctts.as_ref().map_or(0, |c| c.0) as u32) << 24 }
                        else if let Some(g) = group { (g.version as u32) << 24 }
                        else if cslg.is_some() { (cslg_version << 24) | original.get(1..4).map_or(0, |f| u32::from_be_bytes([0, f[0], f[1], f[2]])) }
                        else { 0 };
    output_file.write_all(&version_flags.to_be_bytes())?; // version and flags

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
    if typ == fourcc("ctts") {
        let entries = track_desc.ctts.as_ref().map_or(&[][..], |c| &c.1);
        output_file.write_u32::<BigEndian>(entries.len() as u32)?;
        new_size += 4;
        for (count, offset) in entries {
            output_file.write_u32::<BigEndian>(*count)?;
            output_file.write_i32::<BigEndian>(*offset)?;
            new_size += 8;
        }
    }
    if let Some(values) = cslg {
        for x in values {
            if cslg_version == 1 { output_file.write_i64::<BigEndian>(x)?; new_size += 8; }
            else                 { output_file.write_i32::<BigEndian>(x as i32)?; new_size += 4; }
        }
    }
    if typ == fourcc("stsz") || typ == fourcc("stz2") {
        output_file.write_u32::<BigEndian>(track_desc.stsz_sample_size)?; // sample_size
        output_file.write_u32::<BigEndian>(track_desc.stsz_count)?;
        new_size += 8;
        for x in &track_desc.stsz { output_file.write_u32::<BigEndian>(*x)?; new_size += 4; }
    }
    if typ == fourcc("stss") || typ == fourcc("stps") {
        let list = if typ == fourcc("stss") { &track_desc.stss } else { &track_desc.stps };
        let list = list.as_deref().unwrap_or_default();
        output_file.write_u32::<BigEndian>(list.len() as u32)?;
        new_size += 4;
        for x in list { output_file.write_u32::<BigEndian>(*x)?; new_size += 4; }
    }
    if typ == fourcc("stco") || typ == fourcc("co64") {
        output_file.write_u32::<BigEndian>(track_desc.stco.len() as u32)?;
        new_size += 4;
        track_desc.co64_final_position = Some(output_file.stream_position()?);
        for x in &track_desc.stco {
            output_file.write_u64::<BigEndian>(*x + mdat_final_position)?;
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
    if let Some(g) = group {
        output_file.write_u32::<BigEndian>(g.grouping_type)?;
        new_size += 4;
        if let Some(parameter) = g.grouping_type_parameter { output_file.write_u32::<BigEndian>(parameter)?; new_size += 4; }
        output_file.write_u32::<BigEndian>(g.entries.len() as u32)?;
        new_size += 4;
        for (count, index) in &g.entries {
            output_file.write_u32::<BigEndian>(*count)?;
            output_file.write_u32::<BigEndian>(*index)?;
            new_size += 8;
        }
    }
    patch_bytes(output_file, out_pos, &(new_size as u32).to_be_bytes())?;
    track_desc.written.push(new_typ);
    Ok(new_size)
}

fn too_short(typ: u32) -> std::io::Error { std::io::Error::new(ErrorKind::InvalidData, format!("{} is too short", typ_to_str(typ))) }

/// Writes a box with the header form of the original (a 32- or 64-bit size), and returns its size
fn write_box<W: Write>(output_file: &mut W, typ: u32, header_size: i64, body: &[u8]) -> Result<u64> {
    let size = header_size as u64 + body.len() as u64;
    if header_size == 16 {
        output_file.write_all(&1u32.to_be_bytes())?;
        output_file.write_all(&typ.to_be_bytes())?;
        output_file.write_all(&size.to_be_bytes())?;
    } else {
        output_file.write_all(&(size as u32).to_be_bytes())?;
        output_file.write_all(&typ.to_be_bytes())?;
    }
    output_file.write_all(body)?;
    Ok(size)
}

fn box_bytes(typ: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + body.len());
    b.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    b.extend_from_slice(&typ.to_be_bytes());
    b.extend_from_slice(body);
    b
}

/// Sets the duration of an `mvhd`, `tkhd` or `mdhd` (`body`, after the box header). A version 0 box whose duration doesn't
/// fit 32 bits (all ones is an unknown duration) becomes version 1, whose times and duration are 64-bit
fn set_duration(body: &mut Vec<u8>, typ: u32, duration: u64) -> Result<()> {
    // The duration follows version and flags, the creation and modification times, the timescale (`tkhd`: track_ID, reserved)
    let (at0, at1) = if typ == fourcc("tkhd") { (4+4+4+4+4, 4+8+8+4+4) } else { (4+4+4+4, 4+8+8+4) };
    if body.first() == Some(&1) {
        body.get_mut(at1..at1 + 8).ok_or_else(|| too_short(typ))?.copy_from_slice(&duration.to_be_bytes());
    } else if duration < u32::MAX as u64 {
        body.get_mut(at0..at0 + 4).ok_or_else(|| too_short(typ))?.copy_from_slice(&(duration as u32).to_be_bytes());
    } else {
        if body.len() < at0 + 4 { return Err(too_short(typ)); }
        log::debug!("Writing {} as version 1 for a duration of {duration}", typ_to_str(typ));
        let mut v1 = Vec::with_capacity(body.len() + 12);
        v1.push(1);
        v1.extend_from_slice(&body[1..4]);
        v1.extend_from_slice(&(u32::from_be_bytes(body[4..8].try_into().unwrap()) as u64).to_be_bytes());
        v1.extend_from_slice(&(u32::from_be_bytes(body[8..12].try_into().unwrap()) as u64).to_be_bytes());
        v1.extend_from_slice(&body[12..at0]);
        v1.extend_from_slice(&duration.to_be_bytes());
        v1.extend_from_slice(&body[at0 + 4..]);
        *body = v1;
    }
    Ok(())
}

/// The body of an `elst` with `edits`, in `version`, or in version 1 when a value doesn't fit version 0
fn elst_body(version: u8, flags: u32, edits: &[Edit]) -> Vec<u8> {
    let version = if version == 1 || edits.iter().any(|e| e.segment_duration > u32::MAX as u64 || e.media_time > i32::MAX as i64) { 1 } else { 0 };
    let mut b = Vec::with_capacity(8 + edits.len() * 20);
    b.extend_from_slice(&(((version as u32) << 24) | flags).to_be_bytes());
    b.extend_from_slice(&(edits.len() as u32).to_be_bytes());
    for e in edits {
        if version == 1 {
            b.extend_from_slice(&e.segment_duration.to_be_bytes());
            b.extend_from_slice(&e.media_time.to_be_bytes());
        } else {
            b.extend_from_slice(&(e.segment_duration as u32).to_be_bytes());
            b.extend_from_slice(&(e.media_time as i32).to_be_bytes());
        }
        b.extend_from_slice(&e.media_rate.to_be_bytes());
    }
    b
}

/// The boxes of a `udta` payload without mp4-trim's segment table (`MTRM`), or `None` when it has none. What doesn't parse
/// as a box (QuickTime's 4 zero bytes at the end of `udta`, or anything after a box whose size doesn't fit) is kept as it is
fn without_segment_table(udta: &[u8]) -> Option<Vec<u8>> {
    let mut kept = Vec::with_capacity(udta.len());
    let mut found = false;
    let mut pos = 0;
    while pos + 8 <= udta.len() {
        let (size, header) = match u32::from_be_bytes(udta[pos..pos + 4].try_into().unwrap()) {
            1 if pos + 16 <= udta.len() => (u64::from_be_bytes(udta[pos + 8..pos + 16].try_into().unwrap()), 16),
            0 | 1 => break,
            size => (size as u64, 8)
        };
        if size < header || size > (udta.len() - pos) as u64 { break; }
        let end = pos + size as usize;
        if &udta[pos + 4..pos + 8] == b"MTRM" {
            found = true;
        } else {
            kept.extend_from_slice(&udta[pos..end]);
        }
        pos = end;
    }
    kept.extend_from_slice(&udta[pos..]);
    found.then_some(kept)
}

pub fn patch_bytes<W: Write + Seek>(writer: &mut W, position: u64, bytes: &[u8]) -> Result<()> {
    let new_pos = writer.stream_position()?;
    writer.seek(SeekFrom::Start(position))?;
    writer.write_all(bytes)?;
    writer.seek(SeekFrom::Start(new_pos))?;
    Ok(())
}