# Retrieval backend benchmark

- VERDICT: arm B fails a row that arm C passes at the same size; a new ADR replaces the backend clause.
- 50,000: arm B FAILS: lexical p95 after 1% append / before, lexical p95 ms added by a 20,000-key list, vector p95 ms added by a 20,000-key list
- 50,000: arm C passes where B fails: lexical p95 after 1% append / before, lexical p95 ms added by a 20,000-key list, vector p95 ms added by a 20,000-key list
- 50,000: arm C FAILS: index build s, on-disk size / raw f32 bytes
- 250,000: arm B FAILS: lexical p95 after 1% append / before, vector p95 after 1% append / before, vector p95 ms added by a 20,000-key list, vector p95 ms, 100% filter, 90% live
- 250,000: arm C passes where B fails: lexical p95 after 1% append / before, vector p95 after 1% append / before, vector p95 ms added by a 20,000-key list, vector p95 ms, 100% filter, 90% live
- 250,000: arm C FAILS: on-disk size / raw f32 bytes
- 1,000,000 (published, does not decide): arm B fails 3 checks: lexical p95 after 1% append / before, lexical p95 ms added by a 20,000-key list, vector p95 ms, 100% filter, 100% live

## 50,000 chunks

| Metric | Rule | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|---|
| recall@10, 1% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 1% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 1% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 100% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 90% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 50% live | >= 0.99 | 1.000 | 1.000 ok | 1.000 ok |
| vector p95 ms, 1% filter, 100% live | <= 50 | 10.5 | 1.687 ok | 0.643 ok |
| vector p95 ms, 10% filter, 100% live | <= 50 | 11.5 | 3.295 ok | 1.229 ok |
| vector p95 ms, 100% filter, 100% live | <= 50 | 10.7 | 11.8 ok | 1.999 ok |
| vector p95 ms, 1% filter, 90% live | <= 50 | 26.8 | 16.1 ok | 2.124 ok |
| vector p95 ms, 10% filter, 90% live | <= 50 | 39.7 | 17.1 ok | 2.653 ok |
| vector p95 ms, 100% filter, 90% live | <= 50 | 32.0 | 17.8 ok | 3.059 ok |
| vector p95 ms, 1% filter, 50% live | <= 50 | 20.1 | 8.864 ok | 1.584 ok |
| vector p95 ms, 10% filter, 50% live | <= 50 | 21.5 | 9.733 ok | 1.967 ok |
| vector p95 ms, 100% filter, 50% live | <= 50 | 21.3 | 12.3 ok | 2.512 ok |
| lexical p95 ms, 1% filter, 100% live | <= 30 | n/a | 0.673 ok | 0.979 ok |
| lexical p95 ms, 10% filter, 100% live | <= 30 | n/a | 1.234 ok | 0.906 ok |
| lexical p95 ms, 100% filter, 100% live | <= 30 | n/a | 0.781 ok | 0.956 ok |
| lexical p95 ms, 1% filter, 90% live | <= 30 | n/a | 10.1 ok | 2.401 ok |
| lexical p95 ms, 10% filter, 90% live | <= 30 | n/a | 10.5 ok | 2.717 ok |
| lexical p95 ms, 100% filter, 90% live | <= 30 | n/a | 4.292 ok | 2.433 ok |
| lexical p95 ms, 1% filter, 50% live | <= 30 | n/a | 5.407 ok | 1.994 ok |
| lexical p95 ms, 10% filter, 50% live | <= 30 | n/a | 6.105 ok | 1.860 ok |
| lexical p95 ms, 100% filter, 50% live | <= 30 | n/a | 2.882 ok | 2.652 ok |
| vector p95 ms added by a 2,000-key list | <= 10 | 15.9 | 4.907 ok | 1.007 ok |
| vector p95 ms added by a 20,000-key list | <= 10 | 139.7 | 66.0 FAIL | 6.350 ok |
| lexical p95 ms added by a 2,000-key list | <= 10 | n/a | 4.185 ok | 1.619 ok |
| lexical p95 ms added by a 20,000-key list | <= 10 | n/a | 17.8 FAIL | 5.465 ok |
| cold open + first query ms | <= 1000 | 22.5 | 16.6 ok | 730.8 ok |
| index build s | <= 30 | 0.000 | 1.416 ok | 39.3 FAIL |
| on-disk size / raw f32 bytes | <= 1.6 | 1.107 | 1.171 ok | 1.906 FAIL |
| recall@10 lost after 1% append | <= 0.005 | 0.000 | 0.000 ok | 0.000 ok |
| vector p95 after 1% append / before | <= 1.5 | 1.243 | 0.973 ok | 1.448 ok |
| lexical p95 after 1% append / before | <= 1.5 | n/a | 5.338 FAIL | 0.963 ok |

