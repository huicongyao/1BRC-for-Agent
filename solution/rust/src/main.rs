// 1BRC Rust track - naive baseline (correct, intentionally unoptimized).
//
// Interface: onebrc <input_file>   -> result to stdout
//
// Improve this file. Keep it a single source file with no external crates.
// See AGENTS.md for rules, semantics and the iteration loop.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

struct Agg {
    min: f64,
    max: f64,
    sum: f64,
    count: u64,
}

fn parse_t10(s: &[u8]) -> i64 {
    let (neg, mut i) = if s.first() == Some(&b'-') { (true, 1usize) } else { (false, 0usize) };
    let mut ip: i64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        ip = ip * 10 + (s[i] - b'0') as i64;
        i += 1;
    }
    let mut fp: i64 = 0;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        if i < s.len() && s[i].is_ascii_digit() {
            fp = (s[i] - b'0') as i64;
        }
    }
    let v = ip * 10 + fp;
    if neg {
        -v
    } else {
        v
    }
}

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: onebrc <input_file>");
            std::process::exit(1);
        }
    };

    let mut stats: BTreeMap<Vec<u8>, Agg> = BTreeMap::new();
    let file = File::open(&path).unwrap_or_else(|e| {
        eprintln!("{}: {}", path, e);
        std::process::exit(1);
    });
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line: Vec<u8> = Vec::with_capacity(64);

    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {}", e);
                std::process::exit(1);
            }
        }
        let mut end = line.len();
        while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
            end -= 1;
        }
        let l = &line[..end];
        let semi = match l.iter().position(|&b| b == b';') {
            Some(p) => p,
            None => continue,
        };
        let name = &l[..semi];
        let t10 = parse_t10(&l[semi + 1..]);
        let v = t10 as f64 / 10.0;

        let entry = stats.entry(name.to_vec()).or_insert(Agg {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            sum: 0.0,
            count: 0,
        });
        if v < entry.min {
            entry.min = v;
        }
        if v > entry.max {
            entry.max = v;
        }
        entry.sum += v;
        entry.count += 1;
    }

    let mut out = String::from("{");
    let mut first = true;
    for (name, a) in &stats {
        if !first {
            out.push_str(", ");
        }
        first = false;
        let mean = ((a.sum / a.count as f64) * 10.0 + 0.5).floor() / 10.0;
        out.push_str(&format!(
            "{}={:.1}/{:.1}/{:.1}",
            String::from_utf8_lossy(name),
            a.min,
            mean,
            a.max
        ));
    }
    out.push_str("}\n");
    print!("{}", out);
}
