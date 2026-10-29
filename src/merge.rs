// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2026 Adrian <adrian.eddy at gmail>

// Builds the merged description (`Desc::moov_tracks`) from the files' own ones, after checking that they can be merged:
// - Tracks are matched by their position, so every file must have the same tracks, in the same order
// - The merged track keeps the first file's sample descriptions, so every file's must be the same
// - Each file's timing is moved into the merged track's media timescale exactly, or the merge is refused, so the
//   merged file never plays at another speed than its files
// - Each file's tables are appended to the others', so its sample-to-chunk table must give its chunks exactly its
//   samples, and every sample keeps its sample groups
// - The files of one recording, the chapters a camera splits it into, continue each other: every track's samples follow
//   the previous file's. Separate recordings follow each other's picture: a file starts where the previous file's first
//   video track ends, which keeps that track and the metadata continuous, and every other picture and sound track of the
//   file starts with it there (`junctions`, `plan`)

use std::borrow::Cow;
use std::io::{ Error, ErrorKind, Result };
use crate::typ_to_str;
use crate::{ fourcc, desc_reader::{ CompactToGroup, DecoderConfig, Desc, Edit, PartDesc, PartTrack, SampleToGroup, TrackDesc, MEDIA_RATE_1 } };
use crate::segment_table::{ self, Table };

fn refused(msg: String) -> Error { Error::new(ErrorKind::InvalidInput, format!("These files can't be merged: {msg}")) }
fn invalid(msg: String) -> Error { Error::new(ErrorKind::InvalidData, msg) }

fn gcd(mut a: u64, mut b: u64) -> u64 { while b != 0 { (a, b) = (b, a % b); } a }

fn add(a: u64, b: u64) -> Result<u64> { a.checked_add(b).ok_or_else(|| invalid("a duration doesn't fit 64 bits".into())) }

/// `value` from timescale `from` to `to`, rounded up. Durations in the movie timescale (`mvhd`, `tkhd`, `elst`) are
/// rounded in the files already, and up is how a muxer rounds them (FFmpeg's `mov_write_edts_tag`), so nothing ends early
fn rescale_up(value: u64, from: u32, to: u32) -> Result<u64> {
    u64::try_from((value as u128 * to as u128).div_ceil(from as u128)).map_err(|_| invalid(format!("a duration of {value} doesn't fit 64 bits in timescale {to}")))
}

/// `value` from timescale `from` to `to`, when `to` holds it exactly
fn rescale_exact(value: u64, from: u32, to: u32) -> Option<u64> {
    let x = value as u128 * to as u128;
    x.is_multiple_of(from as u128).then(|| u64::try_from(x / from as u128).ok()).flatten()
}

pub fn merge(desc: &mut Desc) -> Result<()> {
    let parts = &desc.parts;
    let Some(first) = parts.first() else { return Ok(()); };
    for (i, p) in parts.iter().enumerate() {
        if p.mvhd_timescale == 0 {
            return Err(invalid(format!("file {} has no movie timescale (mvhd)", i + 1)));
        }
        if p.tracks.len() != first.tracks.len() {
            return Err(refused(format!("file {} has {} tracks and file 1 has {}", i + 1, p.tracks.len(), first.tracks.len())));
        }
        for (t, (a, b)) in first.tracks.iter().zip(&p.tracks).enumerate() {
            if a.handler != b.handler {
                return Err(refused(format!("track {} is '{}' in file 1 and '{}' in file {}", t + 1, typ_to_str(a.handler), typ_to_str(b.handler), i + 1)));
            }
            if b.mdhd_timescale == 0 {
                return Err(invalid(format!("track {} of file {} has no media timescale (mdhd)", t + 1, i + 1)));
            }
        }
    }
    for (i, p) in parts.iter().enumerate() {
        if p.mvex && p.tracks.iter().all(|x| x.stsz_count == 0) {
            // Its samples are in movie fragments only (ISO/IEC 14496-12 §8.8.1), and it has none
            return Err(refused(format!("file {} is a fragmented MP4 without its movie fragments: its sample tables have no samples", i + 1)));
        }
    }
    let movie = Movie {
        parts,
        timescale: first.mvhd_timescale,
        continues: junctions(parts),
        ends: parts.iter().map(|p| file_end(p, first.mvhd_timescale)).collect::<Result<_>>()?,
    };
    let mut filler = Filler { at: desc.mdat_offset, runs: Vec::new() };
    let mut tracks: Vec<Option<TrackDesc>> = vec![None; first.tracks.len()];
    // The merged movie lasts at least as long as each track presents the files (`Plan::end`)
    let mut presents = 0;
    // Where each file starts in the merged movie after another recording: where the first video track ends the previous
    // file's picture (so that track is merged first), or without one, where the previous file's tracks end
    let starts = match (0..tracks.len()).find(|&t| anchors(parts, t)) {
        Some(t) => {
            let (track, starts, end) = merge_track(&movie, t, Role::Anchor, &mut filler)?;
            tracks[t] = Some(track);
            presents = end;
            starts
        }
        None => {
            let mut ticks = Vec::with_capacity(parts.len());
            let mut at = 0;
            for &end in &movie.ends {
                ticks.push(at);
                at = add(at, end)?;
            }
            Starts { exact: ticks.iter().map(|&t| t as u128).collect(), ticks, timescale: 1 }
        }
    };
    for (t, track) in tracks.iter_mut().enumerate() {
        if track.is_none() {
            let (merged, _, end) = merge_track(&movie, t, Role::Aligned(&starts), &mut filler)?;
            *track = Some(merged);
            presents = presents.max(end);
        }
    }
    let tracks: Vec<TrackDesc> = tracks.into_iter().flatten().collect();
    // The merged movie: every file as long as its own tracks (`file_end`), and the files before another recording up to
    // where that one starts, where each of their picture and sound tracks ends (`plan`), also after a chapter whose sound
    // outlasts its picture. And as long as every track presents the files, as far as their own movies present them
    // (ISO/IEC 14496-12 §8.2.2.3: its longest track's duration): after another recording, a sound track that starts a new
    // edit does so on the tick nearest its picture, which can be after where the picture's edits end, and metadata follows
    // the previous file's, which can end after its picture (`Plan::end`)
    let mut duration = 0;
    for (i, &end) in movie.ends.iter().enumerate() {
        duration = match movie.continues.get(i) {
            Some(false) => starts.ticks[i + 1],
            _ => add(duration, end)?
        };
    }
    let duration = duration.max(presents);
    desc.moov_mvhd_timescale = movie.timescale;
    desc.moov_mvhd_duration = duration;
    desc.moov_tracks = tracks;
    desc.mdat_filler = filler.runs;
    Ok(())
}

/// Where each file starts in the merged movie after another recording (`Plan::starts`): in its ticks, and exactly, in its
/// ticks times `timescale` (the anchor's media timescale)
#[derive(Debug)]
struct Starts {
    ticks: Vec<u64>,
    exact: Vec<u128>,
    timescale: u32,
}

/// The files, as every track is merged
struct Movie<'a> {
    parts: &'a [PartDesc],
    timescale: u32,       // The merged movie's: the first file's
    continues: Vec<bool>, // For each file but the last, whether the next one continues its recording (`junctions`)
    ends: Vec<u64>,       // Each file's end by its own tracks (`file_end`), in `timescale`
}

/// Where a file ends by its own tracks, in the timescale of the merged movie (`movie_timescale`): where the last of them
/// ends, as it presents its samples (its edits, an edit at the normal rate no further than its media: one that runs past
/// it is a placeholder, see `plan`), and no later than its movie (`mvhd`), whose duration is its longest track's (ISO/IEC
/// 14496-12 §8.2.2.3) and nothing more. Neither timecode nor a track without samples, or whose samples last no time, counts.
/// A movie whose duration overstates its tracks' doesn't move what follows it: FFmpeg plays such a file as long as its
/// tracks, and the merge never fills time no track of the file has
fn file_end(p: &PartDesc, movie_timescale: u32) -> Result<u64> {
    let mut end = 0u64;
    for x in p.tracks.iter().filter(|x| !x.skip && x.stsz_count > 0) {
        end = end.max(presented(&x.stts, x.ctts.as_ref(), x.elst.as_deref(), x.mdhd_timescale, p.mvhd_timescale)?);
    }
    rescale_up(end.min(p.mvhd_duration), p.mvhd_timescale, movie_timescale)
}

/// How long a track presents its samples, in its movie's timescale `movie_timescale`: its samples' durations (`stts`) and
/// composition offsets (`ctts`), in `media_timescale`, through its edits (or as they are, without an edit list), each edit
/// at the normal rate no further than the media (one that runs past it is a placeholder, see `Timeline::plan`). 0 for
/// samples that last no time
fn presented(stts: &[(u32, u32)], ctts: Option<&(u8, Vec<(u32, i32)>)>, elst: Option<&[Edit]>, media_timescale: u32, movie_timescale: u32) -> Result<u64> {
    let media = u64::try_from(composition_end(stts, ctts)?).unwrap_or(0);
    if media == 0 { return Ok(0); }
    match elst {
        None => rescale_up(media, media_timescale, movie_timescale),
        Some(edits) => edits.iter().try_fold(0u64, |sum, e| {
            let presents = if e.media_time >= 0 && e.media_rate == MEDIA_RATE_1 {
                e.segment_duration.min(rescale_up(media.saturating_sub(e.media_time as u64), media_timescale, movie_timescale)?)
            } else {
                e.segment_duration
            };
            add(sum, presents)
        })
    }
}

/// Whether track `t` is the one whose picture tells where each file starts after another recording: the first video
/// track whose samples last some time in a file (not one of thumbnails, as DJI's `nail`, nor timecode). A track that is
/// timecode in the first file is merged as timecode (`merge_track`), so it has no picture to start the files at
fn anchors(parts: &[PartDesc], t: usize) -> bool {
    let picture = |x: &PartTrack| x.handler == fourcc("vide") && !x.skip && x.stts.iter().any(|e| e.0 > 0 && e.1 > 0);
    let anchor = |u: usize| !parts[0].tracks[u].skip && parts.iter().any(|p| picture(&p.tracks[u]));
    anchor(t) && !(0..t).any(anchor)
}

/// For each file but the last, whether the next one continues its recording: whether it is the next chapter of a
/// recording that the camera split into files (by size or by time), rather than another recording.
///
/// A chapter continues the previous one's streams: the camera splits each of them where the file ends, so a track of
/// one chapter may end some milliseconds before or after its video, and the next chapter's continues from there (GoPro's
/// and DJI's chapters start their audio without a new encoder's priming). A separate recording starts every track anew,
/// where its file starts. No box links chapters, and their edit lists are like any recording's (RunCam gives every track
/// of every file the video's duration, DJI each track its own). What tells them apart are the files' creation times
/// (`mvhd`, in whole seconds): a camera stamps the chapters of a recording with the recording's start (GoPro) or each
/// with its own start (DJI, RunCam, Caddx), and every recording with its own. In the corpus:
/// - every chapter has the previous one's time, or that time plus the previous chapter's duration to within 1.14 s: the
///   stamps' resolution of 1 s, and when the camera stamps the file (DJI_0037 → DJI_0038: 196.864 s, then +198 s). That
///   is 15 junctions of GoPro, DJI, Insta360, RunCam and Caddx chapters;
/// - separate recordings are further apart: 1.49 s more at the least (back-to-back shots of a Sony a9 III; also 1.67,
///   1.81 and 1.99 s, a camera closes one file before it starts the next), and usually far more.
///
/// So a file continues the previous one when it has the same creation time, or one within 2 s of the previous one's plus
/// that one's duration. The margin keeps every chapter one recording, whose tracks must not get gaps; a recording started
/// sooner than that after the last is merged as before, as its continuation. So are files whose times say nothing of
/// this (both 0, or a tool's copy of its source's). Only a file with exactly the previous one's samples is that recording
/// again, not its continuation.
///
/// The rule rests on how cameras stamp a file, which ISO/IEC 14496-12 §8.2.2.3 leaves open ("the creation time of the
/// presentation"), and QuickTime too ("when the movie atom was created", which a camera writes at the end). The corpus'
/// cameras stamp the start: where a recording's last chapter is shorter, the step to it is the previous chapter's
/// duration, not its own (DJI_0001: +286 s after 286.0 s, the last chapter 29.7 s; ThumbPro_0002: +300 s, 82.5 s;
/// Insta360 `_005`: +1799 s, 877 s), DJI's stamps are the start times in their file names, and an iPhone stamps the start
/// as creation and the end as modification. Where the rule errs:
/// - a camera that stamps each file when it ends (or writes its `moov`) has its chapters a duration apart too while they
///   are as long as each other, but its last, shorter chapter would be taken for another recording;
/// - a recording started within 2 s of the end of the previous one, a camera without a clock (0), or a tool that copies
///   its source's time onto every output, is taken for a chapter.
///
/// Neither moves the picture or the metadata, which follow the previous file's either way (`plan`). The first moves the
/// sound of that last chapter to where its picture starts, by the few milliseconds the previous chapter's sound ended
/// before or after its picture; the second keeps the previous file's sound running into the next one's, as up to 0.1.11.
///
/// A file cut from camera recordings says which frames of which recordings it holds, in its segment table (`MTRM`,
/// `segment_table`), and the cut started and ended each of its tracks with its picture, wherever its creation time says it
/// was cut from. So the recordings decide, not the files' times:
/// - a file that starts where the previous one ends in a recording (its first frame the one after the other's last)
///   continues it: the cut split the recording's streams there, as a camera splits a chapter;
/// - one that starts after its recording's first frame, or follows one that ends before its recording's last, starts
///   another moment of it, or of another recording: every track starts anew with its picture;
/// - otherwise the junction is between the recordings themselves: the last one of the previous file and the first one of
///   the next, whose times (the table's sources') and durations are those of the camera's files, which the rule above
///   takes as it takes the files. The same recording again is another moment of it
fn junctions(parts: &[PartDesc]) -> Vec<bool> {
    // Each file's segment table, where it's valid for the file
    let tables: Vec<Option<Table>> = parts.iter().enumerate().map(|(i, p)| match Table::of_file(p.segment_tables.as_deref().unwrap_or_default(), p) {
        Some(Ok(table)) => Some(table),
        Some(Err(e)) => {
            log::warn!("The segment table of file {} isn't valid for it, and is ignored: {e}", i + 1);
            None
        }
        None => None,
    }).collect();
    parts.windows(2).zip(tables.windows(2)).map(|(p, t)| continues((&p[0], t[0].as_ref()), (&p[1], t[1].as_ref()))).collect()
}

/// A recording at one end of a file: the file itself, or a source of its segment table
#[derive(Clone, Copy)]
enum Recording<'a> {
    File(&'a PartDesc),
    Source(&'a segment_table::Source),
}

impl Recording<'_> {
    /// Its creation time, seconds since 1904 (0 for none)
    fn creation(&self) -> u64 {
        match self { Self::File(p) => p.mvhd_creation, Self::Source(s) => s.creation }
    }
    /// Whether a recording that starts `since` seconds after this one does, by its creation time, starts within 2 s of
    /// this one's end: of its movie's duration, or of its frames at its frame rate
    fn ends_near(&self, since: i128) -> bool {
        match self {
            Self::File(p) => (since * p.mvhd_timescale as i128 - p.mvhd_duration as i128).abs() <= 2 * p.mvhd_timescale as i128,
            Self::Source(s) => (since * s.fps.0 as i128 - s.frames as i128 * s.fps.1 as i128).abs() <= 2 * s.fps.0 as i128,
        }
    }
    /// Whether it's the same recording as `other`: the same samples of two files, the same name and size of two sources,
    /// and a file of a source's size and frames
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(a), Self::File(b)) => same_recording(a, b),
            (Self::Source(a), Self::Source(b)) => a.name == b.name && a.size == b.size,
            (Self::File(p), Self::Source(s)) | (Self::Source(s), Self::File(p)) => p.size == s.size && segment_table::presented_frames(p) == Some(s.frames),
        }
    }
}

