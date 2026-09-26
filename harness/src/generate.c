/*
 * Deterministic 1BRC-style data generator.
 *
 *   generate <rows> <seed> <output>
 *
 * Emits lines of the form "<station>;<temperature>\n" where
 *   - station is drawn uniformly from the 413-station official universe
 *   - temperature = round_half_up(mean + 10 * N(0,1), 1 decimal)
 * rounded exactly like Java's Math.round(x * 10.0) / 10.0.
 *
 * The same (rows, seed) pair always produces byte-identical output.
 */
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "stations.h"

static uint64_t sm_state;

static uint64_t next_u64(void) {
    sm_state += 0x9E3779B97F4A7C15ULL;
    uint64_t z = sm_state;
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
    return z ^ (z >> 31);
}

static double next_double(void) {
    return (double)(next_u64() >> 11) * (1.0 / 9007199254740992.0);
}

static double gauss_spare;
static int gauss_has_spare;

static double next_gaussian(void) {
    if (gauss_has_spare) {
        gauss_has_spare = 0;
        return gauss_spare;
    }
    double u, v, s;
    do {
        u = 2.0 * next_double() - 1.0;
        v = 2.0 * next_double() - 1.0;
        s = u * u + v * v;
    } while (s >= 1.0 || s == 0.0);
    double m = sqrt(-2.0 * log(s) / s);
    gauss_spare = v * m;
    gauss_has_spare = 1;
    return u * m;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s <rows> <seed> <output>\n", argv[0]);
        return 1;
    }
    long long rows = atoll(argv[1]);
    if (rows <= 0) {
        fprintf(stderr, "rows must be > 0\n");
        return 1;
    }
    sm_state = strtoull(argv[2], NULL, 10);

    FILE *f = fopen(argv[3], "wb");
    if (!f) {
        perror(argv[3]);
        return 1;
    }

    enum { BUFSZ = 1 << 20 };
    char *buf = malloc(BUFSZ);
    if (!buf) {
        fprintf(stderr, "out of memory\n");
        fclose(f);
        return 1;
    }
    size_t pos = 0;

    for (long long i = 0; i < rows; i++) {
        int st = (int)(next_u64() % (uint64_t)STATION_COUNT);
        long long t10 =
            (long long)floor((STATION_MEANS[st] + 10.0 * next_gaussian()) * 10.0 + 0.5);
        const char *name = STATION_NAMES[st];
        size_t nlen = strlen(name);

        if (pos + nlen + 32 > BUFSZ) {
            if (fwrite(buf, 1, pos, f) != pos) {
                perror("fwrite");
                free(buf);
                fclose(f);
                return 1;
            }
            pos = 0;
        }

        memcpy(buf + pos, name, nlen);
        pos += nlen;
        buf[pos++] = ';';

        if (t10 < 0) {
            buf[pos++] = '-';
            t10 = -t10;
        }
        long long ip = t10 / 10;
        long long fp = t10 % 10;
        char tmp[24];
        int tn = 0;
        if (ip == 0) {
            tmp[tn++] = '0';
        }
        while (ip > 0) {
            tmp[tn++] = (char)('0' + (int)(ip % 10));
            ip /= 10;
        }
        while (tn > 0) {
            buf[pos++] = tmp[--tn];
        }
        buf[pos++] = '.';
        buf[pos++] = (char)('0' + (int)fp);
        buf[pos++] = '\n';

        if ((i + 1) % 50000000LL == 0) {
            if (fwrite(buf, 1, pos, f) != pos) {
                perror("fwrite");
                free(buf);
                fclose(f);
                return 1;
            }
            pos = 0;
            double secs = (double)clock() / (double)CLOCKS_PER_SEC;
            fprintf(stderr, "  %lld rows written (%.1fM rows/s)\n", i + 1,
                    (double)(i + 1) / secs / 1e6);
        }
    }

    if (fwrite(buf, 1, pos, f) != pos) {
        perror("fwrite");
        free(buf);
        fclose(f);
        return 1;
    }
    free(buf);
    if (fclose(f) != 0) {
        perror("fclose");
        return 1;
    }
    fprintf(stderr, "done: %lld rows -> %s\n", rows, argv[3]);
    return 0;
}
