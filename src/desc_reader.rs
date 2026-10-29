// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::{ Read, Seek, Result, SeekFrom, Error, ErrorKind };
use byteorder::{ ReadBytesExt, BigEndian };
use crate::{ fourcc, next_box, read_body, in_file, typ_to_str };

/// A track of the merged file, as the writer writes it (built by `merge::merge`)
#[derive(Default, Clone, Debug)]
pub struct TrackDesc {
    pub tkhd_duration: u64,
    pub elst: Option<Vec<Edit>>,
    pub insert_edts: bool, // The first file's track has no edit list, the merged one needs one
    pub mdhd_timescale: u32,
    pub mdhd_duration: u64,
    pub stts: Vec<(u32, u32)>,
    pub ctts: Option<(u8, Vec<(u32, i32)>)>, // version, (sample_count, sample_offset)
    pub cslg: Option<[i64; 5]>,
    pub stsz: Vec<u32>,
    pub stco: Vec<u64>,
    pub stss: Option<Vec<u32>>,
    pub stps: Option<Vec<u32>>,
    pub sdtp: Vec<u8>,
    pub sample_groups: Vec<SampleToGroup>, // One for each `sbgp` of the first file's track, in its order
    pub compact_groups: Vec<CompactToGroup>, // One for each `csgm` of the first file's track, in its order
    pub stsz_sample_size: u32,
    pub stsz_count: u32,
    pub stsc: Vec<(u32, u32, u32)>, // first_chunk, samples_per_chunk, sample_description_index
    pub co64_final_position: Option<u64>, // Where the writer wrote the chunk offsets
    pub stsd: Option<Vec<u8>>, // The sample descriptions, after the box header, when they aren't the first file's
    pub written: Vec<u32>, // The sample table boxes written so far
}

/// A track of one of the files, as read
#[derive(Default, Clone, Debug)]
pub struct PartTrack {
    pub handler: u32,
    pub tkhd_duration: u64,
    pub has_edts: bool,
    pub elst: Option<Vec<Edit>>,
    pub mdhd_timescale: u32,
    pub mdhd_duration: u64,
    pub stsd: Vec<u8>, // After the box header
    pub sample_entries: Vec<SampleEntry>,
    pub stts: Vec<(u32, u32)>,
    pub ctts: Option<(u8, Vec<(u32, i32)>)>,
    pub stss: Option<Vec<u32>>,
    pub stps: Option<Vec<u32>>,
    pub stsz_sample_size: u32,
    pub stsz_count: u32,
    pub stsz: Vec<u32>,
    pub stsc: Vec<(u32, u32, u32)>,
    pub stco: Vec<u64>, // As read, in the file; then (lib.rs) in the merged mdat, u64::MAX for one before the file's mdat
    pub sdtp: Option<Vec<u8>>,
    pub sbgp: Vec<SampleToGroup>,
    pub csgm: Vec<CompactToGroup>, // Those in the form `compact_to_group` reads; another form is `unmerged`
    pub sgpd: Vec<(u32, Vec<u8>)>, // grouping_type, the box without its header
    pub has_cslg: bool,
    pub encrypted: bool,
    pub unmerged: Vec<u32>, // Per-sample boxes the merge doesn't rebuild
    pub skip: bool,
}

/// A sample entry of `stsd`
#[derive(Clone, Debug, PartialEq)]
pub struct SampleEntry {
    pub format: u32,
    pub at: usize, // Its offset in `stsd`, after the box header
    // The entry as compared between the files: its bytes without the fields that only state the stream's bitrate, and
    // without its AVC or HEVC decoder configuration, whose parameter sets are compared one by one
    pub comparable: Vec<u8>,
    pub config: Option<DecoderConfig>,
}

/// An AVCDecoderConfigurationRecord (`avcC`, ISO/IEC 14496-15 §5.3.3.1) or HEVCDecoderConfigurationRecord (`hvcC`,
/// §8.3.3.1), split around its arrays of parameter sets
#[derive(Clone, Debug, PartialEq)]
pub struct DecoderConfig {
    pub typ: u32,
    pub at: (usize, usize), // The box, in its entry
    pub header: Vec<u8>,    // The fields before the arrays
    // The arrays: NAL unit type, the byte that holds the type or the count with its other bits (`hvcC`: array_completeness,
    // `avcC`: the reserved bits before numOfSequenceParameterSets), the NAL units
    pub arrays: Vec<(u8, u8, Vec<Vec<u8>>)>,
    pub trailing: Vec<u8>,  // The fields after them (`avcC`: chroma_format, the bit depths, the SPS extensions)
}

/// An `elst` entry
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edit {
    pub segment_duration: u64, // In the movie timescale
    pub media_time: i64,       // In the media timescale, -1 for an empty edit
    pub media_rate: i32,       // 16.16 fixed point
}
pub const MEDIA_RATE_1: i32 = 0x0001_0000;