/// Whether the file `b` continues the file `a`'s recording (`junctions`), each with its segment table
fn continues(a: (&PartDesc, Option<&Table>), b: (&PartDesc, Option<&Table>)) -> bool {
    if let (Some(ta), Some(tb)) = (a.1, b.1) {
        let (last, first) = (ta.last(), tb.first());
        let (sa, sb) = (ta.source_of(last), tb.source_of(first));
        if sa.name == sb.name && sa.size == sb.size && last.src_first_frame + last.frame_count == first.src_first_frame {
            return true;
        }
    }
    // The recording each file ends or starts with, where it ends with its last frame or starts with its first
    let end = match a.1 {
        None => Some(Recording::File(a.0)),
        Some(t) => { let s = t.last(); let src = t.source_of(s); (s.src_first_frame + s.frame_count == src.frames).then_some(Recording::Source(src)) }
    };
    let start = match b.1 {
        None => Some(Recording::File(b.0)),
        Some(t) => { let s = t.first(); (s.src_first_frame == 0).then_some(Recording::Source(t.source_of(s))) }
    };
    let (Some(end), Some(start)) = (end, start) else { return false };
    let since = start.creation() as i128 - end.creation() as i128;
    if since == 0 {
        return !end.same(&start);
    }
    end.ends_near(since)
}

/// Whether two files hold the same recording: the same movie duration, and in every track the same samples
fn same_recording(a: &PartDesc, b: &PartDesc) -> bool {
    a.mvhd_duration == b.mvhd_duration && a.mvhd_timescale == b.mvhd_timescale && a.tracks.len() == b.tracks.len() &&
        a.tracks.iter().zip(&b.tracks).all(|(x, y)| x.mdhd_timescale == y.mdhd_timescale && x.stts == y.stts && x.ctts == y.ctts &&
            x.stsz_sample_size == y.stsz_sample_size && x.stsz_count == y.stsz_count && x.stsz == y.stsz)
}

/// The silence the merge puts into the merged mdat, after the files' data: where its next byte goes in the merged mdat,
/// and its runs of one byte
struct Filler {
    at: u64,
    runs: Vec<(u64, u8)>,
}

impl Filler {
    /// Where `len` bytes of `byte` go
    fn add(&mut self, len: u64, byte: u8) -> Result<u64> {
        let at = self.at;
        self.at = add(self.at, len)?;
        match self.runs.last_mut() {
            Some(last) if last.1 == byte => last.0 += len,
            _ => self.runs.push((len, byte))
        }
        Ok(at)
    }
}

/// Merges track `t` of the files, in its `role` (`Role::Anchor` or `Role::Aligned`, which a track that isn't one of picture
/// or sound plays as `Role::Continuous`). Also returns where its picture has each file start after another recording (the
/// anchor's: `Plan::starts`), and how long it presents the files (`Plan::end`; 0 for timecode, which doesn't count)
fn merge_track(movie: &Movie, t: usize, role: Role, filler: &mut Filler) -> Result<(TrackDesc, Starts, u64)> {
    let (parts, movie_timescale) = (movie.parts, movie.timescale);
    let tracks: Vec<&PartTrack> = parts.iter().map(|p| &p.tracks[t]).collect();
    let first = tracks[0];
    let name = format!("track {} ('{}')", t + 1, typ_to_str(first.handler));
    let mut out = TrackDesc::default();
    for (p, x) in parts.iter().zip(&tracks) {
        out.tkhd_duration = add(out.tkhd_duration, rescale_up(x.tkhd_duration, p.mvhd_timescale, movie_timescale)?)?;
    }

    if first.skip {
        // Timecode isn't merged: the track keeps the first file's samples, and spans all the files. So every file's must be
        // timecode, or the merged track would leave out a later file's samples of another kind
        if let Some(i) = tracks.iter().position(|x| !x.skip) {
            return Err(refused(format!("{name} has another sample description in file {} than in file 1 ('{}' and '{}'): file 1's is timecode, of which the merged track keeps the first file's samples alone", i + 1, formats(tracks[i]), formats(first))));
        }
        within_mdat(parts[0].mdat, first).map_err(|_| refused(format!("{name} of file 1 has samples outside its mdat box, and only that box is merged")))?;
        out.mdhd_timescale = first.mdhd_timescale;
        for x in &tracks {
            out.mdhd_duration = add(out.mdhd_duration, rescale_up(x.mdhd_duration, x.mdhd_timescale, first.mdhd_timescale)?)?;
        }
        out.elst = first.elst.clone();
        if let Some(last) = out.elst.as_mut().and_then(|e| e.iter_mut().rev().find(|e| e.media_time != -1)) {
            for (p, x) in parts.iter().zip(&tracks).skip(1) {
                let presented = match &x.elst {
                    Some(edits) => edits.iter().filter(|e| e.media_time != -1).map(|e| rescale_up(e.segment_duration, p.mvhd_timescale, movie_timescale)).sum::<Result<u64>>()?,
                    None => rescale_up(media_duration(x)?, x.mdhd_timescale, movie_timescale)?
                };
                last.segment_duration = add(last.segment_duration, presented)?;
            }
        }
        out.stts = first.stts.clone();
        out.ctts = first.ctts.clone();
        out.stss = first.stss.clone();
        out.stps = first.stps.clone();
        out.stsz_sample_size = first.stsz_sample_size;
        out.stsz_count = first.stsz_count;
        out.stsz = first.stsz.clone();
        out.stsc = first.stsc.clone();
        out.stco = first.stco.clone();
        out.sdtp = first.sdtp.clone().unwrap_or_default();
        out.sample_groups = first.sbgp.clone();
        return Ok((out, Starts { ticks: Vec::new(), exact: Vec::new(), timescale: 1 }, 0));
    }

    if first.encrypted {
        return Err(refused(format!("{name} is encrypted")));
    }
    for (i, x) in tracks.iter().enumerate() {
        let samples: u64 = x.stts.iter().map(|e| e.0 as u64).sum();
        if samples != x.stsz_count as u64 {
            return Err(invalid(format!("{name} of file {} has {samples} samples in its time-to-sample table (stts) and {} in its sample size table (stsz)", i + 1, x.stsz_count)));
        }
        check_chunks(parts[i].mdat, x, &name, i)?;
        for typ in &x.unmerged {
            log::warn!("{name}: the '{}' box of file {} isn't merged, so the merged file keeps the first file's", typ_to_str(*typ), i + 1);
        }
    }

    out.stsd = sample_descriptions(&tracks, &name)?;
    let timescale = media_timescale(&tracks, &name)?;
    out.mdhd_timescale = timescale;
    // Each file's sample durations in the merged timescale
    let mut scaled = Vec::with_capacity(tracks.len());
    for (i, x) in tracks.iter().enumerate() {
        let too_long = || refused(format!("{name}: the timing of file {} doesn't fit 32 bits in media timescale {timescale}", i + 1));
        scaled.push(x.stts.iter().map(|&(count, delta)| {
            rescale_exact(delta as u64, x.mdhd_timescale, timescale).and_then(|d| u32::try_from(d).ok()).map(|d| (count, d)).ok_or_else(too_long)
        }).collect::<Result<Vec<(u32, u32)>>>()?);
    }
    // A track that isn't one of picture and sound, or whose samples last no time, has no picture or sound to align
    let picture_or_sound = (first.handler == fourcc("vide") || first.handler == fourcc("soun")) && scaled.iter().flatten().any(|e| e.0 > 0 && e.1 > 0);
    let role = match role {
        Role::Aligned(_) if !picture_or_sound => Role::Continuous,
        role => role
    };
    let pcm = pcm(&tracks, &scaled);
    let mut timeline = Timeline { movie, tracks: tracks.iter().map(|&x| Cow::Borrowed(x)).collect(), scaled, timescale, role, pcm, name };
    let plan = timeline.plan()?;
    // The files' tracks as the merged one keeps them: a file whose sound `plan` cut where the next recording starts has only
    // the samples before that
    let Timeline { tracks: kept, scaled, pcm, name, .. } = timeline;
    let tracks: Vec<&PartTrack> = kept.iter().map(|x| x.as_ref()).collect();
    let constant_size = tracks.iter().all(|x| x.stsz_sample_size != 0 && x.stsz_sample_size == first.stsz_sample_size);
    out.stsz_sample_size = if constant_size { first.stsz_sample_size } else { 0 };
    let ctts_version = tracks.iter().filter_map(|x| x.ctts.as_ref().map(|c| c.0)).max();
    let mut ctts = Vec::new();
    let any_stss = tracks.iter().any(|x| x.stss.is_some());
    let any_stps = tracks.iter().any(|x| x.stps.is_some());
    let (mut stss, mut stps) = (Vec::new(), Vec::new());
    let mut media_offset = 0u64; // Where the file's media starts in the merged track's
    let mut offsets = Vec::with_capacity(tracks.len());
    let (mut sample_offset, mut chunk_offset) = (0u32, 0u32);
    for (i, x) in tracks.iter().enumerate() {
        let ts = x.mdhd_timescale;
        let too_long = || refused(format!("{name}: the timing of file {} doesn't fit 32 bits in media timescale {timescale}", i + 1));
        offsets.push(media_offset);
        let fill = plan.fills[i];
        let mut sample = 0u32;
        for &(count, delta) in &scaled[i] {
            match fill.lengthen {
                // The sample that presents the gap after the file lasts that much longer
                Some((s, ticks)) if (sample..sample + count).contains(&s) => {
                    let longer = u32::try_from(delta as u64 + ticks).map_err(|_| refused(format!("{name}: a gap of {ticks} ticks after file {} doesn't fit a sample's 32-bit duration", i + 1)))?;
                    for (n, d) in [(s - sample, delta), (1, longer), (sample + count - s - 1, delta)] {
                        if n > 0 { out.stts.push((n, d)); }
                    }
                }
                _ => out.stts.push((count, delta))
            }
            sample += count;
            media_offset = add(media_offset, count as u64 * delta as u64)?;
        }
        let gap = add(fill.lengthen.map_or(0, |l| l.1), fill.silence)?;
        media_offset = add(media_offset, gap)?;
        // Its media duration, less what `plan` shortened its last sample by, in the merged timescale, which holds that
        // exactly, and with the gap after it
        let media = rescale_up(x.mdhd_duration, ts, timescale)?.saturating_sub(plan.shortened[i]);
        out.mdhd_duration = add(out.mdhd_duration, add(media, gap)?)?;
        if ctts_version.is_some() {
            let mut left = x.stsz_count;
            for &(count, offset) in x.ctts.iter().flat_map(|c| &c.1) {
                let count = count.min(left);
                if count == 0 { continue; }
                let scaled = rescale_exact(offset.unsigned_abs() as u64, ts, timescale).and_then(|o| i32::try_from(o).ok()).ok_or_else(too_long)?;
                ctts.push((count, if offset < 0 { -scaled } else { scaled }));
                left -= count;
            }
            // Samples without a composition offset have none, as in a track without `ctts`, or as FFmpeg reads a short one
            if left > 0 { ctts.push((left, 0)); }
        }
        let next_sample_offset = sample_offset.checked_add(x.stsz_count).ok_or_else(|| invalid(format!("{name} has more than 2^32 samples")))?;
        let numbers = || invalid(format!("{name} of file {} has a sample or chunk number past 2^32", i + 1));
        if !constant_size {
            // The sizes of each sample, when the files don't all have the same constant size
            if x.stsz_sample_size != 0 { out.stsz.extend(std::iter::repeat_n(x.stsz_sample_size, x.stsz_count as usize)); }
            else                       { out.stsz.extend(&x.stsz); }
        }
        if any_stss {
            // A file without a sync sample table has only sync samples
            match &x.stss {
                Some(list) => for s in list { stss.push(s.checked_add(sample_offset).ok_or_else(numbers)?); },
                None => stss.extend((sample_offset..next_sample_offset).map(|s| s + 1))
            }
        }
        if any_stps {
            for s in x.stps.iter().flatten() { stps.push(s.checked_add(sample_offset).ok_or_else(numbers)?); }
        }
        for c in &x.stsc { out.stsc.push((c.0.checked_add(chunk_offset).ok_or_else(numbers)?, c.1, c.2)); }
        out.stco.extend(&x.stco);
        if first.sdtp.is_some() {
            // Dependency 0 is unknown
            let mut sdtp = x.sdtp.clone().unwrap_or_default();
            sdtp.resize(x.stsz_count as usize, 0);
            out.sdtp.extend(sdtp);
        }
        sample_offset = next_sample_offset;
        chunk_offset = u32::try_from(x.stco.len()).ok().and_then(|n| chunk_offset.checked_add(n)).ok_or_else(numbers)?;
        if let (Pcm::Silence { frame, byte }, samples @ 1..) = (&pcm, fill.silence) {
            // The gap after the file in a PCM track is silence: a chunk of its own, in the silence after the files' data. A
            // sample of PCM lasts one tick, which FFmpeg needs to read it chunk by chunk (`mov_build_index`). The chunk
            // has the description of the file's last one. Its samples are sync samples, of unknown dependencies (`sdtp`),
            // and in the groups that samples no mapping covers are (`sample_groups`)
            let count = u32::try_from(samples).ok().filter(|_| sample_offset.checked_add(samples as u32).is_some()).ok_or_else(|| invalid(format!("{name} has more than 2^32 samples")))?;
            let description = x.stsc.last().map_or(1, |c| c.2);
            out.stts.push((count, 1));
            if any_stss { stss.extend((sample_offset..sample_offset + count).map(|s| s + 1)); }
            if first.sdtp.is_some() { out.sdtp.extend(std::iter::repeat_n(0, count as usize)); }
            out.stsc.push((chunk_offset.checked_add(1).ok_or_else(numbers)?, count, description));
            out.stco.push(filler.add(samples * *frame as u64, *byte)?);
            sample_offset += count;
            chunk_offset = chunk_offset.checked_add(1).ok_or_else(numbers)?;
        }
    }
    out.stsz_count = sample_offset;
    out.ctts = ctts_version.map(|v| (v, ctts));
    out.stss = any_stss.then_some(stss);
    out.stps = any_stps.then_some(stps);
    if first.has_cslg {
        out.cslg = Some(composition_bounds(&out.stts, out.ctts.as_ref(), &name)?);
    }
    let silence = plan.fills.iter().map(|f| u32::try_from(f.silence).map_err(|_| invalid(format!("{name} has more than 2^32 samples")))).collect::<Result<Vec<_>>>()?;
    (out.sample_groups, out.compact_groups) = sample_groups(&tracks, &silence, &name)?;

    let (edits, whole) = merged_edits(&plan, &offsets, media_offset, timescale, &name)?;
    // A track's duration is its edits' (ISO/IEC 14496-12 §8.3.2.3)
    out.tkhd_duration = plan.tkhd;
    if first.elst.is_some() || !whole {
        out.insert_edts = first.elst.is_none();
        out.elst = Some(edits);
    }
    Ok((out, Starts { ticks: plan.starts, exact: plan.exact, timescale }, plan.end))
}

