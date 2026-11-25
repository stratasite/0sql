# The Linux build: from static musl to glibc

A plan, started 2026-11-25 and reviewed the same day. Phases are the
checklist; tick them off here.

## What changes, and why

Today a release carries two Linux binaries, statically linked against
musl. A static binary cannot load a shared library, so the ADBC code
is compiled out of them (`#[cfg(not(target_env = "musl"))]`) and a
Linux `zsql` always reaches Snowflake and BigQuery over REST, with
every column as text.

After this, a release carries one Linux binary per architecture,
dynamically linked against glibc and built on Ubuntu 22.04. It loads
the ADBC drivers when they are installed, as the macOS builds already
do, and it is the same kind of build `bin/ci` and CI test. There is no
musl build.

Decided on 2026-11-25:

- **One Linux build, glibc.** No musl build beside it and no fallback.
- **Built on Ubuntu 22.04**, so it needs glibc 2.35 or newer at most.
  No image with an older glibc: a server runs in a container whose
  base we choose, and the requirement is stated.
- **The installer refuses** a machine without glibc, or with one too
  old, and says why.
- **The installer looks for `dbc`** and, without it, says how to get it
  and the drivers, and recommends them.

This reverses a decision recorded on 2026-11-24 in `docs/exec-layer.md`
and issue #41: "a separate, dynamically linked executor the command
line hands a query to, not a dynamic `zsql`". It is a dynamic `zsql`.

## What it means for whoever installs it

| Works | Does not |
|---|---|
| Ubuntu 22.04 and newer | Ubuntu 20.04 |
| Debian 12 and newer | Debian 11 |
| Fedora, Arch, RHEL 10 | Alpine, `scratch` |
| a container based on any of those | |

RHEL 9 and Amazon Linux 2023 carry glibc 2.34, one short of what
Ubuntu 22.04 has. Whether they work depends on the newest symbol the
binary actually asks for, which is often older than the build
machine's glibc. Phase 0 measures it, and the table, the installer and
the release check all follow the measured number.

macOS does not change: its builds are dynamic already and have the
ADBC code in them.

## Where it is built

