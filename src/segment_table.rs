// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2026 Adrian <adrian.eddy at gmail>

//! The segment table of a file cut from camera recordings (`moov/udta/MTRM`, whose payload is a JSON document of format
//! version 1), as far as a merge needs it: which recording the file's video starts in and which it ends in, and where in
//! them, so that the merge tells a file that continues the previous one's recording from one of another moment
//! (`merge::junctions`).
//!
//! A table is taken only where it's valid for its file, by the format's reader validity rule (as telemetry-parser's
//! `segment_table` reads it):
//! 1. `moov/udta` holds exactly one `MTRM`, and its JSON is a valid version-1 document: every normative member present
//!    with its type, no source of 0 frames or of a rate with a 0, every creation time a UTC time to the second;
//! 2. the segments tile: each starts at the frame, the time and the capture time the one before ends at;
//! 3. they cover the file's video: as many frames as it presents, each segment's start the presentation time of its
//!    first frame (to the µs, both rounded the same way), and the last one's end within a nominal frame of the video's.
//!
//! A table that isn't (a copy another tool kept with a video it changed fails rule 3) is ignored: the file is then taken as
//! a camera's

use crate::desc_reader::{ PartDesc, PartTrack };
use crate::fourcc;

/// A recording a file's frames come from (`sources[]`)
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    /// Its file name, without a directory
    pub name: String,
    /// Its size in bytes
    pub size: u64,
    /// The frames its video presents
    pub frames: u64,
    /// Its nominal frame rate, `(numerator, denominator)`
    pub fps: (u64, u64),
    /// Its `mvhd` creation time in seconds since 1904, as a file's own is read (`PartDesc::mvhd_creation`), 0 for none
    pub creation: u64,
}

/// A run of frames that follow each other in one source (`segments[]`), as far as the merge needs it
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// Its source, an index in `Table::sources`
    pub source: usize,
    pub src_first_frame: u64,
    pub frame_count: u64,
    pub out_first_frame: u64,
    /// Where it starts in the file, and how long it lasts, µs
    pub out_start_us: i64,
    pub duration_us: i64,
}

/// A file's segment table, valid for the file
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub sources: Vec<Source>,
    /// At least one
    pub segments: Vec<Segment>,
}

impl Table {
    /// The table of a file whose `moov/udta` holds the `MTRM` payloads `payloads`: `None` for a file without one, and
    /// `Some(Err)` with the reason for one that isn't valid for the file (`part`), which is then ignored
    pub fn of_file(payloads: &[Vec<u8>], part: &PartDesc) -> Option<std::result::Result<Table, String>> {
        let payload = match payloads {
            [] => return None,
            [one] => one,
            more => return Some(Err(format!("moov/udta has {} MTRM boxes", more.len()))),
        };
        Some(Self::parse(payload).and_then(|table| table.covers(part).map(|_| table)))
    }

    /// The segment the file's video starts with, and the one it ends with
    pub fn first(&self) -> &Segment { &self.segments[0] }
    pub fn last(&self) -> &Segment { &self.segments[self.segments.len() - 1] }
    /// The source of a segment
    pub fn source_of(&self, segment: &Segment) -> &Source { &self.sources[segment.source] }

