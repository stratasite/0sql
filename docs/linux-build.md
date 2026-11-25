# The Linux build: from static musl to glibc

A plan, started 2026-11-25 and reviewed the same day. Phases are the
checklist; tick them off here. "What the trial found" is phase 0's
answer, and where it differs from what the plan first assumed, it is
the trial that stands.

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
  (Measured: 2.34.)
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
| RHEL 9 and its clones, Amazon Linux 2023 | Alpine, `scratch` |
| Fedora, Arch, RHEL 10 | |
| a container based on any of those | |

The binary is built against glibc 2.35 and asks for nothing newer than
2.34, so RHEL 9 and Amazon Linux 2023 are in.

macOS does not change: its builds are dynamic already and have the
ADBC code in them. They need macOS 11.

## What the trial found

Run on 2026-11-25, x86_64, built in `ubuntu:22.04` on a laptop.

| | |
|---|---|
| compiler | GCC 11.4, Ubuntu 22.04's own. DuckDB builds; 6 m 43 s on 12 cores |
| links to | `libc`, `libm`, `libgcc_s`. `libstdc++` is linked in |
| newest glibc symbol | 2.34 (`dlopen`, `pthread_create` and their kin) |
| runs and serves in | `ubuntu:22.04`, `debian:12`, `amazonlinux:2023`, `rockylinux:9` |
| refuses to start in | `debian:11` (glibc 2.31), `alpine:3` |
| size | 91.6 MB, against the musl build's 89.8 MB |
| drivers | Snowflake 1.16.0 and BigQuery 1.15.0 load from the release binary, in Ubuntu 22.04 and Amazon Linux 2023. They are Go, link to glibc alone and need nothing newer than 2.3.2. The SQLite driver, used only by tests, needs the host's `libstdc++` |
| under the service unit | both drivers load with `MemoryDenyWriteExecute`, the system call filter and the rest of `ops/zsqld.service`'s restrictions. Loading only: no query was run through one |

`libgcc_s` stays dynamic: Rust's own runtime asks for it by name. It is
part of the base system of every glibc distribution tried.

**The two builds under load**, one server each, same machine, three
runs each, 0.7.0 musl against this build:

| | musl | glibc |
|---|---|---|
| a DuckDB aggregate over 20 million rows, three times in a row | 5.0 to 5.5 s | 3.4 s |
| 96 SQLite reads of 300,000 rows, 32 at a time, each kept as a result | 31 s | 5.0 to 5.5 s |
| peak memory | 2.0 GB | 2.1 GB |

That is musl's allocator under threads. It was not a reason for the
change and it is the largest effect of it.

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

## Still open

- **The server that runs `app.0sql.io`.** It installs the released
  Linux binary and needs glibc 2.34. `ldd --version` there has not
  been run; it gates phase 6.
- **A query through a driver under the service unit.** Loading works.

## Phases

- [x] **Phase 0 — a trial build, publishing nothing.** Done but for
  the server's glibc; see "What the trial found". On a branch,
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

- [x] **Phase 1 — arm64.** Built and run on GitHub on 2026-11-25:
  the runner is available to this repository, the build needs glibc
  2.34 as x86_64 does, and DuckDB links. On `ubuntu-24.04-arm` in the same
  container. The arm64 binary is run before a release carries it:
  today nothing runs the cross-built one. arm64 links with GNU ld
  where x86_64 does not (the note in
  `crates/datasource/src/adapters/duckdb.rs`); confirm DuckDB's
  extensions link.

- [x] **Phase 2 — the release workflow.**
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
  - A step proves a driver loads from the released binary: `zsql
    datasource drivers`, added for this, says which drivers a build
    finds and loads. The workflow installs both with `dbc` in an
    Amazon Linux 2023 container and reads its answer.
  - `Cross.toml` and the `cross` step go. `rust-toolchain.toml` names
    the glibc target.

- [x] **Phase 3 — CI covers the driver path.** `bin/adbc-drivers`
  fetches `dbc` and three drivers into `target/adbc`, and `bin/ci` and
  `.github/workflows/ci.yml` run the three driver tests that need no
  account, in place of `--skip adbc::tests`. Until then a green run says nothing about the drivers.

- [x] **Phase 4 — the installer.** `public/install.sh`, in the
  `0sql-marketing` repository. Written and tried in containers; not
  committed or published there.
  - It tries a download from beside where it will live before it
    replaces anything, and leaves the installed `zsql` alone when the
    new one will not run. Before, it replaced first.
  - When a download will not run on Linux it says why: musl, or the
    glibc version found, what is needed, and what works. This is asked
    only after a failure, so it needs no knowledge of which versions
    are static, and it is safe to publish before the release.
  - After installing it asks the new `zsql` for its drivers. Where
    one is missing it recommends them and says how to get `dbc`. A
    `zsql` without that command, 0.7 and older, is asked nothing.
  - Its header states the requirement.

  Anything that fetches `releases/latest/download/zsql-x86_64-linux`
  without the installer, a Dockerfile on Alpine for one, gets the new
  build with no check. The release notes are the only warning it has.

- [x] **Phase 5 — the docs and what the code says of itself.** Done
  here; the site and issue #41 wait for the release.
  - `docs/releasing.md`: "Why musl, and static" becomes why glibc, the
    floor, and what moves it; its other mentions of static with it.
  - `docs/operating.md`: what a server's machine or container needs,
    how to install the drivers, and where a server finds them.
  - `README.md`: "One static binary" goes; the requirement is stated
    under installing. Done.
  - In `0sql-marketing`, the site says "one static binary" in
    `src/components/OpenSource.astro` and in the docs under
    `src/content/docs/` (installation, quickstart, cli). **Not done:**
    true of 0.7.0, so they change with the release.
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
