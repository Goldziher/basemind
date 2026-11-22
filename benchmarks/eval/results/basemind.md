# basemind eval

root `$CORPUS/basemind`, tasks `benchmarks/eval/tasks/basemind.jsonl`, tokenizer `o200k`, savings credited at recall >= 0.80

binary 0.33.0 (build `7bdf56cb440c`, features `tokenizer,code-intel,intelligence,documents,code-search`)

## Retrieval quality

| mode | tasks | err | p50 us | p95 us | P | R | F1 | hit@1 | hit@5 | MRR | nDCG | tokens |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 100 | 0 | 672 | 2540 | 0.957 | 0.990 | 0.964 | 0.973 | 0.987 | 0.976 | 0.978 | 168 |
| outline | 25 | 0 | 853 | 2360 | 0.952 | 0.995 | 0.971 | - | - | - | - | 690 |
| references | 25 | 0 | 971 | 2210 | 1.000 | 1.000 | 1.000 | - | - | - | - | 44 |
| callers | 25 | 0 | 579 | 967 | 1.000 | 1.000 | 1.000 | - | - | - | - | 77 |
| grep | 25 | 0 | 1853 | 4867 | 1.000 | 1.000 | 1.000 | - | - | - | - | 145 |
| find | 25 | 0 | 366 | 601 | 0.837 | 1.000 | 0.872 | 0.920 | 1.000 | 0.953 | 0.965 | 21 |
| dependents | 25 | 0 | 183 | 509 | 1.000 | 0.980 | 0.987 | - | - | - | - | 43 |
| git_search | 25 | 0 | 406 | 473 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 39 |
| implementations | 25 | 0 | 306 | 1020 | 1.000 | 1.000 | 1.000 | - | - | - | - | 99 |
| read | 25 | 0 | 742 | 1174 | 1.000 | 1.000 | 1.000 | - | - | - | - | 241 |
| git_path | 25 | 0 | 746 | 1418 | 0.997 | 1.000 | 0.998 | - | - | - | - | 260 |
| git_symbol | 25 | 0 | 9481 | 62091 | 1.000 | 1.000 | 1.000 | - | - | - | - | 40 |
| blame | 25 | 0 | 14860 | 35345 | 0.983 | 0.983 | 0.983 | - | - | - | - | 113 |
| blame_symbol | 25 | 0 | 17699 | 36556 | 0.982 | 0.982 | 0.982 | - | - | - | - | 93 |
| diff | 25 | 0 | 1644 | 2714 | 0.999 | 0.999 | 0.999 | - | - | - | - | 507 |
| diff_structural | 25 | 0 | 3735 | 7438 | 0.916 | 0.880 | 0.891 | - | - | - | - | 149 |
| impact | 25 | 0 | 210 | 460 | 1.000 | 0.961 | 0.976 | - | - | - | - | 76 |
| cycles | 26 | 0 | 107 | 226 | 0.845 | 0.844 | 0.844 | - | - | - | - | 109 |
| grep_changed | 25 | 0 | 7215 | 9099 | 1.000 | 1.000 | 1.000 | - | - | - | - | 108 |
| find_large | 23 | 0 | 247 | 724 | 1.000 | 1.000 | 1.000 | - | - | - | - | 169 |
| read_around | 25 | 0 | 2412 | 3706 | 1.000 | 1.000 | 1.000 | - | - | - | - | 456 |
| explain | 25 | 0 | 16702 | 68638 | 1.000 | 1.000 | 1.000 | - | - | - | - | 367 |
| **all** | 624 | 0 | 832 | 25252 | 0.973 | 0.983 | 0.974 | 0.968 | 0.992 | 0.976 | 0.980 | 181 |

## Token savings vs `savings.rs` multipliers