    /// Rules 1 and 2: the document and how its segments follow each other
    fn parse(text: &[u8]) -> std::result::Result<Table, String> {
        let doc = Json::parse(text)?;
        if doc.get("version").and_then(Json::as_u64) != Some(1) {
            return Err("it isn't a version-1 table".into());
        }
        let pair = |v: Option<&Json>, what: &str| -> std::result::Result<(u64, u64), String> {
            match v.and_then(Json::as_array) {
                Some([a, b]) => a.as_u64().zip(b.as_u64()).ok_or_else(|| format!("{what} isn't a pair of whole numbers")),
                _ => Err(format!("{what} isn't a pair of numbers")),
            }
        };
        // Each source, and its clock's modulus where it has a clock (`clock_shift` counts its ticks)
        let mut sources = Vec::new();
        let mut clocks: Vec<Option<Option<u64>>> = Vec::new();
        for s in doc.get("sources").and_then(Json::as_array).ok_or("no sources")? {
            if !matches!(s, Json::Obj(_)) { return Err("a source isn't an object".into()); }
            let field = |k: &str| s.get(k).ok_or_else(|| format!("a source has no {k}"));
            let whole = |k: &str| field(k).and_then(|v| v.as_u64().ok_or_else(|| format!("a source's {k} isn't a whole number")));
            let name = field("name")?.as_str().ok_or("a source's name isn't a string")?.to_owned();
            let (size, frames, fps) = (whole("size")?, whole("frames")?, pair(Some(field("fps")?), "a source's fps")?);
            let creation = match field("creation_time")? {
                Json::Null => 0,
                t => utc_second_since_1904(t.as_str().ok_or("a source's creation_time isn't a string")?).ok_or("a source's creation_time isn't a UTC time YYYY-MM-DDTHH:MM:SSZ")?,
            };
            clocks.push(match field("clock")? {
                Json::Null => None,
                c => {
                    c.get("name").and_then(Json::as_str).ok_or("a clock without a name")?;
                    let rate = pair(c.get("rate"), "a clock's rate")?;
                    let modulus = match c.get("modulus").ok_or("a clock without a modulus")? {
                        Json::Null => None,
                        m => Some(m.as_u64().ok_or("a clock's modulus isn't a whole number")?),
                    };
                    if rate.0 == 0 || rate.1 == 0 || modulus == Some(0) {
                        return Err(format!("a source's clock runs at {}/{} ticks per second, wrapping at {modulus:?}", rate.0, rate.1));
                    }
                    Some(modulus)
                }
            });
            if frames == 0 || fps.0 == 0 || fps.1 == 0 {
                return Err(format!("a source has {frames} frames at {}/{} fps", fps.0, fps.1));
            }
            sources.push(Source { name, size, frames, fps, creation });
        }
        if sources.is_empty() { return Err("no sources".into()); }
        let mut segments = Vec::new();
        let (mut frame, mut time, mut capture) = (0u64, None, None);
        for (k, e) in doc.get("segments").and_then(Json::as_array).ok_or("no segments")?.iter().enumerate() {
            if !matches!(e, Json::Obj(_)) { return Err("a segment isn't an object".into()); }
            let field = |key: &str| e.get(key).ok_or_else(|| format!("a segment has no {key}"));
            let u = |key: &str| field(key).and_then(|v| v.as_u64().ok_or_else(|| format!("a segment's {key} isn't a whole number")));
            let i = |key: &str| field(key).and_then(|v| v.as_i64().ok_or_else(|| format!("a segment's {key} isn't a whole number")));
            let source = usize::try_from(u("source")?).map_err(|_| "a segment's source is out of range")?;
            let (src_first_frame, frame_count, out_first_frame) = (u("src_first_frame")?, u("frame_count")?, u("out_first_frame")?);
            let (src_start_us, out_start_us, duration_us) = (i("src_start_us")?, i("out_start_us")?, i("duration_us")?);
            let (src_capture_us, out_capture_us, capture_us) = (i("src_capture_start_us")?, i("out_capture_start_us")?, i("capture_duration_us")?);
            if !matches!(field("requested")?, Json::Null | Json::Obj(_)) { return Err("a segment's requested isn't an object or null".into()); }
            for key in ["start", "entry", "end"] {
                field(key)?.as_str().ok_or_else(|| format!("a segment's {key} isn't a string"))?;
            }
            let clock_shift = match field("clock_shift")? {
                Json::Null => None,
                v => Some(v.as_i64().ok_or("a segment's clock_shift isn't a whole number")?),
            };
            let src = sources.get(source).ok_or_else(|| format!("segment {k} names source {source} of {}", sources.len()))?;
            if frame_count == 0 { return Err(format!("segment {k} has no frames")); }
            let (Some(end), Some(src_end)) = (out_first_frame.checked_add(frame_count), src_first_frame.checked_add(frame_count)) else {
                return Err(format!("segment {k}'s frames overflow"));
            };
            if src_end > src.frames {
                return Err(format!("segment {k} has source frames {src_first_frame}..{src_end}, its source has {}", src.frames));
            }
            if [src_start_us, out_start_us, duration_us, src_capture_us, out_capture_us, capture_us].iter().any(|t| *t < 0) {
                return Err(format!("segment {k} has a negative time"));
            }
            let sum = |a: i64, b: i64| a.checked_add(b).ok_or_else(|| format!("segment {k}'s times overflow"));
            sum(src_start_us, duration_us)?;
            sum(src_capture_us, capture_us)?;
            if out_first_frame != frame {
                return Err(format!("segment {k} starts at frame {out_first_frame}, not {frame}"));
            }
            if let (Some(t), Some(c)) = (time, capture) {
                if (t, c) != (out_start_us, out_capture_us) {
                    return Err(format!("segment {k} starts at {out_start_us} µs (capture {out_capture_us} µs), not where the one before ends, {t} µs (capture {c} µs)"));
                }
            }
            match (clocks[source], clock_shift) {
                (None, None) => {}
                (Some(modulus), Some(d)) if modulus.is_none_or(|m| u64::try_from(d).is_ok_and(|d| d < m)) => {}
                (_, d) => return Err(format!("segment {k} has the clock shift {d:?} for its source's clock")),
            }
            frame = end;
            time = Some(sum(out_start_us, duration_us)?);
            capture = Some(sum(out_capture_us, capture_us)?);
            segments.push(Segment { source, src_first_frame, frame_count, out_first_frame, out_start_us, duration_us });
        }
        if segments.is_empty() { return Err("no segments".into()); }
        Ok(Table { sources, segments })
    }

