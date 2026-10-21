# Fedora RPM support

The release RPM supports Fedora 43 and 44, x86_64. Build on the oldest
supported Fedora (43) and validate the exact resulting artifact on every
supported version. Building on a newer Fedora can introduce unavailable ELF
symbol requirements even when the source and explicit `Requires` do not
change; alpha.7's Fedora 44 build required `GLIBC_2.43` from libm and failed
to install on Fedora 43.

`.github/workflows/native-platforms.yml` separates the build from the runtime
matrix. The latter starts clean Fedora containers without build dependencies,
installs the RPM with weak dependencies disabled, checks dynamically loaded
display libraries before installing desktop test tools, renders PDF fixtures as an
unprivileged user, checks desktop metadata, renders an application frame on
X11 and Wayland, and verifies removal. Publication requires the whole matrix
to pass. Transaction logs, requirements, the installed package list, and
startup results are retained as CI artifacts. Headless tests cover the default
CPU renderer; they do not replace manual KDE Discover or hardware GPU checks.

To reproduce the runtime check locally with Docker, from the repository root:

```bash
mkdir -p target/rpm-test
docker run --rm \
  --mount "type=bind,source=$PWD,target=/work,readonly" \
  --mount "type=bind,source=$PWD/target/rpm-test,target=/results" \
  fedora:43 \
  bash /work/scripts/test-rpm.sh /work/target/rpm/rustmail.rpm /results
```

Repeat with `fedora:44`. The test script changes system packages and creates a
test user, so run it only in a disposable container.

When changing supported versions, update the runtime matrix, build container,
Fedora-specific Cargo cache key, README translations, and generated release
notes together. Add a new Fedora version to the matrix before advertising
support. Keep the oldest supported version as the build baseline until its
support is deliberately retired. If native dependencies diverge so that one
RPM cannot pass the matrix, publish separately labelled per-release builds.

Keep RPM's automatic ELF requirements enabled. Removing a requirement or
using `--nodeps` hides an incompatible binary instead of fixing it. The public
RPM filename remains stable, and checksums/attestations cover the tested bytes.
