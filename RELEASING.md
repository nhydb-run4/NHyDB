# Versioning and releases

## Version numbers

The project follows [Semantic Versioning](https://semver.org/):
`MAJOR.MINOR.PATCH`.

* bug fixes → PATCH (`0.27.0` → `0.27.1`)
* new features → MINOR (`0.27.0` → `0.28.0`)
* while MAJOR is `0`, the API may still change between MINOR versions

There is **one** product version. It is set in exactly one place – the
`version` in `[workspace.package]` of `Cargo.toml` – and used
automatically by the Rust crates, the Python package (wheel name and
metadata) and `nhydb.ENGINE_VERSION`.

The file identifier `NHYDB016` written into every database file is **not**
a product version. It identifies the on-disk format and only changes when
the format changes (`crates/nhydb-core/src/format.rs`).

## Release procedure

1. Update `version` in `Cargo.toml` and run `cargo update -w` so that
   `Cargo.lock` follows.
2. Rename `## [Unreleased]` in `CHANGELOG.md` to `## [X.Y.Z] – <date>`
   (the release notes are taken from this section) and add a new empty
   `## [Unreleased]` above it.
   If dependencies changed (`Cargo.lock`), regenerate the third-party
   notices: `cargo fetch && python tools/gen_third_party_licenses.py`, and
   commit `THIRD-PARTY-LICENSES.txt`.
3. Build and test locally (see [BUILDING.md](BUILDING.md)).
4. Optional dry run: start the workflow *Release* manually
   (Actions → Release → Run workflow). It builds and tests all wheels and
   the source distribution and publishes nothing.
5. Commit and tag:

   ```bash
   git commit -am "Release X.Y.Z"
   git tag -a vX.Y.Z -m "X.Y.Z"
   git push origin main vX.Y.Z
   ```

6. The tag starts `.github/workflows/release.yml`:
   * checks that tag, `Cargo.toml` version and `CHANGELOG.md` section match,
   * builds wheels for CPython 3.9–3.14 on Linux (x86_64, aarch64;
     manylinux_2_28), macOS (x86_64, arm64) and Windows (x64) plus the
     source distribution,
   * checks the complete file set (`tools/check_dist.py`) and the package
     metadata (`twine check`),
   * installs the wheels and runs the Python tests on every platform with
     Python 3.9 and 3.14,
   * creates a **draft** GitHub release with all files. If a release for
     the tag already exists, the job fails and changes nothing.
7. Check the draft release on GitHub and publish it.

## Publishing on PyPI

Publishing the GitHub release (step 7) starts the PyPI job of the same
workflow. It downloads exactly the files attached to the release, checks
them again and uploads them via *trusted publishing* (OpenID Connect; no
password or token is stored anywhere). The job only runs if

* the repository variable `PUBLISH_PYPI` is `true`, and
* the deployment to the protected environment `pypi` is approved.

One-time setup:

1. On pypi.org (account settings → Publishing), add a *pending publisher*
   for the project `nhydb`: owner `nhydb`, repository `NHyDB`, workflow
   `release.yml`, environment `pypi`.
2. In the GitHub repository settings → Environments, create `pypi` with a
   required reviewer and restrict deployments to tags `v*`.
3. Create the repository variable `PUBLISH_PYPI` = `true` only when PyPI
   releases are wanted.

A PyPI version can never be uploaded twice; a faulty upload needs a new
version number.

## Continuous integration

`.github/workflows/ci.yml` runs the Rust tests and the Python tests
(against the built wheel) on every push to `main` and on pull requests.
