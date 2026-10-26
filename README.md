# mp4-merge
A tool and library to losslessly join multiple .mp4 files shot with same camera and settings.

This is useful to merge multiple files that are created by the camera because of the 4GB limit on the SD Card.

This tool can merge all these separate files to a new one without transcoding or losing any data.

All original tracks are preserved, all metadata is kept as in the original.

It was created to help stabilizing such files in [Gyroflow](https://github.com/gyroflow/gyroflow).

## Download:
See the [Releases](https://github.com/gyroflow/mp4-merge/releases) page.

## Usage:
The easiest way is to just drag & drop multiple .mp4 files onto the `mp4_merge` executable.

Usage from command line:
- Merge specified files and output to `IN_FILE1.mp4_joined.mp4`
```shell
mp4_merge IN_FILE1.mp4 IN_FILE2.mp4 IN_FILE3.mp4 ...
```
- Merge specified files and output to `result.mp4`

```shell
mp4_merge IN_FILE1.mp4 IN_FILE2.mp4 IN_FILE3.mp4 ... --out result.mp4
```

## Use as a Rust library:

```toml
[dependencies]
mp4-merge = "0.1.11"
```
```rust
let files = ["IN_FILE1.mp4", "IN_FILE2.mp4"];
mp4_merge::join_files(&files, "out.mp4", |progress| {
    println!("Merging... {:.2}%", progress * 100.0);
}).unwrap();

```
`join_files` never writes into an input: an output that is one of the inputs is refused, by file identity, so also when its path is another spelling of an input's, a letter case variant of it, or a link to it. The output is written under a temporary name next to it and gets its name only once it is complete and on disk, so a merge that fails leaves no partial file under that name, and an existing file there is replaced only then.

To merge streams, `join_file_streams` writes into an output stream you give it, and `join_file_streams_with` creates the output stream only once every file has been read and checked, so a merge that is refused never creates it.

## How does this work?
The idea is to merge the raw track data together, and then rewrite the `stbl` box (which is the descriptor of the raw data) to account for the additional data. In order to do this this library does the following:
1. Scan every provided file and collect:
    - `mdat` offset and size
    - Duration stored in `mvhd`, `tkhd`, `mdhd` boxes, and the edit lists (`elst`)
    - The sample descriptions (`stsd`)
    - `stbl` descriptions: `stts`, `ctts`, `stsz`, `stss`, `stps`, `sdtp`, `sbgp`, Apple's compact `csgm`, `stsc`, `stco`/`co64`
2. Check that the files can be merged. Tracks are matched by their order, and the merged file describes every file's samples with the first file's sample descriptions, so all files must have the same tracks, with the same codec configuration. Only the bitrates the descriptions state may differ, and the picture parameter sets of an AVC or HEVC configuration: the merged one holds those of every file (and says that an HEVC one's may also be in the samples when a file's does). Every box must be whole (one of size 0 is the last one, up to the end of the file) and every table must fit in its box, a file needs one `moov` and can't be fragmented (its movie fragments, `moof`, hold samples its tables don't; a Movie Extends Box without fragments is fine), its samples must all be in one `mdat`, and each track's sample-to-chunk table must give its chunks exactly its samples, so a damaged file or an unfinished recording isn't merged. Samples keep their sample groups, those a description gives by default included, or the files aren't merged; only groups of a type the first file has none of are left out, with a warning, unless they are marked essential.
3. Merge all these descriptions: sum durations, append `stbl` lists to each other and add chunk offsets based on previous file `mdat` size.
    - A track's timing stays in the first file's media timescale when that holds every file's timing exactly, otherwise it moves to one that does. When no 32-bit timescale does, the files aren't merged, so a merged file never plays at another speed than its parts.
    - Each file's edits follow the previous file's, so every file presents what it presented on its own. An edit that runs past its file's media (an audio edit given the video's duration, a metadata placeholder) ends where that media does, so the next file's first samples aren't presented twice.
    - The chapters of one recording, which the files' creation times tell apart from separate recordings, continue each other: each track's samples follow the previous file's. After another recording, the video still follows the previous file's, and so does the metadata, whose samples keep their durations (gyro readers time the readings by them), so the gyro data stays where it was against the frames. Every other video and audio track of a file starts where the file's video does, so the audio keeps its sync with the video in every file: audio that runs past the previous file's video is cut there, at a whole sample, and a gap this leaves, or a later file's track that starts late, is filled by the previous sample lasting longer, or by silence in PCM: never by an empty edit in the middle of the track, where FFmpeg presents the track's first samples again, and never by a longer PCM sample, which would make FFmpeg read every sample on its own.
4. Take the first file, go through every box and write it to the output file, while:
    - If it's the `mdat` the samples are in: write the raw data of each file's such `mdat`, then the silence of PCM gaps, and store it as a large box (64-bit). Another `mdat`, which no track uses, is left out
    - If `mvhd`, `tkhd` or `mdhd`: patch the duration value to the sum of all durations (and `mdhd`'s timescale), as version 1 when it doesn't fit 32 bits
    - If `elst`: write the merged edits
    - If `stbl`: write these boxes from scratch, using merged lists from the description
    - If `mvex`: leave it out, as the merged file has no movie fragments
    - If `stco`: rewrite to `co64` to be able to fit more than 4 GB of data.
    - If `moov/udta`: write it without the segment table (`MTRM`), which describes the frames of the file it was written into, not those of the merged one.
    - Any read or write error stops the merge with that error, and so does a file that ends before its boxes do.
5. Done

<br>

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>