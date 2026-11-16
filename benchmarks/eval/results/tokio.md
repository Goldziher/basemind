# basemind eval

root `$CORPUS/tokio`, tasks `benchmarks/eval/tasks/tokio.jsonl`, tokenizer `o200k`, savings credited at recall >= 0.80

## Retrieval quality

| mode | tasks | err | p50 us | p95 us | P | R | F1 | hit@1 | hit@5 | MRR | nDCG | tokens |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 100 | 0 | 13294 | 52436 | 0.836 | 0.908 | 0.853 | 0.947 | 0.947 | 0.947 | 0.901 | 500 |
| outline | 25 | 0 | 33674 | 158819 | 0.806 | 0.860 | 0.800 | - | - | - | - | 696 |
| references | 25 | 0 | 5827 | 10924 | 0.890 | 0.754 | 0.784 | - | - | - | - | 127 |
| callers | 25 | 4 | 6365 | 17088 | 0.742 | 0.698 | 0.713 | - | - | - | - | 139 |
| grep | 25 | 0 | 20352 | 60705 | 1.000 | 1.000 | 1.000 | - | - | - | - | 634 |
| find | 25 | 0 | 10785 | 42012 | 0.741 | 0.880 | 0.768 | 0.840 | 0.880 | 0.860 | 0.865 | 63 |
| dependents | 25 | 0 | 6532 | 14735 | 0.920 | 0.830 | 0.862 | - | - | - | - | 49 |
| git_search | 25 | 0 | 15572 | 26716 | 1.000 | 0.967 | 0.979 | 1.000 | 1.000 | 1.000 | 1.000 | 407 |
| implementations | 25 | 0 | 5112 | 16697 | 0.680 | 0.511 | 0.560 | - | - | - | - | 148 |
| read | 25 | 0 | 7938 | 23792 | 1.000 | 1.000 | 1.000 | - | - | - | - | 142 |
| git_path | 25 | 0 | 20950 | 70235 | 0.442 | 0.993 | 0.583 | - | - | - | - | 1525 |
| git_symbol | 25 | 1 | 61155 | 385030 | 0.910 | 0.957 | 0.921 | - | - | - | - | 208 |
| blame | 25 | 0 | 562329 | 1787402 | 0.987 | 0.987 | 0.987 | - | - | - | - | 323 |
| blame_symbol | 25 | 2 | 997599 | 3364808 | 0.875 | 0.875 | 0.875 | - | - | - | - | 401 |
| diff | 25 | 0 | 18139 | 33660 | 0.991 | 0.994 | 0.993 | - | - | - | - | 509 |
| diff_structural | 25 | 0 | 18518 | 50287 | 0.916 | 0.792 | 0.824 | - | - | - | - | 102 |
| **all** | 475 | 7 | 14769 | 797823 | 0.855 | 0.881 | 0.845 | 0.936 | 0.944 | 0.940 | 0.913 | 393 |

## Token savings vs `savings.rs` multipliers

