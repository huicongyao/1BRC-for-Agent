/*
 * 1BRC C++ track - naive baseline (correct, intentionally unoptimized).
 *
 * Interface: 1brc <input_file>   -> result to stdout
 *
 * Improve this file. Keep it a single translation unit with no external
 * dependencies. See AGENTS.md for rules, semantics and the iteration loop.
 */
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <string>

struct Agg {
    double min = 1e308;
    double max = -1e308;
    double sum = 0.0;
    long long count = 0;
};

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

    std::map<std::string, Agg> stats;
    char line[512];
    while (fgets(line, sizeof(line), f)) {
        char *semi = strchr(line, ';');
        if (!semi) {
            continue;
        }
        *semi = '\0';
        const char *p = semi + 1;

        bool neg = false;
        if (*p == '-') {
            neg = true;
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

        Agg &a = stats[std::string(line)];
        if (v < a.min) {
            a.min = v;
        }
        if (v > a.max) {
            a.max = v;
        }
        a.sum += v;
        a.count++;
    }
    fclose(f);

    std::string out = "{";
    bool first = true;
    for (const auto &[name, a] : stats) {
        if (!first) {
            out += ", ";
        }
        first = false;
        double mean = std::floor((a.sum / (double)a.count) * 10.0 + 0.5) / 10.0;
        char buf[128];
        snprintf(buf, sizeof(buf), "%s=%.1f/%.1f/%.1f", name.c_str(), a.min, mean, a.max);
        out += buf;
    }
    out += "}\n";
    fwrite(out.data(), 1, out.size(), stdout);
    return 0;
}
