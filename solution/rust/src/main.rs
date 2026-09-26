// 1BRC Rust track - parallel, I/O-bound implementation.
//
//   onebrc <input_file>   -> result to stdout
//
// Design notes (details and measurements in EXPERIMENTS.md)
// ---------------------------------------------------------
// The 1B input does not fit in the page cache, so wall clock is dominated by
// reading ~13.8 GB from the SSD (~2.3 GB/s on this machine, and concurrent
// streams do not raise it).  kThreads threads each pread() their own
// contiguous, newline-aligned slice in kBlock chunks and parse the block in
// place, overlapping parsing with I/O.  std only: FileExt::read_at for the
// slice reads, std::thread::scope for the workers.
//
// Parsing uses 8-byte SWAR loads (locate ';' with a haszero trick, decode the
// canonical temperature forms from one 8-byte load); anything unusual falls
// back to a scalar parser that mirrors harness/src/reference.c exactly.
// Aggregation uses per-thread open-addressing tables of 64-byte slots.
//
// Mean rounding: the reference accumulates `sum` as a double in file order, so
// this implementation computes the exact rational mean instead and flags any
// station whose exact value falls inside the reference's rounding error of a
// .5 boundary; those are recomputed with a sequential double scan in file
// order, exactly like the reference.  See the C++ track notes for the bound.

use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::thread;

// ---------------------------------------------------------------- tuning ----
const KTHREADS: usize = 4; // reader+parser threads
const KBLOCK: usize = 4 << 20; // read block per thread
const KSLACK: usize = 4096; // buffer tail slack for 8-byte over-reads
const KMAX_LINE: usize = 96; // fast path assumption: every line is shorter
const KMAX_NAME: usize = 32; // slot name capacity

const KSEMI: u64 = 0x3B3B_3B3B_3B3B_3B3B; // ';' x8
const KONES: u64 = 0x0101_0101_0101_0101;
const KHIGH: u64 = 0x8080_8080_8080_8080;
const KMUL1: u64 = 0x9E37_79B9_7F4A_7C15;
const KMUL2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const KU: f64 = 1.110_223_024_625_156_5e-16; // 2^-53

#[inline(always)]
unsafe fn load64(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

#[inline(always)]
fn low_mask(k: usize) -> u64 {
    if k == 0 {
        0
    } else if k >= 8 {
        !0u64
    } else {
        !0u64 >> (64 - 8 * k)
    }
}

#[inline(always)]
unsafe fn hash_name(p: *const u8, len: usize) -> u64 {
    let h = if len <= 8 {
        (load64(p) & low_mask(len)).wrapping_mul(KMUL1)
    } else {
        (load64(p) ^ load64(p.add(len - 8)).wrapping_mul(KMUL2) ^ len as u64).wrapping_mul(KMUL1)
    };
    h ^ (h >> 32)
}

#[inline(always)]
unsafe fn name_eq(a: *const u8, b: *const u8, len: usize) -> bool {
    let mut i = 0;
    while i + 8 <= len {
        if load64(a.add(i)) != load64(b.add(i)) {
            return false;
        }
        i += 8;
    }
    if i < len {
        let m = low_mask(len - i);
        if (load64(a.add(i)) ^ load64(b.add(i))) & m != 0 {
            return false;
        }
    }
    true
}

// ------------------------------------------------------------- hash table ----
#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    sum: i64,
    mn: i32,
    mx: i32,
    cnt: u32,
    len: u32,
    name: [u8; KMAX_NAME],
}

const EMPTY_SLOT: Slot = Slot {
    hash: 0,
    sum: 0,
    mn: 0,
    mx: 0,
    cnt: 0,
    len: 0,
    name: [0; KMAX_NAME],
};

struct Table {
    slots: Vec<Slot>,
    mask: usize,
    used: usize,
}

impl Table {
    fn new() -> Table {
        Table {
            slots: vec![EMPTY_SLOT; 1024],
            mask: 1023,
            used: 0,
        }
    }

