# Security

Tokenrein sits in the path of your AI agents' calls and holds provider keys, so we treat security
reports as the top priority.

## Reporting a vulnerability

Please do **not** open a public issue. Report it privately through GitHub's
[private vulnerability reporting](https://github.com/tokenrein/tokenrein/security/advisories/new),
or email hello@tokenrein.com with "Security" in the subject.

Include what you found, how to reproduce it, the version (image tag or commit) and the impact you
expect. We aim to reply within
two working days and to ship a fix for confirmed issues within 30 days, crediting you in the release
notes unless you prefer otherwise.

## Supported versions

Fixes go into the latest release. Upgrade to it before reporting if you can.

## What is in scope

The server (`proxy/`), its container image and Helm chart, the SDKs (`sdk/`) and the connector
scripts. How data is handled, what is encrypted and how access works are described in
[docs/enterprise.md](docs/enterprise.md#data-handling-and-security).
