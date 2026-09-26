/*
 * 1BRC C++ track - parallel, I/O-bound implementation.
 *
 *  1brc <input_file>   -> result to stdout
 *
 * Design notes
 * ------------
 * The 1B input does not fit in the page cache, so wall clock is dominated by
 * reading ~13.8 GB from the SSD.  Measurements on this machine:
 *
 *   read(2), 1 stream, F_NOCACHE, 4096-aligned offset+buffer  ~3.2-3.5 GB/s
 *   read(2), 1 stream, F_NOCACHE, misaligned                   ~2.0 GB/s
 *   read(2), 1 stream, buffered (page cache)                   ~2.25 GB/s
 *   mmap + page faults                                         ~0.59 GB/s
 *   2/4/8 parallel streams                                     no gain, often worse
 *
 * So the fast path is: *one* reader thread streaming the file sequentially in
 * large page-aligned blocks with F_NOCACHE (no cache pollution, no double
 * buffering), and kParsers worker threads parsing those blocks in parallel.
 * The reader splits each block at the last line boundary so parsers only ever
 * see whole lines; the partial line is parked in the kCarryArea bytes in front
 * of the aligned read area, which keeps the parser input contiguous without
 * ever misaligning the F_NOCACHE destination.
 *
 * Parsing uses 8-byte SWAR loads: locate ';' with a haszero trick, then decode
 * the canonical temperature forms [-]D.D / [-]DD.D / [-]DDD.D from a single
 * 8-byte load.  Anything else falls back to a tolerant scalar parser that
 * mirrors harness/src/reference.c.  Aggregation keeps per-thread
 * open-addressing tables (64-byte slots) of exact integer tenths.
 *
 * Mean rounding
 * -------------
 * The reference accumulates `sum` as a double in file order and computes
 * mean = floor((sum / count) * 10 + 0.5) / 10.  Parallel accumulation cannot
 * reproduce that summation order, so this implementation uses the exact
 * rational value floor((2*S + n) / (2*n)), S = exact sum of tenths, n = count.
 * The two agree unless the exact value lands inside the reference's rounding
 * error of a .5 boundary; that window is bounded by
 *     |dQ| <= 8 * u * (n * max|t10| + 8 * (|S|/n + 1)),   u = 2^-53
 * (per-add bound + per-value conversion + final ops).  Every station inside it
 * is recomputed with a sequential double scan in file order, exactly like the
 * reference, so the output stays byte-identical.  For the harness inputs the
 * window is ~1e-6 wide, so it triggers with probability ~1e-3.
 */
#include <algorithm>
#include <cerrno>
#include <cmath>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <fcntl.h>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <unistd.h>
#include <vector>