/// An `sbgp`
#[derive(Clone, Debug)]
pub struct SampleToGroup {
    pub version: u8,
    pub grouping_type: u32,
    pub grouping_type_parameter: Option<u32>,
    pub entries: Vec<(u32, u32)>, // sample_count, group_description_index
}

/// A `csgm`: Apple's compact sample-to-group mapping (`compact_to_group`). Each pattern maps `sample_count` samples,
/// which take its indices in turn from its first, cycling
#[derive(Clone, Debug)]
pub struct CompactToGroup {
    pub grouping_type: u32,
    pub patterns: Vec<(u32, Vec<u32>)>, // sample_count, the indices
}

/// One of the files
#[derive(Default, Clone, Debug)]
pub struct PartDesc {
    pub mvhd_timescale: u32,
    pub mvhd_duration: u64,
    pub mvhd_creation: u64, // creation_time, in seconds since 1904
    pub mvex: bool,         // It has a Movie Extends Box
    pub tracks: Vec<PartTrack>,
    pub mdat: (u64, u64), // Where its mdat's data is in the merged one's, and its size
    pub mdats: Vec<(u64, u64)>, // Its top-level mdat boxes: where their data is, and its size
    pub end: u64,         // Where its boxes end
    pub walk_end: u64,    // Where they do as walked: the end of its last box. What follows isn't boxes
    pub moov: u32,        // How many moov boxes it has
    pub size: u64,        // Its size in bytes
    pub segment_tables: Option<Vec<Vec<u8>>>, // The payloads of the MTRM boxes of its first moov/udta (`segment_table`)
}

#[derive(Default, Clone, Debug)]
pub struct Desc {
    pub mdat_position: Vec<(Option<usize>, u64, u64)>, // file path, offset, size
    pub parts: Vec<PartDesc>,
    pub moov_mvhd_timescale: u32,
    pub moov_mvhd_duration: u64,
    pub moov_tracks: Vec<TrackDesc>,
    pub mdat_offset: u64,
    pub mdat_final_position: Option<u64>, // Where the writer wrote the merged mdat's data
    pub mdat_filler: Vec<(u64, u8)>, // The silence the merge puts after the files' data in the merged mdat: its runs of one byte
}

