# Verifying downloads

## Where a file was built

Each file of a tagged release carries a GitHub build attestation that names the repository, workflow and commit that
built it. With the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify contr-core-windows-x64-v0.2.0.zip --repo contr-hq/contr
```

Use `--repo contr-hq/contr-linux` or `--repo contr-hq/contr-windows` for the desktop apps' files.

## That a file is unchanged

[contr-packages](https://github.com/contr-hq/contr-packages) publishes every release's `SHA256SUMS` with a signature by
the project's key, fingerprint `B9475E35EE33A93291272EFB2EE1E61ADBE87687`:

```sh
curl -fsSO https://contr-hq.github.io/contr-packages/checksums/contr/v0.2.0/SHA256SUMS
curl -fsSO https://contr-hq.github.io/contr-packages/checksums/contr/v0.2.0/SHA256SUMS.asc
curl -fsS https://contr-hq.github.io/contr-packages/KEY.asc | gpg --import
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

On Windows, compare `Get-FileHash <file>` with the line for that file in the verified `SHA256SUMS`.

## Windows binaries without a signature

Until Windows releases are Authenticode-signed (see the [code signing policy](code-signing-policy.md)):

- **SmartScreen** shows "Windows protected your PC" for the installer. After verifying the file as above, choose
  **More info**, then **Run anyway**.
- **Smart App Control**, on Windows 11 computers where it is on, may block unsigned programs outright; such computers
  cannot run Contr until it is signed.
- **Organizations** that allow only approved programs can allow Contr's files by hash, using the verified
  `SHA256SUMS`, in App Control for Business or AppLocker, or re-sign them with their own code-signing certificate
  before deployment.