    /// Rule 3: the segments cover the frames the file's video presents (`VideoFrames`)
    fn covers(&self, part: &PartDesc) -> std::result::Result<(), String> {
        let video = VideoFrames::of(part).ok_or("the file's video frames can't be read")?;
        let last = self.last();
        let frames = last.out_first_frame + last.frame_count;
        if frames != video.starts.len() as u64 {
            return Err(format!("its segments cover {frames} frames, the file's video has {}", video.starts.len()));
        }
        for s in &self.segments {
            let t = video.time_us(s.out_first_frame as usize).ok_or("a frame time out of range")?;
            if t != s.out_start_us {
                return Err(format!("it puts frame {} at {} µs, the file's video at {t} µs", s.out_first_frame, s.out_start_us));
            }
        }
        // Within one nominal frame and half a µs (the rounding of the end), exactly: in units of 1 / (2 · timescale) µs
        let (timescale, end_us) = (video.timescale as i128, (last.out_start_us as i128) + last.duration_us as i128);
        let diff = (2 * end_us * timescale - 2 * video.end as i128 * 1_000_000).abs();
        if diff > 2 * video.nominal as i128 * 1_000_000 + timescale {
            return Err(format!("it ends at {end_us} µs, more than a frame from the end of the file's video"));
        }
        Ok(())
    }
}

/// The presentation times of a file's frames: its first video track's, as FFmpeg's demuxer presents them through the
/// track's edit list (an optional empty edit, then one edit of the media at normal rate; ISO/IEC 14496-12 §8.6.6), in
/// ticks of the media timescale, in presentation order
struct VideoFrames {
    timescale: u64,
    starts: Vec<i64>,
    /// Where the last frame ends: the end of the video
    end: i64,
    /// The most common frame duration
    nominal: i64,
}

