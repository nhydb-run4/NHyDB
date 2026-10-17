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
2. Add a section to `CHANGELOG.md`.
3. Build and test locally (see [BUILDING.md](BUILDING.md)).
4. Commit and tag:

   ```bash
   git commit -am "Release X.Y.Z"
   git tag -a vX.Y.Z -m "X.Y.Z"
   git push origin main vX.Y.Z
   ```

5. GitHub Actions (`.github/workflows/release.yml`) builds the wheels and
   the source distribution and creates a GitHub release with all files.

## Publishing on PyPI (optional)

The release workflow can upload to PyPI via *trusted publishing* (no
password or token in the repository):

1. On pypi.org, add a trusted publisher for this GitHub repository,
   workflow `release.yml`, environment `pypi`.
2. In the GitHub repository settings, create the environment `pypi` and
   the repository variable `PUBLISH_PYPI` = `true`.

Without these settings the PyPI step is skipped.

## Continuous integration

`.github/workflows/ci.yml` runs the Rust tests and the Python tests
(against the built wheel) on every push to `main` and on pull requests.
