// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright © 2022 Adrian <adrian.eddy at gmail>

use std::io::Write;
use std::path::*;
use mp4_merge::{join_files, update_file_times};

fn main() {
    let _time = std::time::Instant::now();

    let mut files = Vec::new();
    let mut output_file = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--out" {
            if let Some(out) = args.next() {
                output_file = Some(Path::new(&out).to_owned())
            }
            continue;
        }
        let p = Path::new(&arg);
        if !p.exists() {
            eprintln!("File doesn't exist {:?}", p);
            continue;
        }
        println!("Merging file {:?}", p);
        files.push(p.to_owned());
        if output_file.is_none() {
            output_file = Some(p.with_file_name(format!("{}_joined.mp4", p.file_name().unwrap().to_str().unwrap())));
        }
    }
    if files.is_empty() { eprintln!("No input files!"); return; }
    if output_file.is_none() { eprintln!("Output file not specified!"); return; }

    let final_output_file = output_file.as_ref().unwrap();

    println!("Output file {:?}", final_output_file);

    join_files(&files, final_output_file, |progress| {
        print!("\rMerging... {:.2}%", progress * 100.0);
        std::io::stdout().flush().unwrap();
    }).unwrap();

    update_file_times(&files[0], final_output_file);

    println!("\rDone in {:.3}s                ", _time.elapsed().as_millis() as f64 / 1000.0);
    std::io::stdout().flush().unwrap();
}