/// The sample descriptions of the merged track (`stsd`, after its header), when they aren't the first file's. The merged
/// track describes every file's samples with the first file's sample entries, so every file's must be the same, except
/// for the picture parameter sets of an AVC or HEVC decoder configuration: the files of one recording may hold different
/// ones, each those its own samples use (Sony's do). The merged configuration holds all of them, so it decodes each file's
/// samples as the file's own does, as long as no two files hold different parameter sets with the same id
fn sample_descriptions(tracks: &[&PartTrack], name: &str) -> Result<Option<Vec<u8>>> {
    let first = tracks[0];
    let mut configs: Vec<Option<DecoderConfig>> = first.sample_entries.iter().map(|e| e.config.clone()).collect();
    for (i, x) in tracks.iter().enumerate().skip(1) {
        let differs = || refused(format!("{name} has another sample description in file {} than in file 1 ('{}' and '{}'): their codec configuration differs, and the merged track would decode every file's samples with the first file's", i + 1, formats(x), formats(first)));
        if x.sample_entries.len() != first.sample_entries.len() {
            return Err(differs());
        }
        for ((a, b), merged) in first.sample_entries.iter().zip(&x.sample_entries).zip(&mut configs) {
            if a.format != b.format || a.comparable != b.comparable {
                return Err(differs());
            }
            match (merged, &b.config) {
                (None, None) => {}
                (Some(m), Some(c)) => {
                    if m.typ != c.typ || m.header != c.header || m.trailing != c.trailing || m.arrays.len() != c.arrays.len() {
                        return Err(differs());
                    }
                    // The NAL unit type of picture parameter sets, and the largest id one can have (ITU-T H.264 §7.4.2.2, H.265 §7.4.3.3.1)
                    let (pps, max_id) = if m.typ == fourcc("avcC") { (8, 255) } else { (34, 63) };
                    // An hvcC array's array_completeness, the top bit of its first byte, says whether every NAL unit of its type is in
                    // the array and none in the samples (1), or more may be in the samples (0, ISO/IEC 14496-15 §8.3.3.1). It doesn't
                    // change how the samples decode, and it differs between recordings of one camera, so it isn't compared. The merged
                    // track has every file's samples, so it is 1 only when it is 1 in every file
                    let completeness = if m.typ == fourcc("hvcC") { 0x80 } else { 0 };
                    for ((typ, byte, nalus), (c_typ, c_byte, c_nalus)) in m.arrays.iter_mut().zip(&c.arrays) {
                        if typ != c_typ || (*byte | completeness) != (c_byte | completeness) || (*typ != pps && nalus != c_nalus) {
                            return Err(differs());
                        }
                        *byte &= c_byte | !completeness;
                        for nalu in c_nalus {
                            if *typ != pps || nalus.contains(nalu) { continue; }
                            let id = DecoderConfig::pps_id(m.typ, nalu).ok_or_else(differs)?;
                            if id > max_id {
                                return Err(invalid(format!("{name} of file {}: its decoder configuration has a picture parameter set with id {id}, and ids go up to {max_id}", i + 1)));
                            }
                            if nalus.iter().any(|n| DecoderConfig::pps_id(m.typ, n) == Some(id)) {
                                return Err(refused(format!("{name}: file {} has another picture parameter set {id} than an earlier file", i + 1)));
                            }
                            log::debug!("{name}: picture parameter set {id} of file {} added to the decoder configuration", i + 1);
                            nalus.push(nalu.clone());
                        }
                    }
                }
                _ => return Err(differs())
            }
        }
    }
    // The first file's entries, with the merged decoder configurations. Replaced from the last, so the offsets before stay
    let mut stsd = first.stsd.clone();
    let mut changed = false;
    for (entry, merged) in first.sample_entries.iter().zip(&configs).rev() {
        if let (Some(c), Some(m)) = (&entry.config, merged) {
            if c != m {
                // It counts its parameter sets of a type in 5 (avcC SPS), 8 (avcC PPS) or 16 bits (hvcC)
                let too_many = || {
                    let sets = m.arrays.iter().map(|a| a.2.len()).max().unwrap_or(0);
                    refused(format!("{name}: the files have {sets} parameter sets of one type in all, more than an {} can hold", typ_to_str(m.typ)))
                };
                let new = m.to_box().ok_or_else(too_many)?;
                let size = u32::try_from(u32::from_be_bytes(stsd[entry.at..entry.at + 4].try_into().unwrap()) as usize - (c.at.1 - c.at.0) + new.len()).map_err(|_| too_many())?;
                stsd.splice(entry.at + c.at.0..entry.at + c.at.1, new);
                stsd[entry.at..entry.at + 4].copy_from_slice(&size.to_be_bytes());
                changed = true;
            }
        }
    }
    Ok(changed.then_some(stsd))
}

/// The formats of a file's track's sample entries, for messages
fn formats(x: &PartTrack) -> String {
    x.sample_entries.iter().map(|e| typ_to_str(e.format)).collect::<Vec<_>>().join(", ")
}

/// The media timescale of the merged track: the first file's, when it holds every file's timing (its sample durations,
/// composition offsets and edit start times) exactly. Otherwise the least common multiple of the first file's and of
/// the coarsest timescale that holds each file's timing exactly, which holds them all. When that doesn't fit 32 bits, no
/// timescale holds the files' timing, and the merge is refused
fn media_timescale(tracks: &[&PartTrack], name: &str) -> Result<u32> {
    let first = tracks[0].mdhd_timescale as u64;
    let mut timescale = first;
    for (i, x) in tracks.iter().enumerate() {
        let ts = x.mdhd_timescale as u64;
        let g = x.stts.iter().map(|e| e.1 as u64)
            .chain(x.ctts.iter().flat_map(|c| c.1.iter().map(|e| e.1.unsigned_abs() as u64)))
            .chain(x.elst.iter().flatten().filter(|e| e.media_time > 0).map(|e| e.media_time as u64))
            .fold(ts, gcd);
        let coarsest = ts / g;
        timescale = timescale / gcd(timescale, coarsest) * coarsest;
        if timescale > u32::MAX as u64 {
            return Err(refused(format!("{name} has media timescale {first} in file 1 and {ts} in file {}, and no 32-bit timescale holds the timing of both exactly", i + 1)));
        }
        if ts != first {
            log::debug!("{name}: file {} has media timescale {ts}, file 1 {first}; merged timescale {timescale}", i + 1);
        }
    }
    Ok(timescale as u32)
}

/// Checks that every chunk of a file's track is in the data of the file's mdat (`mdat`: where it is in the merged mdat,
/// and its size), as its offsets (made relative to the merged mdat) and sample sizes say. The merge copies that data only.
/// A chunk without samples must start in it too. For a track whose tables are written as they are (timecode): its chunks
/// hold the samples a reader of the file finds in them, the last ones none past the last sample
fn within_mdat(mdat: (u64, u64), x: &PartTrack) -> std::result::Result<(), ()> {
    let (mut sample, mut run) = (0usize, 0usize);
    for (k, &offset) in x.stco.iter().enumerate() {
        // The samples in chunk k + 1: those of the last stsc entry whose first_chunk isn't after it
        while x.stsc.get(run + 1).is_some_and(|c| c.0 as usize <= k + 1) { run += 1; }
        let count = x.stsc.get(run).filter(|c| c.0 as usize <= k + 1).map_or(0, |c| c.1 as usize).min(x.stsz_count as usize - sample.min(x.stsz_count as usize));
        if offset < mdat.0 || offset > mdat.0 + mdat.1 {
            return Err(());
        }
        if count == 0 { continue; }
        let size: u64 = if x.stsz_sample_size != 0 { x.stsz_sample_size as u64 * count as u64 }
                        else { x.stsz.get(sample..sample + count).ok_or(())?.iter().map(|s| *s as u64).sum() };
        sample += count;
        if offset.checked_add(size).is_none_or(|e| e > mdat.0 + mdat.1) {
            return Err(());
        }
    }
    Ok(())
}

/// Checks the chunks of file `i`'s track, whose sample-to-chunk table (`stsc`, ISO/IEC 14496-12 §8.7.4) the merge
/// appends to the other files' with its chunk numbers moved: it must give every chunk the file has, from chunk 1, its
/// samples and one of the sample descriptions its `stsd` declares, and its chunks must hold every sample (`stsz`) once,
/// exactly. A table that gives the chunks more samples, or fewer, would give the excess, or what is missing, to the next
/// file's chunks. (The merged track has the first file's descriptions, which every file's equal, so an index names the
/// same one there.) Then every chunk must be in the data of the file's mdat, as `within_mdat` checks for the samples it
/// holds
fn check_chunks(mdat: (u64, u64), x: &PartTrack, name: &str, i: usize) -> Result<()> {
    let malformed = |what: String| invalid(format!("{name} of file {}: its sample-to-chunk table (stsc) {what}", i + 1));
    let descriptions = x.stsd.get(4..8).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap())); // entry_count
    let chunks = x.stco.len() as u64;
    if x.stsc.first().map_or(chunks > 0, |e| e.0 != 1) {
        return Err(malformed("doesn't start at chunk 1".into()));
    }
    let mut samples = 0u64;
    for (k, &(first, per_chunk, description)) in x.stsc.iter().enumerate() {
        // Its chunks: from its first to the next entry's, or to the last
        let next = x.stsc.get(k + 1).map_or(chunks + 1, |e| e.0 as u64);
        if first as u64 > chunks {
            return Err(malformed(format!("has an entry for chunk {first}, and the track has {chunks} chunks")));
        }
        if next <= first as u64 {
            return Err(malformed(format!("has an entry for chunk {next} after one for chunk {first}")));
        }
        if description == 0 || description > descriptions {
            return Err(malformed(format!("refers to sample description {description}, and the track has {descriptions}")));
        }
        samples += (next - first as u64) * per_chunk as u64; // At most 2^32 chunks of less than 2^32 samples in all
    }
    if samples != x.stsz_count as u64 {
        return Err(malformed(format!("gives its {chunks} chunks {samples} samples, and the track has {} (stsz)", x.stsz_count)));
    }
    if x.stsz_sample_size == 0 && x.stsz.len() != x.stsz_count as usize {
        return Err(invalid(format!("{name} of file {} has {} sample sizes for {} samples (stsz)", i + 1, x.stsz.len(), x.stsz_count)));
    }
    let outside = || refused(format!("{name} of file {} has samples outside its mdat box, and only that box is merged", i + 1));
    let end = mdat.0 + mdat.1;
    let (mut sample, mut run) = (0usize, 0usize);
    for (k, &offset) in x.stco.iter().enumerate() {
        // Chunk k + 1 is in the last entry whose first chunk isn't after it
        while x.stsc.get(run + 1).is_some_and(|c| c.0 as usize <= k + 1) { run += 1; }
        let count = x.stsc[run].1 as usize;
        if offset < mdat.0 || offset > end {
            return Err(outside());
        }
        let size: u64 = if x.stsz_sample_size != 0 { x.stsz_sample_size as u64 * count as u64 }
                        else { x.stsz[sample..sample + count].iter().map(|s| *s as u64).sum() };
        sample += count;
        if offset.checked_add(size).is_none_or(|e| e > end) {
            return Err(outside());
        }
    }
    Ok(())
}

/// The duration of a file's media: the sum of its sample durations
fn media_duration(x: &PartTrack) -> Result<u64> {
    x.stts.iter().try_fold(0u64, |sum, e| add(sum, e.0 as u64 * e.1 as u64))
}

/// Where a track's media ends in its composition times (in its media ticks), which an edit's media times are in, from
/// its samples' durations (`stts`) and composition offsets (`ctts`): the latest end of a sample's presentation, its decode
/// time plus its composition offset and duration. Without `ctts`, the end of its decode times. A sample past a short
/// `ctts` has the offset 0, as the merge gives it
fn composition_end(stts: &[(u32, u32)], ctts: Option<&(u8, Vec<(u32, i32)>)>) -> Result<i128> {
    let Some((_, offsets)) = ctts else { return Ok(stts.iter().try_fold(0u64, |sum, e| add(sum, e.0 as u64 * e.1 as u64))? as i128); };
    let mut offsets = offsets.iter().flat_map(|&(n, o)| std::iter::repeat_n(o as i128, n as usize));
    let (mut dts, mut end) = (0i128, 0i128);
    for &(n, d) in stts {
        for _ in 0..n {
            let o = offsets.next().unwrap_or(0);
            end = end.max(dts + o + d as i128);
            dts += d as i128;
        }
    }
    Ok(end)
}

/// What the merged track presents of a file: one of its edits, as merged
#[derive(Clone, Copy, Debug)]
struct Piece {
    duration: u64,   // In the movie timescale
    media_time: i64, // In the merged media timescale, from where the file's media starts in it; -1 for an empty edit
    media_rate: i32,
    // At the normal rate, where it ends in the file's media: in its ticks times `.1`, and `.1`, the timescale of its
    // duration (its precision), or the media timescale where the merge ends it exactly (`Timeline::plan`)
    end: Option<(u128, u32)>,
}

impl Piece {
    /// Whether it presents any of its file's media from `at` on (in ticks of the merged media timescale, from where the
    /// file's media starts): an edit at the normal rate that ends after it, a dwell (rate 0) on a sample from there, or an
    /// edit at another rate, whose media this doesn't follow
    fn presents_after(&self, at: u128) -> bool {
        match (self.media_time, self.end) {
            (-1, _) => false,
            (_, Some((end, den))) => end > at * den as u128,
            (media_time, None) => self.media_rate != 0 || media_time as u128 >= at
        }
    }
}

/// Whether any of a file's edits `pieces` presents its media from `at` on (`Piece::presents_after`), as edits out of media
/// order can: then its media from there may be neither dropped nor moved
fn any_presents_after(pieces: &[Piece], at: u128) -> bool {
    pieces.iter().any(|p| p.presents_after(at))
}

/// Whether an edit at the normal rate that ends at `end` (a `Piece::end`: in media ticks times `.1`, and `.1`) ends more
/// than one unit after `point` (in media ticks times the movie timescale). Saturating: an edit that isn't cut to its
/// media (`Timeline::plan`) can end near 2^97 there, and where one side saturates the answer still holds
fn ends_past((end, den): (u128, u32), point: u128, movie_timescale: u32) -> bool {
    end.saturating_mul(movie_timescale as u128) > point.saturating_add(1).saturating_mul(den as u128)
}

/// How the media fills the gap after a file: one of its samples (by its index in the file) lasts longer by so many ticks,
/// or so many samples of silence follow the file's (PCM)
#[derive(Clone, Copy, Debug, Default)]
struct Fill {
    lengthen: Option<(u32, u64)>,
    silence: u64,
}

/// How the merged edits present the gap before a file: an empty edit, at the track's start before any of its media (its
/// duration in the movie timescale); or the last edit, presenting the fill after the media it ends with for longer (in
/// the movie timescale, and the fill's ticks)
#[derive(Clone, Copy, Debug)]
enum Gap {
    None,
    Empty(u64),
    Fill(u64, u64),
}

/// Where the merged track presents each file
struct Plan {
    pieces: Vec<Vec<Piece>>, // What each file presents
    gaps: Vec<Gap>,          // Before each file
    fills: Vec<Fill>,        // After each file
    // How much shorter each file's media is in the merged track than in the file, in ticks of the merged media timescale:
    // by what its last sample lost where it lasts only up to the next recording (`Timeline::shorten`)
    shortened: Vec<u64>,
    tkhd: u64,               // The track's duration, in the movie timescale
    // Where each file's presentation starts, before its leading empty edits: in the movie timescale, as the edits' durations
    // add up; and exactly, where the merged edits present it, in ticks of the movie timescale times the media timescale
    starts: Vec<u64>,
    exact: Vec<u128>,
    // How long it presents the files, in the movie timescale: up to where it presents the end of each file's media, as
    // far as the file's own movie presents it (`file_end`); in metadata, whose files follow each other's samples, after
    // the previous files' media as far as theirs present it
    end: u64,
}

