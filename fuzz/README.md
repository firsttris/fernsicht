# Fuzzing

Coverage-guided fuzzing of everything that parses untrusted network input.

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cargo +nightly fuzz run proto_decode --target x86_64-unknown-linux-gnu -- -max_total_time=60
cargo +nightly fuzz run reassembler -- -max_total_time=60
cargo +nightly fuzz run fec_roundtrip -- -max_total_time=60
cargo +nightly fuzz run synthetic_decode -- -max_total_time=60
```

| Target | Checks |
|---|---|
| `proto_decode` | Any datagram: no panic; parsed packets re-encode losslessly |
| `reassembler` | Hostile datagram streams: no panic, completed frames have the declared length |
| `fec_roundtrip` | Arbitrary frames and loss within the FEC budget reconstruct exactly |
| `synthetic_decode` | Malformed bitstreams are rejected without panicking |

CI runs every target for a short time on each push and longer every night.
Crashes are uploaded as artifacts; add the input to a regression test before
fixing.