Not judged:

| Measurement | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|
| 90% live: keys / applied as / queries short at the cap | 2,248 / predicate / 0 | 2,248 / predicate / 0 | 2,248 / predicate / 0 |
| 50% live: keys / applied as / queries short at the cap | 1,244 / predicate / 0 | 1,244 / predicate / 0 | 1,244 / predicate / 0 |
| lexical recall@10 vs brute-force BM25, 1% | n/a | 0.976 | 1.000 |
| lexical recall@10 vs brute-force BM25, 10% | n/a | 0.933 | 1.000 |
| lexical recall@10 vs brute-force BM25, 100% | n/a | 0.922 | 1.000 |
| vector queries answered exact / ANN, 1% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector queries answered exact / ANN, 10% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector queries answered exact / ANN, 100% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector p95 ms after optimize | 12.6 | 11.5 | 2.3 |
| lexical p95 ms after optimize | n/a | 0.9 | 1.2 |
| ingest s | 0.7 | 0.7 | 32.6 |
| ingest peak RSS MiB | 580 | 584 | 439 |
| index s | 0.0 | 1.4 | 7.5 |
| index peak RSS MiB | 18 | 156 | 257 |
| append s | 0.2 | 0.3 | 1.5 |
| append peak RSS MiB | 38 | 40 | 109 |
| optimize s | 1.5 | 1.8 | 0.1 |
| optimize peak RSS MiB | 638 | 114 | 103 |

## 250,000 chunks

| Metric | Rule | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|---|
| recall@10, 1% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 1% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 1% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 10% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| recall@10, 100% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 100% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 90% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 1% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 10% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| nDCG@10, 100% filter, 50% live | >= 0.97 | 1.000 | 1.000 ok | 1.000 ok |
| vector p95 ms, 1% filter, 100% live | <= 100 | 66.2 | 6.088 ok | 0.906 ok |
| vector p95 ms, 10% filter, 100% live | <= 100 | 45.5 | 19.6 ok | 1.918 ok |
| vector p95 ms, 100% filter, 100% live | <= 100 | 43.0 | 61.4 ok | 4.420 ok |
| vector p95 ms, 1% filter, 90% live | <= 100 | 400.6 | 97.2 ok | 8.786 ok |
| vector p95 ms, 10% filter, 90% live | <= 100 | 347.2 | 81.6 ok | 9.803 ok |
| vector p95 ms, 100% filter, 90% live | <= 100 | 434.2 | 112.6 FAIL | 12.2 ok |
| vector p95 ms, 1% filter, 50% live | <= 100 | 120.6 | 39.3 ok | 5.513 ok |
| vector p95 ms, 10% filter, 50% live | <= 100 | 244.5 | 40.6 ok | 5.744 ok |
| vector p95 ms, 100% filter, 50% live | <= 100 | 367.2 | 54.9 ok | 27.4 ok |
| lexical p95 ms, 1% filter, 100% live | <= 60 | n/a | 2.816 ok | 1.018 ok |
| lexical p95 ms, 10% filter, 100% live | <= 60 | n/a | 5.255 ok | 0.880 ok |
| lexical p95 ms, 100% filter, 100% live | <= 60 | n/a | 3.122 ok | 1.144 ok |
| lexical p95 ms, 1% filter, 90% live | <= 60 | n/a | 44.0 ok | 13.6 ok |
| lexical p95 ms, 10% filter, 90% live | <= 60 | n/a | 52.8 ok | 11.1 ok |
| lexical p95 ms, 100% filter, 90% live | <= 60 | n/a | 19.9 ok | 9.085 ok |
| lexical p95 ms, 1% filter, 50% live | <= 60 | n/a | 50.6 ok | 6.280 ok |
| lexical p95 ms, 10% filter, 50% live | <= 60 | n/a | 27.6 ok | 6.806 ok |
| lexical p95 ms, 100% filter, 50% live | <= 60 | n/a | 11.7 ok | 5.938 ok |
| vector p95 ms added by a 2,000-key list | <= 25 | 41.1 | 0.000 ok | 0.820 ok |
| vector p95 ms added by a 20,000-key list | <= 25 | 387.3 | 63.4 FAIL | 11.6 ok |
| lexical p95 ms added by a 2,000-key list | <= 25 | n/a | 2.929 ok | 1.693 ok |
| lexical p95 ms added by a 20,000-key list | <= 25 | n/a | 23.4 ok | 12.2 ok |
| cold open + first query ms | <= 1500 | 48.6 | 60.2 ok | 800.0 ok |
| index build s | <= 180 | 0.000 | 4.868 ok | 35.0 ok |
| on-disk size / raw f32 bytes | <= 1.6 | 1.107 | 1.156 ok | 1.673 FAIL |
| recall@10 lost after 1% append | <= 0.005 | 0.000 | 0.000 ok | 0.000 ok |
| vector p95 after 1% append / before | <= 1.5 | 1.390 | 1.555 FAIL | 0.955 ok |
| lexical p95 after 1% append / before | <= 1.5 | n/a | 5.366 FAIL | 1.067 ok |

