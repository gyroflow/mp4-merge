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
mp4-merge = "0.1.7"
```
```rust
let files = ["IN_FILE1.mp4", "IN_FILE2.mp4"];
mp4_merge::join_files(&files, "out.mp4", |progress| {
    println!("Merging... {:.2}%", progress * 100.0);
}).unwrap();

```

## How does this work?
The idea is to merge the raw track data together, and then rewrite the `stbl` box (which is the descriptor of the raw data) to account for the additional data. In order to do this this library does the following:
1. Scan every provided file and collect:
    - `mdat` offset and size
    - Duration stored in `mvhd`, `tkhd`, `mdhd` boxes
    - `stbl` descriptions: `stts`, `stsz`, `stss`, `stsc`, `stco`/`co64`
2. Merge all these descriptions: sum durations, append `stbl` lists to each other and add chunk offsets based on previous file `mdat` size.
3. Take the first file, go through every box and write it to the output file, while:
    - If `mdat`: write raw data from all `mdat` boxes from all files, and store it as a large box (64-bit)
    - If `mvhd`, `tkhd` or `mdhd`: patch the duration value to the sum of all durations
    - If `stbl`: write these boxes from scratch, using merged lists from the description
    - If `stco`: rewrite to `co64` to be able to fit more than 4 GB of data.
4. Done

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