# Engine crates

The four engine crates Spindle builds on. They keep their `dext-*` names because they
come from the Dext coding agent; this directory holds only these crates, with their own
workspace manifest and lockfile.

| Crate | Responsibility |
| --- | --- |
| [`dext-core`](core) | Model, turn, recovery, response and image state types (no I/O) |
| [`dext-transcript`](transcript) | Transcript representation, checkpoint codecs, checked continuation |
| [`dext-native`](native) | Strict and host command runners, process and cgroup cleanup, limits, SSE and stream parsing, wire attempts, model transport |
| [`dext-provider`](provider) | Provider and model catalog, credentials and sign-in, request shaping per wire format |

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

Feature flags, portability and the provider catalog are documented in
[docs/engine.md](../docs/engine.md).
