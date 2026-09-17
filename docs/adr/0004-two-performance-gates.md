# Two performance gates, because they answer different questions

Beating OpenSearch on every dimension is a promise this project makes, and a
promise nobody checks is a promise nobody keeps. But a single hard gate would
have blocked the translog -- durability cost 23% of indexing throughput, and
that was the right trade -- so the check is split in two.

**Every commit** is measured against *our own last measurement*, and CI fails
if any dimension falls more than 5%. This is what keeps performance a design
concern while features are being written: a change that costs throughput has to
say so in its commit message rather than be noticed a quarter later.

**Every release** is measured against OpenSearch, and every dimension must be
ahead. Red means no release, not a warning. Between releases a dimension may be
behind while a feature lands; it may not be behind on the day the version is
cut.

## Consequences

Correctness comes first and tuning comes after, but not silently: a dimension
that is behind is visible in CI from the commit that made it so, and the
release gate is what forces it to be paid back before anyone else sees it. Only
dimensions ahead by more than 20% are claimed publicly; between 5% and 20% we
report parity, because a number that close is one tuning pass on the other side
away from being wrong.

## Status

Half enforced, and the half that is enforceable without an OpenSearch to hand.

`tools/bench_gate.py` measures this build against a baseline of this repository's
own numbers (`tools/bench_baseline.json`) and reddens when a dimension falls
more than 5%. The baseline is taken from three runs, so it records what the
machine's own spread is dimension by dimension, and a fall smaller than that
spread is not counted -- a gate that cannot tell noise from a change is a gate
nobody believes. It also records which machine it was taken on: on another
machine the comparison is printed and nothing fails, because a slower laptop
is not a regression.

The release gate -- every dimension ahead of OpenSearch -- does not need
OpenSearch running every time. It was measured once, on this corpus, on one
machine, beside this engine measured the same way; both sets of numbers are in
`bench/results/final-os-clean-*.json` and `final-velosearch-clean-*.json`, and every
run of the gate reports what they said: **ahead on all 34 dimensions**. That is
a reading of a file rather than a fresh measurement, and it is labelled as
such. Measuring against OpenSearch again is a thing to do when the reference
version changes, not a thing to do on every commit.

What is still not automatic: `.github/workflows/ci.yml` runs the gate, but a
GitHub runner is not the machine the baseline was taken on, so there it prints
the comparison and does not fail. A machine-matched baseline is what makes it
a gate; on a dedicated bench machine, `--strict` makes it one everywhere.