| mode | credited | withheld | median x | p90 x | median saved | p90 saved | model x | model | deviation | flag |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 87 | 13 | 0.32 | 0.50 | -102 | -39 | 25.00 | grep_plus_read_top_hits | -99% | DEVIATES |
| outline | 17 | 8 | 1.69 | 4.23 | 905 | 2728 | 1.20 | full_file_read | +41% | DEVIATES |
| references | 16 | 9 | 1.06 | 1.96 | 7 | 99 | 1.00 | grep_top_hits | +6% |  |
| callers | 16 | 5 | 0.68 | 1.11 | -34 | 34 | 1.00 | grep_top_hits | -32% | DEVIATES |
| grep | 25 | 0 | 0.35 | 0.55 | -89 | -55 | 1.00 | no_baseline | -65% | DEVIATES |
| find | 22 | 3 | 0.21 | 0.29 | -34 | -34 | 1.00 | git_ls_files_grep | -79% | DEVIATES |
| dependents | 19 | 6 | 0.55 | 0.81 | -20 | -12 | 1.00 | grep_top_hits | -45% | DEVIATES |
| git_search | 23 | 2 | 0.23 | 0.28 | -103 | -68 | 1.00 | no_baseline | -77% | DEVIATES |
| implementations | 9 | 16 | 0.49 | 1.58 | -46 | 162 | 1.00 | grep_top_hits | -51% | DEVIATES |
| read | 25 | 0 | 25.87 | 86.34 | 2704 | 10400 | 1.00 | no_baseline | +2487% | DEVIATES |
| git_path | 25 | 0 | 0.11 | 0.28 | -1079 | -348 | 1.00 | no_baseline | -89% | DEVIATES |
| git_symbol | 24 | 0 | 0.22 | 0.50 | -95 | -62 | 1.00 | no_baseline | -78% | DEVIATES |
| blame | 24 | 1 | 1.87 | 3.25 | 287 | 573 | 1.00 | no_baseline | +87% | DEVIATES |
| blame_symbol | 22 | 1 | 1.11 | 2.66 | 36 | 926 | 1.00 | no_baseline | +11% |  |
| diff | 25 | 0 | 1.00 | 1.21 | -2 | 47 | 1.00 | no_baseline | -0% |  |
| diff_structural | 18 | 7 | 9.80 | 41.75 | 768 | 6808 | 1.00 | no_baseline | +880% | DEVIATES |

## Value gate (ADR-0022: credited >= 0.7, median >= 1.5x)

| kind | tool | tasks | credited | median x | tool R | baseline R | baseline R>=min | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|
| symbols | code_symbols | 100 | 87 (0.87) | 0.32 | 0.908 | 0.978 | 97/100 | - | FAIL (median 0.32x < 1.5x) |
| outline | code_outline | 25 | 17 (0.68) | 1.69 | 0.860 | 1.000 | 25/25 | - | FAIL (credited 0.68 < 0.7) |
| references | code_refs | 25 | 16 (0.64) | 1.06 | 0.754 | 1.000 | 25/25 | - | FAIL (credited 0.64 < 0.7; median 1.06x < 1.5x) |
| callers | code_refs | 25 | 16 (0.64) | 0.68 | 0.698 | 1.000 | 21/21 | - | FAIL (credited 0.64 < 0.7; median 0.68x < 1.5x) |
| grep | code_grep | 25 | 25 (1.00) | 0.35 | 1.000 | 1.000 | 25/25 | 0.979 | FAIL (median 0.35x < 1.5x) |
| find | find_files | 25 | 22 (0.88) | 0.21 | 0.880 | 0.880 | 22/25 | - | FAIL (median 0.21x < 1.5x) |
| dependents | code_refs | 25 | 19 (0.76) | 0.55 | 0.830 | 0.898 | 21/25 | - | FAIL (median 0.55x < 1.5x) |
| git_search | git_log | 25 | 23 (0.92) | 0.23 | 0.967 | 0.965 | 23/25 | - | FAIL (median 0.23x < 1.5x) |
| implementations | code_refs | 25 | 9 (0.36) | 0.49 | 0.511 | 1.000 | 25/25 | - | FAIL (credited 0.36 < 0.7; median 0.49x < 1.5x) |
| read | code_read | 25 | 25 (1.00) | 25.87 | 1.000 | 1.000 | 25/25 | - | PASS |
| git_path | git_log | 25 | 25 (1.00) | 0.11 | 0.993 | 0.985 | 24/25 | - | FAIL (median 0.11x < 1.5x) |
| git_symbol | git_log | 25 | 24 (0.96) | 0.22 | 0.957 | 0.702 | 16/24 | - | FAIL (median 0.22x < 1.5x) |
| blame | git_blame | 25 | 24 (0.96) | 1.87 | 0.987 | 1.000 | 25/25 | - | PASS |
| blame_symbol | git_blame | 25 | 22 (0.88) | 1.11 | 0.875 | 1.000 | 23/23 | - | FAIL (median 1.11x < 1.5x) |
| diff | git_diff | 25 | 25 (1.00) | 1.00 | 0.994 | 1.000 | 25/25 | - | FAIL (median 1.00x < 1.5x) |
| diff_structural | git_diff | 25 | 18 (0.72) | 9.80 | 0.792 | 1.000 | 25/25 | - | PASS |