    #[inline(always)]
    fn add(&mut self, name: &[u8], hash: u64, t10: i32) {
        if self.used * 2 >= self.slots.len() {
            self.grow();
        }
        let len = name.len();
        let mut i = (hash as usize) & self.mask;
        let base = self.slots.as_mut_ptr();
        unsafe {
            loop {
                let s = &mut *base.add(i);
                if s.cnt == 0 {
                    s.hash = hash;
                    s.len = len as u32;
                    s.sum = t10 as i64;
                    s.mn = t10;
                    s.mx = t10;
                    s.cnt = 1;
                    s.name[..len].copy_from_slice(name);
                    self.used += 1;
                    return;
                }
                if s.hash == hash && s.len as usize == len && name_eq(s.name.as_ptr(), name.as_ptr(), len) {
                    s.sum += t10 as i64;
                    s.cnt += 1;
                    if t10 < s.mn {
                        s.mn = t10;
                    }
                    if t10 > s.mx {
                        s.mx = t10;
                    }
                    return;
                }
                i = (i + 1) & self.mask;
            }
        }
    }

    fn grow(&mut self) {
        let newlen = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![EMPTY_SLOT; newlen]);
        self.mask = self.slots.len() - 1;
        self.used = 0;
        for s in old.iter() {
            if s.cnt == 0 {
                continue;
            }
            let mut i = (s.hash as usize) & self.mask;
            while self.slots[i].cnt != 0 {
                i = (i + 1) & self.mask;
            }
            self.slots[i] = *s;
            self.used += 1;
        }
    }
}

// Names longer than KMAX_NAME (impossible for the harness station universe).
#[derive(Default)]
struct LongNameTable {
    map: std::collections::BTreeMap<Vec<u8>, Rec>,
}

impl LongNameTable {
    #[inline]
    fn add(&mut self, name: &[u8], _hash: u64, t10: i32) {
        let r = self.map.entry(name.to_vec()).or_insert_with(|| Rec {
            long_name: name.to_vec(),
            len: name.len(),
            mn: t10,
            mx: t10,
            ..Default::default()
        });
        r.sum += t10 as i64;
        r.cnt += 1;
        if t10 < r.mn {
            r.mn = t10;
        }
        if t10 > r.mx {
            r.mx = t10;
        }
    }
}

// One station aggregate.
#[derive(Default, Clone)]
struct Rec {
    long_name: Vec<u8>,
    name: [u8; KMAX_NAME],
    len: usize,
    sum: i64,
    mn: i32,
    mx: i32,
    cnt: u32,
}

impl Rec {
    #[inline]
    fn key(&self) -> &[u8] {
        if self.long_name.is_empty() {
            &self.name[..self.len]
        } else {
            &self.long_name
        }
    }
    fn absorb(&mut self, o: &Rec) {
        self.sum += o.sum;
        self.cnt += o.cnt;
        if o.mn < self.mn {
            self.mn = o.mn;
        }
        if o.mx > self.mx {
            self.mx = o.mx;
        }
    }
}

// ------------------------------------------------------------------- sink ----
trait Sink {
    fn add(&mut self, name: &[u8], hash: u64, t10: i32);
}

impl Sink for Table {
    #[inline(always)]
    fn add(&mut self, name: &[u8], hash: u64, t10: i32) {
        Table::add(self, name, hash, t10);
    }
}

impl Sink for LongNameTable {
    #[inline(always)]
    fn add(&mut self, name: &[u8], hash: u64, t10: i32) {
        LongNameTable::add(self, name, hash, t10);
    }
}

struct WorkerSink<'a> {
    table: &'a mut Table,
    longs: &'a mut LongNameTable,
}

impl Sink for WorkerSink<'_> {
    #[inline(always)]
    fn add(&mut self, name: &[u8], hash: u64, t10: i32) {
        if name.len() <= KMAX_NAME {
            self.table.add(name, hash, t10);
        } else {
            self.longs.add(name, hash, t10);
        }
    }
}

// ------------------------------------------------------------ line parser ----
unsafe fn find_byte(p: *const u8, n: usize, b: u8) -> Option<usize> {
    for i in 0..n {
        if *p.add(i) == b {
            return Some(i);
        }
    }
    None
}