/// `parent` is the type of the box whose children are read, 0 for the top level. Every box must be one (`next_box`), end
/// within its parent, and a container must be in the box it belongs in. At the top level, what follows the movie isn't
/// read as boxes: an Insta360 file's metadata, padding. Every table is read from its own box only
pub fn read_desc<R: Read + Seek>(d: &mut R, desc: &mut Desc, track: usize, max_read: u64, file_index: usize, parent: u32) -> Result<()> {
    let start_offs = d.stream_position()?;
    let parent_end = start_offs.saturating_add(max_read);
    if desc.parts.len() <= file_index {
        desc.parts.resize(file_index + 1, Default::default());
    }
    let mut once = Vec::new(); // The boxes so far of those this container may have only one of
    let mut pos = start_offs;
    while pos < parent_end {
        if parent_end - pos < 8 {
            // Fewer bytes than a box header. After the file's boxes, they aren't read. A list of QuickTime atoms may end
            // with a 32-bit 0, which readers allow for and writers may leave out (QTFF, "User data list"), as the writer
            // does. Anything else is damage
            if parent == 0 { break; }
            let mut rest = vec![0u8; (parent_end - pos) as usize];
            d.read_exact(&mut rest)?;
            if rest == [0; 4] { break; }
            return Err(in_file(file_index, Error::new(ErrorKind::InvalidData, format!("the {} box ends with {} bytes at {pos} that aren't a box", typ_to_str(parent), rest.len()))));
        }
        let (typ, offs, size, header_size, end) = match next_box(d, desc.parts[file_index].end, parent_end)? {
            Ok(b) => b,
            // What follows the movie isn't boxes: an Insta360 file's metadata, padding, junk
            Err(_) if parent == 0 && desc.parts[file_index].moov > 0 => break,
            Err(e) => return Err(in_file(file_index, e))
        };
        if parent == 0 && typ == 0 { break; } // Padding
        pos = end;
        if parent == 0 { desc.parts[file_index].walk_end = end; }
        if let Some(name) = only_one(parent, typ) {
            if once.contains(&name) {
                return Err(in_file(file_index, Error::new(ErrorKind::InvalidData, format!("the {} box at {offs} is another {name} box in a {} box, which has only one", typ_to_str(typ), typ_to_str(parent)))));
            }
            once.push(name);
        }
        if typ == fourcc("moof") && parent == 0 {
            // Movie fragments hold samples that the sample tables don't (ISO/IEC 14496-12 §8.8.4), and aren't merged
            return Err(Error::new(ErrorKind::InvalidInput, format!("These files can't be merged: file {} is a fragmented MP4 (it has a moof box at {offs}), whose movie fragments hold samples its sample tables don't", file_index + 1)));
        }
        if typ == fourcc("mvex") && parent == fourcc("moov") {
            // It only says the file may have movie fragments, and sets defaults for their samples (§8.8.1, §8.8.3): a
            // file without them has every sample in its sample tables. `merge::merge` refuses one whose tables have none
            desc.parts[file_index].mvex = true;
        }
        if typ == fourcc("mdat") && parent == 0 {
            desc.parts[file_index].mdats.push((offs + header_size as u64, end - offs - header_size as u64));
        }
        if crate::has_children(typ) {
            if belongs_in(typ) != parent {
                return Err(in_file(file_index, Error::new(ErrorKind::InvalidData, format!("the {} box at {offs} is {}, and belongs {}", typ_to_str(typ), place(parent), place(belongs_in(typ))))));
            }
            let mut tl_track = track;
            if typ == fourcc("moov") {
                desc.parts[file_index].moov += 1;
            }
            if typ == fourcc("trak") {
                let tracks = &mut desc.parts[file_index].tracks;
                tracks.push(Default::default());
                tl_track = tracks.len() - 1;
            }
            if typ == fourcc("edts") {
                if let Some(track_desc) = desc.parts[file_index].tracks.get_mut(track) { track_desc.has_edts = true; }
            }
            read_desc(d, desc, tl_track, end - offs - header_size as u64, file_index, typ)?;
            d.seek(SeekFrom::Start(end))?;
        } else {
            log::debug!("Reading {}, offset: {}, size: {size}, header_size: {header_size}", typ_to_str(typ), offs);
            let body_size = end - offs - header_size as u64;
            let body = |d: &mut R| read_body(d, body_size, typ, offs).map_err(|e| in_file(file_index, e));
            let fields = |e: Error| in_file(file_index, field_error(e, typ, offs));
            let part = &mut desc.parts[file_index];
            if typ == fourcc("udta") && parent == fourcc("moov") && part.segment_tables.is_none() {
                // A file cut from recordings has its segment table there
                part.segment_tables = Some(crate::segment_table::payloads(&body(d)?));
            }
            if typ == fourcc("mvhd") && parent == fourcc("moov") {
                let body = body(d)?;
                (part.mvhd_timescale, part.mvhd_duration) = header_fields(typ, &body).map_err(fields)?;
                // creation_time, after version and flags (`header_fields` has checked the version and the size)
                part.mvhd_creation = if body[0] == 1 { u64::from_be_bytes(body[4..12].try_into().unwrap()) } else { u32::from_be_bytes(body[4..8].try_into().unwrap()) as u64 };
            }
            if let Some(track_desc) = part.tracks.get_mut(track) {
                if typ == fourcc("tkhd") && parent == fourcc("trak") {
                    track_desc.tkhd_duration = header_fields(typ, &body(d)?).map_err(fields)?.1;
                }
                if typ == fourcc("mdhd") && parent == fourcc("mdia") {
                    (track_desc.mdhd_timescale, track_desc.mdhd_duration) = header_fields(typ, &body(d)?).map_err(fields)?;
                }
                if typ == fourcc("hdlr") && parent == fourcc("mdia") {
                    let body = body(d)?;
                    // Version and flags, pre_defined, handler_type
                    track_desc.handler = body.get(8..12).map(|h| u32::from_be_bytes(h.try_into().unwrap())).ok_or_else(|| fields(ErrorKind::UnexpectedEof.into()))?;
                }
                // A track's edit list is in its edts (ISO/IEC 14496-12 §8.6.6). RunCam cameras write it in the trak, where
                // FFmpeg reads it too, so it's read and rewritten there as well
                if typ == fourcc("elst") && (parent == fourcc("edts") || parent == fourcc("trak")) {
                    if track_desc.elst.is_some() {
                        return Err(in_file(file_index, Error::new(ErrorKind::InvalidData, format!("the elst box at {offs} is another edit list of its track, which has only one"))));
                    }
                    track_desc.elst = Some(edit_list(&body(d)?).map_err(fields)?);
                }
                if parent == fourcc("stbl") {
                    read_sample_table_box(d, typ, offs, body_size, track_desc).map_err(|e| in_file(file_index, e))?;
                }
            }
            d.seek(SeekFrom::Start(end))?;
        }
        if size == 0 { break; } // The last box
    }
    Ok(())
}

/// The box a container that `read_desc` walks belongs in (ISO/IEC 14496-12 §8.2-8.5), 0 for the top level
fn belongs_in(typ: u32) -> u32 {
    if typ == fourcc("trak") { fourcc("moov") }
    else if typ == fourcc("edts") || typ == fourcc("mdia") { fourcc("trak") }
    else if typ == fourcc("minf") { fourcc("mdia") }
    else if typ == fourcc("stbl") { fourcc("minf") }
    else { 0 }
}

