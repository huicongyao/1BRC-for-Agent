/*
 * Independent single-threaded reference implementation / ground truth.
 *
 *   reference <input_file>
 *
 * Semantics (authoritative, deliberately kept simple):
 *   - parse each line "name;value" (missing final newline tolerated)
 *   - value is a decimal with optional fraction, parsed as scaled tenths and
 *     converted with t10 / 10.0 (IEEE-754 correctly-rounded)
 *   - per station: min, max, sum (double, sequential accumulation), count
 *   - output <- "{name=min/mean/max, ...}" sorted by station name bytes
 *     (UTF-8 byte order), one decimal per number, mean =
 *     floor((sum/count) * 10 + 0.5) / 10   (Java Math.round semantics)
 */
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    char *name;
    double min;
    double max;
    double sum;
    long long count;
} Agg;

enum { TABLE_SIZE = 2048, MAX_STATIONS = 1024 };

static Agg table[TABLE_SIZE];
static unsigned char used[TABLE_SIZE];
static Agg *stations[MAX_STATIONS];

static uint64_t hash_name(const char *s, size_t len) {
    uint64_t h = 1469598103934665603ULL; /* FNV-1a 64 */
    for (size_t i = 0; i < len; i++) {
        h ^= (unsigned char)s[i];
        h *= 1099511628211ULL;
    }
    return h;
}

static int cmp_station(const void *a, const void *b) {
    const Agg *x = *(const Agg *const *)a;
    const Agg *y = *(const Agg *const *)b;
    return strcmp(x->name, y->name);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <input_file>\n", argv[0]);
        return 1;
    }
    FILE *f = fopen(argv[1], "rb");
    if (!f) {
        perror(argv[1]);
        return 1;
    }

    char *line = NULL;
    size_t cap = 0;
    ssize_t n;
    int nstations = 0;

    while ((n = getline(&line, &cap, f)) != -1) {
        while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
            line[--n] = '\0';
        }
        if (n == 0) {
            continue;
        }
        char *semi = memchr(line, ';', (size_t)n);
        if (!semi) {
            continue;
        }
        size_t namelen = (size_t)(semi - line);
        const char *p = semi + 1;

        int neg = 0;
        if (*p == '-') {
            neg = 1;
            p++;
        }
        long long ip = 0;
        while (*p >= '0' && *p <= '9') {
            ip = ip * 10 + (*p++ - '0');
        }
        long long fp = 0;
        if (*p == '.') {
            p++;
            if (*p >= '0' && *p <= '9') {
                fp = *p++ - '0';
            }
        }
        long long t10 = ip * 10 + fp;
        if (neg) {
            t10 = -t10;
        }
        double v = (double)t10 / 10.0;

        uint64_t h = hash_name(line, namelen);
        size_t idx = (size_t)(h & (TABLE_SIZE - 1));
        for (;;) {
            if (!used[idx]) {
                Agg *a = &table[idx];
                a->name = (char *)malloc(namelen + 1);
                memcpy(a->name, line, namelen);
                a->name[namelen] = '\0';
                a->min = v;
                a->max = v;
                a->sum = 0.0;
                a->count = 0;
                used[idx] = 1;
                stations[nstations++] = a;
                break;
            }
            Agg *a = &table[idx];
            if (strlen(a->name) == namelen && memcmp(a->name, line, namelen) == 0) {
                break;
            }
            idx = (idx + 1) & (TABLE_SIZE - 1);
        }

        Agg *a = &table[idx];
        if (v < a->min) {
            a->min = v;
        }
        if (v > a->max) {
            a->max = v;
        }
        a->sum += v;
        a->count++;
    }

    free(line);
    fclose(f);

    qsort(stations, (size_t)nstations, sizeof(Agg *), cmp_station);

    fputs("{", stdout);
    for (int i = 0; i < nstations; i++) {
        Agg *a = stations[i];
        double mean = floor((a->sum / (double)a->count) * 10.0 + 0.5) / 10.0;
        if (i > 0) {
            fputs(", ", stdout);
        }
        printf("%s=%.1f/%.1f/%.1f", a->name, a->min, mean, a->max);
    }
    fputs("}\n", stdout);

    return 0;
}
