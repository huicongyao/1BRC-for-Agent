// 1BRC Rust track - parallel, I/O-bound implementation.
//
//   onebrc <input_file>   -> result to stdout
//
// Design notes (measurements in EXPERIMENTS.md)
// --------------------------------------------
// The 1B input does not fit in the page cache, so wall clock is dominated by
// reading ~13.8 GB from the SSD:
//
//   read(2), 1 stream, F_NOCACHE, 4096-aligned offset+buffer  ~3.2-3.5 GB/s
//   read(2), 1 stream, F_NOCACHE, misaligned                   ~2.0 GB/s
//   read(2), 1 stream, buffered (page cache)                   ~2.25 GB/s
//   2/4/8 parallel streams                                     no gain, often worse
//
// So: *one* reader thread streams the file sequentially in large page-aligned
// blocks with F_NOCACHE, and KPARSERS worker threads parse those blocks in
// parallel.  The reader splits every block at its last line boundary, so
// parsers only see whole lines; the partial line is parked in KCARRY_AREA
// bytes in front of the aligned read area, keeping parser input contiguous
// without ever misaligning the read destination.
//
// std only: FileExt::read_at for the reads, std::thread::scope for the
// workers, std::alloc for the 4096-byte-aligned buffers.  The single F_NOCACHE
// fcntl is declared as a direct extern "C" binding to the C library that std
// already links (std has no fcntl wrapper, and the flag is worth ~15-30% of
// the whole run); no crate is involved.
//
// Parsing uses 8-byte SWAR loads and a tolerant scalar fallback that mirrors
// harness/src/reference.c.  Mean rounding: the reference accumulates `sum` as
// a double in file order, which parallel accumulation cannot reproduce, so the
// exact rational mean is used instead and every station landing inside the
// reference's rounding error of a .5 boundary is recomputed with a sequential
// double scan in file order (see the C++ track notes for the bound).

use std::alloc::{alloc, dealloc, Layout};
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::sync::{Condvar, Mutex};
use std::thread;

// ---------------------------------------------------------------- tuning ----
const KPARSERS: usize = 10; // worker threads
const KSLOTS: usize = 12; // buffer pool (KPARSERS + lookahead)
const KBLOCK: usize = 16 << 20; // streamed read block
const KCARRY: usize = 4096; // max partial line carried between blocks
const KALIGN: usize = 4096; // F_NOCACHE DMA alignment
const KCARRY_AREA: usize = 8192; // space reserved in front of the read area
const KSLACK: usize = 4096; // buffer tail slack for 8-byte over-reads
const KMAX_NAME: usize = 32; // slot name capacity
const KNOCACHE_MIN: usize = 256 << 20; // bypass the cache for big inputs

const KSEMI: u64 = 0x3B3B_3B3B_3B3B_3B3B; // ';' x8
const KONES: u64 = 0x0101_0101_0101_0101;
const KHIGH: u64 = 0x8080_8080_8080_8080;
const KMUL1: u64 = 0x9E37_79B9_7F4A_7C15;
const KMUL2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const KU: f64 = 1.110_223_024_625_156_5e-16; // 2^-53
const F_NOCACHE: i32 = 48; // <sys/fcntl.h>, macOS

extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

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
                if s.hash == hash
                    && s.len as usize == len
                    && name_eq(s.name.as_ptr(), name.as_ptr(), len)
                {
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
    while q < line_end && *p.add(q) >= b'0' && *p.add(q) <= b'9' {
        ip = ip * 10 + (*p.add(q) - b'0') as i64;
        q += 1;
    }
    let mut fp: i64 = 0;
    if q < line_end && *p.add(q) == b'.' {
        q += 1;
        if q < line_end && *p.add(q) >= b'0' && *p.add(q) <= b'9' {
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

// Page-aligned buffer handed from the reader to the parsers.
struct Block {
    raw: *mut u8,
    cap: usize,
    data: *const u8, // parser input (carry area + fresh bytes)
    len: usize,
    last: bool,
}

unsafe impl Send for Block {}

impl Block {
    fn new() -> Block {
        let mut b = Block {
            raw: std::ptr::null_mut(),
            cap: 0,
            data: std::ptr::null(),
            len: 0,
            last: false,
        };
        b.reserve(KCARRY_AREA + KBLOCK + KSLACK);
        b
    }
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(cap, KALIGN).unwrap()
    }
    fn reserve(&mut self, need: usize) {
        if self.cap >= need {
            return;
        }
        let rounded = (need + KALIGN - 1) & !(KALIGN - 1);
        unsafe {
            let p = alloc(Block::layout(rounded));
            if p.is_null() {
                eprintln!("out of memory");
                std::process::exit(1);
            }
            if !self.raw.is_null() {
                dealloc(self.raw, Block::layout(self.cap));
            }
            self.raw = p;
            self.cap = rounded;
        }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { dealloc(self.raw, Block::layout(self.cap)) };
        }
    }
}

// Bounded hand-off queue with a buffer pool.
struct QueueState {
    free: Vec<Block>,
    ready: Vec<Block>,
    done: bool,
}

struct BlockQueue {
    state: Mutex<QueueState>,
    cv_free: Condvar,
    cv_ready: Condvar,
}

impl BlockQueue {
    fn new() -> BlockQueue {
        let mut free = Vec::with_capacity(KSLOTS);
        for _ in 0..KSLOTS {
            free.push(Block::new());
        }
        BlockQueue {
            state: Mutex::new(QueueState {
                free,
                ready: Vec::new(),
                done: false,
            }),
            cv_free: Condvar::new(),
            cv_ready: Condvar::new(),
        }
    }
    fn acquire(&self) -> Block {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(b) = st.free.pop() {
                return b;
            }
            st = self.cv_free.wait(st).unwrap();
        }
    }
    fn publish(&self, b: Block) {
        let mut st = self.state.lock().unwrap();
        st.ready.push(b);
        drop(st);
        self.cv_ready.notify_one();
    }
    fn take(&self) -> Option<Block> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(b) = st.ready.pop() {
                return Some(b);
            }
            if st.done {
                return None;
            }
            st = self.cv_ready.wait(st).unwrap();
        }
    }
    fn release(&self, b: Block) {
        let mut st = self.state.lock().unwrap();
        st.free.push(b);
        drop(st);
        self.cv_free.notify_one();
    }
    fn finish(&self) {
        let mut st = self.state.lock().unwrap();
        st.done = true;
        drop(st);
        self.cv_ready.notify_all();
    }
}

