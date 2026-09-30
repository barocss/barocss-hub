# barocss-hub-bench (crates/hub-bench)

Scale benchmarks and a multi-replica scenario runner for barocss-hub
(Issue #27). It is a separate binary (`publish = false`, `test = false`), so
`cargo test --workspace` never runs it. It drives the release `barocss-hub` binary as
a subprocess under `/usr/bin/time -l` (macOS: wall time + peak RSS) with a
hard per-command timeout (the process group is SIGKILLed). All generated
data is seeded and deterministic. Results are printed and appended as
Markdown table rows to `<root>/results.md`.

Results and analysis: Wiki page `Performance`.

## Rerun

```sh
cargo build --release            # builds target/release/barocss-hub and barocss-hub-bench
B=/path/to/scratch/bench         # needs ~5 GB free for the blobs case
T="caffeinate -i timeout -s KILL"
E=./target/release/barocss-hub-bench

$T 1500 $E files    --root $B --n 1000,10000 --timeout 600
$T 700  $E files    --root $B --n 100000 --timeout 300      # expected: record TIMEOUT
$T 3600 $E ops      --root $B --files 5000 --per 5000 --targets 10000,100000,1000000 --timeout 600
$T 3600 $E delta    --root $B --files 10000 --k 10,1000,100000 --timeout 900
$T 1800 $E blobs    --root $B --count 1 --mb 1024 --timeout 600
$T 1800 $E blobs    --root $B --count 1024 --mb 1 --timeout 600
$T 600  $E watch    --root $B --n 1000,10000 --secs 60
$T 5400 $E scenario --root $B --r 2,10,50 --files 300 --edits 3 --timeout 600
$T 2400 $E d030     --root $B --r 100 --topo star --rounds 10 --per 100 --timeout 900   # D-030, Issue #34
$T 1300 $E d030     --root $B --r 100 --topo ring --rounds 10 --per 100 --timeout 900
rm -rf $B
ps -ax | grep -E 'barocss-hub (watch|record|sync)|barocss-hub-bench' | grep -v grep   # must be empty
```

Common flags: `--root` (data dir), `--hub` (default `target/release/barocss-hub`),
`--timeout` (seconds per hub command, default 300).

## Cases

| case | what it measures |
|---|---|
| `files --n N,..` | N files (1 KiB, 100/dir): init, first record, status clean / after 1 edit, record 1 edit, log, sync into an empty repo, checkout into an empty dir |
| `ops --files F --per P --targets T,..` | grows the op log with record rounds of P edits (identical content per round, so blobs dedup and op-log cost is isolated) and measures status/record/log/sync/checkout at each target; stops the axis when a command exceeds the timeout |
| `delta --files F --k K,..` | A and B share an F-file base; B records K ops offline; time `A sync B` and the following checkout |
| `blobs --count C --mb M` | C files of M MiB: record, status, sync into empty repo, checkout |
| `watch --n N,.. --secs S` | CPU time of an idle `barocss-hub watch` (default `--poll 100`) over S seconds, via `ps -o time`; then SIGINT + SIGKILL |
| `scenario --r R,.. --files F --edits E` | R replicas cloned from one base each record E offline rounds (5 edits, 1 add, sometimes a delete or move), then ring and star sync passes until all heads agree (max 4 passes); asserts equal head and identical checked-out trees; exit 1 if any run does not converge |
| `d030 --r R --topo star\|ring --files F --rounds N --per P [--passes M --every K]` | D-030 (Issue #34): R replicas copied from one base (`cp -R` + `barocss-hub reid`), each records N rounds of P unique-content edits (~R*N*P ops, one blob each); star (every replica syncs with replica 0) or ring passes until heads agree (max M); prints every K-th sync of pass 1 (wall, peak RSS), pass totals, status/record at replica 0; asserts same head and identical trees |

## Profiling

No profiler is required. On macOS the built-in `sample` works on the
release binary (symbols are present):

```sh
(target/release/barocss-hub record >/dev/null &); sleep 1
sample $(pgrep -x barocss-hub) 5 -file /tmp/rec.txt
awk '/Sort by top of stack/,/Binary/' /tmp/rec.txt
```