impl Plan {
    /// Whether an edit of file `j` other than its last presents its media from `at` on (`any_presents_after`)
    fn others_present_after(&self, j: usize, at: u128) -> bool {
        self.pieces[j].split_last().is_some_and(|(_, others)| any_presents_after(others, at))
    }
}

/// How a track's files line up in the merged movie (`Timeline::plan`)
#[derive(Clone, Copy, Debug)]
enum Role<'a> {
    /// The first video track (`anchors`): each file's picture follows the previous file's, and where it ends is where the
    /// next file starts after another recording (`Plan::starts`)
    Anchor,
    /// Another picture or sound track: after another recording, a file's starts where its file does, at these starts
    Aligned(&'a Starts),
    /// A metadata, data or text track, or one whose samples last no time: each file's samples follow the previous file's
    Continuous,
}

/// How a gap in a sound track whose every sample lasts one tick is filled. FFmpeg reads such a track chunk by chunk, and
/// only then (`mov_build_index`, one `stts` entry of duration 1): with one sample made longer it indexes every sample on
/// its own, a packet each, millions of them in a long recording. So none of its samples is made longer
enum Pcm {
    /// Not such a track: a gap makes a sample longer
    No,
    /// PCM: silence fills a gap, in frames of so many bytes of one byte
    Silence { frame: u32, byte: u8 },
    /// Such a track that silence can't fill, and why: a gap in it is left out
    Unfilled(String),
}

/// Where the merged track's presentation has arrived, file by file, as `Timeline::plan` plans it and `merged_edits` will
/// present it
#[derive(Clone, Copy, Debug, Default)]
struct Arrival {
    pos: u64,                   // In the movie timescale: the durations of the edits so far
    offset: u128,               // Where the file's media starts in the merged media
    owner: Option<usize>,       // The last file that presents its media, with a piece of the normal rate as its last
    // The merged edit the last piece is in, when it is one of media at the normal rate: where it starts, in ticks of the
    // movie timescale times the media timescale, which hold it exactly, and in the merged media
    edit: Option<(u128, u128)>,
    end: Option<(u128, u32)>,   // Where that piece ends in the merged media, in its ticks times `.1`, and `.1`
}

impl Arrival {
    /// Where the merged edits present the media at `media` in the merged media when a piece of the normal rate that
    /// starts there continues the last edit, as `merged_edits` decides (to within one tick of the timescale of that edit's
    /// duration): exactly, in ticks of the movie timescale times the media timescale. A sample's place there is its place
    /// in the media, not the sum of the edits' durations, which are rounded to the movie timescale
    fn presents(&self, media: u128, timescale: u32, movie_timescale: u32) -> Option<u128> {
        let ((end, den), (start, from)) = (self.end?, self.edit?);
        (end.abs_diff(media * den as u128) < timescale as u128 && media >= from).then(|| start + (media - from) * movie_timescale as u128)
    }

    /// Follows a piece that is presented from `piece_at` (in the movie timescale), as `merged_edits` merges it
    fn push(&mut self, piece: &Piece, piece_at: u64, timescale: u32, movie_timescale: u32) {
        if piece.media_time == -1 || piece.media_rate != MEDIA_RATE_1 {
            (self.edit, self.end) = (None, None);
            return;
        }
        let media = self.offset + piece.media_time as u128;
        if self.presents(media, timescale, movie_timescale).is_none() {
            self.edit = Some((piece_at as u128 * timescale as u128, media));
        }
        self.end = piece.end.map(|(end, den)| (end + self.offset * den as u128, den));
    }
}

/// How a file's edit ends where it runs past where the next recording starts in the track, or the file's media does
/// (`Timeline::ending`)
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// Where the next file's sound continues this file's media: with a sample as the file's last (`Timeline::keeps_last`)
    Last(Last),
    /// At the start of its sample `kept`, where the file's media then ends (`end`, in ticks of the merged timescale from
    /// the file's media start): the samples from there on are left out (`Timeline::sound_cut`)
    Cut { kept: u32, end: u64 },
    /// Up to the tick nearest the next file's picture, where it runs past it: keeping the sample that tick falls in
    /// (`Timeline::keeps`), at `.0` in the file's media (in media ticks times the movie timescale, from the file's media
    /// start), or where no sample decides it (an edit that isn't the file's last of media, a track that isn't sound or
    /// whose samples have composition offsets or last no time, a point within the edit's first sample, or samples after
    /// the cut that another edit of the file presents)
    Tick(Option<u128>),
}

/// The sample of a file's sound that `Timeline::keeps_last` keeps as the file's last
#[derive(Clone, Copy, Debug)]
struct Last {
    kept: u32,          // Its index in the file
    ends: u64,          // Where it ends then, in ticks of the merged timescale from the file's media start
    lasts: Option<u32>, // How long it lasts then, in those ticks, where it runs past the point: less than its own duration
    at_point: bool,     // Whether it ends on the tick of the media nearest the point
}

/// A track of every file, as `merge_track` plans where the merged track presents each (`plan`, `close_gap`)
struct Timeline<'a> {
    movie: &'a Movie<'a>,
    tracks: Vec<Cow<'a, PartTrack>>, // Each file's track; of a file whose sound `plan` cuts, the samples it keeps (`head`)
    // Their sample durations, in `timescale`, as the merged track gives them: also of a sample `plan` shortens (`shorten`),
    // which the file's track keeps as it is, with its media duration (`Plan::shortened`)
    scaled: Vec<Vec<(u32, u32)>>,
    timescale: u32,                  // The merged track's media timescale
    role: Role<'a>,
    pcm: Pcm,
    name: String,
}