fn place(parent: u32) -> String {
    if parent == 0 { "at the top level".into() } else { format!("in a {} box", typ_to_str(parent)) }
}

/// The boxes the merge reads or rewrites that their container has only one of (ISO/IEC 14496-12 §8, "Quantity"): the name
/// of `typ`'s kind when it's one of them in `parent`
fn only_one(parent: u32, typ: u32) -> Option<&'static str> {
    let kinds: &[&'static str] = if parent == fourcc("moov") { &["mvhd"] }
        else if parent == fourcc("trak") { &["tkhd", "edts", "mdia"] }
        else if parent == fourcc("edts") { &["elst"] }
        else if parent == fourcc("mdia") { &["mdhd", "hdlr", "minf"] }
        else if parent == fourcc("minf") { &["stbl"] }
        else if parent == fourcc("stbl") { &["stsd", "stts", "ctts", "cslg", "stsz or stz2", "stsc", "stco or co64", "stss", "stps", "sdtp"] }
        else { &[] };
    kinds.iter().copied().find(|kind| kind.split(" or ").any(|k| fourcc(k) == typ))
}

/// An error reading the fields of the `typ` box at `offs` from its body: a body that ends first is too short
fn field_error(e: Error, typ: u32, offs: u64) -> Error {
    if e.kind() == ErrorKind::UnexpectedEof {
        return Error::new(ErrorKind::InvalidData, format!("the {} box at {offs} is too short for its fields", typ_to_str(typ)));
    }
    Error::new(e.kind(), format!("the {} box at {offs} {e}", typ_to_str(typ)))
}

/// Refuses a full box's version above `max`, whose fields aren't known
fn known_version(v: u8, max: u8) -> Result<()> {
    if v > max {
        return Err(Error::new(ErrorKind::InvalidData, format!("has version {v}, whose fields aren't known")));
    }
    Ok(())
}

/// Checks that `count` entries of `bits` bits each fit in `rest`, what's left of a box after its fields, before they're
/// read or allocated
fn entries(rest: &[u8], count: u32, bits: u64) -> Result<usize> {
    let need = (count as u64 * bits).div_ceil(8);
    if need > rest.len() as u64 {
        return Err(Error::new(ErrorKind::InvalidData, format!("has {count} entries, which need {need} bytes, and it has {} left", rest.len())));
    }
    Ok(count as usize)
}

fn skip(d: &mut &[u8], n: usize) -> Result<()> {
    *d = d.get(n..).ok_or_else(|| Error::from(ErrorKind::UnexpectedEof))?;
    Ok(())
}

/// The field after the times (the timescale, a `tkhd`'s track_ID) and the duration of an `mvhd`, `tkhd` or `mdhd` body
fn header_fields(typ: u32, body: &[u8]) -> Result<(u32, u64)> {
    let d = &mut &body[..];
    let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);
    known_version(v, 1)?;
    skip(d, if v == 1 { 8+8 } else { 4+4 })?; // creation_time, modification_time
    let field = d.read_u32::<BigEndian>()?;
    if typ == fourcc("tkhd") { skip(d, 4)?; } // reserved
    let duration = if v == 1 { d.read_u64::<BigEndian>()? } else { d.read_u32::<BigEndian>()? as u64 };
    Ok((field, duration))
}

/// The edits of an `elst` body
fn edit_list(body: &[u8]) -> Result<Vec<Edit>> {
    let d = &mut &body[..];
    let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);
    known_version(v, 1)?;
    let entry_count = d.read_u32::<BigEndian>()?;
    let mut edits = Vec::with_capacity(entries(d, entry_count, if v == 1 { 20*8 } else { 12*8 })?);
    for _ in 0..entry_count {
        let segment_duration = if v == 1 { d.read_u64::<BigEndian>()? } else { d.read_u32::<BigEndian>()? as u64 };
        let media_time       = if v == 1 { d.read_i64::<BigEndian>()? } else { d.read_i32::<BigEndian>()? as i64 };
        let media_rate = d.read_i32::<BigEndian>()?;
        edits.push(Edit { segment_duration, media_time, media_rate });
    }
    Ok(edits)
}

