# AGENTS.md

Operational notes for working in `sendspin-vst3`.

## Bundle Build

Build a distributable VST3 bundle:

```bash
cargo xtask bundle sendspin-vst3 --release
```

Bundle output:

```text
target/bundled/Sendspin VST3.vst3
```

## Release Flow (GitHub CLI)

Releases are cut from `main` only.

1. Ensure `main` is current:
   ```bash
   git checkout main
   git pull --ff-only origin main
   ```
2. Create and push an annotated tag:
   ```bash
   git tag -a <version> -m "Release <version>"
   git push origin <version>
   ```
3. Create the GitHub Release for that tag:
   ```bash
   gh release create <version> \
     --repo Sendspin/sendspin-vst \
     --title "<version>" \
     --notes-file <notes.md>
   ```

## Changelog Policy

Do not add or maintain a `CHANGELOG.md` in this repository.
Release notes live in GitHub Releases / PR descriptions.