impl VideoFrames {
    fn of(part: &PartDesc) -> Option<Self> {
        let track: &PartTrack = part.tracks.iter().find(|t| t.handler == fourcc("vide"))?;
        let (timescale, movie_timescale) = (track.mdhd_timescale as u64, part.mvhd_timescale as u64);
        if timescale == 0 || movie_timescale == 0 { return None; }
        // Decode durations (§8.6.1.2) and composition offsets (§8.6.1.3, signed in either version, as FFmpeg reads them)
        let mut durations: Vec<i64> = Vec::new();
        for &(count, delta) in &track.stts {
            if durations.len() + count as usize > track.stsz_count as usize { return None; }
            durations.extend(std::iter::repeat_n(delta as i64, count as usize));
        }
        let mut offsets = vec![0i64; durations.len()];
        if let Some((_, ctts)) = &track.ctts {
            let mut at = 0usize;
            for &(count, offset) in ctts {
                for o in offsets.iter_mut().skip(at).take(count as usize) { *o = offset as i64; }
                at += count as usize;
            }
        }
        let mut dts = 0i64;
        let decoded: Vec<(i64, i64, i64)> = durations.iter().zip(&offsets).map(|(d, o)| { let f = (dts, dts + o, *d); dts += d; f }).collect();
        let (mut empty, mut media_time, mut media_end, mut edited) = (0i64, 0i64, i64::MAX, false);
        if let Some(edits) = &track.elst {
            let mut e = 0;
            if edits.len() > 1 {
                if edits[0].media_time != -1 { return None; }
                empty = i64::try_from(edits[0].segment_duration).ok()?;
                e = 1;
            }
            if let Some(edit) = edits.get(e) {
                if edits.len() > e + 1 || edit.media_time < 0 || edit.media_rate != crate::desc_reader::MEDIA_RATE_1 { return None; }
                media_time = edit.media_time;
                edited = true;
                if edit.segment_duration > 0 {
                    let d = (edit.segment_duration as i128 * timescale as i128 + movie_timescale as i128 / 2) / movie_timescale as i128;
                    media_end = media_time.checked_add(i64::try_from(d).ok()?)?;
                }
            }
        }
        let empty_ticks = i64::try_from((empty as i128 * timescale as i128 + movie_timescale as i128 / 2) / movie_timescale as i128).ok()?;
        let in_edit = |cts: i64| cts >= media_time && cts < media_end;
        // The first frame presented starts the edit (FFmpeg's `mov_fix_index`)
        let origin = if edited {
            let first_dts = decoded.iter().find(|(_, cts, _)| in_edit(*cts))?.0;
            let first_cts = decoded.iter().filter(|(_, cts, _)| in_edit(*cts)).map(|(_, cts, _)| *cts).min()?;
            first_cts.max(first_dts)
        } else {
            0
        };
        let mut presented: Vec<(i64, i64)> = decoded.iter().filter(|(_, cts, _)| in_edit(*cts)).map(|(_, cts, d)| (*cts, *d)).collect();
        presented.sort_by_key(|f| f.0);
        let (last_cts, last_duration) = *presented.last()?;
        let end = (last_cts + last_duration).min(media_end) - origin + empty_ticks;
        let starts = presented.iter().map(|(cts, _)| cts - origin + empty_ticks).collect();
        // The most common duration, the shorter one on a tie
        let mut sorted = durations;
        sorted.sort_unstable();
        let (mut nominal, mut best, mut run) = (sorted[0], 0usize, 0usize);
        for (i, d) in sorted.iter().enumerate() {
            run = if i > 0 && sorted[i - 1] == *d { run + 1 } else { 1 };
            if run > best { best = run; nominal = *d; }
        }
        (nominal > 0).then_some(Self { timescale, starts, end, nominal })
    }

