# basemind eval

root `$CORPUS/basemind`, tasks `benchmarks/eval/tasks/basemind.jsonl`, tokenizer `o200k`, savings credited at recall >= 0.80

## Retrieval quality

| mode | tasks | err | p50 us | p95 us | P | R | F1 | hit@1 | hit@5 | MRR | nDCG | tokens |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 100 | 0 | 18662 | 53274 | 0.952 | 0.984 | 0.958 | 0.973 | 0.987 | 0.976 | 0.973 | 357 |
| outline | 25 | 0 | 16915 | 45102 | 0.951 | 0.988 | 0.968 | - | - | - | - | 914 |
| references | 25 | 0 | 7772 | 27343 | 1.000 | 0.877 | 0.913 | - | - | - | - | 123 |
| callers | 25 | 0 | 10988 | 29266 | 1.000 | 0.880 | 0.909 | - | - | - | - | 146 |
| grep | 25 | 0 | 37131 | 101232 | 1.000 | 1.000 | 1.000 | - | - | - | - | 308 |
| find | 25 | 0 | 1763 | 11963 | 0.493 | 0.600 | 0.518 | 0.520 | 0.600 | 0.553 | 0.565 | 57 |
| dependents | 25 | 0 | 10323 | 26317 | 1.000 | 0.980 | 0.987 | - | - | - | - | 60 |
| git_search | 25 | 0 | 9798 | 21503 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 314 |
| implementations | 25 | 0 | 9868 | 26928 | 0.480 | 0.419 | 0.438 | - | - | - | - | 152 |
| read | 25 | 0 | 14943 | 30922 | 1.000 | 1.000 | 1.000 | - | - | - | - | 246 |
| git_path | 25 | 0 | 19846 | 42087 | 0.925 | 1.000 | 0.957 | - | - | - | - | 960 |
| git_symbol | 25 | 0 | 70273 | 393863 | 1.000 | 1.000 | 1.000 | - | - | - | - | 145 |
| blame | 25 | 0 | 274662 | 927690 | 0.983 | 0.983 | 0.983 | - | - | - | - | 277 |
| blame_symbol | 25 | 1 | 302369 | 834648 | 0.942 | 0.942 | 0.942 | - | - | - | - | 190 |
| diff | 25 | 0 | 15420 | 37618 | 0.999 | 0.999 | 0.999 | - | - | - | - | 572 |
| diff_structural | 25 | 0 | 24119 | 61243 | 0.913 | 0.865 | 0.880 | - | - | - | - | 189 |
| **all** | 475 | 1 | 16263 | 395334 | 0.921 | 0.920 | 0.912 | 0.888 | 0.912 | 0.896 | 0.897 | 320 |

## Token savings vs `savings.rs` multipliers

| mode | credited | withheld | median x | p90 x | median saved | p90 saved | model x | model | deviation | flag |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 98 | 2 | 0.29 | 0.44 | -84 | -38 | 25.00 | grep_plus_read_top_hits | -99% | DEVIATES |
| outline | 25 | 0 | 3.49 | 5.52 | 1701 | 4295 | 1.20 | full_file_read | +191% | DEVIATES |
| references | 19 | 6 | 1.10 | 23.79 | 5 | 7019 | 1.00 | grep_top_hits | +10% |  |
| callers | 20 | 5 | 0.83 | 1.40 | -24 | 158 | 1.00 | grep_top_hits | -17% |  |
| grep | 25 | 0 | 0.30 | 0.51 | -75 | -57 | 1.00 | no_baseline | -70% | DEVIATES |
| find | 15 | 10 | 0.19 | 0.88 | -34 | -5 | 1.00 | git_ls_files_grep | -81% | DEVIATES |
| dependents | 24 | 1 | 0.50 | 0.84 | -21 | -19 | 1.00 | grep_top_hits | -50% | DEVIATES |
| git_search | 25 | 0 | 0.11 | 0.26 | -246 | -72 | 1.00 | no_baseline | -89% | DEVIATES |
| implementations | 9 | 16 | 0.60 | 0.75 | -58 | -37 | 1.00 | grep_top_hits | -40% | DEVIATES |
| read | 25 | 0 | 32.68 | 87.46 | 5587 | 9937 | 1.00 | no_baseline | +3168% | DEVIATES |
| git_path | 25 | 0 | 0.25 | 0.27 | -522 | -126 | 1.00 | no_baseline | -75% | DEVIATES |
| git_symbol | 25 | 0 | 0.20 | 0.41 | -100 | -68 | 1.00 | no_baseline | -80% | DEVIATES |
| blame | 24 | 1 | 2.76 | 7.41 | 397 | 884 | 1.00 | no_baseline | +176% | DEVIATES |
| blame_symbol | 23 | 1 | 2.24 | 12.16 | 204 | 1797 | 1.00 | no_baseline | +124% | DEVIATES |
| diff | 25 | 0 | 1.04 | 1.36 | 25 | 107 | 1.00 | no_baseline | +4% |  |
| diff_structural | 21 | 4 | 25.22 | 52.97 | 5187 | 9459 | 1.00 | no_baseline | +2422% | DEVIATES |

