# The geoip processor, and the databases it reads

The `geoip` ingest processor turns an address into where it is. The lookup is
a MaxMind database in the MMDB format -- the same format, and the same files,
that OpenSearch reads. A cluster that already has its databases needs to
change nothing but where the server looks for them.

## Where they are looked for

In this order, first hit wins:

1. `$BOOSTSEARCH_GEOIP_PATH`
2. `$BOOSTSEARCH_CONFIG/ingest-geoip/`
3. `$BOOSTSEARCH_DATA/config/ingest-geoip/`
4. `./config/ingest-geoip/`
5. beside the binary: `ingest-geoip/`, then `../modules/ingest-geoip/`

The third and fourth are where OpenSearch keeps user-supplied databases
(`$OPENSEARCH_HOME/config/ingest-geoip`), and the last is where its
distribution keeps the ones it ships. Point any of them at a directory holding
`GeoLite2-City.mmdb`, `GeoLite2-Country.mmdb` and `GeoLite2-ASN.mmdb` and the
processor works exactly as OpenSearch's does.

A processor names its database with `database_file`, defaulting to
`GeoLite2-City.mmdb`. What a database can be asked for follows from what it
holds rather than from what it is called: a file whose metadata says ASN
answers `asn`, `organization_name` and `network` whatever its name is.

## Why they are not in this repository

They are MaxMind's, not ours. OpenSearch's distribution redistributes the
GeoLite2 databases under MaxMind's terms, with the attribution those terms
require, and shipping them here is a licensing decision for whoever cuts a
release rather than something to be done quietly in a commit. Seventy
megabytes of someone else's data does not belong in a source tree either.

What that means today: **the processor works, and finds nothing until it is
given a database.** A pipeline that uses it stores and runs; an address it
cannot look up leaves the document alone, which is what OpenSearch does for an
address its own databases do not know.

Before a release, one of these has to be chosen:

- **Ship them**, as OpenSearch does, with MaxMind's attribution and licence
  text carried alongside. This is what makes the engine a drop-in replacement
  for a cluster that relies on geoip out of the box.
- **Fetch them at install time**, from MaxMind directly with the user's own
  licence key, which is what MaxMind's terms are written for and what
  `geoipupdate` exists to do.
- **Ship nothing**, and document the directory. Correct, and the least useful.

The suites are run against the databases OpenSearch's own container carries,
copied to a directory outside the repository. That is enough to prove the
processor reads what OpenSearch reads and answers what OpenSearch answers; it
is not a decision about what a release contains.
