# basemind eval

root `$CORPUS/tokio`, tasks `benchmarks/eval/tasks/tokio.jsonl`, tokenizer `o200k`, savings credited at recall >= 0.80

binary 0.33.0 (build `7bdf56cb440c`, features `tokenizer,code-intel,intelligence,documents,code-search`)

## Retrieval quality

| mode | tasks | err | p50 us | p95 us | P | R | F1 | hit@1 | hit@5 | MRR | nDCG | tokens |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 100 | 0 | 604 | 2492 | 0.887 | 0.972 | 0.908 | 0.987 | 0.987 | 0.987 | 0.967 | 257 |
| outline | 25 | 0 | 1067 | 1868 | 0.814 | 0.937 | 0.855 | - | - | - | - | 554 |
| references | 25 | 0 | 681 | 1803 | 1.000 | 0.990 | 0.994 | - | - | - | - | 69 |
| callers | 25 | 0 | 486 | 781 | 1.000 | 1.000 | 1.000 | - | - | - | - | 84 |
| grep | 25 | 0 | 1885 | 5908 | 1.000 | 1.000 | 1.000 | - | - | - | - | 301 |
| find | 25 | 0 | 247 | 408 | 0.821 | 1.000 | 0.861 | 0.960 | 1.000 | 0.980 | 0.985 | 22 |
| dependents | 25 | 0 | 207 | 436 | 0.960 | 0.898 | 0.920 | - | - | - | - | 34 |
| git_search | 25 | 0 | 351 | 711 | 1.000 | 0.967 | 0.979 | 1.000 | 1.000 | 1.000 | 1.000 | 79 |
| implementations | 25 | 0 | 342 | 1136 | 1.000 | 1.000 | 1.000 | - | - | - | - | 125 |
| read | 25 | 0 | 628 | 841 | 1.000 | 1.000 | 1.000 | - | - | - | - | 137 |
| git_path | 25 | 0 | 631 | 1841 | 1.000 | 1.000 | 1.000 | - | - | - | - | 204 |
| git_symbol | 25 | 0 | 7178 | 53267 | 0.980 | 1.000 | 0.987 | - | - | - | - | 53 |
| blame | 25 | 0 | 45651 | 81427 | 0.987 | 0.987 | 0.987 | - | - | - | - | 150 |
| blame_symbol | 25 | 0 | 55670 | 100827 | 0.955 | 0.955 | 0.955 | - | - | - | - | 149 |
| diff | 25 | 0 | 1088 | 3511 | 0.991 | 0.994 | 0.993 | - | - | - | - | 441 |
| diff_structural | 25 | 0 | 1997 | 5838 | 0.996 | 0.973 | 0.982 | - | - | - | - | 90 |
| impact | 25 | 0 | 211 | 583 | 1.000 | 1.000 | 1.000 | - | - | - | - | 95 |
| cycles | 26 | 0 | 146 | 824 | 0.923 | 0.922 | 0.923 | - | - | - | - | 283 |
| grep_changed | 20 | 0 | 4150 | 5178 | 1.000 | 1.000 | 1.000 | - | - | - | - | 139 |
| find_large | 20 | 0 | 266 | 707 | 1.000 | 1.000 | 1.000 | - | - | - | - | 228 |
| read_around | 25 | 0 | 1968 | 3611 | 1.000 | 1.000 | 1.000 | - | - | - | - | 314 |
| explain | 25 | 0 | 17234 | 121492 | 1.000 | 1.000 | 1.000 | - | - | - | - | 358 |
| **all** | 616 | 0 | 736 | 53370 | 0.958 | 0.980 | 0.962 | 0.984 | 0.992 | 0.988 | 0.977 | 198 |

## Token savings vs `savings.rs` multipliers