namespace {

// ---------------------------------------------------------------- tuning ----
constexpr int      kParsers = 10;         // worker threads
constexpr int      kSlots   = 12;         // buffer pool (kParsers + lookahead)
constexpr size_t   kBlock   = 16u << 20;  // streamed read block
constexpr size_t   kCarry   = 4096;       // max partial line carried between blocks
constexpr size_t   kAlign   = 4096;       // F_NOCACHE DMA alignment
constexpr size_t   kCarryArea = 8192;     // space reserved in front of the read area
constexpr size_t   kSlack   = 4096;       // buffer tail slack for 8-byte over-reads
constexpr uint32_t kMaxLine = 96;         // fast path assumption: every line is shorter
constexpr uint32_t kMaxName = 32;         // slot name capacity
constexpr size_t   kNoCacheMin = 256u << 20;  // bypass the cache for big inputs

constexpr uint64_t kSemi = 0x3B3B3B3B3B3B3B3BULL;  // ';' x8
constexpr uint64_t kOnes = 0x0101010101010101ULL;
constexpr uint64_t kHigh = 0x8080808080808080ULL;
constexpr uint64_t kMul1 = 0x9E3779B97F4A7C15ULL;
constexpr uint64_t kMul2 = 0xC2B2AE3D27D4EB4FULL;
constexpr double   kU    = 1.1102230246251565e-16;  // 2^-53

inline uint64_t load64(const void *p) {
    uint64_t v;
    std::memcpy(&v, p, 8);
    return v;
}
inline uint64_t low_mask(uint32_t k) {
    if (k == 0) return 0;
    if (k >= 8) return ~0ULL;
    return ~0ULL >> (64 - 8 * k);
}

// ------------------------------------------------------------- hash table ----
struct Slot {
    uint64_t hash;
    int64_t  sum;      // exact sum of tenths
    int32_t  mn, mx;   // min/max in tenths
    uint32_t cnt;      // 0 == empty
    uint32_t len;
    uint8_t  name[kMaxName];
};
static_assert(sizeof(Slot) == 64, "slot should be one cache line");

inline uint64_t hash_name(const uint8_t *p, uint32_t len) {
    uint64_t h;
    if (len <= 8) {
        h = (load64(p) & low_mask(len)) * kMul1;
    } else {
        h = (load64(p) ^ (load64(p + len - 8) * kMul2) ^ (uint64_t)len) * kMul1;
    }
    return h ^ (h >> 32);
}

inline bool name_eq(const uint8_t *a, const uint8_t *b, uint32_t len) {
    uint32_t i = 0;
    for (; i + 8 <= len; i += 8) {
        if (load64(a + i) != load64(b + i)) return false;
    }
    if (i < len) {
        uint64_t m = low_mask(len - i);
        if (((load64(a + i) ^ load64(b + i)) & m) != 0) return false;
    }
    return true;
}

struct Rec {  // one station aggregate
    std::string long_name;  // non-empty iff len > kMaxName
    uint8_t  name[kMaxName];
    uint32_t len = 0;
    int64_t  sum = 0;
    int32_t  mn = INT32_MAX;
    int32_t  mx = INT32_MIN;
    uint32_t cnt = 0;
    const uint8_t *key() const {
        return long_name.empty() ? name : (const uint8_t *)long_name.data();
    }
    void absorb(const Rec &o) {
        sum += o.sum;
        cnt += o.cnt;
        if (o.mn < mn) mn = o.mn;
        if (o.mx > mx) mx = o.mx;
    }
};

// Names longer than kMaxName (impossible for the harness station universe).
class LongNameTable {
  public:
    inline void add(const uint8_t *name, uint32_t len, uint64_t, int32_t t10) {
        Rec &r = map_[std::string((const char *)name, len)];
        if (r.cnt == 0) {
            r.long_name.assign((const char *)name, len);
            r.len = len;
            r.mn = r.mx = t10;
        }
        r.sum += t10;
        r.cnt++;
        if (t10 < r.mn) r.mn = t10;
        if (t10 > r.mx) r.mx = t10;
    }
    const std::map<std::string, Rec> &map() const { return map_; }

  private:
    std::map<std::string, Rec> map_;
};

// Per-thread aggregation table keyed by station name.
class Table {
  public:
    void init() {
        slots_.assign(kInitial, Slot{});
        mask_ = kInitial - 1;
        used_ = 0;
    }
    const std::vector<Slot> &slots() const { return slots_; }

    inline void add(const uint8_t *name, uint32_t len, uint64_t h, int32_t t10) {
        if (used_ * 2 >= slots_.size()) grow();
        uint32_t i = (uint32_t)h & mask_;
        for (;;) {
            Slot &s = slots_[i];
            if (s.cnt == 0) {
                s.hash = h;
                s.len = len;
                std::memcpy(s.name, name, len);
                s.sum = t10;
                s.mn = t10;
                s.mx = t10;
                s.cnt = 1;
                used_++;
                return;
            }
            if (s.hash == h && s.len == len && name_eq(s.name, name, len)) {
                s.sum += t10;
                s.cnt++;
                if (t10 < s.mn) s.mn = t10;
                if (t10 > s.mx) s.mx = t10;
                return;
            }
            i = (i + 1) & mask_;
        }
    }

