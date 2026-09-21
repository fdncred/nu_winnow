//! `nufmt`-style formatter example.
//!
//! ```text
//! cargo run --example nufmt -- file.nu            # print formatted source
//! cargo run --example nufmt -- --write file.nu    # format in place
//! cargo run --example nufmt -- --check dir/       # exit 1 if any file would change
//! echo 'ls|where size>1kb' | cargo run --example nufmt
//! ```

mod format;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use format::{Options, format};

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                collect(&entry.path(), out);
            }
        }
    } else if path.extension().is_some_and(|e| e == "nu") {
        out.push(path.to_path_buf());
    }
}

fn main() -> ExitCode {
    let mut write = false;
    let mut check = false;
    let mut paths = Vec::new();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--write" | "-w" => write = true,
            "--check" => check = true,
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    let options = Options::default();
    if paths.is_empty() {
        let mut src = String::new();
        if std::io::stdin().read_to_string(&mut src).is_err() {
            eprintln!("error: stdin is not valid UTF-8");
            return ExitCode::FAILURE;
        }
        return match format(&src, &options) {
            Ok(out) => {
                print!("{out}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprint!("{}", e.render(&src, Some("<stdin>")));
                ExitCode::FAILURE
            }
        };
    }
    let mut files = Vec::new();
    for p in &paths {
        collect(p, &mut files);
    }
    files.sort();
    let mut status = ExitCode::SUCCESS;
    let mut changed = 0;
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else { continue };
        match format(&src, &options) {
            Ok(out) => {
                if check {
                    if out != src {
                        println!("would reformat {}", file.display());
                        changed += 1;
                    }
                } else if write {
                    if out != src {
                        std::fs::write(file, &out).expect("write");
                        println!("formatted {}", file.display());
                    }
                } else {
                    print!("{out}");
                }
            }
            Err(e) => {
                eprint!("{}", e.render(&src, Some(&file.display().to_string())));
                status = ExitCode::FAILURE;
            }
        }
    }
    if check && changed > 0 {
        println!("{changed} of {} files would be reformatted", files.len());
        return ExitCode::FAILURE;
    }
    status
}