/// Reads a child of `stbl`, the `typ` box at `offs`, whose body after its header is the next `size` bytes
fn read_sample_table_box<R: Read>(d: &mut R, typ: u32, offs: u64, size: u64, track_desc: &mut PartTrack) -> Result<()> {
    if typ == fourcc("stsd") {
        let body = read_body(d, size, typ, offs)?;
        let entry_count = body.get(4..8).map_or(0, |x| u32::from_be_bytes(x.try_into().unwrap()));
        let mut pos = 8; // Version and flags, entry_count
        for _ in 0..entry_count {
            if pos + 8 > body.len() { break; }
            let entry_size = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
            if entry_size < 8 || entry_size > body.len() - pos { break; }
            let format = u32::from_be_bytes(body[pos + 4..pos + 8].try_into().unwrap());
            if format == fourcc("tmcd") {
                // Timecode shouldn't be merged
                track_desc.skip = true;
            }
            if format == fourcc("encv") || format == fourcc("enca") || format == fourcc("enct") || format == fourcc("encs") {
                track_desc.encrypted = true;
            }
            track_desc.sample_entries.push(sample_entry(track_desc.handler, &body[pos..pos + entry_size], pos));
            pos += entry_size;
        }
        track_desc.stsd = body;
        return Ok(());
    }
    if typ == fourcc("cslg") { track_desc.has_cslg = true; }
    if typ == fourcc("saiz") || typ == fourcc("saio") || typ == fourcc("senc") { track_desc.encrypted = true; }
    if typ == fourcc("csgm") {
        let body = read_body(d, size, typ, offs)?;
        match compact_to_group(&body) {
            Some(g) => track_desc.csgm.push(g),
            None => track_desc.unmerged.push(typ)
        }
        return Ok(());
    }
    if typ == fourcc("csgp") || typ == fourcc("subs") || typ == fourcc("padb") || typ == fourcc("stsh") || typ == fourcc("stdp") {
        track_desc.unmerged.push(typ);
    }
    if typ == fourcc("sgpd") {
        let body = read_body(d, size, typ, offs)?;
        if let Some(grouping_type) = body.get(4..8) {
            track_desc.sgpd.push((u32::from_be_bytes(grouping_type.try_into().unwrap()), body));
        }
        return Ok(());
    }
    if typ != fourcc("stts") && typ != fourcc("ctts") && typ != fourcc("stsz") && typ != fourcc("stz2") && typ != fourcc("stss") && typ != fourcc("stps") &&
       typ != fourcc("stco") && typ != fourcc("co64") && typ != fourcc("sdtp") && typ != fourcc("stsc") && typ != fourcc("sbgp") {
        return Ok(());
    }
    let body = read_body(d, size, typ, offs)?;
    sample_table(typ, &body, track_desc).map_err(|e| field_error(e, typ, offs))
}

/// Reads the table of a sample table box, `body` (after its header). Each count is the file's, so the entries are checked
/// to fit in the box before they're read
fn sample_table(typ: u32, body: &[u8], track_desc: &mut PartTrack) -> Result<()> {
    let d = &mut &body[..];
    let (v, _flags) = (d.read_u8()?, d.read_u24::<BigEndian>()?);

    if typ == fourcc("stsz") {
        track_desc.stsz_sample_size = d.read_u32::<BigEndian>()?;
        track_desc.stsz_count = d.read_u32::<BigEndian>()?;
        if track_desc.stsz_sample_size == 0 {
            track_desc.stsz.reserve_exact(entries(d, track_desc.stsz_count, 32)?);
            for _ in 0..track_desc.stsz_count { track_desc.stsz.push(d.read_u32::<BigEndian>()?); }
        }
    }
    if typ == fourcc("stz2") {
        // Written as `stsz`
        let field_size = d.read_u32::<BigEndian>()? & 0xFF;
        track_desc.stsz_count = d.read_u32::<BigEndian>()?;
        if !matches!(field_size, 4 | 8 | 16) {
            return Err(Error::new(ErrorKind::InvalidData, format!("has a field_size of {field_size}")));
        }
        track_desc.stsz.reserve_exact(entries(d, track_desc.stsz_count, field_size as u64)?);
        // Two 4-bit sizes in a byte, the first in its high bits
        let mut packed = 0;
        for i in 0..track_desc.stsz_count {
            track_desc.stsz.push(match field_size {
                4 => { if i % 2 == 0 { packed = d.read_u8()?; (packed >> 4) as u32 } else { (packed & 0x0F) as u32 } },
                8 => d.read_u8()? as u32,
                _ => d.read_u16::<BigEndian>()? as u32
            });
        }
    }
    if typ == fourcc("sdtp") {
        // One byte for each sample, to the end of the box
        track_desc.sdtp = Some(d.to_vec());
    }
    if typ == fourcc("sbgp") {
        known_version(v, 1)?;
        let grouping_type = d.read_u32::<BigEndian>()?;
        let grouping_type_parameter = if v == 1 { Some(d.read_u32::<BigEndian>()?) } else { None };
        let count = d.read_u32::<BigEndian>()?;
        let mut entries = Vec::with_capacity(entries(d, count, 64)?);
        for _ in 0..count { entries.push((d.read_u32::<BigEndian>()?, d.read_u32::<BigEndian>()?)); }
        track_desc.sbgp.push(SampleToGroup { version: v, grouping_type, grouping_type_parameter, entries });
    }
    if typ == fourcc("stss") || typ == fourcc("stps") || typ == fourcc("stco") || typ == fourcc("co64") || typ == fourcc("stts") || typ == fourcc("ctts") || typ == fourcc("stsc") {
        if typ == fourcc("ctts") { known_version(v, 1)?; }
        let count = d.read_u32::<BigEndian>()?;
        let bits = if typ == fourcc("stss") || typ == fourcc("stps") || typ == fourcc("stco") { 32 } else if typ == fourcc("stsc") { 96 } else { 64 };
        let n = entries(d, count, bits)?;
        let mut list = Vec::new();
        let mut ctts = Vec::new();
        // The room for the table, now that its entries are known to be in the box
        if typ == fourcc("stss") || typ == fourcc("stps") { list.reserve_exact(n); }
        else if typ == fourcc("ctts") { ctts.reserve_exact(n); }
        else if typ == fourcc("stco") || typ == fourcc("co64") { track_desc.stco.reserve_exact(n); }
        else if typ == fourcc("stts") { track_desc.stts.reserve_exact(n); }
        else { track_desc.stsc.reserve_exact(n); }
        for _ in 0..count {
            if typ == fourcc("stss") || typ == fourcc("stps") { list.push(d.read_u32::<BigEndian>()?); }
            if typ == fourcc("stco") { track_desc.stco.push(d.read_u32::<BigEndian>()? as u64); }
            if typ == fourcc("co64") { track_desc.stco.push(d.read_u64::<BigEndian>()?); }
            if typ == fourcc("stts") { track_desc.stts.push((d.read_u32::<BigEndian>()?, d.read_u32::<BigEndian>()?)); }
            // Version 0 offsets are unsigned, but writers store negative ones there too (as FFmpeg reads them), so both are read as signed
            if typ == fourcc("ctts") { ctts.push((d.read_u32::<BigEndian>()?, d.read_i32::<BigEndian>()?)); }
            if typ == fourcc("stsc") { track_desc.stsc.push((
                d.read_u32::<BigEndian>()?,
                d.read_u32::<BigEndian>()?,
                d.read_u32::<BigEndian>()?
            )); }
        }
        if typ == fourcc("stss") { track_desc.stss = Some(list); }
        else if typ == fourcc("stps") { track_desc.stps = Some(list); }
        else if typ == fourcc("ctts") { track_desc.ctts = Some((v, ctts)); }
    }
    Ok(())
}