    // Doubles the table, keeping the load factor below 50%.
    void grow() {
        std::vector<Slot> old;
        old.swap(slots_);
        slots_.assign(old.size() * 2, Slot{});
        mask_ = (uint32_t)slots_.size() - 1;
        used_ = 0;
        for (const Slot &s : old) {
            if (!s.cnt) continue;
            uint32_t i = (uint32_t)s.hash & mask_;
            while (slots_[i].cnt) i = (i + 1) & mask_;
            slots_[i] = s;
            used_++;
        }
    }

  private:
    static constexpr uint32_t kInitial = 1024;
    std::vector<Slot> slots_;
    uint32_t mask_ = 0;
    uint32_t used_ = 0;
};

// ------------------------------------------------------------ line parser ----
// Reference-compatible parse of one line whose content is [name, line_end)
// (trailing \r and \n already stripped by the caller).
template <typename Sink>
void parse_one(const uint8_t *name, const uint8_t *line_end, Sink &sink) {
    const uint8_t *semi = (const uint8_t *)std::memchr(name, ';', (size_t)(line_end - name));
    if (!semi) return;
    const uint8_t *q = semi + 1;
    bool neg = false;
    if (q < line_end && *q == '-') {
        neg = true;
        q++;
    }
    int64_t ip = 0;
    while (q < line_end && *q >= '0' && *q <= '9') ip = ip * 10 + (*q++ - '0');
    int64_t fp = 0;
    if (q < line_end && *q == '.') {
        q++;
        if (q < line_end && *q >= '0' && *q <= '9') fp = *q++ - '0';
    }
    int64_t t10 = ip * 10 + fp;
    if (neg) t10 = -t10;
    if (t10 >= INT32_MIN && t10 <= INT32_MAX) {
        uint32_t len = (uint32_t)(semi - name);
        sink.add(name, len, hash_name(name, len), (int32_t)t10);
    }
}

inline const uint8_t *strip_eol(const uint8_t *p, const uint8_t *line_end) {
    while (line_end > p && (line_end[-1] == '\r' || line_end[-1] == '\n')) line_end--;
    return line_end;
}

// Tolerant parser for one complete line starting at p, the '\n' being inside
// [p, end).  Returns the position just past that '\n', or `p` when the buffer
// ends inside the line (the caller must supply more data).
template <typename Sink>
const uint8_t *slow_line(const uint8_t *p, const uint8_t *end, Sink &sink) {
    const uint8_t *nl = (const uint8_t *)std::memchr(p, '\n', (size_t)(end - p));
    if (!nl) return p;
    parse_one(p, strip_eol(p, nl), sink);
    return nl + 1;
}

// Parses complete lines in [buf, buf+n).  The caller guarantees that n counts
// whole lines only and that at least 24 bytes are readable past buf+n.
// Returns the end of the last complete line.
template <typename Sink>
const uint8_t *parse_lines(const uint8_t *buf, size_t n, Sink &sink) {
    const uint8_t *p = buf;
    const uint8_t *end = buf + n;
    while (p < end) {
        // Locate ';' within the first 32 bytes of the line.
        uint64_t m = 0;
        const uint8_t *q = p;
        for (int i = 0; i < 4; i++) {
            uint64_t x = load64(q) ^ kSemi;
            m = (x - kOnes) & ~x & kHigh;
            if (m) break;
            q += 8;
        }
        uint32_t adv = 0;
        int32_t t10 = 0;
        bool ok = false;
        uint32_t len = 0;
        const uint8_t *t = p;
        if (m) {
            len = (uint32_t)(q - p) + (uint32_t)(__builtin_ctzll(m) >> 3);
            t = p + len + 1;
            uint64_t w = load64(t);
            uint32_t neg = (uint32_t)((w & 0xFF) == '-');
            uint64_t v = neg ? (w >> 8) : w;
            uint32_t b0 = (uint32_t)(v & 0xFF);
            uint32_t b1 = (uint32_t)((v >> 8) & 0xFF);
            uint32_t b2 = (uint32_t)((v >> 16) & 0xFF);
            if (b2 == '.' && b0 - '0' <= 9 && b1 - '0' <= 9) {
                uint32_t b3 = (uint32_t)((v >> 24) & 0xFF);  // fraction digit
                if (b3 - '0' <= 9) {
                    t10 = (int32_t)(((b0 - '0') * 10 + (b1 - '0')) * 10 + (b3 - '0'));
                    adv = 5 + neg;
                    ok = true;
                }
            } else if (b1 == '.' && b0 - '0' <= 9) {
                uint32_t d = b2 - '0';
                if (d <= 9) {
                    t10 = (int32_t)((b0 - '0') * 10 + d);
                    adv = 4 + neg;
                    ok = true;
                }
            } else {
                uint32_t d0 = b0 - '0', d1 = b1 - '0', d2 = b2 - '0';
                uint32_t b3 = (uint32_t)((v >> 24) & 0xFF);
                uint32_t b4 = (uint32_t)((v >> 32) & 0xFF);
                if (b3 == '.' && d0 <= 9 && d1 <= 9 && d2 <= 9 && b4 - '0' <= 9) {
                    t10 = (int32_t)(((d0 * 10 + d1) * 10 + d2) * 10 + (b4 - '0'));
                    adv = 6 + neg;
                    ok = true;
                }
            }
            if (ok && t[adv - 1] != '\n') ok = false;
            if (ok && neg) t10 = -t10;
        }
        if (ok) {
            sink.add(p, len, hash_name(p, len), t10);
            p = t + adv;
            continue;
        }
        const uint8_t *nx = slow_line(p, end, sink);
        if (nx == p) return p;
        p = nx;
    }
    return p;
}

// --------------------------------------------------------------- I/O layer ----
bool pread_full(int fd, void *buf, size_t n, size_t off, size_t *got) {
    uint8_t *p = (uint8_t *)buf;
    size_t total = 0;
    while (total < n) {
        ssize_t r = pread(fd, p + total, n - total, (off_t)(off + total));
        if (r < 0) {
            if (errno == EINTR) continue;
            *got = total;
            return false;
        }
        if (r == 0) break;
        total += (size_t)r;
    }
    *got = total;
    return true;
}

struct WorkerSink {
    Table *table;
    LongNameTable *longs;
    inline void add(const uint8_t *name, uint32_t len, uint64_t h, int32_t t10) {
        if (len <= kMaxName) {
            table->add(name, len, h, t10);
        } else {
            longs->add(name, len, h, t10);
        }
    }
};

// Buffer handed from the reader to the parsers.
struct Block {
    uint8_t *raw = nullptr;         // page-aligned allocation
    size_t   cap = 0;
    const uint8_t *data = nullptr;  // parser input (carry + fresh bytes)
    size_t   len = 0;
    bool     last = false;
    ~Block() { std::free(raw); }
    void reserve(size_t need) {
        if (cap >= need) return;
        size_t rounded = (need + kAlign - 1) & ~(kAlign - 1);
        void *p = nullptr;
        if (posix_memalign(&p, kAlign, rounded) != 0) {
            std::fprintf(stderr, "out of memory\n");
            std::exit(1);
        }
        std::free(raw);
        raw = (uint8_t *)p;
        cap = rounded;
    }
};

// Bounded hand-off queue with a buffer pool.
class BlockQueue {
  public:
    BlockQueue() {
        for (int i = 0; i < kSlots; i++) {
            Block *b = new Block();
            b->reserve(kCarryArea + kBlock + kSlack);
            free_.push_back(b);
        }
    }
    ~BlockQueue() {
        for (Block *b : free_) delete b;
        for (Block *b : ready_) delete b;
    }
    Block *acquire() {
        std::unique_lock<std::mutex> lk(m_);
        cv_free_.wait(lk, [this] { return !free_.empty(); });
        Block *b = free_.back();
        free_.pop_back();
        return b;
    }
    void publish(Block *b) {
        {
            std::lock_guard<std::mutex> lk(m_);
            ready_.push_back(b);
        }
        cv_ready_.notify_one();
    }
    Block *take() {
        std::unique_lock<std::mutex> lk(m_);
        cv_ready_.wait(lk, [this] { return !ready_.empty() || done_; });
        if (ready_.empty()) return nullptr;
        Block *b = ready_.front();
        ready_.pop_front();
        return b;
    }
    void release(Block *b) {
        {
            std::lock_guard<std::mutex> lk(m_);
            free_.push_back(b);
        }
        cv_free_.notify_one();
    }
    void finish() {
        {
            std::lock_guard<std::mutex> lk(m_);
            done_ = true;
        }
        cv_ready_.notify_all();
    }