| mode | credited | withheld | median x | p90 x | median saved | p90 saved | model x | model | deviation | flag |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 99 | 1 | 0.75 | 0.89 | -19 | -3 | 1.00 | git_grep | -25% |  |
| outline | 25 | 0 | 4.71 | 6.71 | 2189 | 4605 | 1.60 | full_file_read | +194% | DEVIATES |
| references | 25 | 0 | 3.58 | 106.19 | 83 | 7258 | 1.00 | grep_top_hits | +258% | DEVIATES |
| callers | 25 | 0 | 1.62 | 3.94 | 45 | 415 | 1.00 | grep_top_hits | +62% | DEVIATES |
| grep | 25 | 0 | 0.78 | 1.03 | -8 | 1 | 1.00 | no_baseline | -22% |  |
| find | 25 | 0 | 0.73 | 7.47 | -3 | 322 | 1.00 | git_ls_files_grep | -27% | DEVIATES |
| dependents | 24 | 1 | 0.83 | 0.96 | -4 | -4 | 1.00 | grep_top_hits | -17% |  |
| git_search | 25 | 0 | 0.78 | 0.86 | -8 | -6 | 1.00 | no_baseline | -22% |  |
| implementations | 25 | 0 | 1.27 | 1.77 | 20 | 75 | 1.00 | grep_top_hits | +27% | DEVIATES |
| read | 25 | 0 | 33.28 | 91.75 | 5592 | 9942 | 25.80 | full_file_read | +29% | DEVIATES |
| git_path | 25 | 0 | 0.85 | 0.88 | -30 | -9 | 1.00 | no_baseline | -15% |  |
| git_symbol | 25 | 0 | 0.77 | 1.76 | -7 | 21 | 1.00 | no_baseline | -23% |  |
| blame | 24 | 1 | 6.71 | 14.39 | 553 | 979 | 1.10 | git_blame | +512% | DEVIATES |
| blame_symbol | 24 | 1 | 4.71 | 25.43 | 289 | 1881 | 1.09 | git_blame | +331% | DEVIATES |
| diff | 25 | 0 | 1.16 | 1.80 | 67 | 188 | 1.00 | no_baseline | +16% |  |
| diff_structural | 21 | 4 | 32.19 | 62.93 | 5194 | 9505 | 1.00 | no_baseline | +3119% | DEVIATES |
| impact | 23 | 2 | 0.72 | 5.94 | -12 | 692 | 1.00 | grep_top_hits | -28% | DEVIATES |
| grep_changed | 25 | 0 | 0.83 | 1.09 | -7 | 27 | 1.00 | no_baseline | -17% |  |
| find_large | 23 | 0 | 0.85 | 0.88 | -14 | -4 | 1.00 | git_ls_files_grep | -15% |  |
| read_around | 25 | 0 | 1.43 | 1.88 | 150 | 448 | 1.00 | no_baseline | +43% | DEVIATES |
| explain | 25 | 0 | 4.96 | 10.14 | 1242 | 2505 | - |  | - |  |

## Value gate (ADR-0031: credited >= 0.7 for every job; reducing median >= 1.5x; locating median >= 0.6667x (2/3) and recall >= baseline - 0.02; grep symbol accuracy >= 0.95)

| kind | tool | job | tasks | credited | baseline failed | median x | tool R | baseline R | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | code_symbols | locating | 100 | 99 (0.99) | 0 | 0.75 | 0.990 | 0.990 | - | PASS |
| outline | code_outline | reducing | 25 | 25 (1.00) | 0 | 4.71 | 0.995 | 1.000 | - | PASS |
| references | code_refs | locating | 25 | 25 (1.00) | 0 | 3.58 | 1.000 | 1.000 | - | PASS |
| callers | code_refs | locating | 25 | 25 (1.00) | 0 | 1.62 | 1.000 | 1.000 | - | PASS |
| grep | code_grep | locating | 25 | 25 (1.00) | 0 | 0.78 | 1.000 | 1.000 | 1.000 | PASS |
| find | find_files | locating | 25 | 25 (1.00) | 5 | 0.73 | 1.000 | 0.720 | - | PASS |
| dependents | code_refs | locating | 25 | 24 (0.96) | 0 | 0.83 | 0.980 | 0.980 | - | PASS |
| git_search | git_log | locating | 25 | 25 (1.00) | 0 | 0.78 | 1.000 | 1.000 | - | PASS |
| implementations | code_refs | locating | 25 | 25 (1.00) | 0 | 1.27 | 1.000 | 1.000 | - | PASS |
| read | code_read | reducing | 25 | 25 (1.00) | 0 | 33.28 | 1.000 | 1.000 | - | PASS |
| git_path | git_log | locating | 25 | 25 (1.00) | 0 | 0.85 | 1.000 | 1.000 | - | PASS |
| git_symbol | git_log | locating | 25 | 25 (1.00) | 0 | 0.77 | 1.000 | 0.980 | - | PASS |
| blame | git_blame | reducing | 25 | 24 (0.96) | 0 | 6.71 | 0.983 | 1.000 | - | PASS |
| blame_symbol | git_blame | reducing | 25 | 24 (0.96) | 0 | 4.71 | 0.982 | 1.000 | - | PASS |
| diff | git_diff | locating | 25 | 25 (1.00) | 0 | 1.16 | 0.999 | 1.000 | - | PASS |
| diff_structural | git_diff | reducing | 25 | 21 (0.84) | 0 | 32.19 | 0.880 | 1.000 | - | PASS |
| impact | code_refs | locating | 25 | 23 (0.92) | 1 | 0.72 | 0.961 | 0.912 | - | PASS |
| cycles | code_refs | correctness | 26 | 22 (0.85) | 0 | - | - | - | - | PASS |
| grep_changed | code_grep | locating | 25 | 25 (1.00) | 0 | 0.83 | 1.000 | 1.000 | - | PASS |
| find_large | find_files | locating | 23 | 23 (1.00) | 0 | 0.85 | 1.000 | 1.000 | - | PASS |
| read_around | code_grep | reducing | 25 | 25 (1.00) | 0 | 1.43 | 1.000 | 1.000 | - | FAIL (median 1.43x < 1.50x) |
| explain | code_explain | reducing | 25 | 25 (1.00) | 0 | 4.96 | 1.000 | 1.000 | - | PASS |
