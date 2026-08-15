# Contributing

Thanks for improving `sbc`.

1. Open an issue for behavior changes or anything that affects the CLI contract.
2. Keep Docker's `sbx` CLI as the source of sandbox state; `sbc` should remain a
   thin client.
3. Add tests with behavior changes.
4. Run the project checks before opening a pull request:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Use Conventional Commit subjects. By contributing, you agree that your work is
licensed under the MIT License.
