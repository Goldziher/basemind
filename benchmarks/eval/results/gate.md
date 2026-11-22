# Value gate (ADR-0022, by job as ADR-0031 amends it)

Corpora: `basemind`, `tokio`; tokenizer: basemind `o200k`, tokio `o200k`.

Binary: `basemind 0.33.0`, source commit `ecebe76d654c41181db993eef72905123933f92b`, build `7bdf56cb440c`, features `tokenizer,code-intel,intelligence,documents,code-search`.

Every job needs a credited share. A reducing kind needs a median ratio; a locating kind a median
floor and mean recall close to the baseline's (`grep` also a symbol accuracy); a correctness kind
has no shell baseline and is judged on credited alone. The thresholds are `src/eval/gate.rs`'s, as
each corpus's `<corpus>.md` prints them. A baseline that errored or printed nothing (`base fail`)
scores recall 0 and is left out of the ratio.

| tool | verdict |
|---|---|
| `code_symbols` | PASS |
| `code_outline` | PASS |
| `code_read` | PASS |
| `code_grep` | FAIL |
| `code_search` | not run (needs the `code-search` build and an embedding model download (network is limited to cloning)) |
| `code_refs` | PASS |
| `find_files` | PASS |
| `docs_search` | not run (needs the `documents` build and an embedding model download (network is limited to cloning)) |
| `git_log` | PASS |
| `git_blame` | PASS |
| `git_diff` | PASS |
| `code_explain` | PASS |
| `memory_save`, `memory_search`, `index_status`, `index_rescan` | exempt (ADR-0022) |

Per kind and corpus. `R` is mean recall against the gold, `P` mean precision; tokens are
medians over all tasks of the kind; `x` is the median baseline/tool ratio over the credited tasks.