GitHub retires the `ubuntu-22.04` and `ubuntu-22.04-arm` runners on
2027-04-17, with days of deliberate failures from 2027-03-23
(actions/runner-images#14254). So the build does not run on that
runner: it runs **in an `ubuntu:22.04` container on a current
runner**, x86_64 on `ubuntu-24.04` and arm64 on `ubuntu-24.04-arm`.
The glibc is the same 2.35, the floor does not move when a runner is
retired, and the same container builds it on a laptop.

GitHub's documentation lists the arm64 Linux runners as standard for
private repositories. The comment in the workflow that this plan has
none predates that; phase 1 finds out by running one. If it is
refused, arm64 is built in the same container under emulation, or in
cross's glibc container with its floor measured.

## Open, to settle in phase 0

- **The measured floor**, as above.
- **The C++ runtime.** A plain glibc build needs the host's
  `libstdc++.so.6` and `libgcc_s.so.1`. `CXXSTDLIB=static:-bundle=stdc++`
  is what the `cc` crate reads to link the first statically;
  `-static-libgcc` may do the second. Recommended: both linked in, if
  it links and runs, so that glibc is the only requirement.
- **The compiler.** Ubuntu 22.04's default is GCC 11. Use it unless
  DuckDB will not build; a newer one's static runtime can raise the
  floor.
- **The drivers' own needs.** The Snowflake and BigQuery drivers are
  shared libraries with a glibc floor of their own, which Columnar
  does not state. Read it off them.

No other system library is expected: TLS is rustls, SQLite and DuckDB
are bundled, compression is Rust, and nothing sets an allocator.

## Phases

- [ ] **Phase 0 — a trial build, publishing nothing.** On a branch,
  first on a laptop in the container, then through the release
  workflow's manual trigger (its release job runs only on a tag):
  - x86_64 built in `ubuntu:22.04` for `x86_64-unknown-linux-gnu`;
  - what it links to, and the newest glibc symbol it needs;
  - it starts and serves in `ubuntu:22.04` and `debian:12`; what it
    does in `amazonlinux:2023` and `alpine`;
  - the SQLite ADBC driver installed with `dbc`, and the two tests
    that need only it run: `a_driver_answers_in_typed_batches_under_the_cap`
    and `a_statement_past_the_timeout_is_given_up_on_even_when_it_cannot_be_cancelled`
    in `crates/datasource/src/adbc.rs`. The other five ignored tests
    there need the warehouse drivers, the network or a BigQuery
    project, and are not part of this;
  - the glibc the Snowflake and BigQuery driver libraries need;
  - its size beside the musl build's;
  - the two builds serving the same requests, 32 at a time against a
    DuckDB datasource, to see whether musl's allocator against
    glibc's is a difference worth stating. A number to report, not a
    gate;
  - the minimum macOS version the macOS builds carry, read off the
    binary, to state beside the Linux requirement;
  - **the glibc of the server that runs `app.0sql.io`.** It installs
    the released Linux binary. Below the floor, its service will not
    start after the upgrade.

  If the build does not compile or link in the container, the plan
  stops here and is revisited.

- [ ] **Phase 1 — arm64.** On `ubuntu-24.04-arm` in the same
  container. The arm64 binary is run before a release carries it:
  today nothing runs the cross-built one. arm64 links with GNU ld
  where x86_64 does not (the note in
  `crates/datasource/src/adapters/duckdb.rs`); confirm DuckDB's
  extensions link.

- [ ] **Phase 2 — the release workflow.**
  - The Linux rows become the glibc targets, built in the container.
    The asset names stay `zsql-x86_64-linux` and `zsql-aarch64-linux`,
    so every download URL keeps working.
  - "It is really static" goes. In its place: the binary links to
    nothing but glibc's own libraries, needs no symbol newer than the
    measured floor, and runs in a container at that floor.
  - "It really runs", "The server really starts" and "It can reach a
    server" keep running, now on both Linux builds. The last still
    asks `app.0sql.io`; it wants another host before that one is taken
    down.
  - A step proves a driver loads from the released binary. The binary
    only loads a driver for a Snowflake or BigQuery connection, so
    this needs either a command that lists the drivers it can find
    (which the installer could use too) or a Snowflake run against an
    account that does not exist, failing after the load. Decide in
    phase 0.
  - `Cross.toml` and the `cross` step go. `rust-toolchain.toml` names
    the glibc target.

- [ ] **Phase 3 — CI covers the driver path.** `bin/ci` and
  `.github/workflows/ci.yml` install `dbc` and the SQLite driver and
  run the two tests named in phase 0, in place of `--skip
  adbc::tests`. Until then a green run says nothing about the drivers.

- [ ] **Phase 4 — the installer.** `public/install.sh`, in the
  `0sql-marketing` repository. Two changes, shipped apart.

  **Now, ahead of everything:** it moves the download over the
  installed binary and only then tries to run it. On a machine the new
  build cannot run on, that replaces a working `zsql` with a dead one.
  Run the download from the temporary directory first and leave the
  installed one alone when it fails. Safe with today's builds.

  **With the release in phase 6, and not before:**
  - On Linux: find the C library and its version. musl, or glibc older
    than the floor, stops with what was found, what is needed, and the
    container bases that work. A pinned `ZSQL_VERSION` of 0.7 or older
    is a musl build and is not refused.
  - On Linux and macOS: look for `dbc`. Without it, say what the
    drivers give (typed columns and faster reads from Snowflake and
    BigQuery), how to install `dbc` and the two drivers, and that it
    is recommended. With it, say which of the two are installed.
  - Its header no longer says the binary is static and has no
    dependencies.

  Anything that fetches `releases/latest/download/zsql-x86_64-linux`
  without the installer, a Dockerfile on Alpine for one, gets the new
  build with no check. The release notes are the only warning it has.

- [ ] **Phase 5 — the docs and what the code says of itself.**
  - `docs/releasing.md`: "Why musl, and static" becomes why glibc, the
    floor, and what moves it; its other mentions of static with it.
  - `docs/operating.md`: what a server's machine or container needs,
    how to install the drivers, and where a server finds them.
  - `README.md` and, in `0sql-marketing`,
    `src/components/OpenSource.astro`: "One static binary" goes; the
    requirement is stated under installing.
  - `docs/exec-layer.md`: the notes that the Linux release cannot load
    a driver, and the reversed decision.
  - `ops/zsqld.service`: the comment that the binary is static, and
    more than the comment. `ProtectHome=yes` hides a driver installed
    under a home directory, and `MemoryDenyWriteExecute=yes` and the
    system call filter have never had a Go driver loaded under them.
    Load one under the unit, and say in `operating.md` where the
    drivers go for a service (`/etc/adbc/drivers` or
    `ADBC_DRIVER_PATH`).
  - The `target_env = "musl"` gates in `crates/datasource`
    (`Cargo.toml`, `src/lib.rs`, `src/adbc.rs`,
    `src/adapters/duckdb.rs`) stay: someone can still build for musl
    and get a REST-only binary. Their comments stop saying that is
    what a release is.
  - Issue #41: "ADBC on the Linux release" closes, with the reversal
    said. "Installing a driver without a second tool" stays open.
  - The older plans that mention musl (`execution-engine.md`,
    `exec.md`, `execution.md`, `local-sql.md`) are history and stay
    as written.

- [ ] **Phase 6 — the release.** A minor version, since where it runs
  changes. In order: the server of `app.0sql.io` is known to be at or
  above the floor; the workflow publishes the glibc builds; the new
  installer goes live; the release notes name the requirement and what
  no longer works.

## Not in this plan

- **A container image** for the server, with `dbc` and the drivers in
  it. Wanted, and separate.
- **The Go drivers in a server that lives for weeks**: what a crash in
  one does, how its memory grows, what it does to signal handling.
  Recommending the drivers in the installer makes this more pressing;
  `docs/exec-layer.md` has the list.
- **A Snowflake account through the driver.** Only BigQuery has been
  run.
