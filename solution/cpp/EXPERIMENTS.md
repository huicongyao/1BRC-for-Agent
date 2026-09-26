# C++ track experiments

One row per experiment. Keep it append-only and honest (including failed ideas
and measurements that looked like noise). `median` is the score from
`python3 harness/bench.py bench --track cpp`.

| # | date (UTC) | hypothesis / change | median (s) | delta vs best | peak RSS (GB) | decision | commit |
|---|---|---|---|---|---|---|---|
| 0 | - | naive baseline (fgets + std::map + manual parse) | - | - | - | starting point | - |