    /// Frame `k`'s presentation time in µs, rounded to the nearest, halves away from zero
    fn time_us(&self, k: usize) -> Option<i64> {
        let ticks = *self.starts.get(k)? as i128;
        let (n, d) = (ticks.checked_mul(1_000_000)?, self.timescale as i128);
        let q = (n.abs() * 2 + d) / (2 * d);
        i64::try_from(if n < 0 { -q } else { q }).ok()
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339 in UTC, to the second, a date of the proleptic Gregorian calendar, no leap second) in
/// seconds since 1904-01-01, the epoch of a `mvhd` creation time; `None` for anything else, or a time before 1904
fn utc_second_since_1904(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| b[r].iter().try_fold(0i64, |v, &c| c.is_ascii_digit().then(|| v * 10 + (c - b'0') as i64));
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match mo { 1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 4 | 6 | 9 | 11 => 30, 2 => if leap { 29 } else { 28 }, _ => return None };
    if !(1..=days_in_month).contains(&d) || h >= 24 || mi >= 60 || se >= 60 { return None; }
    // Days since 1970-01-01 of a date of the proleptic Gregorian calendar (H. Hinnant, "chrono-Compatible Low-Level Date
    // Algorithms", `days_from_civil`)
    let days_from_civil = |y: i64, m: i64, d: i64| {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    };
    let seconds = (days_from_civil(y, mo, d) - days_from_civil(1904, 1, 1)) * 86400 + h * 3600 + mi * 60 + se;
    u64::try_from(seconds).ok()
}

/// A JSON value (RFC 8259), as much of one as the table needs: numbers are kept as their text, read as whole numbers
/// only where they're written as one
#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    /// Its members in order; a name that repeats takes its last value, as the format's readers take it
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// How deep arrays and objects may nest: the table nests 4 deep, and a handler's member a few more
    const MAX_DEPTH: usize = 64;

