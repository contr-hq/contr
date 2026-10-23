# Code signing policy

Contr's Linux packages and its release checksums are signed with the project's GnuPG key, fingerprint
`B9475E35EE33A93291272EFB2EE1E61ADBE87687`; [contr-packages](https://github.com/contr-hq/contr-packages) publishes the
key and the signatures. Every release file also carries a GitHub build attestation naming the workflow and commit that
built it. [Verifying downloads](verifying.md) shows how to check both.

Windows releases are not Authenticode-signed. Their attestations and signed checksums show that a file is the one this
organization's release workflow built; [Verifying downloads](verifying.md) also covers what Windows shows for unsigned
programs. If Windows signing is added, it signs only what the next section lists, and this policy names the signer.

## What gets signed

Only files built from this organization's repositories by their release workflows on GitHub Actions: `contrd.exe` and
`contr.exe` from [contr](https://github.com/contr-hq/contr), and the desktop app and its installer from
[contr-windows](https://github.com/contr-hq/contr-windows). Third-party programs Contr bundles unchanged, such as the
Docker CLI, Docker Compose, Docker Buildx and kubectl, keep their publishers' signatures or none; Contr never signs them.

## Team

| Role | Members |
|---|---|
| Committers and reviewers | [contr-hq members](https://github.com/orgs/contr-hq/people): [@SekiroKenjii](https://github.com/SekiroKenjii) |
| Approvers | [@SekiroKenjii](https://github.com/SekiroKenjii) |

Changes from anyone else reach `main` only through a pull request a committer reviews. Every signing request is
approved by an approver. Members use multi-factor authentication for GitHub and for signing.

## Privacy policy

Contr collects no telemetry and sends no data about its users anywhere. It reaches other networked systems only in
these cases:

- **Update check:** once a day, and when you ask, the daemon reads the list of Contr releases from GitHub's API
  (`api.github.com`). The request carries the daemon's version in its `User-Agent` and comes from your network address.
  Turn it off with the `updates.check` setting; administrators can turn it off for every user.
- **Images:** pulling, pushing or running an image contacts the registry the image names, with the credentials
  `docker login` stored for that registry.
- **Kubernetes:** enabling the cluster pulls the k3s image from Docker Hub.
- **Fixes you approve:** installing WSL runs `wsl --install`, which downloads from Microsoft; installing the engine on
  Linux uses your distribution's package manager and its mirrors.

Diagnostics bundles are written to a file you choose and never leave your computer unless you send them. The
privacy policies of those services apply to what they receive: [GitHub](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement),
[Docker](https://www.docker.com/legal/docker-privacy-policy/), [Microsoft](https://privacy.microsoft.com/privacystatement)
and your distribution or registry.
