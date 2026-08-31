# The cluster is written against a transport it does not own, so it can be simulated

BoostSearch is becoming a real cluster: shards on different nodes, replicas,
elections, and shards moving without anyone editing a file. That is the part of
a search engine where the bugs are rare, timing-dependent and expensive, and
OpenSearch's own tests for it are Java tests against Java classes -- 295,676
lines that cannot be pointed at an HTTP server.

So the cluster layer takes its network and its clock as dependencies rather
than calling them: every message goes through a `Transport` trait and every
timeout reads an injected `Clock`. In production those are TCP and the system
clock. In tests they are a queue and a counter that a seed drives, so a whole
cluster runs inside one process and one thread, and any interleaving --
reordered messages, a partition, a node that dies mid-write, a clock that jumps
-- is something a test asks for rather than something it waits for. A failing
seed reproduces exactly, every time.

## Consequences

Nothing in the cluster layer may call `std::time` or open a socket directly,
and a review has to enforce that; the constraint is cheap while the code is
being written and close to impossible to retrofit afterwards. It also means the
simulation is only as honest as the model of the network beneath it, which is
why it does not replace running the real binary on real machines with real
partitions -- it finds the bugs that are too rare to catch that way.