| tool | kind | job | corpus | tasks | err | credited | base fail | tool R | tool P | baseline R | tool tok | baseline tok | x | symbol acc | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `code_symbols` | symbols | locating | basemind | 100 | 0 | 99 (0.99) | 0 | 0.990 | 0.957 | 0.990 | 47 | 27 | 0.7518 | - | PASS |
| `code_symbols` | symbols | locating | tokio | 100 | 0 | 96 (0.96) | 0 | 0.972 | 0.887 | 0.978 | 94 | 41 | 0.7778 | - | PASS |
| `code_outline` | outline | reducing | basemind | 25 | 0 | 25 (1.00) | 0 | 0.995 | 0.952 | 1.000 | 669 | 2539 | 4.7078 | - | PASS |
| `code_outline` | outline | reducing | tokio | 25 | 0 | 21 (0.84) | 0 | 0.937 | 0.814 | 1.000 | 359 | 1402 | 2.2132 | - | PASS |
| `code_read` | read | reducing | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 188 | 5719 | 33.2764 | - | PASS |
| `code_read` | read | reducing | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 114 | 2796 | 26.4386 | - | PASS |
| `code_grep` | grep | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 39 | 33 | 0.7812 | 1.000 | PASS |
| `code_grep` | grep | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 53 | 55 | 0.8594 | 1.000 | PASS |
| `code_grep` | grep_changed | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 47 | 35 | 0.8333 | - | PASS |
| `code_grep` | grep_changed | locating | tokio | 20 | 0 | 20 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 74 | 84 | 0.9849 | - | PASS |
| `code_grep` | read_around | reducing | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 340 | 543 | 1.4295 | - | FAIL (median 1.43x < 1.50x) |
| `code_grep` | read_around | reducing | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 231 | 432 | 1.6118 | - | PASS |
| `code_refs` | references | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 32 | 123 | 3.5789 | - | PASS |
| `code_refs` | references | locating | tokio | 25 | 0 | 24 (0.96) | 0 | 0.990 | 1.000 | 1.000 | 44 | 126 | 3.0156 | - | PASS |
| `code_refs` | callers | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 70 | 119 | 1.6184 | - | PASS |
| `code_refs` | callers | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 70 | 88 | 1.2222 | - | PASS |
| `code_refs` | implementations | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 59 | 84 | 1.2683 | - | PASS |
| `code_refs` | implementations | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 67 | 110 | 1.2453 | - | PASS |
| `code_refs` | dependents | locating | basemind | 25 | 0 | 24 (0.96) | 0 | 0.980 | 1.000 | 0.980 | 17 | 19 | 0.8261 | - | PASS |
| `code_refs` | dependents | locating | tokio | 25 | 0 | 21 (0.84) | 1 | 0.898 | 0.960 | 0.898 | 27 | 23 | 0.8519 | - | PASS |
| `code_refs` | impact | locating | basemind | 25 | 0 | 23 (0.92) | 1 | 0.961 | 1.000 | 0.912 | 43 | 31 | 0.7167 | - | PASS |
| `code_refs` | impact | locating | tokio | 25 | 0 | 25 (1.00) | 3 | 1.000 | 1.000 | 0.682 | 39 | 89 | 1.4872 | - | PASS |
| `code_refs` | cycles | correctness | basemind | 26 | 0 | 22 (0.85) | 0 | 0.844 | 0.845 | - | 56 | - | - | - | PASS |
| `code_refs` | cycles | correctness | tokio | 26 | 0 | 24 (0.92) | 0 | 0.922 | 0.923 | - | 90 | - | - | - | PASS |
| `find_files` | find | locating | basemind | 25 | 0 | 25 (1.00) | 5 | 1.000 | 0.837 | 0.720 | 11 | 8 | 0.7273 | - | PASS |
| `find_files` | find | locating | tokio | 25 | 0 | 25 (1.00) | 1 | 1.000 | 0.821 | 0.880 | 13 | 10 | 0.7500 | - | PASS |
| `find_files` | find_large | locating | basemind | 23 | 0 | 23 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 93 | 79 | 0.8492 | - | PASS |
| `find_files` | find_large | locating | tokio | 20 | 0 | 20 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 106 | 94 | 0.8851 | - | PASS |
| `git_log` | git_search | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 32 | 25 | 0.7812 | - | PASS |
| `git_log` | git_search | locating | tokio | 25 | 0 | 23 (0.92) | 0 | 0.967 | 1.000 | 0.965 | 35 | 38 | 0.7714 | - | PASS |
| `git_log` | git_path | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 0.997 | 1.000 | 286 | 252 | 0.8544 | - | PASS |
| `git_log` | git_path | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 0.985 | 138 | 157 | 0.8310 | - | PASS |
| `git_log` | git_symbol | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 0.980 | 32 | 27 | 0.7667 | - | PASS |
| `git_log` | git_symbol | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 0.980 | 0.714 | 30 | 29 | 0.9200 | - | PASS |
| `git_blame` | blame | reducing | basemind | 25 | 0 | 24 (0.96) | 0 | 0.983 | 0.983 | 1.000 | 75 | 803 | 6.7090 | - | PASS |
| `git_blame` | blame | reducing | tokio | 25 | 0 | 24 (0.96) | 0 | 0.987 | 0.987 | 1.000 | 121 | 475 | 3.8410 | - | PASS |
| `git_blame` | blame_symbol | reducing | basemind | 25 | 0 | 24 (0.96) | 0 | 0.982 | 0.982 | 1.000 | 81 | 365 | 4.7071 | - | PASS |
| `git_blame` | blame_symbol | reducing | tokio | 25 | 0 | 24 (0.96) | 0 | 0.955 | 0.955 | 1.000 | 84 | 224 | 2.0896 | - | PASS |
| `git_diff` | diff | locating | basemind | 25 | 0 | 25 (1.00) | 0 | 0.999 | 0.999 | 1.000 | 381 | 419 | 1.1598 | - | PASS |
| `git_diff` | diff | locating | tokio | 25 | 0 | 25 (1.00) | 0 | 0.994 | 0.991 | 1.000 | 209 | 262 | 1.1716 | - | PASS |
| `git_diff` | diff_structural | reducing | basemind | 25 | 0 | 21 (0.84) | 0 | 0.880 | 0.916 | 1.000 | 114 | 4211 | 32.1864 | - | PASS |
| `git_diff` | diff_structural | reducing | tokio | 25 | 0 | 23 (0.92) | 0 | 0.973 | 0.996 | 1.000 | 74 | 1432 | 13.4603 | - | PASS |
| `code_explain` | explain | reducing | basemind | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 339 | 1637 | 4.9554 | - | PASS |
| `code_explain` | explain | reducing | tokio | 25 | 0 | 25 (1.00) | 0 | 1.000 | 1.000 | 1.000 | 320 | 1990 | 5.7573 | - | PASS |
