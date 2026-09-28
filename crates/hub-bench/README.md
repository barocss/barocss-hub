# hub-bench

Scale benchmarks and a multi-replica scenario runner for barocss-hub
(Issue #27). It is a separate binary (`publish = false`, `test = false`), so
`cargo test --workspace` never runs it. It drives the release `hub` binary as
a subprocess under `/usr/bin/time -l` (macOS: wall time + peak RSS) with a
hard per-command timeout (the process group is SIGKILLed). All generated
data is seeded and deterministic. Results are printed and appended as
Markdown table rows to `<root>/results.md`.

Results and analysis: Wiki page `Performance`.

## Rerun

```sh
cargo build --release            # builds target/release/hub and hub-bench
B=/path/to/scratch/bench         # needs ~5 GB free for the blobs case
T="caffeinate -i timeout -s KILL"
E=./target/release/hub-bench

$T 1500 $E files    --root $B --n 1000,10000 --timeout 600
$T 700  $E files    --root $B --n 100000 --timeout 300      # expected: record TIMEOUT
$T 3600 $E ops      --root $B --files 5000 --per 5000 --targets 10000,100000,1000000 --timeout 600
$T 3600 $E delta    --root $B --files 10000 --k 10,1000,100000 --timeout 900
$T 1800 $E blobs    --root $B --count 1 --mb 1024 --timeout 600
$T 1800 $E blobs    --root $B --count 1024 --mb 1 --timeout 600
$T 600  $E watch    --root $B --n 1000,10000 --secs 60
$T 5400 $E scenario --root $B --r 2,10,50 --files 300 --edits 3 --timeout 600
rm -rf $B
ps -ax | grep -E 'hub (watch|record|sync)|hub-bench' | grep -v grep   # must be empty
```

Common flags: `--root` (data dir), `--hub` (default `target/release/hub`),
`--timeout` (seconds per hub command, default 300).

## Cases

| case | what it measures |
|---|---|
| `files --n N,..` | N files (1 KiB, 100/dir): init, first record, status clean / after 1 edit, record 1 edit, log, sync into an empty repo, checkout into an empty dir |
| `ops --files F --per P --targets T,..` | grows the op log with record rounds of P edits (identical content per round, so blobs dedup and op-log cost is isolated) and measures status/record/log/sync/checkout at each target; stops the axis when a command exceeds the timeout |
| `delta --files F --k K,..` | A and B share an F-file base; B records K ops offline; time `A sync B` and the following checkout |
| `blobs --count C --mb M` | C files of M MiB: record, status, sync into empty repo, checkout |
| `watch --n N,.. --secs S` | CPU time of an idle `hub watch` (default `--poll 100`) over S seconds, via `ps -o time`; then SIGINT + SIGKILL |
| `scenario --r R,.. --files F --edits E` | R replicas cloned from one base each record E offline rounds (5 edits, 1 add, sometimes a delete or move), then ring and star sync passes until all heads agree (max 4 passes); asserts equal head and identical checked-out trees; exit 1 if any run does not converge |

## Profiling

No profiler is required. On macOS the built-in `sample` works on the
release binary (symbols are present):

```sh
(target/release/hub record >/dev/null &); sleep 1
sample $(pgrep -x hub) 5 -file /tmp/rec.txt
awk '/Sort by top of stack/,/Binary/' /tmp/rec.txt
```