/// A sample entry of `stsd`, at `at`, with its comparable form: its bytes with the fields that only state the stream's
/// bitrate zeroed, and without its AVC or HEVC decoder configuration. Those fields are the `btrt` box and the buffer size
/// and bitrates of an `esds` decoder configuration (ISO/IEC 14496-12 §8.5.2.2, 14496-1 §7.2.6.6), which differ between the
/// files of one recording. Every other byte describes how the samples are decoded or presented
fn sample_entry(handler: u32, entry: &[u8], at: usize) -> SampleEntry {
    let format = u32::from_be_bytes(entry[4..8].try_into().unwrap());
    let mut e = entry.to_vec();
    let mut config = None;
    // The boxes of an entry follow its fields: 8 bytes of SampleEntry, then the AudioSampleEntry (or QuickTime
    // SoundDescription, whose versions 1 and 2 add 16 and 36 bytes) or VisualSampleEntry fields
    let boxes_at = if handler == fourcc("soun") {
        match e.get(16..18).map(|v| u16::from_be_bytes([v[0], v[1]])) {
            Some(1) => Some(8 + 8 + 20 + 16),
            Some(2) => Some(8 + 8 + 20 + 36),
            _ => Some(8 + 8 + 20),
        }
    } else if handler == fourcc("vide") {
        Some(8 + 8 + 70)
    } else {
        None
    };
    if let Some(boxes_at) = boxes_at {
        let end = e.len();
        without_bitrates(&mut e, boxes_at, end, &mut config);
    }
    if let Some(c) = &config {
        // And the entry's size, which counts it
        e.drain(c.at.0..c.at.1);
        e[..4].fill(0);
    }
    SampleEntry { format, at, comparable: e, config }
}

fn without_bitrates(b: &mut [u8], mut pos: usize, end: usize, config: &mut Option<DecoderConfig>) {
    while pos + 8 <= end {
        let size = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || size > end - pos { break; }
        match &b[pos + 4..pos + 8] {
            b"btrt" => b[pos + 8..pos + size].fill(0),
            b"esds" if size > 12 => esds_without_bitrates(&mut b[pos + 12..pos + size]),
            b"wave" => without_bitrates(b, pos + 8, pos + size, config), // QuickTime's sound description extension
            b"avcC" | b"hvcC" if config.is_none() => *config = decoder_config(&b[pos..pos + size], pos),
            _ => {}
        }
        pos += size;
    }
}

