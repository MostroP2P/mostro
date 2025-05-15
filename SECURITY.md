# Security Policy

## Table of Contents
- [Supported Versions](#supported-versions)
- [Reporting a Vulnerability](#reporting-a-vulnerability)
- [Responsible Disclosure Policy](#responsible-disclosure-policy)
- [Security Best Practices](#security-best-practices)
- [Dependencies and Third-Party Components](#dependencies-and-third-party-components)
- [Commitment to Timely Response](#commitment-to-timely-response)
- [Legal Notice and Disclaimer](#legal-notice-and-disclaimer)

This document outlines how security is managed in this project, including supported versions, how to report vulnerabilities, and best practices for contributors and users. Please review each section to understand your responsibilities and how to keep the project secure.

## Supported Versions

We are committed to maintaining the security of this project. Only the latest stable release is actively supported with security updates. Older versions may not receive patches or updates. Please ensure you are using the latest version to benefit from the most recent security fixes.


## Reporting a Vulnerability

If you discover a security vulnerability, please report it as soon as possible. We take security issues seriously and will respond promptly to all reports.

- **Contact:** [Telegram: Mostro_dev]([https://t.me/mostro_dev), or by email using the pgp key:
  (mQINBGOKeIwBEAC6vt3dVYg73Cs3OTqDp/UFQIdpax9wXNghiuZ2KHTulr+TYPnf
gNVmAnJp6C8Td2UMqKYKqEwWVYuDAGQ3k15vDY/MmozZGmA+BtRV+aAaeC4Iw+ka
mPNtldBdGbiG5JJud5KPLMKweoxWsqbqUzPYZhIu8OEX+SML9vKlwn0T8Mrs7B8T
GKtug14rdA1FPh4vOzWj2eHmiuKatv55WM+T4Yh9gH+x9pb/btkR+ZfXYMCRAIfF
aMmtspIJDVWLCS93X4pcdNFjfD+sbjJ5khU4/o94DftoBp//4t03ccxJ0Q8rBHaX)

at [security@mostrop2p.io](calderon@mostro.network).
- **Do not** disclose vulnerabilities publicly until they have been addressed.
- Provide as much detail as possible to help us reproduce and resolve the issue quickly.
- We appreciate responsible disclosure and will acknowledge your contribution.

## Responsible Disclosure Policy

- Please report security issues directly to the contact above before disclosing them publicly.
- We will investigate all legitimate reports and aim to respond within 7 business days.
- Once the issue is confirmed, we will work to provide a fix as soon as possible and coordinate disclosure with you.

## Security Best Practices

### For Users
- Always use the latest version of this project.
- Regularly check for updates and apply security patches promptly.
- Follow the principle of least privilege when deploying or running this software.
- Never expose sensitive configuration or credentials in public repositories.

### For Contributors
- Do not introduce dependencies with known vulnerabilities.
- Avoid using deprecated or insecure APIs.
- Review code for potential security issues before submitting pull requests.
- Ensure all secrets, keys, and credentials are kept out of the codebase.

## Dependencies and Third-Party Components

- We monitor dependencies for vulnerabilities and update them regularly, using tools such as [OWASP Dependency-Check](https://jeremylong.github.io/DependencyCheck/) and [GitHub Dependabot](https://docs.github.com/en/code-security/supply-chain-security/keeping-your-dependencies-updated-automatically) to automate this process.
- If you notice a vulnerability in a dependency, please report it as described above.

## Commitment to Timely Response

We are committed to investigating all security reports and providing timely fixes. Our goal is to keep users safe and informed throughout the process.

## Legal Notice and Disclaimer

Security is a shared responsibility. While we strive to address all reported issues promptly, use this project at your own risk. We disclaim any liability for damages resulting from the use or misuse of this software.

This security policy is subject to change. The latest version is always available in the SECURITY.md file in the repository’s default branch.

---

Thank you for helping us keep this project secure and reliable.