// Reference-compatible parse of the single line [p, p+n) (trailing \r and \n
// are stripped like the reference does).
unsafe fn parse_one<S: Sink>(p: *const u8, n: usize, sink: &mut S) {
    let mut line_end = n;
    while line_end > 0 && (*p.add(line_end - 1) == b'\r' || *p.add(line_end - 1) == b'\n') {
        line_end -= 1;
    }
    let semi = match find_byte(p, line_end, b';') {
        Some(i) => i,
        None => return,
    };
    let mut q = semi + 1;
    let mut neg = false;
    if q < line_end && *p.add(q) == b'-' {
        neg = true;
        q += 1;
    }
    let mut ip: i64 = 0;
    while q < line_end && p.add(q).read() >= b'0' && p.add(q).read() <= b'9' {
        ip = ip * 10 + (*p.add(q) - b'0') as i64;
        q += 1;
    }
    let mut fp: i64 = 0;
    if q < line_end && *p.add(q) == b'.' {
        q += 1;
        if q < line_end && p.add(q).read() >= b'0' && p.add(q).read() <= b'9' {
            fp = (*p.add(q) - b'0') as i64;
        }
    }
    let mut t10 = ip * 10 + fp;
    if neg {
        t10 = -t10;
    }
    if t10 >= i32::MIN as i64 && t10 <= i32::MAX as i64 {
        let name = std::slice::from_raw_parts(p, semi);
        sink.add(name, hash_name(p, semi), t10 as i32);
    }
}

// Tolerant parse of one complete line starting at p (its '\n' is inside
// [p, end)).  Returns the position just past the '\n'.
unsafe fn slow_line<S: Sink>(p: *const u8, end: *const u8, sink: &mut S) -> *const u8 {
    let n = end as usize - p as usize;
    match find_byte(p, n, b'\n') {
        None => p,
        Some(i) => {
            parse_one(p, i, sink);
            p.add(i + 1)
        }
    }
}

// Parses complete lines in [buf, buf+n); returns how many bytes were consumed.
// The caller guarantees n counts whole lines only and that at least 24 bytes
// are readable past buf+n.
unsafe fn parse_lines<S: Sink>(buf: *const u8, n: usize, sink: &mut S) -> usize {
    let end = buf.add(n);
    let mut p = buf;
    while p < end {
        let mut m: u64 = 0;
        let mut q = p;
        for _ in 0..4 {
            let x = load64(q) ^ KSEMI;
            m = (x.wrapping_sub(KONES)) & !x & KHIGH;
            if m != 0 {
                break;
            }
            q = q.add(8);
        }
        let mut ok = false;
        let mut t10: i32 = 0;
        let mut adv: usize = 0;
        let mut len: usize = 0;
        let mut t = p;
        if m != 0 {
            len = (q as usize - p as usize) + (m.trailing_zeros() as usize >> 3);
            t = p.add(len + 1);
            let w = load64(t);
            let neg = (w & 0xFF) == b'-' as u64;
            let v = if neg { w >> 8 } else { w };
            let b0 = (v & 0xFF) as u32;
            let b1 = ((v >> 8) & 0xFF) as u32;
            let b2 = ((v >> 16) & 0xFF) as u32;
            let d0 = b0.wrapping_sub(b'0' as u32);
            let d1 = b1.wrapping_sub(b'0' as u32);
            if b2 == b'.' as u32 && d0 <= 9 && d1 <= 9 {
                let d3 = (((v >> 24) & 0xFF) as u32).wrapping_sub(b'0' as u32);
                if d3 <= 9 {
                    t10 = ((d0 * 10 + d1) * 10 + d3) as i32;
                    adv = 5 + neg as usize;
                    ok = true;
                }
            } else if b1 == b'.' as u32 && d0 <= 9 {
                let d2 = b2.wrapping_sub(b'0' as u32);
                if d2 <= 9 {
                    t10 = (d0 * 10 + d2) as i32;
                    adv = 4 + neg as usize;
                    ok = true;
                }
            } else {
                let d2 = b2.wrapping_sub(b'0' as u32);
                let b3 = ((v >> 24) & 0xFF) as u32;
                let d4 = (((v >> 32) & 0xFF) as u32).wrapping_sub(b'0' as u32);
                if b3 == b'.' as u32 && d0 <= 9 && d1 <= 9 && d2 <= 9 && d4 <= 9 {
                    t10 = (((d0 * 10 + d1) * 10 + d2) * 10 + d4) as i32;
                    adv = 6 + neg as usize;
                    ok = true;
                }
            }
            if ok && *t.add(adv - 1) != b'\n' {
                ok = false;
            }
            if ok && neg {
                t10 = -t10;
            }
        }
        if ok {
            let name = std::slice::from_raw_parts(p, len);
            sink.add(name, hash_name(p, len), t10);
            p = t.add(adv);
            continue;
        }
        let nx = slow_line(p, end, sink);
        if nx == p {
            return p as usize - buf as usize;
        }
        p = nx;
    }
    p as usize - buf as usize
}