/// Reads an `avcC` or `hvcC` box at `at` of its entry, or `None` when its arrays don't fit in it
fn decoder_config(b: &[u8], at: usize) -> Option<DecoderConfig> {
    let typ = u32::from_be_bytes(b[4..8].try_into().unwrap());
    let c = &b[8..];
    let nalus = |pos: &mut usize, count: usize| -> Option<Vec<Vec<u8>>> {
        let mut list = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u16::from_be_bytes(c.get(*pos..*pos + 2)?.try_into().unwrap()) as usize;
            list.push(c.get(*pos + 2..*pos + 2 + len)?.to_vec());
            *pos += 2 + len;
        }
        Some(list)
    };
    let mut arrays = Vec::new();
    let mut pos;
    if typ == fourcc("avcC") {
        // numOfSequenceParameterSets in the low 5 bits of byte 5, then numOfPictureParameterSets
        pos = 5;
        let sps = *c.get(pos)?;
        pos += 1;
        arrays.push((7, sps & 0xE0, nalus(&mut pos, (sps & 0x1F) as usize)?));
        let pps = *c.get(pos)? as usize;
        pos += 1;
        arrays.push((8, 0, nalus(&mut pos, pps)?));
    } else {
        pos = 22;
        let count = *c.get(pos)?;
        pos += 1;
        for _ in 0..count {
            let byte = *c.get(pos)?;
            let n = u16::from_be_bytes(c.get(pos + 1..pos + 3)?.try_into().unwrap()) as usize;
            pos += 3;
            arrays.push((byte & 0x3F, byte, nalus(&mut pos, n)?));
        }
    }
    let header = c.get(..if typ == fourcc("avcC") { 5 } else { 22 })?.to_vec();
    Some(DecoderConfig { typ, at: (at, at + b.len()), header, arrays, trailing: c[pos..].to_vec() })
}

impl DecoderConfig {
    /// The box, with its header, or `None` when a count or a length doesn't fit its field: an avcC counts its sequence
    /// parameter sets in 5 bits and its picture parameter sets in 8 (ISO/IEC 14496-15 §5.3.3.1), an hvcC its arrays in 8
    /// and each array's NAL units in 16 (§8.3.3.1), and both give each NAL unit's length in 16 bits
    pub fn to_box(&self) -> Option<Vec<u8>> {
        let mut c = self.header.clone();
        let push = |c: &mut Vec<u8>, nalus: &[Vec<u8>]| -> Option<()> {
            for n in nalus { c.extend_from_slice(&u16::try_from(n.len()).ok()?.to_be_bytes()); c.extend_from_slice(n); }
            Some(())
        };
        if self.typ == fourcc("avcC") {
            for (typ, byte, nalus) in &self.arrays {
                let max = if *typ == 7 { 0x1F } else { 0xFF };
                c.push(byte | u8::try_from(nalus.len()).ok().filter(|&n| n <= max)?);
                push(&mut c, nalus)?;
            }
        } else {
            c.push(u8::try_from(self.arrays.len()).ok()?);
            for (_, byte, nalus) in &self.arrays {
                c.push(*byte);
                c.extend_from_slice(&u16::try_from(nalus.len()).ok()?.to_be_bytes());
                push(&mut c, nalus)?;
            }
        }
        c.extend_from_slice(&self.trailing);
        let mut b = u32::try_from(8 + c.len()).ok()?.to_be_bytes().to_vec();
        b.extend_from_slice(&self.typ.to_be_bytes());
        b.extend_from_slice(&c);
        Some(b)
    }

    /// The id of a picture parameter set: pps_pic_parameter_set_id, the first field after the NAL unit header (ITU-T
    /// H.264 §7.3.2.2, H.265 §7.3.2.3.1), an Exp-Golomb code in the bytes without emulation prevention (§7.4.1)
    pub fn pps_id(typ: u32, nalu: &[u8]) -> Option<u32> {
        let header = if typ == fourcc("avcC") { 1 } else { 2 };
        let mut rbsp = Vec::with_capacity(8);
        let mut zeros = 0;
        for &x in nalu.get(header..)?.iter().take(16) {
            if zeros >= 2 && x == 3 { zeros = 0; continue; }
            zeros = if x == 0 { zeros + 1 } else { 0 };
            rbsp.push(x);
        }
        let bit = |i: usize| rbsp.get(i / 8).map(|b| (b >> (7 - i % 8)) & 1);
        let mut i = 0;
        while bit(i)? == 0 { i += 1; if i > 31 { return None; } }
        let leading = i;
        let mut value = 0u32;
        for k in 0..leading { value = (value << 1) | bit(leading + 1 + k)? as u32; }
        Some((1 << leading) - 1 + value)
    }
}

