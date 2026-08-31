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