## Value gate (ADR-0022: credited >= 0.7, median >= 1.5x)

| kind | tool | tasks | credited | median x | tool R | baseline R | baseline R>=min | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|
| symbols | code_symbols | 100 | 98 (0.98) | 0.29 | 0.984 | 0.990 | 99/100 | - | FAIL (median 0.29x < 1.5x) |
| outline | code_outline | 25 | 25 (1.00) | 3.49 | 0.988 | 1.000 | 25/25 | - | PASS |
| references | code_refs | 25 | 19 (0.76) | 1.10 | 0.877 | 1.000 | 25/25 | - | FAIL (median 1.10x < 1.5x) |
| callers | code_refs | 25 | 20 (0.80) | 0.83 | 0.880 | 1.000 | 25/25 | - | FAIL (median 0.83x < 1.5x) |
| grep | code_grep | 25 | 25 (1.00) | 0.30 | 1.000 | 1.000 | 25/25 | 1.000 | FAIL (median 0.30x < 1.5x) |
| find | find_files | 25 | 15 (0.60) | 0.19 | 0.600 | 0.720 | 18/25 | - | FAIL (credited 0.60 < 0.7; median 0.19x < 1.5x) |
| dependents | code_refs | 25 | 24 (0.96) | 0.50 | 0.980 | 0.980 | 24/25 | - | FAIL (median 0.50x < 1.5x) |
| git_search | git_log | 25 | 25 (1.00) | 0.11 | 1.000 | 1.000 | 25/25 | - | FAIL (median 0.11x < 1.5x) |
| implementations | code_refs | 25 | 9 (0.36) | 0.60 | 0.419 | 1.000 | 25/25 | - | FAIL (credited 0.36 < 0.7; median 0.60x < 1.5x) |
| read | code_read | 25 | 25 (1.00) | 32.68 | 1.000 | 1.000 | 25/25 | - | PASS |
| git_path | git_log | 25 | 25 (1.00) | 0.25 | 1.000 | 1.000 | 25/25 | - | FAIL (median 0.25x < 1.5x) |
| git_symbol | git_log | 25 | 25 (1.00) | 0.20 | 1.000 | 0.980 | 24/25 | - | FAIL (median 0.20x < 1.5x) |
| blame | git_blame | 25 | 24 (0.96) | 2.76 | 0.983 | 1.000 | 25/25 | - | PASS |
| blame_symbol | git_blame | 25 | 23 (0.92) | 2.24 | 0.942 | 1.000 | 24/24 | - | PASS |
| diff | git_diff | 25 | 25 (1.00) | 1.04 | 0.999 | 1.000 | 25/25 | - | FAIL (median 1.04x < 1.5x) |
| diff_structural | git_diff | 25 | 21 (0.84) | 25.22 | 0.865 | 1.000 | 25/25 | - | PASS |
