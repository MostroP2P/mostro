# Security Policy

Mostro is a Lightning Network peer-to-peer exchange built on Nostr. The daemon coordinates hold invoices, escrow settlement, disputes and reputation, so defects in those paths can put real money and user privacy at risk. Security reports are treated as a priority.

## Supported Versions

Security fixes are developed against the `main` branch and released in the next published version. Only the latest release on [crates.io](https://crates.io/crates/mostro) and the current `main` branch receive security updates; older releases are not patched or backported.

Operators running a Mostro instance are expected to upgrade to the latest release to receive fixes.

## Reporting a Vulnerability

Report suspected vulnerabilities privately by email to:

**security@mostro.network**

Do not open a public GitHub issue, pull request, or discussion for a security report, and do not post details in the public Telegram groups. Premature public disclosure exposes every operator and their users; see [Coordinated Disclosure](#coordinated-disclosure) below for the timeline we ask you to follow.

Include as much of the following as you can:

- A description of the issue and the impact you believe it has.
- The affected version, tag, or commit hash.
- Step-by-step reproduction instructions, ideally against a regtest or testnet setup.
- Any proof-of-concept code, logs, or Nostr events that demonstrate the problem.
- Whether the issue has been disclosed or reported anywhere else.

Reports in English or Spanish are both fine.

## What to Expect

- **Acknowledgement:** within 72 hours of your report.
- **Initial assessment:** within 7 days, including our severity evaluation and whether we accept the report.
- **Status updates:** at least every 14 days while the issue remains open.
- **Resolution:** we aim to ship a fix within 90 days of triage; complex protocol-level issues may take longer, and we will tell you if that is the case.

If you do not receive an acknowledgement within 72 hours, resend your message — mail delivery failures happen.

## Coordinated Disclosure

We ask that you give us a reasonable window to develop and ship a fix before disclosing publicly. Our target is coordinated disclosure once a patched release is available, or 90 days after triage, whichever comes first.

When a fix is released we publish an advisory describing the issue, the affected versions, and the upgrade path. Reporters are credited by name or handle unless they ask to remain anonymous.

## Scope

In scope: the Mostro daemon in this repository, including order and escrow handling, Lightning hold invoice and settlement logic, Cashu escrow, dispute and admin flows, the RPC interface, Nostr event construction and validation, key handling, and the development fee system.

Out of scope for this policy:

- Vulnerabilities in third-party dependencies. Report those upstream, but tell us if Mostro is exploitable through them.
- Infrastructure operated by third parties, such as public Nostr relays or Lightning nodes you do not control.
- Client applications maintained in other repositories under the [MostroP2P organization](https://github.com/MostroP2P). Report those to the corresponding repository.
- Issues that require an already-compromised operator host, database, or private keys.
- Social engineering, physical attacks, and volumetric denial of service against public infrastructure.

Reports about the Mostro protocol specification itself can also be sent to the same address.

## Safe Harbor

We consider security research conducted in good faith and in accordance with this policy to be authorized, and we will not pursue legal action over it. In return, we ask that you:

- Test against regtest, testnet, or an instance you operate — never against production instances or other users' trades.
- Avoid accessing, modifying, or destroying data that is not yours.
- Avoid degrading the service for other users.
- Give us a reasonable time to respond before disclosing.
