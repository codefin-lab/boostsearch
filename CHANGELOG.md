# Changelog

## Versioning

Two numbers matter to somebody running this, and they are not the same number.

- **The BoostSearch version** is this project's own, and follows semantic
  versioning: a patch fixes something, a minor adds something, a major changes
  an answer somebody could have depended on.
- **The OpenSearch version it answers as** is what `GET /` reports, and is the
  version of the API this speaks. It moves when the API this targets moves,
  which is a different event from a release of this project.

An index written by one version is read by the next, and two adjacent versions
can be in one cluster at once — which is what makes a rolling upgrade
possible. [docs/upgrading.md](docs/upgrading.md) is the procedure, and
downgrading is not one of the things it can do.

## Unreleased

The first release has not been cut. What is built, and how much of it is
checked, is in [README.md](README.md); what it took and what was got wrong on
the way is in [docs/progress.md](docs/progress.md), task by task.

Against OpenSearch 3.1.0, measured rather than asserted:

- **1,100 of 1,100** sections of OpenSearch's core suite that are not skipped
- **880 of 890** sections of its module and plugin suites
- **165 of 183** canonical requests answered byte for byte identically
- **156 of 167** REST endpoints answered; the rest answer 501 rather than
  pretending
- **17 of 18** bench dimensions ahead

The ten sections that do not pass, and the one dimension that is behind, are
named with their reasons in `docs/progress.md`. Two of the ten need
dictionaries that are somebody else's to redistribute; one asserts that its
plugin is the only one installed, which a single binary cannot be.

Not yet done: the console's server (Phase 13), and a run of the bench matrix
on the hardware a release would be cut on rather than on a developer machine.
