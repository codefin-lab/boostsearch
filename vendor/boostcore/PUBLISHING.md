# Publishing BoostCore to crates.io

Eight crates, and each one has to be on the registry before the next can name
it. `ownedbytes` is not among them: our copy was byte for byte the published
0.9.0, so `boostcore-common` takes it from crates.io like anyone else.

## Order

```bash
cargo login                      # once, with a token from crates.io/me

cd tokenizer-api    && cargo publish && cd ..
cd bitpacker        && cargo publish && cd ..
cd common           && cargo publish && cd ..
cd query-grammar    && cargo publish && cd ..
cd stacker          && cargo publish && cd ..   # needs common
cd sstable          && cargo publish && cd ..   # needs common, bitpacker
cd columnar         && cargo publish && cd ..   # needs stacker, sstable, common, bitpacker
cargo publish                                   # boostcore itself
```

The registry takes a few seconds to index each one; if a publish fails saying a
dependency does not exist, wait and run it again.

## What is already checked

`cargo publish --dry-run` passes for the four crates whose dependencies are all
on the registry already: `tokenizer-api`, `bitpacker`, `common`,
`query-grammar`. The others cannot be dry-run until the crates they name are
published, which is the same reason the order above matters.

## Versions

They are tantivy's, because the code is: `boostcore` is 0.26.1, and each
sub-crate keeps the version its upstream had. The names are new, so nothing
collides. A change of our own bumps the patch level from there.

## Afterwards

BoostSearch depends on BoostCore by path, at `vendor/boostcore`. Once these are
published it can name the registry version instead and drop the directory.
