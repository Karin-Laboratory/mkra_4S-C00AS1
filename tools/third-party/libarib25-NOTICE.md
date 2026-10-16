# MULTI2 reference

The scalar Rust implementation in `../multi2.rs` follows the MULTI2 schedule and
CBC/OFB behavior implemented in stz2012/libarib25, commit
`93ff3afd0a13f42724b20564873066f470825e78` (Apache License 2.0).
Source: https://github.com/stz2012/libarib25/tree/93ff3afd0a13f42724b20564873066f470825e78

The Rust module is a separate implementation used by the saved-TS diagnostic
and the mkra live ISDB-S server.
It has been changed to use Rust wrapping arithmetic, fixed four rounds, and no
SIMD or reference-counted interface. It does not access a smart card.

The adjacent `libarib25-LICENSE` preserves the upstream license.
`../fixtures/multi2-reference.txt` contains only synthetic test data, generated
by `../verify_multi2_reference.cc` with the pinned upstream header implementation.
No card identity, broadcast keys, or captured programme data is in these files.

Regenerate after checking out that commit under `target/libarib25-reference`:

```sh
g++ -std=c++11 -O2 -I target/libarib25-reference/src tools/verify_multi2_reference.cc -o /tmp/mkra-multi2-reference
/tmp/mkra-multi2-reference > tools/fixtures/multi2-reference.txt
rustc --edition 2021 --test tools/multi2.rs -o /tmp/mkra-multi2-tests
/tmp/mkra-multi2-tests
```
