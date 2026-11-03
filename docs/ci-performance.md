# CI performance measurements

## Baseline

The baseline is the five most recent completed `CI` push runs on `main`
observed on 2026-11-03. Run 41597292858 was still in progress when the cohort
was selected and is excluded. All five selected runs succeeded. Durations
are GitHub's displayed workflow durations, including job scheduling and
cleanup; they are not the sum of concurrent job durations.

| Run | Commit | Duration |
| --- | --- | ---: |
| [41596192404](https://github.com/trailofbits/coop/actions/runs/41596192404) | `33d524b` | 307 s |
| [41594769523](https://github.com/trailofbits/coop/actions/runs/41594769523) | `8566fbf` | 326 s |
| [41457429700](https://github.com/trailofbits/coop/actions/runs/41457429700) | `dd74439` | 263 s |
| [41453105226](https://github.com/trailofbits/coop/actions/runs/41453105226) | `8c22b2a` | 331 s |
| [41317217555](https://github.com/trailofbits/coop/actions/runs/41317217555) | `2205a4a` | 296 s |

Median: **307 s**. Worst case: **331 s**. A 30% improvement requires a
median at or below **214.9 s**. A matching 30% tail improvement requires a
worst case at or below **231.7 s**.

Timestamped job logs show that `check` is the critical path in every sample.
Its Cargo cache restores a 3.21 GB archive, taking approximately 51–82 s.
Updater/uninstall steps consume approximately 77–127 s together. Kani takes
approximately 86–117 s and already runs concurrently. The test inventory and
policy assertions are unchanged by splitting the two independent workloads
onto separate runners; the serial order within each workload is retained.

## Existing work

- [PR #626](https://github.com/trailofbits/coop/pull/626) changes the functional
  policy matrix to development builds with immutable artifact reuse and an
  optimized smoke gate. This workflow split does not implement those changes.
  When combining them, move its shared policy runner to `policy` and include
  its development profile in that job's cache.
- [PR #580](https://github.com/trailofbits/coop/pull/580) and
  [PR #581](https://github.com/trailofbits/coop/pull/581) already batch Lima
  inventory and Claude directory setup. They are present in the current base.

## Acceptance

Compare three complete passing candidate runs on the same hosted runner class
against this frozen cohort. Report the first cold-cache run separately from
warm repeats, every required job's result, assertion/skip counts, median,
worst case, and total runner time. Cache misses and failed attempts must remain
visible; a failed or partial run is not a performance win.

The 30% reduction has **not yet been measured** for this change. Full Lima and
Firecracker VM performance is a separate benchmark; these CI runs cannot
establish an improvement in image provisioning or VM startup.
