# Releasing

A tag `vX.Y.Z` or `vX.Y.Z-pre.N` on `main` publishes a release. Before tagging, add its notes as
`docs/releases/<tag>.md`; the release workflow refuses a tag without them. A tag with a `-` is published as a
prerelease.

## What the workflow builds

- **Version:** the binaries report the tag's version through `CONTR_VERSION` (`contr_core::product::VERSION`).
- **Windows:** the core payload zip, which the Windows client's installer carries.
- **Linux:** the archive, and the `contr-daemon` `.deb` and `.rpm`.
- **Guest sources:** the source of every package in the guest images.
- **Checksums:** `SHA256SUMS` over all of the above.

## Signing

Nothing below is required to publish. Until a setting exists, the matching step is skipped with a warning. Every
tagged release's files also get a GitHub build attestation, and contr-packages signs every release's `SHA256SUMS`;
[Verifying downloads](verifying.md) shows how to check both, and the [code signing policy](code-signing-policy.md)
says what is signed and by whom.

### Windows binaries (Authenticode)

Releases are unsigned today. The workflow can sign with Azure Artifact Signing once an account exists:
`contrd.exe` and `contr.exe` are signed with [Azure Artifact Signing](https://learn.microsoft.com/azure/artifact-signing/)
through `Azure/artifact-signing-action`. Publicly trusted code-signing keys must live in hardware, so a certificate
file in a secret is not an option. Set these at the organization level, so `contr-windows` signs its installer with
the same account:

| Kind | Name | Value |
|---|---|---|
| Secret | `AZURE_TENANT_ID` | Tenant of the app registration that may sign |
| Secret | `AZURE_CLIENT_ID` | Its application (client) ID |
| Secret | `AZURE_CLIENT_SECRET` | A client secret of that app registration |
| Variable | `ARTIFACT_SIGNING_ENDPOINT` | The signing account's regional endpoint, such as `https://eus.codesigning.azure.net/` |
| Variable | `ARTIFACT_SIGNING_ACCOUNT` | The signing account's name |
| Variable | `ARTIFACT_SIGNING_PROFILE` | The certificate profile's name |

The app registration needs the *Artifact Signing Certificate Profile Signer* role on the certificate profile. After
signing, the workflow fails unless `Get-AuthenticodeSignature` reports both binaries as validly signed.

Artifact Signing accepts organizations in the USA, Canada, the European Union and the United Kingdom, and individual
developers in the USA and Canada. Elsewhere, an OV code-signing certificate whose key a CA keeps in a cloud HSM (for
example DigiCert KeyLocker or SSL.com eSigner) replaces this step, with that CA's signing action.

### Linux repositories

[contr-packages](https://github.com/contr-hq/contr-packages) signs the APT and DNF repositories and the RPMs with one
GnuPG key; its README says how to create the key and which secrets hold it. It picks up new releases daily. To have it
pick them up at once, give this repository a secret `PACKAGES_DISPATCH_TOKEN`: a fine-grained token with Contents read
and write on `contr-packages`.