// Index just past the last '\n' in [buf, buf+n), or 0 if there is none within
// the last KCARRY bytes (then the reader accumulates that line separately).
unsafe fn last_line_end(buf: *const u8, n: usize) -> usize {
    let lo = if n > KCARRY { n - KCARRY } else { 0 };
    let mut i = n;
    while i > lo {
        if *buf.add(i - 1) == b'\n' {
            return i;
        }
        i -= 1;
    }
    0
}

// The single sequential reader: F_NOCACHE, large page-aligned blocks.
fn reader(file: &File, fsize: usize, q: &BlockQueue, giant: &mut WorkerSink) {
    let mut longline: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut carry = 0usize;
    let mut cur = q.acquire();
    loop {
        let want = std::cmp::min(KBLOCK, fsize - pos);
        let rdst = unsafe { cur.raw.add(KCARRY_AREA) };
        let got = {
            let buf = unsafe { std::slice::from_raw_parts_mut(rdst, want) };
            pread_full(file, buf, pos as u64)
        };
        pos += got;
        let at_end = got == 0 || pos >= fsize;

        if !longline.is_empty() {
            // A line longer than KCARRY is being accumulated.
            longline.extend_from_slice(unsafe { std::slice::from_raw_parts(rdst, got) });
            let extra = 64;
            longline.resize(longline.len() + extra, 0);
            let stop = unsafe { parse_lines(longline.as_ptr(), longline.len() - extra, giant) };
            longline.truncate(longline.len() - extra);
            if stop > 0 {
                longline.drain(..stop);
            }
            if at_end {
                if !longline.is_empty() {
                    unsafe { parse_one(longline.as_ptr(), longline.len(), giant) };
                }
                break;
            }
            continue;
        }

        let data = unsafe { rdst.sub(carry) };
        let n = carry + got;
        let cut = if at_end {
            n
        } else {
            unsafe { last_line_end(data, n) }
        };
        if !at_end && cut == 0 {
            longline.extend_from_slice(unsafe { std::slice::from_raw_parts(data, n) });
            carry = 0;
            continue;
        }
        cur.data = data;
        cur.len = cut;
        cur.last = at_end;
        q.publish(cur);
        if at_end {
            break;
        }
        carry = n - cut;
        cur = q.acquire();
        if carry > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.add(cut),
                    cur.raw.add(KCARRY_AREA - carry),
                    carry,
                )
            };
        }
    }
}

fn parser(q: &BlockQueue, table: &mut Table, longs: &mut LongNameTable) {
    let mut sink = WorkerSink { table, longs };
    while let Some(b) = q.take() {
        if b.len > 0 {
            let consumed = unsafe { parse_lines(b.data, b.len, &mut sink) };
            if consumed < b.len {
                let rest = b.data as usize + consumed;
                let total = b.data as usize + b.len;
                unsafe { parse_one(rest as *const u8, total - rest, &mut sink) };
            }
        }
        q.release(b);
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

// Streams [start, end) in KBLOCK chunks, feeding every complete line to sink.
// Used off the hot path (ambiguous-mean rescan).
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
        let safe = if n > 96 { n - 96 } else { 0 };
        let consumed = unsafe { parse_lines(buf.as_ptr(), safe, sink) };
        carry = n - consumed;
        if carry > 0 {
            buf.copy_within(consumed..consumed + carry, 0);
        }
    }
    if carry > 0 {
        let consumed = unsafe { parse_lines(buf.as_ptr(), carry, sink) };
        if consumed < carry {
            unsafe { parse_one(buf.as_ptr().add(consumed), carry - consumed, sink) };
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
    if fsize >= KNOCACHE_MIN {
        // Read once, sequentially: keep the data out of the buffer cache.
        unsafe {
            fcntl(file.as_raw_fd(), F_NOCACHE, 1);
        }
    }

    let queue = BlockQueue::new();
    let mut giant_table = Table::new();
    let mut giant_longs = LongNameTable::default();

    let results: Vec<(Table, LongNameTable)> = thread::scope(|s| {
        let mut handles = Vec::with_capacity(KPARSERS);
        for _ in 0..KPARSERS {
            let q = &queue;
            handles.push(s.spawn(move || {
                let mut table = Table::new();
                let mut longs = LongNameTable::default();
                parser(q, &mut table, &mut longs);
                (table, longs)
            }));
        }
        {
            let mut giant = WorkerSink {
                table: &mut giant_table,
                longs: &mut giant_longs,
            };
            reader(&file, fsize, &queue, &mut giant);
        }
        queue.finish();
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
    for s in giant_table.slots.iter() {
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
    for r in giant_longs.map.values() {
        recs.push(r.clone());
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
                    mean10[i] = ((it.sum / e.cnt as f64) * 10.0 + 0.5).floor() as i64;
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