Not judged:

| Measurement | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|
| 90% live: keys / applied as / queries short at the cap | 11,258 / predicate / 0 | 11,258 / predicate / 0 | 11,258 / predicate / 0 |
| 50% live: keys / applied as / queries short at the cap | 6,226 / predicate / 0 | 6,226 / predicate / 0 | 6,226 / predicate / 0 |
| lexical recall@10 vs brute-force BM25, 1% | n/a | 0.950 | 1.000 |
| lexical recall@10 vs brute-force BM25, 10% | n/a | 0.943 | 1.000 |
| lexical recall@10 vs brute-force BM25, 100% | n/a | 0.908 | 1.000 |
| vector queries answered exact / ANN, 1% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector queries answered exact / ANN, 10% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector queries answered exact / ANN, 100% | 200 / 0 | 200 / 0 | 200 / 0 |
| vector p95 ms after optimize | 147.3 | 51.4 | 5.2 |
| lexical p95 ms after optimize | n/a | 1.4 | 1.1 |
| ingest s | 5.3 | 0.9 | 36.1 |
| ingest peak RSS MiB | 2360 | 2180 | 1246 |
| index s | 0.0 | 4.9 | 4.1 |
| index peak RSS MiB | 18 | 505 | 417 |
| append s | 0.0 | 0.2 | 3.8 |
| append peak RSS MiB | 57 | 60 | 158 |
| optimize s | 2.3 | 1.8 | 0.0 |
| optimize peak RSS MiB | 1047 | 227 | 157 |

## 1,000,000 chunks

