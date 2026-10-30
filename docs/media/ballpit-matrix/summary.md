| Cell | Engager hand lag behind controller | Observer hand lag | Engager rollbacks/s (sweep) | Observer rollbacks/s, re-sim ms/frame (sweep) | Engager tx (sweep) | Observer tx (sweep) | Max queue, queue drops | Peak snapshot memory | Balls >1 cm off at the observer: sweep mean / peak | Snapped ball-frames: engager / observer | Frozen ball-ticks: engager / observer | Converged after gesture end | Final state |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| `naive_0ms_unl` | 1.0 frames (17 ms) | 1.8 frames (30 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 13.2 / 54 | 70 / 655 | 0 / 1838 | 0.08 s | identical |
| `naive_50ms_unl` | 1.0 frames (17 ms) | 4.8 frames (80 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 39.0 / 94 | 70 / 655 | 0 / 1838 | 0.20 s | identical |
| `naive_150ms_unl` | 1.0 frames (17 ms) | 10.8 frames (180 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 56.6 / 114 | 70 / 655 | 0 / 1838 | 0.30 s | identical |
| `naive_300ms_unl` | 1.0 frames (17 ms) | 19.8 frames (331 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 0 ms, 0 | 0.0 MB | 78.8 / 138 | 70 / 655 | 0 / 1838 | 0.45 s | identical |
| `naive_50ms_256k` | 1.0 frames (17 ms) | 32.0 frames (533 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 500 ms, 3959 | 0.0 MB | 218.8 / 372 | 70 / 248 | 0 / 34781 | never (within 4.3 s) | DIFFERENT (367 balls off, max 1.65 m) |
| `naive_50ms_64k` | 1.0 frames (17 ms) | 31.1 frames (519 ms) | 0.0 | 0.0, 0.00 | 844.7 kB/s | 0.0 kB/s | 500 ms, 4063 | 0.0 MB | 233.1 / 388 | 70 / 111 | 0 / 18624 | never (within 4.3 s) | DIFFERENT (387 balls off, max 1.87 m) |
| `pose-d0_0ms_unl` | 1.0 frames (17 ms) | 1.0 frames (17 ms) | 0.0 | 0.5, 0.01 | 8.3 kB/s | 3.3 kB/s | 0 ms, 0 | 3.5 MB | 0.1 / 11 | 70 / 70 | 0 / 0 | by gesture end | identical |
| `pose-d0_50ms_unl` | 1.0 frames (17 ms) | 3.8 frames (63 ms) | 0.0 | 51.0, 2.35 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 16.2 / 105 | 70 / 1826 | 0 / 0 | by gesture end | identical |
| `pose-d0_150ms_unl` | 1.0 frames (17 ms) | 9.8 frames (164 ms) | 0.5 | 49.3, 5.20 | 10.6 kB/s | 3.7 kB/s | 0 ms, 0 | 18.7 MB | 57.3 / 129 | 329 / 3063 | 0 / 0 | by gesture end | identical |
| `pose-d0_300ms_unl` | 1.0 frames (17 ms) | 18.8 frames (314 ms) | 19.7 | 46.6, 7.23 | 18.0 kB/s | 35.3 kB/s | 0 ms, 0 | 33.5 MB | 116.6 / 222 | 8437 / 2260 | 0 / 0 | 0.47 s | identical |
| `pose-d0_50ms_256k` | 1.0 frames (17 ms) | 3.8 frames (63 ms) | 0.0 | 51.0, 2.13 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 16.2 / 105 | 70 / 1826 | 0 / 0 | by gesture end | identical |
| `pose-d0_50ms_64k` | 1.0 frames (17 ms) | 17.3 frames (289 ms) | 7.0 | 43.9, 5.74 | 9.8 kB/s | 13.6 kB/s | 500 ms, 783 | 54.8 MB | 105.1 / 243 | 4506 / 2077 | 2 / 0 | 2.97 s | identical |
| `pose-d4_0ms_unl` | 5.0 frames (83 ms) | 5.0 frames (83 ms) | 0.0 | 0.0, 0.00 | 8.3 kB/s | 3.3 kB/s | 0 ms, 0 | 3.5 MB | 0.0 / 0 | 70 / 70 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_unl` | 5.0 frames (83 ms) | 5.0 frames (84 ms) | 0.0 | 0.8, 0.07 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 2.0 / 84 | 70 / 202 | 0 / 0 | by gesture end | identical |
| `pose-d4_150ms_unl` | 5.0 frames (83 ms) | 9.8 frames (164 ms) | 0.5 | 49.3, 3.40 | 10.6 kB/s | 3.7 kB/s | 0 ms, 0 | 18.7 MB | 44.0 / 126 | 315 / 2583 | 0 / 0 | by gesture end | identical |
| `pose-d4_300ms_unl` | 5.0 frames (83 ms) | 18.8 frames (314 ms) | 0.7 | 46.6, 9.93 | 18.0 kB/s | 3.8 kB/s | 0 ms, 0 | 33.2 MB | 98.2 / 218 | 555 / 2955 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_256k` | 5.0 frames (83 ms) | 5.0 frames (84 ms) | 0.0 | 0.8, 0.06 | 8.8 kB/s | 3.3 kB/s | 0 ms, 0 | 8.1 MB | 2.0 / 84 | 70 / 202 | 0 / 0 | by gesture end | identical |
| `pose-d4_50ms_64k` | 5.0 frames (83 ms) | 17.7 frames (294 ms) | 6.2 | 29.8, 5.02 | 10.0 kB/s | 9.8 kB/s | 500 ms, 770 | 55.0 MB | 87.9 / 270 | 3615 / 2573 | 1531 / 1400 | 4.00 s | identical |