  private:
    std::mutex m_;
    std::condition_variable cv_free_, cv_ready_;
    std::deque<Block *> free_, ready_;
    bool done_ = false;
};

// Index just past the last '\n' in [buf, buf+n), or 0 if there is none within
// the last kCarry bytes (then the reader accumulates that line separately).
inline size_t last_line_end(const uint8_t *buf, size_t n) {
    size_t lo = n > kCarry ? n - kCarry : 0;
    size_t i = n;
    while (i > lo) {
        if (buf[i - 1] == '\n') return i;
        i--;
    }
    return 0;
}

// The single sequential reader: F_NOCACHE, large page-aligned blocks.
void reader(int fd, size_t fsize, BlockQueue &q, WorkerSink &giant) {
    std::vector<uint8_t> longline;  // only for lines longer than kCarry
    size_t pos = 0;
    size_t carry = 0;
    Block *cur = q.acquire();
    for (;;) {
        size_t want = fsize - pos;
        if (want > kBlock) want = kBlock;
        uint8_t *rdst = cur->raw + kCarryArea;  // page aligned
        size_t got = 0;
        pread_full(fd, rdst, want, pos, &got);
        pos += got;
        bool at_end = (got == 0) || (pos >= fsize);

        if (!longline.empty()) {
            // A line longer than kCarry is being accumulated.
            longline.insert(longline.end(), rdst, rdst + got);
            const size_t extra = 64;
            longline.resize(longline.size() + extra, 0);
            const uint8_t *stop =
                parse_lines(longline.data(), longline.size() - extra, giant);
            size_t consumed = (size_t)(stop - longline.data());
            longline.resize(longline.size() - extra);
            if (consumed) longline.erase(longline.begin(), longline.begin() + (ptrdiff_t)consumed);
            if (at_end) {
                if (!longline.empty()) {
                    parse_one(longline.data(),
                              strip_eol(longline.data(), longline.data() + longline.size()),
                              giant);
                }
                break;
            }
            continue;
        }

        uint8_t *data = rdst - carry;  // carry area + fresh bytes
        size_t n = carry + got;
        size_t cut = at_end ? n : last_line_end(data, n);
        if (!at_end && cut == 0) {
            longline.assign(data, data + n);
            carry = 0;
            continue;
        }
        cur->data = data;
        cur->len = cut;
        cur->last = at_end;
        q.publish(cur);
        if (at_end) break;
        carry = n - cut;
        cur = q.acquire();
        if (carry) std::memcpy(cur->raw + kCarryArea - carry, data + cut, carry);
    }
}

void parser(BlockQueue &q, Table &table, LongNameTable &longs) {
    WorkerSink sink{&table, &longs};
    for (;;) {
        Block *b = q.take();
        if (!b) break;
        if (b->len) {
            const uint8_t *stop = parse_lines(b->data, b->len, sink);
            if (stop < b->data + b->len) {
                parse_one(stop, strip_eol(stop, b->data + b->len), sink);
            }
        }
        q.release(b);
    }
}

// Streams [start, end) in kBlock chunks, feeding every complete line to sink.
// Used off the hot path (the ambiguous-mean rescan) and for small inputs.
template <typename Sink>
void stream_range(int fd, size_t start, size_t end, Sink &sink) {
    std::vector<uint8_t> buf(kBlock + kSlack);
    size_t pos = start;
    size_t carry = 0;
    while (pos < end) {
        if (carry + kBlock + kSlack > buf.size()) buf.resize((carry + kBlock + kSlack) * 2);
        size_t want = end - pos;
        if (want > kBlock) want = kBlock;
        size_t got = 0;
        pread_full(fd, buf.data() + carry, want, pos, &got);
        if (got == 0) break;
        pos += got;
        size_t n = carry + got;
        size_t safe = n > kMaxLine ? n - kMaxLine : 0;
        const uint8_t *p = parse_lines(buf.data(), safe, sink);
        carry = n - (size_t)(p - buf.data());
        if (carry) std::memmove(buf.data(), p, carry);
    }
    if (carry) {
        const uint8_t *p = parse_lines(buf.data(), carry, sink);
        if (p < buf.data() + carry) {
            parse_one(p, strip_eol(p, buf.data() + carry), sink);
        }
    }
}

// -------------------------------------------------------------- formatting ----
void append_i10(std::string &out, int64_t v10) {
    if (v10 < 0) {
        out.push_back('-');
        v10 = -v10;
    }
    uint64_t a = (uint64_t)v10;
    uint64_t ip = a / 10;
    uint32_t fp = (uint32_t)(a % 10);
    char tmp[24];
    int n = 0;
    if (ip == 0) tmp[n++] = '0';
    while (ip) {
        tmp[n++] = (char)('0' + (int)(ip % 10));
        ip /= 10;
    }
    while (n) out.push_back(tmp[--n]);
    out.push_back('.');
    out.push_back((char)('0' + (int)fp));
}

// Sequential scan accumulating a reference-identical double sum for stations
// whose exact mean is ambiguous.
struct RiskySink {
    struct Item {
        const uint8_t *name;
        uint32_t len;
        double sum;
        uint64_t count;
    };
    std::vector<Item> *items;
    inline void add(const uint8_t *name, uint32_t len, uint64_t, int32_t t10) {
        for (Item &it : *items) {
            if (it.len == len && std::memcmp(it.name, name, len) == 0) {
                it.sum += (double)t10 / 10.0;
                it.count++;
                return;
            }
        }
    }
};

}  // namespace

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(stderr, "usage: %s <input_file>\n", argv[0]);
        return 1;
    }
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0) {
        std::perror(argv[1]);
        return 1;
    }
    off_t fsize_off = lseek(fd, 0, SEEK_END);
    if (fsize_off < 0) {
        std::perror("lseek");
        return 1;
    }
    size_t fsize = (size_t)fsize_off;