| Metric | Rule | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|---|
| recall@10, 1% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| recall@10, 10% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| recall@10, 100% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| recall@10, 1% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| recall@10, 10% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| recall@10, 100% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| recall@10, 1% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| recall@10, 10% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| recall@10, 100% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 1% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| nDCG@10, 10% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| nDCG@10, 100% filter, 100% live | >= 0.95 | n/a | 1.000 ok | n/a missing |
| nDCG@10, 1% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 10% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 100% filter, 90% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 1% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 10% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| nDCG@10, 100% filter, 50% live | >= 0.95 | n/a | n/a missing | n/a missing |
| vector p95 ms, 1% filter, 100% live | <= 250 | n/a | 129.0 ok | n/a missing |
| vector p95 ms, 10% filter, 100% live | <= 250 | n/a | 79.1 ok | n/a missing |
| vector p95 ms, 100% filter, 100% live | <= 250 | n/a | 431.2 FAIL | n/a missing |
| vector p95 ms, 1% filter, 90% live | <= 250 | n/a | n/a missing | n/a missing |
| vector p95 ms, 10% filter, 90% live | <= 250 | n/a | n/a missing | n/a missing |
| vector p95 ms, 100% filter, 90% live | <= 250 | n/a | n/a missing | n/a missing |
| vector p95 ms, 1% filter, 50% live | <= 250 | n/a | n/a missing | n/a missing |
| vector p95 ms, 10% filter, 50% live | <= 250 | n/a | n/a missing | n/a missing |
| vector p95 ms, 100% filter, 50% live | <= 250 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 1% filter, 100% live | <= 120 | n/a | 56.1 ok | n/a missing |
| lexical p95 ms, 10% filter, 100% live | <= 120 | n/a | 27.6 ok | n/a missing |
| lexical p95 ms, 100% filter, 100% live | <= 120 | n/a | 2.793 ok | n/a missing |
| lexical p95 ms, 1% filter, 90% live | <= 120 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 10% filter, 90% live | <= 120 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 100% filter, 90% live | <= 120 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 1% filter, 50% live | <= 120 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 10% filter, 50% live | <= 120 | n/a | n/a missing | n/a missing |
| lexical p95 ms, 100% filter, 50% live | <= 120 | n/a | n/a missing | n/a missing |
| vector p95 ms added by a 2,000-key list | <= 60 | n/a | 0.000 ok | n/a missing |
| vector p95 ms added by a 20,000-key list | <= 60 | n/a | 0.000 ok | n/a missing |
| lexical p95 ms added by a 2,000-key list | <= 60 | n/a | 46.6 ok | n/a missing |
| lexical p95 ms added by a 20,000-key list | <= 60 | n/a | 61.7 FAIL | n/a missing |
| cold open + first query ms | <= 2500 | n/a | 292.4 ok | n/a missing |
| index build s | <= 900 | n/a | 7.353 ok | n/a missing |
| on-disk size / raw f32 bytes | <= 1.6 | n/a | 1.153 ok | n/a missing |
| recall@10 lost after 1% append | <= 0.005 | n/a | 0.000 ok | n/a missing |
| vector p95 after 1% append / before | <= 1.5 | n/a | 0.856 ok | n/a missing |
| lexical p95 after 1% append / before | <= 1.5 | n/a | 12.4 FAIL | n/a missing |

Not judged:

| Measurement | A (flat) | B (indexed) | C (Fjall) |
|---|---|---|---|
| 90% live: keys / applied as / queries short at the cap | n/a | n/a | n/a |
| 50% live: keys / applied as / queries short at the cap | n/a | n/a | n/a |
| lexical recall@10 vs brute-force BM25, 1% | n/a | 0.906 | n/a |
| lexical recall@10 vs brute-force BM25, 10% | n/a | 0.936 | n/a |
| lexical recall@10 vs brute-force BM25, 100% | n/a | 0.924 | n/a |
| vector queries answered exact / ANN, 1% | n/a | 200 / 0 | n/a |
| vector queries answered exact / ANN, 10% | n/a | 200 / 0 | n/a |
| vector queries answered exact / ANN, 100% | n/a | 200 / 0 | n/a |
| vector p95 ms after optimize | n/a | 161.0 | n/a |
| lexical p95 ms after optimize | n/a | 1.6 | n/a |
| ingest s | n/a | 4.1 | n/a |
| ingest peak RSS MiB | n/a | 2563 | n/a |
| index s | n/a | 7.4 | n/a |
| index peak RSS MiB | n/a | 1119 | n/a |
| append s | n/a | 0.1 | n/a |
| append peak RSS MiB | n/a | 117 | n/a |
| optimize s | n/a | 2.4 | n/a |
| optimize peak RSS MiB | n/a | 674 | n/a |