| mode | credited | withheld | median x | p90 x | median saved | p90 saved | model x | model | deviation | flag |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | 96 | 4 | 0.78 | 0.97 | -22 | -3 | 1.00 | git_grep | -22% |  |
| outline | 21 | 4 | 2.21 | 5.95 | 1209 | 2630 | 1.60 | full_file_read | +38% | DEVIATES |
| references | 24 | 1 | 3.02 | 5.73 | 71 | 633 | 1.00 | grep_top_hits | +202% | DEVIATES |
| callers | 25 | 0 | 1.22 | 4.41 | 16 | 341 | 1.00 | grep_top_hits | +22% |  |
| grep | 25 | 0 | 0.86 | 1.20 | -5 | 57 | 1.00 | no_baseline | -14% |  |
| find | 25 | 0 | 0.75 | 0.83 | -3 | -3 | 1.00 | git_ls_files_grep | -25% |  |
| dependents | 21 | 4 | 0.85 | 0.94 | -4 | -4 | 1.00 | grep_top_hits | -15% |  |
| git_search | 23 | 2 | 0.77 | 1.31 | -8 | 14 | 1.00 | no_baseline | -23% |  |
| implementations | 25 | 0 | 1.25 | 2.75 | 21 | 186 | 1.00 | grep_top_hits | +25% |  |
| read | 25 | 0 | 26.44 | 92.59 | 2709 | 10405 | 25.80 | full_file_read | +2% |  |
| git_path | 25 | 0 | 0.83 | 1.80 | -16 | 141 | 1.00 | no_baseline | -17% |  |
| git_symbol | 25 | 0 | 0.92 | 2.14 | -4 | 45 | 1.00 | no_baseline | -8% |  |
| blame | 24 | 1 | 3.84 | 7.69 | 398 | 877 | 1.10 | git_blame | +250% | DEVIATES |
| blame_symbol | 24 | 1 | 2.09 | 6.78 | 117 | 1028 | 1.10 | git_blame | +91% | DEVIATES |
| diff | 25 | 0 | 1.17 | 1.57 | 42 | 190 | 1.00 | no_baseline | +17% |  |
| diff_structural | 23 | 2 | 13.46 | 50.55 | 990 | 5037 | 1.00 | no_baseline | +1246% | DEVIATES |
| impact | 25 | 0 | 1.49 | 39.68 | 2 | 2429 | 1.00 | grep_top_hits | +49% | DEVIATES |
| grep_changed | 20 | 0 | 0.98 | 1.24 | -1 | 84 | 1.00 | no_baseline | -2% |  |
| find_large | 20 | 0 | 0.89 | 0.91 | -13 | -5 | 1.00 | git_ls_files_grep | -11% |  |
| read_around | 25 | 0 | 1.61 | 1.91 | 157 | 490 | 1.00 | no_baseline | +61% | DEVIATES |
| explain | 25 | 0 | 5.76 | 10.83 | 1670 | 3931 | - |  | - |  |

## Value gate (ADR-0031: credited >= 0.7 for every job; reducing median >= 1.5x; locating median >= 0.6667x (2/3) and recall >= baseline - 0.02; grep symbol accuracy >= 0.95)

| kind | tool | job | tasks | credited | baseline failed | median x | tool R | baseline R | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| symbols | code_symbols | locating | 100 | 96 (0.96) | 0 | 0.78 | 0.972 | 0.978 | - | PASS |
| outline | code_outline | reducing | 25 | 21 (0.84) | 0 | 2.21 | 0.937 | 1.000 | - | PASS |
| references | code_refs | locating | 25 | 24 (0.96) | 0 | 3.02 | 0.990 | 1.000 | - | PASS |
| callers | code_refs | locating | 25 | 25 (1.00) | 0 | 1.22 | 1.000 | 1.000 | - | PASS |
| grep | code_grep | locating | 25 | 25 (1.00) | 0 | 0.86 | 1.000 | 1.000 | 1.000 | PASS |
| find | find_files | locating | 25 | 25 (1.00) | 1 | 0.75 | 1.000 | 0.880 | - | PASS |
| dependents | code_refs | locating | 25 | 21 (0.84) | 1 | 0.85 | 0.898 | 0.898 | - | PASS |
| git_search | git_log | locating | 25 | 23 (0.92) | 0 | 0.77 | 0.967 | 0.965 | - | PASS |
| implementations | code_refs | locating | 25 | 25 (1.00) | 0 | 1.25 | 1.000 | 1.000 | - | PASS |
| read | code_read | reducing | 25 | 25 (1.00) | 0 | 26.44 | 1.000 | 1.000 | - | PASS |
| git_path | git_log | locating | 25 | 25 (1.00) | 0 | 0.83 | 1.000 | 0.985 | - | PASS |
| git_symbol | git_log | locating | 25 | 25 (1.00) | 0 | 0.92 | 1.000 | 0.714 | - | PASS |
| blame | git_blame | reducing | 25 | 24 (0.96) | 0 | 3.84 | 0.987 | 1.000 | - | PASS |
| blame_symbol | git_blame | reducing | 25 | 24 (0.96) | 0 | 2.09 | 0.955 | 1.000 | - | PASS |
| diff | git_diff | locating | 25 | 25 (1.00) | 0 | 1.17 | 0.994 | 1.000 | - | PASS |
| diff_structural | git_diff | reducing | 25 | 23 (0.92) | 0 | 13.46 | 0.973 | 1.000 | - | PASS |
| impact | code_refs | locating | 25 | 25 (1.00) | 3 | 1.49 | 1.000 | 0.682 | - | PASS |
| cycles | code_refs | correctness | 26 | 24 (0.92) | 0 | - | - | - | - | PASS |
| grep_changed | code_grep | locating | 20 | 20 (1.00) | 0 | 0.98 | 1.000 | 1.000 | - | PASS |
| find_large | find_files | locating | 20 | 20 (1.00) | 0 | 0.89 | 1.000 | 1.000 | - | PASS |
| read_around | code_grep | reducing | 25 | 25 (1.00) | 0 | 1.61 | 1.000 | 1.000 | - | PASS |
| explain | code_explain | reducing | 25 | 25 (1.00) | 0 | 5.76 | 1.000 | 1.000 | - | PASS |