impl Timeline<'_> {
    /// Where the merged track presents each file, with what it presented on its own (its edits; a file without an edit
    /// list presents its media as it is, ISO/IEC 14496-12 §8.6.6), so that every file's tracks stay as they are against
    /// each other:
    /// - **After a chapter of its own recording** (`Movie::continues`), a file's track follows that chapter's track: the
    ///   camera split each of the recording's streams where its file ends, so the next file's samples follow the previous
    ///   file's.
    /// - **After another recording,** the picture tells where a file starts. The first video track follows the previous
    ///   file's picture (`Role::Anchor`), so it presents every frame for as long as its file does, and the file starts
    ///   where that picture ends (`Plan::starts`; after a file without one, where that file's own tracks end,
    ///   `Movie::ends`). Every other picture and sound track starts with its file there (`Role::Aligned`): exactly where
    ///   it continues the previous file's edit, and otherwise at the nearest tick of the movie timescale. So each file's
    ///   sound keeps its sync with its picture, as when the files play one after the other: a track that ends before the
    ///   next file starts is followed by a gap, and one that runs past it is cut there. Metadata follows the previous
    ///   file's (`Role::Continuous`), as the picture does: its readers time its samples by their durations and order, and
    ///   telemetry-parser spreads a sample's readings over the sample's duration and averages the durations, so a sample
    ///   made longer would move every reading, the earlier files' too. The formats whose readings it times by their
    ///   samples last as long as their picture (Sony's `rtmd`, one sample a frame), so they stay with it as they did on
    ///   their own, and up to 0.1.11.
    /// - **A later file's leading empty edits** (a track that starts after its file does) are a gap before it too, but in
    ///   metadata, which its readers time by its samples alone: there they're left out.
    ///
    /// Sound that runs past where the next file's sound starts, also by less than a tick of the movie timescale, is cut
    /// there, at the start of its last sample that starts by then (`sound_cut`): no sample is presented in part, which
    /// FFmpeg can't do at the end of an edit (it presents a sample an edit ends in as a whole, `mov_fix_index`, so the cut
    /// sample's rest would play over the next file's), and what is left to the next file's start, under a sample, is a gap
    /// like any other, which that sample fills. So the next file's sound starts where its picture does, in every player:
    /// exactly where it continues the cut sound's edit, and otherwise on the nearest tick of the movie timescale. The
    /// samples after the cut are left out of the tables (`head`), so the merged edit continues into the next file's, and
    /// FFmpeg reads PCM up to the cut, chunk by chunk. The file's edits after the one that runs past that point come after
    /// the next file's start: they are left out, also where the cut ends that one before the point (an empty one would be
    /// an empty edit in the middle of the track, below). Coded sound keeps the sample the next file's start falls in where
    /// the cut would take more of the file's own sound within its picture than keeping it plays over the next file's
    /// (`ending`):
    /// - where the next file's sound continues this file's last edit of media (its first edit of media, after any leading
    ///   empty ones, presents its media from the start, and no edit of this file comes between: that edit is the file's
    ///   last, or it runs past the point), the kept sample is the file's last, lasting only up to where the next file's
    ///   picture starts, to the nearest tick of the media, and the next file's sound continues the edit there
    ///   (`keeps_last`). Where the file's edit ends before that point and hides its media after it, the sample kept or cut
    ///   is the one its presentation ends in, and the file's later media is left out. So the next file's sound starts
    ///   where it belongs at every junction, not later at each, and the merged sound is one edit, which a stream copy keeps
    ///   as it is (FFmpeg starts each later edit of sound with up to a second of the media before it, which it presents as
    ///   discarded, and a stream copy as sound);
    /// - otherwise, its edit is cut on the tick nearest the next file's picture, where the next file's sound starts, in
    ///   its own edit (`keeps`).
    ///
    /// A file with another edit that presents some of those samples (edits out of media order, as in a reference movie)
    /// keeps them all, and its edit is cut where the next file starts.
    ///
    /// A file's edit doesn't present past the file's own media, into the next file's: on its own, a file presents nothing
    /// there, so an edit runs past its media only as a placeholder: Canon's metadata edit of 1000 s, or an audio edit a
    /// camera gives the video's duration (RunCam, whose audio ends up to 93 ms before or 81 ms after its video). Such an
    /// edit that doesn't continue into the next file's first edit (to within one tick of the timescale of its duration) is
    /// cut where the file's media ends (rounded down, so it never reaches the next file's first sample). Otherwise the next
    /// file's first samples are presented twice: FFmpeg's mov demuxer presents every sample an edit's media range covers
    /// (`mov_fix_index`). What the placeholder covered is then the gap after the file, or, after a chapter, nothing.
    ///
    /// A gap is never an empty edit in the middle of the track, which FFmpeg presents as the track's first samples (it
    /// handles only leading ones). The media fills it, as joined outputs and a muxer writing a timestamp gap do
    /// (`close_gap`):
    /// - the file's last sample lasts longer: its last in decoding order, when its edit presents its media to the end; the
    ///   one its edit ends in otherwise, which only a track without composition offsets can have lengthened (each later
    ///   sample moves with it, so none may be one that another edit of the file presents);
    /// - in PCM, silence follows the file's samples, in a chunk of its own (`Pcm`);
    /// - before the track's first media, it is an empty edit, as a file's own leading one.
    ///
    /// The edit that presents the file's media to the end then presents the fill too, and continues into the next file's.
    /// Also gives the track's duration (`tkhd`, the sum of its edits, §8.3.2.3): every file's own, but for a file whose edits
    /// were cut, the smaller of its own and its edits now (Canon's metadata track lasts 22 s, its edit 1000 s), with what the
    /// merge made its last edit present longer than the file's own (the sample it keeps as its last, `keeps_last`), and
    /// with the gaps
    fn plan(&mut self) -> Result<Plan> {
        let movie = self.movie;
        let (parts, movie_timescale, timescale, n) = (movie.parts, movie.timescale, self.timescale, movie.parts.len());
        let durations = self.scaled.iter().map(|s| s.iter().try_fold(0u64, |sum, e| add(sum, e.0 as u64 * e.1 as u64))).collect::<Result<Vec<_>>>()?;
        let mut plan = Plan { pieces: Vec::with_capacity(n), gaps: vec![Gap::None; n], fills: vec![Fill::default(); n], shortened: vec![0; n], tkhd: 0, starts: vec![0; n], exact: vec![0; n], end: 0 };
        let mut at = Arrival::default();
        let mut presented = false; // The previous file presents media
        // One of `starts` in this track's ticks of the movie timescale times its media timescale
        let target = |starts: &Starts, i: usize| starts.exact.get(i).and_then(|e| e.checked_mul(timescale as u128)).map(|e| (e + starts.timescale as u128 / 2) / starts.timescale as u128);
        // And to the nearest tick of the movie timescale: where a file whose track doesn't continue the previous file's edit
        // starts, and the previous file's presentation ends. `Starts::ticks` add up the picture's edits, each rounded up to
        // the movie timescale, while its frames follow each other exactly, so a file started there would be late by the
        // rounding of every edit before it
        let nearest = |starts: &Starts, i: usize| target(starts, i).and_then(|t| u64::try_from((t + timescale as u128 / 2) / timescale as u128).ok()).unwrap_or(starts.ticks[i]);
        for (i, p) in parts.iter().enumerate() {
            let x = &self.tracks[i];
            let (file_edits, edit_timescale) = match &x.elst {
                Some(e) => (e.clone(), p.mvhd_timescale),
                None => (vec![Edit { segment_duration: media_duration(x)?, media_time: 0, media_rate: MEDIA_RATE_1 }], x.mdhd_timescale)
            };
            let lead = if i == 0 { 0 } else { file_edits.iter().take_while(|e| e.media_time == -1).count() };
            let lead_duration = file_edits[..lead].iter().try_fold(0u64, |sum, e| add(sum, rescale_up(e.segment_duration, edit_timescale, movie_timescale)?))?;
            // Where the file's first edit of media starts in its media, in the merged timescale
            let first_media = file_edits[lead..].iter().find(|e| e.media_time != -1).and_then(|e| u64::try_from(e.media_time).ok())
                .and_then(|m| rescale_exact(m, x.mdhd_timescale, timescale)).map(|m| m as u128);
            if i > 0 {
                let separate = !movie.continues[i - 1];
                // Where the file's own presentation starts: where its own recording's track has arrived; after another
                // recording, where the file starts
                let start = match self.role {
                    Role::Anchor if separate && !presented => at.pos.max(add(plan.starts[i - 1], movie.ends[i - 1])?),
                    Role::Aligned(starts) if separate => nearest(starts, i).max(at.pos),
                    _ => at.pos
                };
                // Exactly: where the merged edits present the start of the file's media when it follows the previous file's
                // in one edit (as a picture does), or else where its edits start
                let exact = match self.role {
                    Role::Anchor if separate && !presented => (start as u128 * timescale as u128).max(plan.exact[i - 1] + movie.ends[i - 1] as u128 * timescale as u128),
                    _ => first_media.and_then(|m| at.presents(at.offset + m, timescale, movie_timescale)).unwrap_or(at.pos as u128 * timescale as u128)
                };
                (plan.starts[i], plan.exact[i]) = (start, exact);
                // Its leading empty edits, which can't be later than its file's end (`Movie::ends`): a track that starts
                // later presents nothing of the file
                let lead_duration = match self.role {
                    Role::Continuous => lead_duration,
                    _ => lead_duration.min(movie.ends[i])
                };
                let gap = add(start, lead_duration)? - at.pos;
                // After another recording, where the file's first sample belongs exactly: where its file starts in the
                // anchor's presentation and its leading empty edits end
                let belongs = match self.role {
                    Role::Aligned(starts) if separate => target(starts, i).zip(first_media).map(|(t, m)| (t + lead_duration as u128 * timescale as u128, at.offset + m)),
                    _ => None
                };
                let (shown, length) = self.close_gap(&mut plan, &mut at, gap, i, belongs)?;
                plan.gaps[i] = shown;
                at.pos = add(at.pos, length)?;
                plan.tkhd = add(plan.tkhd, length)?;
            }
            let x = &self.tracks[i];
            // Where the track presents the file from, after the gap before it
            let shown_from = at.pos;
            // When a file follows: where its first edit starts in the media, from where this file's starts, if this file's
            // last can continue into it (it presents media at the normal rate), and where this file's media ends in its
            // composition times. Its first edit, not its first of media (`next_from_start`): after a leading empty edit, a
            // gap that `close_gap` fills, this file's edit doesn't run on into the next file's media, so one that runs past
            // its own media is a placeholder
            let next = self.tracks.get(i + 1).map(|y| match y.elst.as_deref() {
                None => Some(durations[i]),
                Some(e) => e.first().filter(|e| e.media_time >= 0 && e.media_rate == MEDIA_RATE_1)
                    .and_then(|e| rescale_exact(e.media_time as u64, y.mdhd_timescale, timescale)).and_then(|m| m.checked_add(durations[i]))
            });
            let own_end = match next {
                Some(_) => u64::try_from(composition_end(&x.stts, x.ctts.as_ref())?).ok().and_then(|c| rescale_exact(c, x.mdhd_timescale, timescale)),
                None => None
            };
            // The next file has no edit list, or its first edit of media (after its leading empty edits, a gap before it)
            // presents its media from the start at the normal rate: its media follows this file's in the merged media, so
            // its sound can continue this file's edit
            let next_from_start = self.tracks.get(i + 1).is_some_and(|y| y.elst.as_deref().is_none_or(|e| {
                e.iter().find(|e| e.media_time != -1).is_some_and(|e| e.media_time == 0 && e.media_rate == MEDIA_RATE_1)
            }));
            // Before another recording: what's left of the file's own presentation, which ends where the next file starts;
            // and in ticks of the movie timescale times the media timescale, where the next file's picture starts (where
            // this file's ends), the tick of the movie timescale nearest it, and where this file's sound ends: exactly
            // where that picture starts, when the next file's sound continues this file's edit (its media ends there,
            // `keeps_last`), and otherwise on that tick, where the next file's own edit starts
            let (mut room, next_start) = match self.role {
                Role::Aligned(starts) if i + 1 < n && !movie.continues[i] => {
                    let tick = nearest(starts, i + 1);
                    let picture = target(starts, i + 1);
                    let sound = if next_from_start { picture } else { Some(tick as u128 * timescale as u128) };
                    (Some(tick.saturating_sub(at.pos)), Some((picture, tick as u128 * timescale as u128, sound)))
                }
                _ => (None, None)
            };
            let last = file_edits.iter().rposition(|e| e.media_time != -1);
            let (mut pieces, mut changed, mut length) = (Vec::new(), lead > 0, 0u64);
            // The samples the file keeps (`truncate`), how long its last one then lasts where that is less than its own
            // (`shorten`), and how much longer than the file's own its last edit then is (`keeps_last`)
            let (mut cut, mut lasts, mut grown, mut has_media) = (None, None, 0u64, false);
            for (k, e) in file_edits.iter().enumerate().skip(lead) {
                let duration = rescale_up(e.segment_duration, edit_timescale, movie_timescale)?;
                let mut piece = Piece { duration, media_time: -1, media_rate: e.media_rate, end: None };
                // Where it starts: in the movie timescale, and where the merged edits present it, exactly
                let piece_at = add(at.pos, length)?;
                let mut shown_at = piece_at as u128 * timescale as u128;
                if e.media_time != -1 {
                    piece.media_time = u64::try_from(e.media_time).ok()
                        .and_then(|m| rescale_exact(m, x.mdhd_timescale, timescale))
                        .and_then(|m| i64::try_from(m).ok())
                        .ok_or_else(|| invalid(format!("{} of file {} has an edit with media time {}", self.name, i + 1, e.media_time)))?;
                    if e.media_rate == MEDIA_RATE_1 {
                        shown_at = at.presents(at.offset + piece.media_time as u128, timescale, movie_timescale).unwrap_or(shown_at);
                    }
                    let mut edit_end = piece.media_time as u128 * edit_timescale as u128 + e.segment_duration as u128 * timescale as u128;
                    if let (Some(own_end), true) = (own_end, e.media_rate == MEDIA_RATE_1) {
                        let continues_next = Some(k) == last && next.flatten().is_some_and(|s| edit_end.abs_diff(s as u128 * edit_timescale as u128) < timescale as u128);
                        if edit_end > own_end as u128 * edit_timescale as u128 && !continues_next {
                            // Past the file's media: cut where it ends
                            let kept = u64::try_from((own_end.saturating_sub(piece.media_time as u64) as u128 * movie_timescale as u128) / timescale as u128).unwrap_or(u64::MAX).min(duration);
                            log::debug!("{}: an edit of file {} presents {} ticks past the file's media, which the next file's samples would fill; cut there", self.name, i + 1, edit_end / edit_timescale as u128 - own_end as u128);
                            changed = true;
                            piece.duration = kept;
                            edit_end = own_end as u128 * edit_timescale as u128;
                        }
                    }
                    piece.end = (e.media_rate == MEDIA_RATE_1).then_some((edit_end, edit_timescale));
                }
                if let Some(room) = room.as_mut() {
                    // In media ticks times the movie timescale, from the file's media start: where the merged edits present
                    // a point of the presentation (in ticks of the movie timescale times the media timescale) in this edit
                    let media = u128::try_from(piece.media_time).ok().map(|m| m * movie_timescale as u128);
                    let within = |at: u128| media.and_then(|m| m.checked_add(at.saturating_sub(shown_at)));
                    // Where the next file's sound starts, for the file's last edit of media
                    let point = match next_start {
                        Some((_, _, sound)) if Some(k) == last => sound.map_or(media.and_then(|m| m.checked_add(*room as u128 * timescale as u128)), within),
                        _ => None
                    };
                    // Whether that edit presents media past it exactly, by more than the point's precision (a tick of the
                    // movie timescale times the media timescale): its duration in the movie timescale doesn't tell when
                    // it is by less than a tick
                    let ends_after = match (point, piece.end) {
                        (Some(point), Some(end)) => ends_past(end, point, movie_timescale),
                        _ => false
                    };
                    // Whether the next file's sound continues that edit: its media follows this file's, and no edit of the
                    // file comes between (it is the file's last edit, or it runs past the point: the file's later edits
                    // then come after the next file's start, and get no room, below). And then, whether the file's media
                    // runs past the point by more than half a tick of the media, also where the edit hides it: the next
                    // file's sound would start that much late, and later at every such junction
                    let next_continues = next_from_start && point.is_some() && piece.end.is_some() && (k + 1 == file_edits.len() || ends_after);
                    let past = ends_after || (next_continues && point.zip(own_end).is_some_and(|(point, m)| {
                        (m as u128).saturating_mul(movie_timescale as u128) > point.saturating_add(movie_timescale as u128 / 2)
                    }));
                    if piece.duration > *room || past {
                        // Past the file's own presentation, into the next file's: it ends there (`ending`), the last edit of
                        // media of sound at a sample boundary, whose later samples are left out (`head`)
                        let on_tick = next_start.and_then(|(picture, tick, _)| within(tick).zip(picture.and_then(within)));
                        let before = piece.duration;
                        match self.ending(i, &piece, &pieces, point, next_continues, on_tick) {
                            Ending::Last(Last { kept, ends, lasts: shorter, at_point }) => {
                                log::debug!("{}: the sound of file {} keeps its sample {kept}, which its presentation ends in, as its last", self.name, i + 1);
                                changed = true;
                                // Its samples after it are left out, and it lasts up to the point, when it runs past it: the
                                // next file's media, which follows this file's, then starts there, in the same edit
                                if kept + 1 < x.stsz_count { cut = Some(kept + 1); }
                                lasts = shorter;
                                // Up to the tick nearest the next file's picture when it ends at the point, and otherwise
                                // rounded down, and the gap to the next file is filled. Where the file's edit ends before the
                                // point, in that sample, the edit then presents more than the file's own, and the track's
                                // duration has that too
                                piece.duration = if at_point {
                                    *room
                                } else {
                                    u64::try_from((ends as u128 - piece.media_time as u128) * movie_timescale as u128 / timescale as u128).unwrap_or(u64::MAX)
                                };
                                piece.end = Some((ends as u128 * timescale as u128, timescale));
                                grown = piece.duration.saturating_sub(before);
                            }
                            Ending::Cut { kept, end } => {
                                log::debug!("{}: the sound of file {} is cut at its sample {kept}, where the next file starts", self.name, i + 1);
                                changed = true;
                                // Rounded down: the gap to the next file is filled
                                piece.duration = u64::try_from((end as u128 - piece.media_time as u128) * movie_timescale as u128 / timescale as u128).unwrap_or(u64::MAX);
                                piece.end = Some((end as u128 * movie_timescale as u128, movie_timescale));
                                if kept < x.stsz_count { cut = Some(kept); }
                            }
                            Ending::Tick(tick) if piece.duration > *room => {
                                match tick {
                                    Some(_) => log::debug!("{}: the sound of file {} keeps its sample the next file starts in, and its edit is cut there", self.name, i + 1),
                                    None => log::debug!("{}: an edit of file {} presents {} ticks past the file's end; cut there", self.name, i + 1, piece.duration - *room)
                                }
                                changed = true;
                                piece.duration = *room;
                                // Exactly, in ticks of the media timescale, when it keeps its sample: the next file's sound,
                                // whose media follows this file's, starts a new edit on the tick, however near this file's
                                // media ends after it
                                let exact = tick.and_then(|at| at.checked_mul(timescale as u128)).map(|at| ((at + movie_timescale as u128 / 2) / movie_timescale as u128, timescale));
                                piece.end = piece.end.and(exact.or(media.map(|m| (m + *room as u128 * timescale as u128, movie_timescale))));
                            }
                            // Past it by less than a tick of the movie timescale, and no sample boundary takes that off, or it
                            // keeps its sample, and ends by the tick: it stays
                            Ending::Tick(tick) => if tick.is_some() {
                                log::debug!("{}: the sound of file {} keeps its sample the next file starts in", self.name, i + 1);
                            }
                        }
                    }
                    // What's left for the file's later edits: nothing once its last edit of media runs past the point, also
                    // where the edit was cut before it, as they come after the next file's start then
                    *room = if ends_after { 0 } else { room.saturating_sub(piece.duration) };
                }
                if piece.duration == 0 && duration > 0 { continue; } // It presents only media after the file's, or after the file's end
                length = add(length, piece.duration)?;
                if piece.media_time != -1 && x.stsz_count > 0 {
                    at.owner = Some(i);
                    has_media = true;
                }
                at.push(&piece, piece_at, timescale, movie_timescale);
                pieces.push(piece);
            }
            at.pos = add(at.pos, length)?;
            // How far the track presents the file, which its own movie presents up to its end (`Movie::ends`), from where
            // it presents its media: in picture and sound, where the merged edits do; in metadata, which leaves out a gap
            // and follows the previous file's samples, after what the track presents of the previous files
            let from = if matches!(self.role, Role::Continuous) { plan.end } else { shown_from };
            plan.end = plan.end.max(add(from, length.min(movie.ends[i].saturating_sub(lead_duration)))?);
            // Its own duration of the track (without the leading empty edits that are now a gap), or for a file whose edits
            // changed, the smaller of that and what they present now within the file's own, with what its last edit now
            // presents past its own end
            let own = rescale_up(x.tkhd_duration, p.mvhd_timescale, movie_timescale)?.saturating_sub(lead_duration);
            plan.tkhd = add(plan.tkhd, if changed && i + 1 < n { add(own.min(length.saturating_sub(grown)), grown)? } else { own })?;
            plan.pieces.push(pieces);
            presented = has_media;
            if let Some(kept) = cut { self.truncate(i, kept); }
            if let Some(duration) = lasts { plan.shortened[i] = self.shorten(i, duration); }
            at.offset += self.scaled[i].iter().map(|e| e.0 as u128 * e.1 as u128).sum::<u128>();
        }
        Ok(plan)
    }

    /// Fills a gap of `gap` ticks of the movie timescale before file `file` in the track planned so far (`plan`, and where
    /// its presentation has arrived, `at`). Returns how the edits present it, and for how long (0 when it's left out: then
    /// that file's samples come that much early). After another recording, `belongs` is where that file's first sample
    /// belongs exactly (in ticks of the movie timescale times the media timescale), and where it is in the merged media:
    /// when it follows the last edit's media in that edit, the media fills the gap up to that point exactly, rather than to
    /// a tick of the movie timescale. In metadata a gap is left out by design (`plan`); in picture and sound, where the
    /// media can't fill it, with a warning:
    /// - after an edit at another rate;
    /// - after an edit that ends before its media does, in a track with composition offsets, or when another edit of the
    ///   file presents the media after it (edits out of media order), which the fill would move or take the place of;
    /// - in a sound track whose samples each last one tick that isn't PCM the merge knows the silence of (`Pcm`);
    /// - when the sample made longer wouldn't fit its 32-bit duration (13 hours at 90 kHz), or the track's samples or chunks
    ///   with the silence wouldn't number in 32 bits.
    ///
    /// In PCM whose edit ends before its media does, the samples it doesn't present are left out (`head`), and the silence
    /// follows the others
    fn close_gap(&mut self, plan: &mut Plan, at: &mut Arrival, gap: u64, file: usize, belongs: Option<(u128, u128)>) -> Result<(Gap, u64)> {
        let (timescale, movie_timescale) = (self.timescale, self.movie.timescale);
        // The media's ticks up to where the file's first sample belongs, exactly
        let exact = belongs.and_then(|(t, m)| at.presents(m, timescale, movie_timescale).map(|shown| (t.saturating_sub(shown) + movie_timescale as u128 / 2) / movie_timescale as u128));
        if gap == 0 && exact.is_none_or(|e| e == 0) {
            return Ok((Gap::None, 0));
        }
        let Some(j) = at.owner else {
            // The track's media starts later
            if gap == 0 {
                return Ok((Gap::None, 0));
            }
            (at.edit, at.end) = (None, None);
            return Ok((Gap::Empty(gap), gap));
        };
        let name = self.name.clone();
        if matches!(self.role, Role::Continuous) {
            log::debug!("{name}: a gap of {gap} ticks of the movie timescale before file {} is left out: the files' metadata follow each other", file + 1);
            return Ok((Gap::None, 0));
        }
        let left_out = |why: &str| {
            log::warn!("{name}: a gap of {:.3} ms before file {} is left out, so that file's samples come that much early: {why}", gap as f64 * 1000.0 / movie_timescale as f64, file + 1);
            Ok((Gap::None, 0))
        };
        // The edit before the gap: the last of the files planned so far, which is the owner's (a file after it presents no
        // media)
        let last = plan.pieces.iter().flatten().last().copied();
        let Some(Piece { media_rate: MEDIA_RATE_1, end: Some((end, den)), .. }) = last else {
            return left_out("the edit before it isn't one of media at the normal rate");
        };
        // Its ticks in the media, and their duration in the movie timescale, rounded to the nearest
        let ticks = exact.unwrap_or((gap as u128 * timescale as u128 + movie_timescale as u128 / 2) / movie_timescale as u128);
        let Some(ticks) = u64::try_from(ticks).ok().filter(|&t| t > 0) else {
            log::debug!("{name}: a gap of {gap} ticks before file {} is left out: it is under half a tick of the media", file + 1);
            return Ok((Gap::None, 0));
        };
        let length = u64::try_from((ticks as u128 * movie_timescale as u128 + timescale as u128 / 2) / timescale as u128).map_err(|_| invalid(format!("{name}: a gap of {ticks} ticks doesn't fit 64 bits")))?;
        // Whether that edit presents the owner's media to its end
        let to_the_end = end.abs_diff(self.media_end(j)? as u128 * den as u128) < timescale as u128;
        if let Pcm::Unfilled(why) = &self.pcm {
            return left_out(&format!("its samples each last one tick, which FFmpeg needs to read them chunk by chunk, so none is made longer, and silence can't fill it: {why}"));
        }
        if matches!(self.pcm, Pcm::Silence { .. }) {
            if !to_the_end {
                // The samples the edit doesn't present are left out, from the one nearest where it ends (each lasts a
                // tick), so the silence follows those it presents: only when no other edit of the file presents them, as
                // edits out of media order can, which would then present the silence and the next file's samples
                let kept = u32::try_from((end + den as u128 / 2) / den as u128).unwrap_or(u32::MAX).min(self.tracks[j].stsz_count);
                if plan.others_present_after(j, kept as u128) {
                    return left_out(&format!("another edit of file {} presents its samples after where its last edit ends, which silence can't follow then", j + 1));
                }
                log::debug!("{name}: file {} presents its PCM up to sample {kept}, so its samples after that are left out and the silence follows", j + 1);
                let before = self.scaled[j].iter().map(|e| e.0 as u128 * e.1 as u128).sum::<u128>();
                self.truncate(j, kept);
                at.offset -= before - self.scaled[j].iter().map(|e| e.0 as u128 * e.1 as u128).sum::<u128>();
                if let Some(piece) = plan.pieces.iter_mut().flatten().last() {
                    piece.end = Some((kept as u128 * den as u128, den));
                }
                at.end = at.end.map(|(e, d)| (e - end + kept as u128 * den as u128, d));
            }
            // The track's samples and chunks then, which are numbered in 32 bits
            let samples = self.tracks.iter().map(|x| x.stsz_count as u64).sum::<u64>()
                .saturating_add(plan.fills.iter().map(|f| f.silence).sum::<u64>()).saturating_add(ticks);
            let chunks = self.tracks.iter().map(|x| x.stco.len() as u64).sum::<u64>() + plan.fills.iter().filter(|f| f.silence > 0).count() as u64 + 1;
            if samples > u32::MAX as u64 || chunks > u32::MAX as u64 {
                return left_out("the track's samples or chunks wouldn't number in 32 bits with its silence");
            }
            plan.fills[j].silence += ticks;
        } else {
            let sample = if to_the_end {
                self.tracks[j].stsz_count - 1
            } else if self.tracks[j].ctts.is_none() {
                // The last sample that starts before the edit ends, and where it ends. Every later sample moves with it, so
                // no other edit of the file may present one, as edits out of media order can
                let (mut dts, mut first, mut found) = (0u128, 0u32, None);
                for &(count, delta) in &self.scaled[j] {
                    if count > 0 && dts * (den as u128) < end {
                        let k = if delta == 0 { count - 1 } else { u32::try_from((end - dts * den as u128 - 1) / (delta as u128 * den as u128)).unwrap_or(u32::MAX).min(count - 1) };
                        found = Some((first + k, dts + (k as u128 + 1) * delta as u128));
                    }
                    dts += count as u128 * delta as u128;
                    first += count;
                }
                match found {
                    None => return left_out("its edit ends before the file's first sample"),
                    Some((_, ends)) if plan.others_present_after(j, ends) => {
                        return left_out(&format!("another edit of file {} presents its samples after the one that would last longer, which would move them", j + 1));
                    }
                    Some((s, _)) => s
                }
            } else {
                return left_out("its edit hides the end of a track with composition offsets");
            };
            let fill = &mut plan.fills[j];
            let longer = match fill.lengthen {
                None => ticks,
                Some((s, t)) if s == sample => t.saturating_add(ticks),
                Some(_) => return left_out("another sample of the file presents a gap already")
            };
            // Its duration then: 32 bits hold 13 hours at 90 kHz
            let delta = self.duration_of(j, sample);
            if (delta as u64).saturating_add(longer) > u32::MAX as u64 {
                return left_out("it doesn't fit a sample's 32-bit duration");
            }
            fill.lengthen = Some((sample, longer));
        }
        // The edit presents the fill after the media it ends with (`merged_edits`)
        at.end = at.end.map(|(end, den)| (end + ticks as u128 * den as u128, den));
        at.offset += ticks as u128;
        Ok((Gap::Fill(length, ticks), length))
    }

    /// Where file `j`'s media ends in its composition times, in ticks of the merged media timescale: without composition
    /// offsets, where its samples end as the merged track times them (`scaled`, with a sample `plan` shortened)
    fn media_end(&self, j: usize) -> Result<u64> {
        let x = &self.tracks[j];
        if x.ctts.is_none() {
            return Ok(u64::try_from(self.scaled[j].iter().map(|e| e.0 as u128 * e.1 as u128).sum::<u128>()).unwrap_or(u64::MAX));
        }
        Ok(u64::try_from(composition_end(&x.stts, x.ctts.as_ref())?).ok().and_then(|c| rescale_exact(c, x.mdhd_timescale, self.timescale)).unwrap_or(u64::MAX))
    }

    /// How file `i`'s edit `piece`, after its edits `pieces`, ends where it runs past where the next recording starts in the
    /// track, or the file's media does (`plan`): for its last edit of media, where the next file's sound starts, `point`
    /// (in media ticks times the movie timescale, from the file's media start). Sound is cut at the start of a sample
    /// (`sound_cut`): the one the point falls in, or where the next file's sound continues this file's media
    /// (`next_continues`), the one the file's presentation ends in, at the point or where the edit ends before it. Coded
    /// sound keeps that sample instead where that leaves out less of the file's own sound: as the file's last, where the
    /// next file's sound continues its media (`keeps_last`), and otherwise with the edit cut on the tick nearest the next
    /// file's picture (`keeps`), which `on_tick` gives with where that picture starts, in the file's media. The samples
    /// left out must be none that another edit of the file presents, as edits out of media order can
    fn ending(&self, i: usize, piece: &Piece, pieces: &[Piece], point: Option<u128>, next_continues: bool, on_tick: Option<(u128, u128)>) -> Ending {
        let Some(point) = point else { return Ending::Tick(None); };
        let by = match piece.end {
            Some((end, den)) if next_continues => point.min(end.saturating_mul(self.movie.timescale as u128) / den as u128),
            _ => point
        };
        let Some(cut @ (kept, end)) = self.sound_cut(i, piece, by).filter(|&(_, end)| !any_presents_after(pieces, end as u128)) else {
            return Ending::Tick(None);
        };
        if next_continues {
            return self.keeps_last(i, piece, cut, point).map_or(Ending::Cut { kept, end }, Ending::Last);
        }
        match on_tick {
            Some((tick, picture)) if self.keeps(i, piece, cut, point, tick, picture) => Ending::Tick(Some(tick)),
            _ => Ending::Cut { kept, end }
        }
    }

    /// Where file `i`'s sound is cut when the next recording starts at `point` in its last edit, `piece` (in media ticks
    /// times the movie timescale, from the file's media start): at the start of its last sample that starts by then, of
    /// those the edit presents up to. Returns the samples kept and where they end in the media (in the merged timescale,
    /// from the file's start); `None` for a track that isn't sound, or whose samples have composition offsets or last no
    /// time, and when the point is within the edit's first sample: then the edit is cut where it is instead
    fn sound_cut(&self, i: usize, piece: &Piece, point: u128) -> Option<(u32, u64)> {
        let x = &self.tracks[i];
        let (end, den) = piece.end?;
        if self.tracks[0].handler != fourcc("soun") || x.ctts.is_some() || piece.media_time < 0 || self.scaled[i].iter().any(|e| e.0 > 0 && e.1 == 0) {
            return None;
        }
        let (movie, from, at) = (self.movie.timescale as u128, piece.media_time as u128, point);
        let (mut t, mut s, mut cut) = (0u128, 0u32, None); // Where sample `s` starts; the last boundary by the point
        for &(count, delta) in &self.scaled[i] {
            if count == 0 { continue; }
            let d = delta as u128;
            // The run's last sample boundary t + m·d (m = 1..=count) by the point
            let m = (at.saturating_sub(t * movie) / (d * movie)).min(count as u128);
            let b = t + m * d;
            if m > 0 && b > from && b * den as u128 <= end { cut = Some((s + m as u32, b as u64)); }
            if m < count as u128 { break; }
            t += count as u128 * d;
            s += count;
        }
        cut
    }

    /// Whether file `i`'s sound keeps the sample the next recording's sound start `point` falls in, rather than being cut
    /// at its start (`sound_cut`: the samples before it, and where it starts), where its last edit `piece` presents past
    /// the point and the next file's sound starts its own edit. Coded sound keeps it when, kept, it plays no more of the
    /// file's own sound over the next file's than the cut would leave out within the file's picture, which ends at
    /// `picture`. Kept, the edit is cut on the tick nearest the next file's picture (`tick`), where the next file's sound
    /// then starts, so it must still present some of the sample there. All in media ticks times the movie timescale, from
    /// the file's media start.
    ///
    /// The cut leaves out the sample's sound from its start, the end of the file's own sound within its picture (up to an
    /// AAC frame: 21 ms at 48 kHz), and the sample before it lasts longer: a gap in every player. Kept, the sample is
    /// presented up to the tick by a player that cuts a sample at the end of an edit (GStreamer's qtdemux), while FFmpeg
    /// presents it whole (`mov_fix_index`), as do players that don't: what the edit presents of it past the tick plays
    /// over the next file's start, and so does the rest the file's own edit hides, as FFmpeg plays it at the end of the
    /// file on its own. FFmpeg's concat demuxer keeps every packet that starts before its out point likewise. So the cut
    /// is made only where the file's own sound it keeps from playing over the next file's is more than the sound it takes
    /// from within the file's picture: the smaller loss either way, and never more than half the sample of the file's own
    /// sound. Where the file's sound runs on past its picture, the samples wholly after the tick are left out of the
    /// presentation either way. PCM, whose samples each last a tick, is always cut at a sample, exactly, and loses nothing
    /// it presents within its picture. Where the next file's sound continues this file's media, `keeps_last` decides
    fn keeps(&self, i: usize, piece: &Piece, (kept, start): (u32, u64), point: u128, tick: u128, picture: u128) -> bool {
        let (Pcm::No, Some((end, den))) = (&self.pcm, piece.end) else { return false; };
        let movie = self.movie.timescale as u128;
        let (from, to) = (start as u128 * movie, (start as u128 + self.duration_of(i, kept) as u128) * movie);
        // Where the edit ends; what the kept sample plays of the file's own sound past the tick, and what the cut would
        // leave out of it before the next file's picture
        let end = end.saturating_mul(movie) / den as u128;
        let (over, lost) = (end.min(to).saturating_sub(tick), end.min(picture).saturating_sub(from));
        from < point && point < to && from < tick && over <= lost
    }

    /// Where the next recording's sound continues file `i`'s media, which runs past where that sound starts, `point`:
    /// whether file `i`'s sound keeps the sample its presentation ends in there, at the point or where its last edit
    /// `piece` ends before it, rather than being cut at its start (`sound_cut`: the samples before it, and where it starts),
    /// and then where it ends (`Last`). The point is in media ticks times the movie timescale, from the file's media start.
    ///
    /// Coded sound keeps it when the file presents some of it before the point, and, kept, it plays no more of the file's
    /// own sound past the point than the cut would leave out (as `keeps`). Kept, it is the file's last sample (its later
    /// ones, which the edit hides or which run past the point, are left out), and where it runs past the point it lasts
    /// only up to it, to the nearest tick, so the next file's media, which follows it, starts where the next file's sound
    /// belongs, in the same edit: at every such junction, and not late by what this file's edit hid of its media. FFmpeg
    /// writes a stream cut within a frame so (its concat demuxer and `mov_write_stts_tag`: the frame kept whole, lasting
    /// up to the next one). Players present the whole frame, its rest over the next file's start, as FFmpeg presents a
    /// frame an edit ends in (`mov_fix_index`), and the merged file has no edit there that FFmpeg would start with a second
    /// of the media before it, which a stream copy keeps as sound. PCM, whose samples each last a tick, is always cut at a
    /// sample, exactly
    fn keeps_last(&self, i: usize, piece: &Piece, (kept, start): (u32, u64), point: u128) -> Option<Last> {
        let (Pcm::No, Some((end, den))) = (&self.pcm, piece.end) else { return None; };
        let movie = self.movie.timescale as u128;
        let to = start.checked_add(self.duration_of(i, kept) as u64)?;
        let (from, end) = (start as u128 * movie, end.saturating_mul(movie) / den as u128);
        // What the kept sample plays of the file's own sound past the point, and what the cut would leave out before it
        let (over, lost) = (end.min(to as u128 * movie).saturating_sub(point), end.min(point).saturating_sub(from));
        // Where it ends: at the point, to the nearest tick, when it runs past it. It ends at the point when it ends on that
        // tick, as also one does that ends less than half a tick before the point
        let nearest = (point + movie / 2) / movie;
        let ends = if to as u128 * movie > point { u64::try_from(nearest).ok()? } else { to };
        (lost > 0 && over <= lost && ends > start).then(|| Last { kept, ends, lasts: (ends < to).then(|| (ends - start) as u32), at_point: ends as u128 == nearest })
    }

    /// The duration of file `i`'s sample `s` in the merged timescale (0 past its last)
    fn duration_of(&self, i: usize, s: u32) -> u32 {
        self.scaled[i].iter().scan(0u32, |first, &(count, delta)| { let at = *first; *first = first.saturating_add(count); Some((at, count, delta)) })
            .find(|&(at, count, _)| (at..at.saturating_add(count)).contains(&s)).map_or(0, |e| e.2)
    }

    /// File `i`'s last sample, which `keeps_last` keeps as its last (after `truncate`), lasts `duration` ticks of the
    /// merged timescale, less than its own: in the merged track's sample durations (`scaled`), which the edits, the gap
    /// after the file and the next file's media follow. Returns by how much less its media then lasts, in those ticks,
    /// exactly (its own timescale may not hold that), by which the merged track's media duration (`mdhd`) has less of the
    /// file's own (`Plan::shortened`)
    fn shorten(&mut self, i: usize, duration: u32) -> u64 {
        let runs = &mut self.scaled[i];
        let Some(k) = runs.iter().rposition(|e| e.0 > 0) else {
            debug_assert!(false, "{}: file {} has no sample to shorten", self.name, i + 1);
            return 0;
        };
        let (count, delta) = runs[k];
        runs.splice(k..=k, [(count - 1, delta), (1, duration)].into_iter().filter(|e| e.0 > 0));
        delta.saturating_sub(duration) as u64
    }

    /// Keeps only file `i`'s first `kept` samples (`head`)
    fn truncate(&mut self, i: usize, kept: u32) {
        self.tracks[i] = Cow::Owned(head(&self.tracks[i], kept));
        let mut left = kept;
        self.scaled[i].retain_mut(|e| {
            e.0 = e.0.min(left);
            left -= e.0;
            e.0 > 0
        });
    }
}