    fn parse(text: &[u8]) -> std::result::Result<Json, String> {
        let text = std::str::from_utf8(text).map_err(|_| "its JSON isn't UTF-8")?;
        let mut p = Parser { s: text.as_bytes(), at: 0 };
        let v = p.value(0)?;
        p.ws();
        if p.at != p.s.len() { return Err(format!("its JSON has more after its value, at byte {}", p.at)); }
        Ok(v)
    }
    fn get(&self, key: &str) -> Option<&Json> {
        match self { Json::Obj(m) => m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v), _ => None }
    }
    fn as_array(&self) -> Option<&[Json]> {
        match self { Json::Arr(a) => Some(a), _ => None }
    }
    fn as_str(&self) -> Option<&str> {
        match self { Json::Str(s) => Some(s), _ => None }
    }
    /// A whole number, written without a fraction or an exponent
    fn as_u64(&self) -> Option<u64> {
        match self { Json::Num(n) if !n.contains(['.', 'e', 'E']) => n.parse().ok(), _ => None }
    }
    fn as_i64(&self) -> Option<i64> {
        match self { Json::Num(n) if !n.contains(['.', 'e', 'E']) => n.parse().ok(), _ => None }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.at).is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')) { self.at += 1; }
    }
    fn err(&self, what: &str) -> String { format!("its JSON has {what} at byte {}", self.at) }
    fn eat(&mut self, lit: &[u8]) -> bool {
        let ok = self.s[self.at..].starts_with(lit);
        if ok { self.at += lit.len(); }
        ok
    }
    fn value(&mut self, depth: usize) -> std::result::Result<Json, String> {
        if depth > Json::MAX_DEPTH { return Err(self.err("values nested too deep")); }
        self.ws();
        match self.s.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                self.ws();
                if self.eat(b"}") { return Ok(Json::Obj(members)); }
                loop {
                    self.ws();
                    let key = self.string()?;
                    self.ws();
                    if !self.eat(b":") { return Err(self.err("no ':' after a member's name")); }
                    members.push((key, self.value(depth + 1)?));
                    self.ws();
                    if self.eat(b"}") { return Ok(Json::Obj(members)); }
                    if !self.eat(b",") { return Err(self.err("no ',' or '}' after a member")); }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat(b"]") { return Ok(Json::Arr(items)); }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(b"]") { return Ok(Json::Arr(items)); }
                    if !self.eat(b",") { return Err(self.err("no ',' or ']' after an element")); }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.eat(b"true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat(b"null") => Ok(Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("no value")),
        }
    }
    /// `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn number(&mut self) -> std::result::Result<Json, String> {
        let start = self.at;
        let digits = |p: &mut Self| { let from = p.at; while p.s.get(p.at).is_some_and(u8::is_ascii_digit) { p.at += 1; } p.at - from };
        self.eat(b"-");
        match self.s.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => { digits(self); }
            _ => return Err(self.err("a number without digits")),
        }
        if self.eat(b".") && digits(self) == 0 { return Err(self.err("a fraction without digits")); }
        if self.s.get(self.at).is_some_and(|c| matches!(c, b'e' | b'E')) {
            self.at += 1;
            if !self.eat(b"+") { self.eat(b"-"); }
            if digits(self) == 0 { return Err(self.err("an exponent without digits")); }
        }
        Ok(Json::Num(String::from_utf8_lossy(&self.s[start..self.at]).into_owned()))
    }
    fn string(&mut self) -> std::result::Result<String, String> {
        if !self.eat(b"\"") { return Err(self.err("no string")); }
        let mut out = String::new();
        loop {
            let start = self.at;
            while self.s.get(self.at).is_some_and(|c| *c != b'"' && *c != b'\\' && *c >= 0x20) { self.at += 1; }
            // The text is UTF-8 and the run ends at an ASCII byte, so it's whole characters
            out.push_str(std::str::from_utf8(&self.s[start..self.at]).map_err(|_| self.err("a string that isn't UTF-8"))?);
            match self.s.get(self.at) {
                Some(b'"') => { self.at += 1; return Ok(out); }
                Some(b'\\') => {
                    self.at += 1;
                    let c = *self.s.get(self.at).ok_or_else(|| self.err("an escape at the end"))?;
                    self.at += 1;
                    match c {
                        b'"' => out.push('"'), b'\\' => out.push('\\'), b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'), b'f' => out.push('\u{c}'), b'n' => out.push('\n'), b'r' => out.push('\r'), b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = match hi {
                                0xD800..=0xDBFF => {
                                    if !self.eat(b"\\u") { return Err(self.err("a lone high surrogate")); }
                                    let lo = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&lo) { return Err(self.err("a high surrogate without a low one")); }
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                }
                                0xDC00..=0xDFFF => return Err(self.err("a lone low surrogate")),
                                c => c,
                            };
                            out.push(char::from_u32(c).ok_or_else(|| self.err("an escape that isn't a character"))?);
                        }
                        _ => return Err(self.err("an unknown escape")),
                    }
                }
                Some(_) => return Err(self.err("a control character in a string")),
                None => return Err(self.err("a string without its end")),
            }
        }
    }
    fn hex4(&mut self) -> std::result::Result<u32, String> {
        let hex = self.s.get(self.at..self.at + 4).ok_or_else(|| self.err("a short \\u escape"))?;
        let v = std::str::from_utf8(hex).ok().filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit())).and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.err("a \\u escape that isn't hexadecimal"))?;
        self.at += 4;
        Ok(v)
    }
}

/// The payloads of the `MTRM` boxes among the boxes of a `udta` payload, up to the first box that doesn't fit (a size of 0
/// is a box to the end; ISO/IEC 14496-12 §4.2)
pub fn payloads(udta: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(rest) = udta.get(pos..).filter(|r| r.len() >= 8) {
        let size32 = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as u64;
        let (size, header) = match size32 {
            0 => (rest.len() as u64, 8),
            1 => match rest.get(8..16) { Some(s) => (u64::from_be_bytes(s.try_into().unwrap()), 16), None => break },
            s => (s, 8),
        };
        if size < header || size > rest.len() as u64 { break; }
        if &rest[4..8] == b"MTRM" { out.push(rest[header as usize..size as usize].to_vec()); }
        pos += size as usize;
    }
    out
}

/// How many frames a file's video presents (`VideoFrames`)
pub fn presented_frames(part: &PartDesc) -> Option<u64> {
    VideoFrames::of(part).map(|v| v.starts.len() as u64)
}