// --------------------------------------------------------------- I/O layer ----
fn pread_full(file: &File, buf: &mut [u8], off: u64) -> usize {
    let mut total = 0;
    while total < buf.len() {
        match file.read_at(&mut buf[total..], off + total as u64) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    total
}

// Streams [start, end) in KBLOCK chunks, feeding every complete line to sink.
// `end` is a line boundary or the end of file.
fn stream_range<S: Sink>(file: &File, start: usize, end: usize, sink: &mut S) {
    let mut buf = vec![0u8; KBLOCK + KSLACK];
    let mut pos = start;
    let mut carry = 0usize;
    while pos < end {
        if carry + KBLOCK + KSLACK > buf.len() {
            buf.resize((carry + KBLOCK + KSLACK) * 2, 0);
        }
        let want = std::cmp::min(KBLOCK, end - pos);
        let got = pread_full(file, &mut buf[carry..carry + want], pos as u64);
        if got == 0 {
            break;
        }
        pos += got;
        let n = carry + got;
        let safe = if n > KMAX_LINE { n - KMAX_LINE } else { 0 };
        let consumed = unsafe { parse_lines(buf.as_ptr(), safe, sink) };
        carry = n - consumed;
        if carry > 0 {
            buf.copy_within(consumed..consumed + carry, 0);
        }
    }
    if carry > 0 {
        // All remaining bytes are complete lines except possibly the last one.
        let consumed = unsafe { parse_lines(buf.as_ptr(), carry, sink) };
        if consumed < carry {
            unsafe { parse_one(buf.as_ptr().add(consumed), carry - consumed, sink) };
        }
    }
}

// -------------------------------------------------------------- formatting ----
fn append_i10(out: &mut String, v10: i64) {
    let mut a = v10;
    if a < 0 {
        out.push('-');
        a = -a;
    }
    let ip = a / 10;
    let fp = (a % 10) as u8;
    let mut tmp = [0u8; 24];
    let mut n = 0;
    let mut v = ip;
    if v == 0 {
        tmp[n] = b'0';
        n += 1;
    }
    while v > 0 {
        tmp[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
    }
    while n > 0 {
        n -= 1;
        out.push(tmp[n] as char);
    }
    out.push('.');
    out.push((b'0' + fp) as char);
}

// Sequential scan accumulating a reference-identical double sum for the few
// stations whose exact mean is ambiguous.
struct RiskySink<'a> {
    items: &'a mut Vec<RiskyItem>,
}

struct RiskyItem {
    name: Vec<u8>,
    sum: f64,
    count: u64,
}

impl Sink for RiskySink<'_> {
    #[inline]
    fn add(&mut self, name: &[u8], _hash: u64, t10: i32) {
        for it in self.items.iter_mut() {
            if it.name == name {
                it.sum += t10 as f64 / 10.0;
                it.count += 1;
                return;
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: onebrc <input_file>");
        std::process::exit(1);
    }
    let file = match File::open(&args[1]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {}", args[1], e);
            std::process::exit(1);
        }
    };
    let fsize = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(e) => {
            eprintln!("metadata: {}", e);
            std::process::exit(1);
        }
    };
    if fsize == 0 {
        print!("{{}}\n");
        return;
    }

    // Split the file into newline-aligned slices.
    let nthreads = if fsize < KBLOCK { 1 } else { KTHREADS };
    let mut bounds = vec![0usize; nthreads + 1];
    bounds[nthreads] = fsize;
    for i in 1..nthreads {
        let b = fsize / nthreads * i;
        let mut probe = b;
        let mut nlpos = fsize;
        let mut tmp = [0u8; 256];
        while probe < fsize {
            let got = pread_full(&file, &mut tmp, probe as u64);
            if got == 0 {
                break;
            }
            match unsafe { find_byte(tmp.as_ptr(), got, b'\n') } {
                Some(k) => {
                    nlpos = probe + k + 1;
                    break;
                }
                None => probe += got,
            }
        }
        bounds[i] = nlpos;
    }
    for i in 1..nthreads {
        if bounds[i] < bounds[i - 1] {
            bounds[i] = bounds[i - 1];
        }
    }

    let results: Vec<(Table, LongNameTable)> = thread::scope(|s| {
        let mut handles = Vec::with_capacity(nthreads);
        for i in 0..nthreads {
            let (start, end) = (bounds[i], bounds[i + 1]);
            let f = &file;
            handles.push(s.spawn(move || {
                let mut table = Table::new();
                let mut longs = LongNameTable::default();
                if start < end {
                    {
                        let mut sink = WorkerSink {
                            table: &mut table,
                            longs: &mut longs,
                        };
                        stream_range(f, start, end, &mut sink);
                    }
                }
                (table, longs)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge per-thread tables.
    let mut recs: Vec<Rec> = Vec::with_capacity(1024);
    for (table, longs) in results.iter() {
        for s in table.slots.iter() {
            if s.cnt == 0 {
                continue;
            }
            recs.push(Rec {
                long_name: Vec::new(),
                name: s.name,
                len: s.len as usize,
                sum: s.sum,
                mn: s.mn,
                mx: s.mx,
                cnt: s.cnt,
            });
        }
        for r in longs.map.values() {
            recs.push(r.clone());
        }
    }

    recs.sort_by(|a, b| a.key().cmp(b.key()));
    let mut merged: Vec<Rec> = Vec::with_capacity(recs.len());
    for r in recs.into_iter() {
        match merged.last_mut() {
            Some(m) if m.key() == r.key() => m.absorb(&r),
            _ => merged.push(r),
        }
    }

    // Mean of every station, plus the ambiguous ones.
    let mut mean10: Vec<i64> = vec![0; merged.len()];
    let mut risky: Vec<RiskyItem> = Vec::new();
    for (i, e) in merged.iter().enumerate() {
        let n = e.cnt as i64;
        let two_n = 2 * n;
        let num = 2 * e.sum + n;
        let mut q = num / two_n;
        if num % two_n != 0 && num < 0 {
            q -= 1;
        }
        mean10[i] = q;
        let r = ((num % two_n) + two_n) % two_n;
        let dist = std::cmp::min(r, two_n - r);
        let maxabs = std::cmp::max(e.mn.abs(), e.mx.abs()) as f64;
        let margin = 8.0 * KU * (n as f64 * maxabs + 8.0 * (e.sum.abs() as f64 / n as f64 + 1.0));
        if dist as f64 <= margin * two_n as f64 {
            risky.push(RiskyItem {
                name: e.key().to_vec(),
                sum: 0.0,
                count: 0,
            });
        }
    }
    if !risky.is_empty() {
        {
            let mut sink = RiskySink { items: &mut risky };
            stream_range(&file, 0, fsize, &mut sink);
        }
        for (i, e) in merged.iter().enumerate() {
            for it in risky.iter() {
                if it.name == e.key() {
                    mean10[i] =
                        ((it.sum / e.cnt as f64) * 10.0 + 0.5).floor() as i64;
                    break;
                }
            }
        }
    }

    let mut out = String::with_capacity(1 << 14);
    out.push('{');
    for (i, e) in merged.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(unsafe { std::str::from_utf8_unchecked(e.key()) });
        out.push('=');
        append_i10(&mut out, e.mn as i64);
        out.push('/');
        append_i10(&mut out, mean10[i]);
        out.push('/');
        append_i10(&mut out, e.mx as i64);
    }
    out.push_str("}\n");
    print!("{}", out);
}