/// File's track `x` with only its first `kept` samples, in decoding order (`Timeline::sound_cut`): its tables describe
/// those, its chunks end with them, its media duration is theirs, and its sample groups map them. Its other samples stay
/// in the merged mdat, where no table refers to them
fn head(x: &PartTrack, kept: u32) -> PartTrack {
    // The first `kept` of runs of samples
    fn runs<T: Copy>(runs: &[(u32, T)], kept: u32) -> Vec<(u32, T)> {
        let mut left = kept;
        runs.iter().filter_map(|&(count, v)| {
            let count = count.min(left);
            left -= count;
            (count > 0).then_some((count, v))
        }).collect()
    }
    let dropped = x.stts.iter().scan(kept, |left, &(count, delta)| {
        let past = count - count.min(*left);
        *left -= count - past;
        Some(past as u64 * delta as u64)
    }).fold(0u64, u64::saturating_add);
    // Its chunks up to the one with the last sample kept, which holds those it has of them
    let (mut stsc, mut stco) = (Vec::new(), Vec::new());
    let (mut sample, mut run) = (0u64, 0usize);
    for (k, &offset) in x.stco.iter().enumerate() {
        if sample >= kept as u64 { break; }
        while x.stsc.get(run + 1).is_some_and(|c| c.0 as usize <= k + 1) { run += 1; }
        let Some(&(_, per_chunk, description)) = x.stsc.get(run) else { break; };
        let n = (per_chunk as u64).min(kept as u64 - sample) as u32;
        if stsc.last().is_none_or(|&(_, count, d)| count != n || d != description) {
            stsc.push((k as u32 + 1, n, description));
        }
        stco.push(offset);
        sample += per_chunk as u64;
    }
    PartTrack {
        handler: x.handler,
        tkhd_duration: x.tkhd_duration,
        has_edts: x.has_edts,
        elst: x.elst.clone(),
        mdhd_timescale: x.mdhd_timescale,
        mdhd_duration: x.mdhd_duration.saturating_sub(dropped),
        stsd: x.stsd.clone(),
        sample_entries: x.sample_entries.clone(),
        stts: runs(&x.stts, kept),
        ctts: x.ctts.as_ref().map(|(v, c)| (*v, runs(c, kept))),
        stss: x.stss.as_ref().map(|s| s.iter().copied().filter(|&s| s <= kept).collect()),
        stps: x.stps.as_ref().map(|s| s.iter().copied().filter(|&s| s <= kept).collect()),
        stsz_sample_size: x.stsz_sample_size,
        stsz_count: kept,
        stsz: x.stsz.iter().copied().take(kept as usize).collect(),
        stsc,
        stco,
        sdtp: x.sdtp.as_ref().map(|s| s.iter().copied().take(kept as usize).collect()),
        sbgp: x.sbgp.iter().map(|g| SampleToGroup { version: g.version, grouping_type: g.grouping_type, grouping_type_parameter: g.grouping_type_parameter, entries: runs(&g.entries, kept) }).collect(),
        csgm: x.csgm.iter().map(|c| {
            let mut left = kept;
            let patterns = c.patterns.iter().filter_map(|(count, indices)| {
                let count = (*count).min(left);
                left -= count;
                (count > 0).then(|| (count, indices.clone()))
            }).collect();
            CompactToGroup { grouping_type: c.grouping_type, patterns }
        }).collect(),
        sgpd: x.sgpd.clone(),
        has_cslg: x.has_cslg,
        encrypted: x.encrypted,
        unmerged: x.unmerged.clone(),
        skip: x.skip,
    }
}