/// A `csgm` body (after its box header), in the one form known: Apple's compact sample-to-group mapping, which iPhone and
/// Blackmagic Camera HEVC recordings carry for the `tscl` and `tsas` groupings of ISO/IEC 14496-15 (CoreMedia hands them
/// to players as each frame's temporal level). Its syntax isn't published. We established it on every `csgm` it has
/// met (`IMG_6440.MOV`, `IMG_6441.MOV`, `A001_10161552_C016.mov`), against each picture's NAL header: version 0 and flags
/// 0, `grouping_type`, a word of 0, 4 (the bits of each index), `pattern_count`, then each pattern's `pattern_length` and
/// `sample_count` (32 bits each), then all the indices, 4 bits each, most significant first, zero bits padding the last
/// byte. A pattern maps `sample_count` samples, which take its indices in turn, as a `csgp`'s (ISO/IEC 14496-12
/// §8.9.5.1); index 0 is no group. `None` for any other form, or a pattern of no indices or more indices than samples
pub(crate) fn compact_to_group(body: &[u8]) -> Option<CompactToGroup> {
    let field = |at: usize| body.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    if field(0)? != 0 || field(8)? != 0 || field(12)? != 4 {
        return None;
    }
    let (grouping_type, count) = (field(4)?, field(16)? as usize);
    let indices_at = count.checked_mul(8)?.checked_add(20)?;
    if indices_at > body.len() { return None; }
    let lengths: Vec<(u32, u32)> = (0..count).map(|k| (field(20 + 8 * k).unwrap(), field(24 + 8 * k).unwrap())).collect();
    let total = lengths.iter().try_fold(0u64, |sum, &(len, samples)| (len > 0 && len <= samples).then_some(sum + len as u64))?;
    if (body.len() - indices_at) as u64 != total.div_ceil(2) || (!total.is_multiple_of(2) && body[body.len() - 1] & 0x0F != 0) {
        return None;
    }
    let index = |k: u64| { let b = body[indices_at + (k / 2) as usize]; (if k.is_multiple_of(2) { b >> 4 } else { b & 0x0F }) as u32 };
    let mut at = 0;
    let patterns = lengths.iter().map(|&(len, samples)| {
        let indices = (at..at + len as u64).map(index).collect();
        at += len as u64;
        (samples, indices)
    }).collect();
    Some(CompactToGroup { grouping_type, patterns })
}

impl CompactToGroup {
    /// The `csgm` box, with its header, in the form `compact_to_group` reads, or `None` when an index doesn't fit its 4
    /// bits or a count its 32
    pub fn to_box(&self) -> Option<Vec<u8>> {
        let mut b = Vec::new();
        for x in [0, self.grouping_type, 0, 4, u32::try_from(self.patterns.len()).ok()?] { b.extend_from_slice(&x.to_be_bytes()); }
        for (samples, indices) in &self.patterns {
            b.extend_from_slice(&u32::try_from(indices.len()).ok()?.to_be_bytes());
            b.extend_from_slice(&samples.to_be_bytes());
        }
        let indices: Vec<u8> = self.patterns.iter().flat_map(|p| &p.1).map(|&i| u8::try_from(i).ok().filter(|&i| i < 16)).collect::<Option<_>>()?;
        b.extend(indices.chunks(2).map(|c| c[0] << 4 | c.get(1).copied().unwrap_or(0)));
        let mut out = u32::try_from(8 + b.len()).ok()?.to_be_bytes().to_vec();
        out.extend_from_slice(b"csgm");
        out.extend_from_slice(&b);
        Some(out)
    }
}

/// Zeroes `bufferSizeDB`, `maxBitrate` and `avgBitrate` of the DecoderConfigDescriptor in an ES_Descriptor
fn esds_without_bitrates(es: &mut [u8]) {
    // A descriptor: its tag, then its size in 1 to 4 bytes of 7 bits
    fn header(b: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
        let tag = *b.get(pos)?;
        let mut size = 0;
        for i in 1..=4 {
            let x = *b.get(pos + i)?;
            size = (size << 7) | (x & 0x7F) as usize;
            if x & 0x80 == 0 { return Some((tag, pos + i + 1, size)); }
        }
        None
    }
    let Some((0x03, mut pos, _)) = header(es, 0) else { return; };
    let Some(&flags) = es.get(pos + 2) else { return; };
    pos += 3; // ES_ID, flags
    if flags & 0x80 != 0 { pos += 2; } // dependsOn_ES_ID
    if flags & 0x40 != 0 { pos += 1 + *es.get(pos).unwrap_or(&0) as usize; } // URL
    if flags & 0x20 != 0 { pos += 2; } // OCR_ES_Id
    if let Some((0x04, at, size)) = header(es, pos) {
        // objectTypeIndication, streamType; then bufferSizeDB (24 bits), maxBitrate, avgBitrate
        if size >= 13 && at + 13 <= es.len() {
            es[at + 2..at + 13].fill(0);
        }
    }
}
