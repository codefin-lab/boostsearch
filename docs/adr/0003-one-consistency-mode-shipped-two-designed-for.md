# One consistency mode ships, two are designed for

BoostSearch replaces OpenSearch, so in version 1 a write is acknowledged when
OpenSearch would acknowledge it and a read from a replica may be behind, which
is what the applications being moved across already expect -- and it is the
mode that does not spend latency OpenSearch does not spend. A stronger mode,
where a write is acknowledged by a quorum and reads go through a primary lease,
is wanted, but as `index.consistency: linearizable` in version 2.

So the replication path takes its acknowledgement policy and its read routing
as parameters from the start, with one value each for now. Retrofitting a
second mode into a path built around a single one is the expensive way to
arrive at the same place.

## Consequences

A reader will find an enum with one useful variant and a code path that looks
more general than it needs to be. That is deliberate. The test layers know
about the parameter too: adding the second mode means running the simulation
and the linearizability checks again under it, not writing them again.