/// The edit list of the merged track, from where it presents each file (`plan`) and where each file's media starts in
/// the merged media (`offsets`). An edit that continues the one before it in the media, to within one tick of the
/// timescale of that one's duration (its precision), extends it, so the files of one recording keep one edit.
///
/// Also says whether the edits present the whole merged media (which ends at `media_end`), as a track without an edit list
/// does: one edit, from its start at the normal rate, that ends where it does, to within one tick of the timescale of
/// the last file's duration. An edit that ends earlier hides the end of the last file's media, and a later one shows the
/// last sample longer
fn merged_edits(plan: &Plan, offsets: &[u64], media_end: u64, timescale: u32, name: &str) -> Result<(Vec<Edit>, bool)> {
    let mut edits: Vec<Edit> = Vec::new();
    let mut end: Option<(u128, u32)> = None; // Where the last edit ends in the media, in media ticks times the timescale of its duration, and that timescale
    fn empty(edits: &mut Vec<Edit>, segment_duration: u64, media_rate: i32) -> Result<()> {
        match edits.last_mut() {
            Some(last) if last.media_time == -1 => last.segment_duration = add(last.segment_duration, segment_duration)?,
            _ => edits.push(Edit { segment_duration, media_time: -1, media_rate })
        }
        Ok(())
    }
    for (i, pieces) in plan.pieces.iter().enumerate() {
        match plan.gaps[i] {
            Gap::None => {}
            Gap::Empty(duration) => {
                empty(&mut edits, duration, MEDIA_RATE_1)?;
                end = None;
            }
            Gap::Fill(duration, ticks) => {
                if let Some(last) = edits.last_mut() { last.segment_duration = add(last.segment_duration, duration)?; }
                end = end.map(|(end, ts)| (end + ticks as u128 * ts as u128, ts));
            }
        }
        for piece in pieces {
            if piece.media_time == -1 {
                empty(&mut edits, piece.duration, piece.media_rate)?;
                end = None;
                continue;
            }
            let media_time = i64::try_from(offsets[i] as i128 + piece.media_time as i128).map_err(|_| invalid(format!("{name} of file {} has an edit past 2^63 ticks", i + 1)))?;
            let continues = piece.media_rate == MEDIA_RATE_1 && end.is_some_and(|(end, ts)| end.abs_diff(media_time as u128 * ts as u128) < timescale as u128);
            match edits.last_mut() {
                Some(last) if continues => last.segment_duration = add(last.segment_duration, piece.duration)?,
                _ => edits.push(Edit { segment_duration: piece.duration, media_time, media_rate: piece.media_rate })
            }
            end = piece.end.map(|(end, ts)| (end + offsets[i] as u128 * ts as u128, ts));
        }
    }
    let whole = matches!(edits.as_slice(), [e] if e.media_time == 0 && e.media_rate == MEDIA_RATE_1)
        && end.is_some_and(|(end, ts)| end.abs_diff(media_end as u128 * ts as u128) < timescale as u128);
    Ok((edits, whole))
}

/// How a gap in a sound track whose every sample lasts one tick in the merged track is filled (`Pcm`): with silence when
/// it is PCM whose silence the merge can write and whose tables it can extend by the silence's samples. That takes:
/// - sample entries that describe PCM frames the same way (`pcm_entry`), and one sample size (`stsz`) for every sample of
///   every file. FFmpeg reads such a track in frames of the size the sample entry gives (`mov_parse_stsd_audio`), which
///   `mov_read_stsz` doesn't overwrite, whatever the sample size is: the frame's, QuickTime's legacy 1, or one channel's
///   sample, as Atomos writes `in24`. Where the entry doesn't give it the frame's size (`pcm_entry`: `ipcm` and `fpcm`,
///   whose coding it knows from their `pcmC` after the entry, an `lpcm` of a coding it has no codec for, frames of 2^31
///   bytes or more), it reads frames of the sample size, which must be the frame's then;
/// - no composition offsets, and no per-sample box the merge doesn't rebuild (`PartTrack::unmerged`). Sync samples,
///   dependencies (`sdtp`) and sample groups are extended: the silence's samples are sync samples, of unknown
///   dependencies, in the groups that samples no mapping covers are (`sample_groups`)
fn pcm(tracks: &[&PartTrack], scaled: &[Vec<(u32, u32)>]) -> Pcm {
    let first = tracks[0];
    let mut runs = scaled.iter().flatten().filter(|e| e.0 > 0).peekable();
    if first.handler != fourcc("soun") || runs.peek().is_none() || !runs.all(|e| e.1 == 1) {
        return Pcm::No;
    }
    for (i, x) in tracks.iter().enumerate() {
        if x.ctts.is_some() {
            return Pcm::Unfilled(format!("file {} gives its samples composition offsets (ctts)", i + 1));
        }
        if let Some(typ) = x.unmerged.first() {
            return Pcm::Unfilled(format!("file {} has a '{}' box, which the merge doesn't extend", i + 1, typ_to_str(*typ)));
        }
        if x.stsz_sample_size == 0 {
            return Pcm::Unfilled(format!("file {} lists its samples' sizes one by one", i + 1));
        }
        if x.stsz_sample_size != first.stsz_sample_size {
            return Pcm::Unfilled(format!("its samples have {} bytes in file 1 and {} in file {}", first.stsz_sample_size, x.stsz_sample_size, i + 1));
        }
    }
    // Every file's sample entries are the first file's (`sample_descriptions`)
    let mut common = None;
    for entry in &first.sample_entries {
        let e = first.stsd.get(entry.at..).unwrap_or_default();
        let size = e.get(..4).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()) as usize);
        match pcm_entry(first.stsd.first().copied().unwrap_or(0), entry.format, e.get(..size).unwrap_or(e)) {
            Err(why) => return Pcm::Unfilled(why),
            Ok(c) if common.is_some_and(|common: PcmFrames| (common.frame, common.byte) != (c.frame, c.byte)) => return Pcm::Unfilled("its sample entries describe other PCM frames".into()),
            Ok(c) if common.is_some_and(|common: PcmFrames| common.by_entry != c.by_entry) => {
                return Pcm::Unfilled("FFmpeg reads the frames of some of its sample entries by the entry, and of others in the samples' size".into());
            }
            Ok(c) => common = Some(c)
        }
    }
    match common {
        None => Pcm::Unfilled("it has no sample entry".into()),
        Some(PcmFrames { frame, by_entry: false, .. }) if first.stsz_sample_size != frame => {
            Pcm::Unfilled(format!("its samples of {} bytes aren't its PCM frames of {frame} bytes, and FFmpeg reads the frames of its coding in the samples' size", first.stsz_sample_size))
        }
        Some(PcmFrames { frame, byte, .. }) => Pcm::Silence { frame, byte }
    }
}

/// The PCM frames a sound sample entry describes (`pcm_entry`)
#[derive(Clone, Copy, PartialEq)]
struct PcmFrames {
    frame: u32,     // Its bytes
    byte: u8,       // The byte silence is
    by_entry: bool, // Whether FFmpeg reads the frames by the entry, rather than in the size `stsz` gives
}

/// The bytes of a PCM frame, and the byte silence is, of the sound sample entry `e` (with its box header) of type `format`,
/// in an `stsd` of version `stsd_version`. Read as FFmpeg reads them (`mov_parse_stsd_audio`, `mov_read_pcmc`):
/// a frame is the channels times the sample size, which `in24`, `in32`, `fl32`, `fl64` and `raw ` fix, and the
/// `pcmC` box of ISO/IEC 23003-5's `ipcm` and `fpcm` gives. QuickTime's version 1 sound description (samplesPerPacket,
/// bytesPerFrame) or version 2 one (constLPCMFramesPerAudioPacket, constBytesPerAudioPacket) must declare packets of one
/// such frame; an ISO `stsd` of version 1 holds ISO's AudioSampleEntryV1 instead, whose channels a ChannelLayout box may
/// give, which isn't read. FFmpeg must read samples of that size: those of the codec the format names
/// (`ff_codec_movaudio_tags`), which the entry's sample size changes for `twos`, `sowt`, `NONE`, `raw ` and an `lpcm` of
/// version 0 or 1 (`mov_parse_stsd_audio`), and which the flags of an `lpcm` of version 2 give (`ff_mov_get_lpcm_codec_id`).
/// Silence is 0 for two's complement and floating point samples, and 0x80 for 8-bit offset binary (`raw `, an 8-bit
/// `NONE`, and an `lpcm` whose version 2 flags say neither float nor signed). Also says whether FFmpeg reads the frames
/// by the entry: not those of `ipcm` and `fpcm`, whose coding it knows only from their `pcmC`, after the entry
/// (`mov_read_pcmc`), nor of an `lpcm` it has no codec for, nor frames of 2^31 bytes or more, which it reads in the size
/// `stsz` gives. `Err` says why the entry isn't PCM whose silence the merge writes: another coding, or a malformed
/// description
fn pcm_entry(stsd_version: u8, format: u32, e: &[u8]) -> std::result::Result<PcmFrames, String> {
    let f = format.to_be_bytes();
    if !matches!(&f, b"twos" | b"sowt" | b"lpcm" | b"NONE" | b"raw " | b"in24" | b"in32" | b"fl32" | b"fl64" | b"ipcm" | b"fpcm") {
        return Err(format!("its samples are '{}', not PCM the merge writes the silence of", typ_to_str(format)));
    }
    let malformed = |what: String| format!("its PCM sample entry '{}' is malformed: {what}", typ_to_str(format));
    let short = || malformed("it's too short for its fields".into());
    let u16_at = |at: usize| e.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as u32).ok_or_else(short);
    let u32_at = |at: usize| e.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or_else(short);
    // The box header, reserved, data_reference_index, then version, revision, vendor (ISO: reserved), channels, sample size
    let version = u16_at(16)?;
    let (mut channels, mut bits) = (u16_at(24)?, u16_at(26)?);
    let (mut packet, mut flags) = (None, None); // Frames and bytes a packet declares
    let boxes = match version {
        0 => 36,
        1 if stsd_version == 0 => {
            packet = Some((u32_at(36)?, u32_at(44)?));
            52
        }
        2 if stsd_version == 0 => {
            // sizeOfStructOnly, audioSampleRate, numAudioChannels, always7F000000, constBitsPerChannel,
            // formatSpecificFlags, constBytesPerAudioPacket, constLPCMFramesPerAudioPacket
            (channels, bits, flags) = (u32_at(48)?, u32_at(56)?, Some(u32_at(60)?));
            packet = Some((u32_at(68)?, u32_at(64)?));
            72
        }
        v => return Err(format!("its sample entry has version {v} in an stsd of version {stsd_version}, whose channels aren't read"))
    };
    let entry_bits = bits; // The entry's sample size
    let bits = match &f {
        b"in24" => 24,
        b"in32" | b"fl32" => 32,
        b"fl64" => 64,
        b"raw " => 8,
        b"ipcm" | b"fpcm" => {
            // Its pcmC: version and flags (0), format_flags, PCM_sample_size
            let mut at = boxes;
            let config = loop {
                let Some(size) = e.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize) else { break None; };
                if size < 8 || at + size > e.len() { break None; }
                if &e[at + 4..at + 8] == b"pcmC" { break e.get(at + 8..at + size); }
                at += size;
            };
            match config {
                Some([0, 0, 0, 0, _, size, ..]) => match (&f, *size as u32) {
                    (b"ipcm", size @ (16 | 24 | 32)) | (b"fpcm", size @ (32 | 64)) => size,
                    (_, size) => return Err(malformed(format!("its pcmC gives samples of {size} bits"))),
                },
                Some(_) => return Err(malformed("its pcmC isn't of version 0".into())),
                None => return Err(malformed("it has no pcmC".into()))
            }
        }
        _ => bits
    };
    if channels == 0 {
        return Err(malformed("it has no channels".into()));
    }
    if bits == 0 || bits % 8 != 0 {
        return Err(malformed(format!("its samples of {bits} bits aren't whole bytes")));
    }
    let frame = (bits / 8).checked_mul(channels).ok_or_else(|| malformed(format!("a frame of {channels} channels × {bits} bits is more than 2^32 bytes")))?;
    match packet {
        Some((frames, _)) if frames != 1 => return Err(malformed(format!("it declares packets of {frames} frames"))),
        Some((_, bytes)) if bytes != frame => return Err(malformed(format!("it declares frames of {bytes} bytes, and {channels} channels × {bits} bits make {frame}"))),
        _ => {}
    }
    // The sample size FFmpeg reads, when it reads its frames by the entry
    let ffmpeg_bits = match (&f, flags) {
        (b"twos" | b"sowt", _) | (b"lpcm", None) => Some(if matches!(entry_bits, 8 | 24 | 32) { entry_bits } else { 16 }),
        (b"lpcm", Some(flags)) => match (flags & 1 != 0, flags & 4 != 0, entry_bits.div_ceil(8)) {
            (true, _, _) => matches!(entry_bits, 32 | 64).then_some(entry_bits),
            (false, true, bytes @ (1..=4 | 8)) | (false, false, bytes @ 1..=4) if entry_bits <= 64 => Some(bytes * 8),
            _ => None
        },
        (b"NONE" | b"raw ", _) => Some(if entry_bits == 16 { 16 } else { 8 }),
        (b"ipcm" | b"fpcm", _) => None,
        _ => Some(bits)
    };
    let by_entry = match ffmpeg_bits {
        Some(read) if read != bits => return Err(format!("FFmpeg reads its '{}' samples of {bits} bits as {read}-bit ones", typ_to_str(format))),
        Some(_) => frame <= i32::MAX as u32,
        None => false
    };
    let silence = match (&f, flags) {
        (b"raw ", _) => 0x80,
        (b"NONE", _) if bits == 8 => 0x80,
        (b"lpcm", Some(flags)) if flags & 1 == 0 && flags & 4 == 0 => {
            if bits == 8 { 0x80 } else { return Err(format!("its unsigned samples of {bits} bits have no silence of one repeated byte")); }
        }
        _ => 0
    };
    Ok(PcmFrames { frame, byte: silence, by_entry })
}

