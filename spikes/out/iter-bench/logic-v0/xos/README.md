# Cross-OS script-build identity (scripts/iter_bench_xos.sh)

- `before-fix/`: main 3353d65f. The lamp built on Windows and in WSL (Ubuntu 24.04), both with rustc 1.99.0
  b940084d7 and LLVM 23.1.1: 9,473 bytes each, sha256 d71f7a24 (Windows) and aaf59021 (Linux), 3 bytes apart
  (`cmp.txt`, offsets 8808/8812/8825 in the data section: `\` against `/` in panic-location paths). The linux log's
  `linux` and `linux bytes` lines are empty: the script then ran `wsl bash -c`, whose login shell expanded `$(...)`
  early; the sha256 line has no `$(...)` and is valid (and equals the module fetched with `wsl --exec cat`).
- `xos-*.log`, `xos.log`: ws/logic b6da1e2d, with every source file remapped whole on Windows: SAME aaf59021.
- Both runs: Git Bash under heavy.sh in Windows Session 0, the WSL side on WSL's own file system, sources streamed
  in by `git archive`.
