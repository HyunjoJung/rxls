## Summary

Briefly describe the problem this PR solves and what changed.

## Related issue

closes #

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test --all-targets && cargo test --doc` pass
- [ ] `cargo doc --no-deps` builds clean
- [ ] New behaviour is covered by a test (a synthetic BIFF record is fine)
- [ ] Spec-referenced changes cite the relevant [MS-XLS]/[MS-CFB] section

## Notes

Anything reviewers should know (parsing edge cases, trade-offs, follow-ups).
