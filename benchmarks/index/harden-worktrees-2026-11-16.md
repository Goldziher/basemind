### ripgrep: 5 added worktrees, sibling seeding on

| worktree | commit | files | diff vs HEAD | open ms | scan ms | resolve ms | resolve share | reopen ms | journal | keyspaces | vectors | file map | other | total | new blobs |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| main | 178cc027e2 | 225 | 0 | 6404 | 24890 | 367 | 1% | 625 | 4.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 4.6 MB | 1.0 MB |
| wt-1 | f03c67e94f | 222 | 31 | 515 | 1922 | 260 | 14% | 238 | 6.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 6.6 MB | 184.0 KB |
| wt-2 | 57c190d56e | 220 | 54 | 694 | 6139 | 365 | 6% | 400 | 8.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 8.6 MB | 296.0 KB |
| wt-3 | 9ec08522be | 220 | 60 | 679 | 4583 | 180 | 4% | 233 | 8.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 8.6 MB | 196.0 KB |
| wt-4 | 83d94672ae | 221 | 91 | 737 | 9838 | 414 | 4% | 265 | 10.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 10.6 MB | 556.0 KB |
| wt-5 | de4baa1002 | 213 | 102 | 752 | 8548 | 271 | 3% | 353 | 10.4 MB | 168.0 KB | 0.0 B | 28.0 KB | 12.0 KB | 10.6 MB | 352.0 KB |

Per added worktree (mean of 5): 9.0 MB total = journal 8.8 MB + keyspaces 168.0 KB + vectors 0.0 B + file map 28.0 KB + other 12.0 KB; journal logical length 8.0 MB; open 675 ms, first scan 6206 ms.
