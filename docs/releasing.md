# Releasing

One tag, one workflow (`.github/workflows/release.yml`), one binary per
platform: `zsql`, which is the CLI and the server. Four builds go on the
release: Linux x86_64 and arm64, statically linked against musl, and
macOS on Apple silicon and Intel. With the `CLI_RELEASES_TOKEN` secret
set, the same files are published on `stratasite/0sql-cli`, which is
what `https://0sql.io/install.sh` reads through the permanent
`releases/latest/download/` redirect. That redirect ignores
prereleases, so anything published as one is invisible to the installer.

`.github/workflows/ci.yml` runs `cargo fmt --check`, clippy with warnings
as errors, the tests, the golden scoreboard, the CLI's feature
combinations and `cargo deny` on every push and pull request, and
compiles the CLI on macOS, because the release builds there and a
Linux-only name once reached a tag. A tag builds nothing but the
release, and does not wait for CI.

## Why musl, and static

A statically linked glibc cannot resolve names: it loads its name
service modules at runtime and dumps core without them, and the CLI
resolves a server's hostname while the server may be given one to bind.
That was observed, not assumed. musl resolves names and still links
statically, so one binary runs on any distribution. SQLite, behind
accounts, is C, so the x86_64 build installs `musl-gcc` and the arm64
build goes through `cross`. `rust-toolchain.toml` pins the toolchain a
release is built with; rustup reads it on its own.

## In order

1. **Bump the version in `Cargo.toml`**, under `[workspace.package]`:
   the first `^version` line in the file, and the one the workflow
   compares a tag against.

2. **Refresh `Cargo.lock` and commit it with the bump.** Every workspace
   crate appears in the lock file with its version, and the build runs
   `cargo build --locked`, which fails rather than updating it. `cargo
   check --workspace` is enough to rewrite it. Nothing locally complains
   until CI does.

3. **Run the tests.** `cargo test --workspace` and `cargo run --release
   -p zsql-devtool -- golden --strict`. CI runs them on the push, but a
   tag does not wait for CI, so nothing else stands between a broken
   commit and a tag.

4. **Tag and push the tag.** `git tag v0.6.0 && git push origin v0.6.0`.
   Pushing a branch builds nothing; only the tag does. The workflow
   refuses a tag that disagrees with the manifest, so a forgotten step 1
   costs a tag rather than a bad release.

5. **Let the workflow prove the binaries.** It asserts the Linux build
   is really static, runs `--version` and `--help` where it can, starts
   `zsql serve` from the binary and waits for `/healthz`, and asks a
   public server how it is from the static Linux binary, which exercises
   the resolver, TLS and HTTP in one go. If any of that fails there is
   no release, by design. It then publishes the four builds and one
   `SHA256SUMS`.

6. **Install it where it runs**, keeping the binary it replaces:

   ```
   gh release download v0.6.0 -p 'zsql-x86_64-linux' -p SHA256SUMS
   grep zsql-x86_64-linux SHA256SUMS | sha256sum -c
   cp /usr/local/bin/zsql /usr/local/bin/zsql.prev
   install -m755 zsql-x86_64-linux /usr/local/bin/zsql
   systemctl restart zsqld
   ```

   Then read the first lines it logs: `docs/operating.md` says what
   they mean. Rolling back is `zsql.prev` over it and another restart.

## Building without releasing

`release.yml` also has `workflow_dispatch`, which builds and attaches
the binaries to the run's artifacts without cutting a release or
needing a tag, so it skips the version check too. That is the way to
get a binary from a branch, or to try the pipeline after changing it:

```
gh workflow run release.yml --ref <branch>
gh run watch
gh run download <run-id>
```

A binary from a branch carries whatever version the manifest said, so
two different builds can call themselves the same thing. Fine for
trying something, wrong for leaving installed.
