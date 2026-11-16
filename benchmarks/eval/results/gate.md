# Value gate (ADR-0022)

Corpora: `basemind`, `tokio`; tokenizer: basemind `o200k`, tokio `o200k`. A kind passes at credited >= 0.70 and median >= 1.50x.

| tool | verdict |
|---|---|
| `code_symbols` | FAIL |
| `code_outline` | FAIL |
| `code_read` | PASS |
| `code_grep` | FAIL |
| `code_search` | not run (needs the `code-search` build and an embedding model download (network is limited to cloning)) |
| `code_refs` | FAIL |
| `find_files` | FAIL |
| `docs_search` | not run (needs the `documents` build and an embedding model download (network is limited to cloning)) |
| `git_log` | FAIL |
| `git_blame` | FAIL |
| `git_diff` | FAIL |
| `memory_save`, `memory_search`, `index_status`, `index_rescan` | exempt (ADR-0022) |

Per kind and corpus. `R` is mean recall against the gold, `P` mean precision; tokens are
medians over all tasks of the kind; `x` is the median baseline/tool ratio over the credited tasks.

| tool | kind | corpus | tasks | err | credited | tool R | tool P | baseline R | tool tok | baseline tok | x | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `code_symbols` | symbols | basemind | 100 | 0 | 98 (0.98) | 0.984 | 0.952 | 0.990 | 112 | 27 | 0.29 | - | FAIL (median 0.29x < 1.5x) |
| `code_symbols` | symbols | tokio | 100 | 0 | 87 (0.87) | 0.908 | 0.836 | 0.978 | 184 | 41 | 0.32 | - | FAIL (median 0.32x < 1.5x) |
| `code_outline` | outline | basemind | 25 | 0 | 25 (1.00) | 0.988 | 0.951 | 1.000 | 875 | 2539 | 3.49 | - | PASS |
| `code_outline` | outline | tokio | 25 | 0 | 17 (0.68) | 0.860 | 0.806 | 1.000 | 476 | 1402 | 1.69 | - | FAIL (credited 0.68 < 0.7) |
| `code_read` | read | basemind | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 1.000 | 193 | 5719 | 32.68 | - | PASS |
| `code_read` | read | tokio | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 1.000 | 119 | 2796 | 25.87 | - | PASS |
| `code_grep` | grep | basemind | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 1.000 | 100 | 33 | 0.30 | 1.000 | FAIL (median 0.30x < 1.5x) |
| `code_grep` | grep | tokio | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 1.000 | 133 | 55 | 0.35 | 0.979 | FAIL (median 0.35x < 1.5x) |
| `code_refs` | references | basemind | 25 | 0 | 19 (0.76) | 0.877 | 1.000 | 1.000 | 119 | 123 | 1.10 | - | FAIL (median 1.10x < 1.5x) |
| `code_refs` | references | tokio | 25 | 0 | 16 (0.64) | 0.754 | 0.890 | 1.000 | 82 | 126 | 1.06 | - | FAIL (credited 0.64 < 0.7; median 1.06x < 1.5x) |
| `code_refs` | callers | basemind | 25 | 0 | 20 (0.80) | 0.880 | 1.000 | 1.000 | 108 | 119 | 0.83 | - | FAIL (median 0.83x < 1.5x) |
| `code_refs` | callers | tokio | 25 | 4 | 16 (0.64) | 0.698 | 0.742 | 1.000 | 90 | 88 | 0.68 | - | FAIL (credited 0.64 < 0.7; median 0.68x < 1.5x) |
| `code_refs` | implementations | basemind | 25 | 0 | 9 (0.36) | 0.419 | 0.480 | 1.000 | 23 | 84 | 0.60 | - | FAIL (credited 0.36 < 0.7; median 0.60x < 1.5x) |
| `code_refs` | implementations | tokio | 25 | 0 | 9 (0.36) | 0.511 | 0.680 | 1.000 | 82 | 110 | 0.49 | - | FAIL (credited 0.36 < 0.7; median 0.49x < 1.5x) |
| `code_refs` | dependents | basemind | 25 | 0 | 24 (0.96) | 0.980 | 1.000 | 0.980 | 33 | 19 | 0.50 | - | FAIL (median 0.50x < 1.5x) |
| `code_refs` | dependents | tokio | 25 | 0 | 19 (0.76) | 0.830 | 0.920 | 0.898 | 44 | 23 | 0.55 | - | FAIL (median 0.55x < 1.5x) |
| `find_files` | find | basemind | 25 | 0 | 15 (0.60) | 0.600 | 0.493 | 0.720 | 42 | 8 | 0.19 | - | FAIL (credited 0.60 < 0.7; median 0.19x < 1.5x) |
| `find_files` | find | tokio | 25 | 0 | 22 (0.88) | 0.880 | 0.741 | 0.880 | 44 | 10 | 0.21 | - | FAIL (median 0.21x < 1.5x) |
| `git_log` | git_search | basemind | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 1.000 | 267 | 25 | 0.11 | - | FAIL (median 0.11x < 1.5x) |
| `git_log` | git_search | tokio | 25 | 0 | 23 (0.92) | 0.967 | 1.000 | 0.965 | 158 | 38 | 0.23 | - | FAIL (median 0.23x < 1.5x) |
| `git_log` | git_path | basemind | 25 | 0 | 25 (1.00) | 1.000 | 0.925 | 1.000 | 1034 | 252 | 0.25 | - | FAIL (median 0.25x < 1.5x) |
| `git_log` | git_path | tokio | 25 | 0 | 25 (1.00) | 0.993 | 0.442 | 0.985 | 1217 | 157 | 0.11 | - | FAIL (median 0.11x < 1.5x) |
| `git_log` | git_symbol | basemind | 25 | 0 | 25 (1.00) | 1.000 | 1.000 | 0.980 | 124 | 27 | 0.20 | - | FAIL (median 0.20x < 1.5x) |
| `git_log` | git_symbol | tokio | 25 | 1 | 24 (0.96) | 0.957 | 0.910 | 0.702 | 119 | 35 | 0.22 | - | FAIL (median 0.22x < 1.5x) |
| `git_blame` | blame | basemind | 25 | 0 | 24 (0.96) | 0.983 | 0.983 | 1.000 | 140 | 803 | 2.76 | - | PASS |
| `git_blame` | blame | tokio | 25 | 0 | 24 (0.96) | 0.987 | 0.987 | 1.000 | 226 | 475 | 1.87 | - | PASS |
| `git_blame` | blame_symbol | basemind | 25 | 1 | 23 (0.92) | 0.942 | 0.942 | 1.000 | 161 | 359 | 2.24 | - | PASS |
| `git_blame` | blame_symbol | tokio | 25 | 2 | 22 (0.88) | 0.875 | 0.875 | 1.000 | 174 | 224 | 1.11 | - | FAIL (median 1.11x < 1.5x) |
| `git_diff` | diff | basemind | 25 | 0 | 25 (1.00) | 0.999 | 0.999 | 1.000 | 423 | 419 | 1.04 | - | FAIL (median 1.04x < 1.5x) |
| `git_diff` | diff | tokio | 25 | 0 | 25 (1.00) | 0.994 | 0.991 | 1.000 | 244 | 262 | 1.00 | - | FAIL (median 1.00x < 1.5x) |
| `git_diff` | diff_structural | basemind | 25 | 0 | 21 (0.84) | 0.865 | 0.913 | 1.000 | 139 | 4211 | 25.22 | - | PASS |
| `git_diff` | diff_structural | tokio | 25 | 0 | 18 (0.72) | 0.792 | 0.916 | 1.000 | 87 | 1432 | 9.80 | - | PASS |