#ifdef F_NOCACHE
    // Large inputs are read once, so keeping them out of the buffer cache both
    // avoids double buffering and avoids evicting everything else; small
    // inputs may still be cached from the generator, where normal reads win.
    if (fsize >= kNoCacheMin) fcntl(fd, F_NOCACHE, 1);
#endif

    std::vector<Table> tables((size_t)kParsers + 1);
    std::vector<LongNameTable> longs((size_t)kParsers + 1);
    BlockQueue queue;
    std::vector<std::thread> threads;
    threads.reserve((size_t)kParsers + 1);
    for (int i = 0; i < kParsers; i++) {
        tables[(size_t)i].init();
        threads.emplace_back(parser, std::ref(queue), std::ref(tables[(size_t)i]),
                             std::ref(longs[(size_t)i]));
    }
    // The reader runs on the main thread so no scheduling handshake is needed
    // before it starts streaming.  Its own tables only ever see lines longer
    // than kCarry (never produced by the generator).
    tables[(size_t)kParsers].init();
    WorkerSink giant{&tables[(size_t)kParsers], &longs[(size_t)kParsers]};
    reader(fd, fsize, queue, giant);
    queue.finish();
    for (std::thread &t : threads) t.join();

    // Merge per-thread tables.
    std::vector<Rec> recs;
    recs.reserve(1024);
    for (int i = 0; i <= kParsers; i++) {
        for (const Slot &s : tables[(size_t)i].slots()) {
            if (!s.cnt) continue;
            Rec r;
            std::memcpy(r.name, s.name, kMaxName);
            r.len = s.len;
            r.sum = s.sum;
            r.mn = s.mn;
            r.mx = s.mx;
            r.cnt = s.cnt;
            recs.push_back(std::move(r));
        }
        for (const auto &kv : longs[(size_t)i].map()) recs.push_back(kv.second);
    }

    std::sort(recs.begin(), recs.end(), [](const Rec &a, const Rec &b) {
        uint32_t m = a.len < b.len ? a.len : b.len;
        int c = std::memcmp(a.key(), b.key(), m);
        if (c != 0) return c < 0;
        return a.len < b.len;
    });
    std::vector<Rec> merged;
    merged.reserve(recs.size());
    for (const Rec &r : recs) {
        if (!merged.empty() && merged.back().len == r.len &&
            std::memcmp(merged.back().key(), r.key(), r.len) == 0) {
            merged.back().absorb(r);
        } else {
            merged.push_back(r);
        }
    }

    // Mean of every station, plus the ambiguous ones.
    std::vector<int64_t> mean10(merged.size());
    std::vector<RiskySink::Item> risky;
    for (size_t i = 0; i < merged.size(); i++) {
        const Rec &e = merged[i];
        int64_t n = (int64_t)e.cnt;
        int64_t two_n = 2 * n;
        int64_t num = 2 * e.sum + n;
        int64_t q = num / two_n;
        if (num % two_n != 0 && num < 0) q--;
        mean10[i] = q;
        int64_t r = (num % two_n + two_n) % two_n;
        int64_t dist = std::min(r, two_n - r);
        double maxabs =
            (double)std::max(std::abs((int64_t)e.mn), std::abs((int64_t)e.mx));
        double margin =
            8.0 * kU * ((double)n * maxabs + 8.0 * (std::abs((double)e.sum) / (double)n + 1.0));
        if ((double)dist <= margin * (double)two_n) {
            risky.push_back(RiskySink::Item{e.key(), e.len, 0.0, 0});
        }
    }
    if (!risky.empty()) {
        RiskySink sink{&risky};
        stream_range(fd, 0, fsize, sink);
        for (size_t i = 0; i < merged.size(); i++) {
            for (const RiskySink::Item &it : risky) {
                if (it.len == merged[i].len &&
                    std::memcmp(it.name, merged[i].key(), it.len) == 0) {
                    mean10[i] = (int64_t)std::floor(
                        (it.sum / (double)merged[i].cnt) * 10.0 + 0.5);
                    break;
                }
            }
        }
    }

    std::string out;
    out.reserve(1 << 14);
    out.push_back('{');
    for (size_t i = 0; i < merged.size(); i++) {
        if (i) out.append(", ");
        out.append((const char *)merged[i].key(), merged[i].len);
        out.push_back('=');
        append_i10(out, merged[i].mn);
        out.push_back('/');
        append_i10(out, mean10[i]);
        out.push_back('/');
        append_i10(out, merged[i].mx);
    }
    out.append("}\n");
    if (std::fwrite(out.data(), 1, out.size(), stdout) != out.size()) {
        std::perror("fwrite");
        return 1;
    }
    close(fd);
    return 0;
}