/// The sample-to-group mappings of the merged track, one for each `sbgp` and each `csgm` of the first file's (the merged
/// track keeps its `sgpd` boxes, and has its mappings where the first file has its, in their forms). Each file's samples
/// keep their groups: a sample is in the group its file's mapping gives it, or, when no mapping covers it, in the default
/// entry of its file's description of the type (`sgpd` version 2 or later, ISO/IEC 14496-12 §8.9.2.3, §8.9.3.3), or in
/// none. So:
/// - a mapping of the first file's maps every file's samples, those a file leaves to its default explicitly. A file that
///   puts samples in groups must describe the type as the first file does, so that the indices mean the same. A file may
///   map a grouping without a parameter in the other form: an `sbgp`'s runs are a `csgm`'s patterns of one index each,
///   and a `csgm`'s patterns are runs of the samples that take each index;
/// - where the merged track has no mapping (a type, or a grouping, that the first file doesn't map), every merged sample
///   is in the first file's default entry. When it has one, every file's samples must be in it: identical defaults stay
///   implicit. When it has none, a later file's groups of the type are left out, with a warning, unless its description
///   marks them essential (version 3), which can't be left out.
///
/// The samples of silence that follow a file's in a PCM track (`silence`) are samples no mapping covers, in their file's
/// default entry. A compact mapping that isn't merged (ISO's `csgp`, or a `csgm` in a form not known) keeps the first
/// file's, and a later file's may map samples of any type
fn sample_groups(tracks: &[&PartTrack], silence: &[u32], name: &str) -> Result<(Vec<SampleToGroup>, Vec<CompactToGroup>)> {
    fn description(x: &PartTrack, grouping_type: u32) -> Option<&Vec<u8>> { x.sgpd.iter().find(|d| d.0 == grouping_type).map(|d| &d.1) }
    // A file's mapping of a grouping, as runs of samples of one index: its `sbgp`, or for a grouping without a parameter,
    // its `csgm`'s patterns, as far as they map the file's samples
    fn mapping(x: &PartTrack, grouping_type: u32, parameter: Option<u32>) -> Option<Vec<(u32, u32)>> {
        if let Some(s) = x.sbgp.iter().find(|s| s.grouping_type == grouping_type && s.grouping_type_parameter == parameter) {
            return Some(s.entries.clone());
        }
        let c = x.csgm.iter().find(|c| c.grouping_type == grouping_type).filter(|_| parameter.is_none())?;
        let mut runs: Vec<(u32, u32)> = Vec::new();
        let mut left = x.stsz_count;
        for (count, indices) in &c.patterns {
            let count = (*count).min(left);
            left -= count;
            for k in 0..count {
                let index = indices[k as usize % indices.len()];
                match runs.last_mut() {
                    Some(last) if last.1 == index => last.0 += 1,
                    _ => runs.push((1, index))
                }
            }
        }
        Some(runs)
    }
    let first = tracks[0];
    // The entry of the samples of file i that no mapping of the type covers
    let default = |i: usize, grouping_type: u32| {
        let d = description(tracks[i], grouping_type);
        default_entry(d).ok_or_else(|| refused(format!("{name}: the default entry of the sample group description of type '{}' (sgpd version {}) of file {} can't be read",
            typ_to_str(grouping_type), d.map_or(0, |d| d[0]), i + 1)))
    };
    let compact = |x: &PartTrack| x.unmerged.contains(&fourcc("csgp")) || x.unmerged.contains(&fourcc("csgm"));
    // The samples of file i a mapping leaves (`left`), with its silence
    let with_silence = |left: u32, i: usize| left.checked_add(silence[i]).ok_or_else(|| invalid(format!("{name} has more than 2^32 samples")));
    for (i, x) in tracks.iter().enumerate().skip(1) {
        if compact(x) && x.sgpd.iter().any(|d| d.1[0] == 3) {
            return Err(refused(format!("{name}: file {} maps samples to groups in a compact sample-to-group box (csgp or csgm) that isn't merged, and has essential sample groups (sgpd version 3)", i + 1)));
        }
    }
    let mut groups = Vec::with_capacity(first.sbgp.len());
    for g in &first.sbgp {
        let typ = typ_to_str(g.grouping_type);
        let mut entries: Vec<(u32, u32)> = Vec::new();
        let mut push = |count: u32, index: u32| {
            match entries.last_mut() {
                Some(last) if last.1 == index => last.0 += count,
                _ if count > 0 => entries.push((count, index)),
                _ => {}
            }
        };
        for (i, x) in tracks.iter().enumerate() {
            let own = mapping(x, g.grouping_type, g.grouping_type_parameter);
            let mut grouped = false; // It puts samples in groups
            let mut left = x.stsz_count;
            for &(count, index) in own.iter().flatten() {
                let count = count.min(left);
                grouped |= count > 0 && index != 0;
                push(count, index);
                left -= count;
            }
            let left = with_silence(left, i)?;
            if left > 0 {
                let index = default(i, g.grouping_type)?;
                if index != 0 && own.is_none() && compact(x) {
                    return Err(refused(format!("{name}: file {} may map samples to sample groups of type '{typ}' in a compact sample-to-group box (csgp or csgm) that isn't merged, and its description of the type has a default entry", i + 1)));
                }
                grouped |= index != 0;
                push(left, index);
            }
            if grouped && description(x, g.grouping_type) != description(first, g.grouping_type) {
                return Err(refused(format!("{name} has other sample group descriptions of type '{typ}' in file {} than in file 1", i + 1)));
            }
        }
        groups.push(SampleToGroup { entries, ..g.clone() });
    }
    let mut compact_groups = Vec::with_capacity(first.csgm.len());
    for c in &first.csgm {
        let (t, typ) = (c.grouping_type, typ_to_str(c.grouping_type));
        let mut patterns: Vec<(u32, Vec<u32>)> = Vec::new();
        let wide = |i: usize| refused(format!("{name}: file {} puts samples in a sample group description of type '{typ}' whose index doesn't fit the 4 bits of the first file's compact mapping (csgm)", i + 1));
        for (i, x) in tracks.iter().enumerate() {
            // Its patterns, or runs of one index each
            let own: Option<Vec<(u32, Vec<u32>)>> = match x.csgm.iter().find(|o| o.grouping_type == t) {
                Some(o) => Some(o.patterns.clone()),
                None => mapping(x, t, None).map(|runs| runs.into_iter().map(|(count, index)| (count, vec![index])).collect())
            };
            let mut grouped = false; // It puts samples in groups
            let mut left = x.stsz_count;
            for (count, mut indices) in own.iter().flatten().cloned() {
                // A pattern maps its first `count` samples, which take its first indices
                let count = count.min(left);
                if count == 0 { continue; }
                indices.truncate(count as usize);
                if indices.iter().any(|&index| index > 15) { return Err(wide(i)); }
                grouped |= indices.iter().any(|&index| index != 0);
                patterns.push((count, indices));
                left -= count;
            }
            let left = with_silence(left, i)?;
            if left > 0 {
                let index = default(i, t)?;
                if index != 0 && own.is_none() && compact(x) {
                    return Err(refused(format!("{name}: file {} may map samples to sample groups of type '{typ}' in a compact sample-to-group box (csgp or csgm) that isn't merged, and its description of the type has a default entry", i + 1)));
                }
                if index > 15 { return Err(wide(i)); }
                grouped |= index != 0;
                match patterns.last_mut() {
                    Some((count, indices)) if indices[..] == [index] => *count += left,
                    _ => patterns.push((left, vec![index]))
                }
            }
            if grouped && description(x, t) != description(first, t) {
                return Err(refused(format!("{name} has other sample group descriptions of type '{typ}' in file {} than in file 1", i + 1)));
            }
        }
        compact_groups.push(CompactToGroup { grouping_type: t, patterns });
    }
    // The groupings the merged track doesn't map: a later file's mappings unlike any of the first file's, and, for a type
    // the first file maps none of, the samples its description leaves to its default entry
    let mut types: Vec<u32> = Vec::new();
    for t in tracks.iter().flat_map(|x| x.sgpd.iter().map(|d| d.0).chain(x.sbgp.iter().map(|s| s.grouping_type)).chain(x.csgm.iter().map(|c| c.grouping_type))) {
        if !types.contains(&t) { types.push(t); }
    }
    for t in types {
        let typ = typ_to_str(t);
        let mapped = |p: Option<u32>| first.sbgp.iter().any(|g| g.grouping_type == t && g.grouping_type_parameter == p) || (p.is_none() && first.csgm.iter().any(|c| c.grouping_type == t));
        let mut groupings: Vec<Option<u32>> = tracks.iter().flat_map(|x| x.sbgp.iter().filter(|s| s.grouping_type == t).map(|s| s.grouping_type_parameter)
            .chain(x.csgm.iter().filter(|c| c.grouping_type == t).map(|_| None))).collect();
        if !mapped(None) && !first.sbgp.iter().any(|g| g.grouping_type == t) { groupings.push(None); }
        groupings.retain(|&p| !mapped(p));
        groupings.sort_unstable();
        groupings.dedup();
        for p in groupings {
            for (i, x) in tracks.iter().enumerate().skip(1) {
                let own = mapping(x, t, p);
                if own.is_none() && !compact(x) && description(x, t) == description(first, t) {
                    continue; // Its samples are in the default entry of the same description, or in no group, as the first file's
                }
                // The merged track puts its samples in the first file's default entry
                let merged = default(0, t)?;
                let (mut other, mut grouped) = (false, false); // Some are in another entry; some are in a group
                let mut left = x.stsz_count;
                for &(count, index) in own.iter().flatten() {
                    let count = count.min(left);
                    if count > 0 { other |= index != merged; grouped |= index != 0; }
                    left -= count;
                }
                if with_silence(left, i)? > 0 {
                    let index = default(i, t)?;
                    other |= index != merged;
                    grouped |= index != 0;
                }
                if merged != 0 {
                    if other || compact(x) || description(x, t) != description(first, t) {
                        return Err(refused(format!("{name}: the samples of file {} would be in the default entry of file 1's sample group description of type '{typ}', which they aren't in", i + 1)));
                    }
                } else if grouped {
                    if description(x, t).is_some_and(|d| d[0] == 3) {
                        return Err(refused(format!("{name}: samples of file {} are in sample groups of type '{typ}' that file 1 doesn't map, and its description marks them essential (sgpd version 3)", i + 1)));
                    }
                    log::warn!("{name}: file 1 has no sample groups of type '{typ}', so the samples of file {} are in none either", i + 1);
                }
            }
        }
    }
    Ok((groups, compact_groups))
}

/// The default entry of a sample group description (`sgpd`, after its box header): `default_group_description_index` of
/// version 2 or later (ISO/IEC 14496-12 §8.9.3.3), the entry of the samples no mapping covers, 0 for none; 0 for earlier
/// versions and without a description (§8.9.3.3: "or else with no group"). Version 2 has two layouts: the 2012 (Amd. 3)
/// and 2015 editions put the index where version 1 has `default_length`, with entries that have no
/// `description_length`; since the 2022 edition `default_length` is coded from version 1 on, and the index after it from
/// version 2 on (version 3, from 2024, marks the grouping essential). A box is read in the current
/// layout, or in the earlier one when only that one describes its bytes exactly, which takes the size of its entries:
/// 2 bytes for `roll` and `prol` (§10.1), 1 for `sync` (ISO/IEC 14496-15 §8.4.4). `None` when neither reads it
fn default_entry(description: Option<&Vec<u8>>) -> Option<u32> {
    let Some(d) = description else { return Some(0); };
    match d[0] {
        0 | 1 => return Some(0),
        2 | 3 => {}
        _ => return None
    }
    let field = |at: usize| d.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    // Version and flags, grouping_type, default_length, default_group_description_index, entry_count, the entries: of
    // default_length bytes, or each after its description_length when that is 0
    let current = (|| {
        let (length, default, count) = (field(8)?, field(12)?, field(16)?);
        let mut end = 20usize;
        if length == 0 {
            for _ in 0..count { end = end.checked_add(4)?.checked_add(field(end)? as usize)?; }
        } else {
            end = (count as usize).checked_mul(length as usize)?.checked_add(end)?;
        }
        (default <= count && end <= d.len()).then_some((default, end == d.len()))
    })();
    // Version and flags, grouping_type, default_sample_description_index, entry_count, the entries
    let earlier = || {
        let size = match &d[4..8] { b"roll" | b"prol" => 2, b"sync" => 1, _ => return None };
        let (default, count) = (field(8)?, field(12)?);
        (d[0] == 2 && default <= count && (count as usize).checked_mul(size)?.checked_add(16)? == d.len()).then_some(default)
    };
    match current {
        Some((default, true)) => Some(default),
        _ => earlier().or(current.map(|c| c.0))
    }
}

/// The values of a `cslg` (ISO/IEC 14496-12 §8.6.1.4): compositionToDTSShift, leastDecodeToDisplayDelta,
/// greatestDecodeToDisplayDelta, compositionStartTime, compositionEndTime. Summed in 128 bits, as the merged track's
/// decode times can reach 2^64; a value that doesn't fit the 64 bits of a `cslg` refuses the merge, since the first
/// file's would be kept
fn composition_bounds(stts: &[(u32, u32)], ctts: Option<&(u8, Vec<(u32, i32)>)>, name: &str) -> Result<[i64; 5]> {
    let Some((_, offsets)) = ctts.filter(|c| !c.1.is_empty()) else { return Ok([0; 5]); };
    let (mut least, mut greatest) = (i128::MAX, i128::MIN);
    let (mut start, mut end) = (i128::MAX, i128::MIN);
    let mut dts = 0i128;
    let mut durations = stts.iter().flat_map(|&(n, d)| std::iter::repeat_n(d as i128, n as usize));
    for &(n, o) in offsets {
        least = least.min(o as i128);
        greatest = greatest.max(o as i128);
        for _ in 0..n {
            let d = durations.next().unwrap_or(0);
            start = start.min(dts + o as i128);
            end = end.max(dts + o as i128 + d);
            dts += d;
        }
    }
    let mut values = [0i64; 5];
    for (v, x) in values.iter_mut().zip([(-least).max(0), least, greatest, start, end]) {
        *v = i64::try_from(x).map_err(|_| refused(format!("{name}: its composition times, up to {end}, don't fit the 64 bits of a composition shift box (cslg)")))?;
    }
    Ok(values)
}
