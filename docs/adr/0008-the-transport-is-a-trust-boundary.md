# The transport is a trust boundary, and it is drawn at the connection

Everything a node believes about the cluster arrives through the transport
port. Until now that port answered anyone: the handshake read the peer's own
description of itself and checked one thing, that the cluster name matched.
Whoever could open a TCP connection to it was a node.

What that bought an attacker, with no credentials at all:

| one frame | what it did |
|---|---|
| `internal:transport/forward` | ran any REST request, as any user, with any roles: the forwarded envelope carries its own `caller` and the receiving node runs the request as it |
| `internal:cluster/coordination/start_join` with `term: u64::MAX` | every node moves to that term, no election can ever reach it, and the cluster has no manager again |
| `internal:cluster/shard_started` | any copy of any shard reported as in sync, on any node |
| any of them, spoofed | the envelope carries the sender's name as a string, so a frame could claim to come from the manager |

The security layer this project already has sits in the REST path
([0005](0005-security-is-in-the-query-path-not-in-front-of-it.md)). It judges
what a user may do. It says nothing about what a *node* may do, because
between nodes there was nothing to judge: a peer was a peer because it said
so.

## What was considered

**A shared secret in the handshake.** One value in the settings, presented by
both sides. Cheap, and it does bound who may join, but it is the same secret
on every node, it travels over a plaintext connection where it can be read,
and it says nothing about which node a peer is once several of them hold it.

**Judging each message by its content.** Refusing a `start_join` whose term
is absurd, capping how far a term may jump, sanity-checking a shard report.
Every one of these is a guess at what an attacker would send, and none of
them help against the forwarded request, which is a request the cluster is
supposed to run.

**Mutual TLS on the transport, with the certificate as the node's identity.**
What OpenSearch's security plugin does, and what its settings are already
named for. A node presents a certificate signed by the cluster's CA; the
other end verifies it before a single frame is read. There is no secret to
leak into a log, the trust is per node rather than per cluster, and an
operator who already runs the reference has the material to hand.

## The decision

**A node is a peer because its certificate says so, and a frame is from the
peer whose connection carried it.**

1. `plugins.security.ssl.transport.enabled` puts the transport behind mutual
   TLS: both sides present a certificate, both verify it against
   `pemtrustedcas_filepath`, and a connection that cannot be verified is
   never handled. `plugins.security.nodes_dn` narrows it further, to the
   certificates whose subject is on the list -- a client certificate issued
   for a user is then not a node.
2. The identity in the handshake is bound to the connection. Every frame that
   arrives is stamped with the peer that shook hands, whatever the sender
   wrote in the envelope, so no peer can speak as another and a response
   cannot be answered by a third party.
3. A node refuses to listen for transport connections on a non-loopback
   address with transport TLS off, unless the operator says so with
   `BOOSTSEARCH_TRANSPORT_INSECURE=true`. A single-node development server on
   127.0.0.1 needs no certificates; a node reachable from another machine
   does. This is the same rule the HTTP port already follows.
4. Coordination messages are additionally held to the cluster's own
   membership: an election is started, and a shard is reported, only by a
   node the cluster knows. That is not the trust boundary -- (1) is -- but it
   keeps a node that is merely *reachable* from moving the cluster.

## What follows from it

A cluster is configured now: with TLS off it is a single machine's cluster,
and with TLS on there are certificates to issue and renew. That is the cost,
and it is the reference's cost too.

The forwarded `caller` is still taken as given. It is worth being plain about
why that is now sound and was not before: the caller was authenticated by the
node that received the REST request, and that node is one this node verified
by certificate. The trust is transitive through a boundary that exists.
