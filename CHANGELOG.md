## Verifying the Release
In order to verify the release, you'll need to have gpg or gpg2 installed on your system. Once you've obtained a copy (and hopefully verified that as well), you'll first need to import the keys that have signed this release if you haven't done so already:
```bash
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/negrunch.asc | gpg --import
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/arkanoider.asc | gpg --import
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/catrya.asc | gpg --import
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/andreadiazcorreia.asc | gpg --import
```
Once you have the required PGP keys, you can verify the release (assuming manifest.txt.sig.negrunch, manifest.txt.sig.arkanoider, manifest.txt.sig.catrya, manifest.txt.sig.andreadiazcorreia and manifest.txt are in the current directory) with:
```bash
gpg --verify manifest.txt.sig.negrunch manifest.txt
gpg --verify manifest.txt.sig.arkanoider manifest.txt
gpg --verify manifest.txt.sig.catrya manifest.txt
gpg --verify manifest.txt.sig.andreadiazcorreia manifest.txt

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 1E41631D137BA2ADE55344F73852B843679AD6F0
gpg: Good signature from "Francisco Calderón <fjcalderon@gmail.com>" [ultimate]

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 2E986CA1C5E7EA1635CD059C4989CC7415A43AEC
gpg: Good signature from "Arkanoider <github.913zc@simplelogin.com>" [ultimate]

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 9A718444050F091D3D24CF6CE15E232F243D73E6
gpg: Good signature from "Catrya (github) <140891948+Catrya@users.noreply.github.com>" [ultimate]

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using EDDSA key 57376B6467F41F565ADDC65B1ED8B40E3A46E21D
gpg: Good signature from "Andrea Diaz Correia <andrea.diaz.correia@gmail.com>" [ultimate]

```
That will verify the signature of the manifest file, which ensures integrity and authenticity of the archive you've downloaded locally containing the binaries. Next, depending on your operating system, you should then re-compute the sha256 hash of the archive with `shasum -a 256 <filename>`, compare it with the corresponding one in the manifest file, and ensure they match exactly.


## What's Changed in 0.18.1

### 🚀 Features


* feat(nip33): advertise pow_first_contact in the info event by [@grunch](https://github.com/grunch) in [#847](https://github.com/MostroP2P/mostro/pull/847)
* feat: add Nostr trusted-node price provider (#697) by [@ToRyVand](https://github.com/ToRyVand) in [#841](https://github.com/MostroP2P/mostro/pull/841)
* feat(cashu): add_cashu_escrow_action lock handler — Track A TA-1 by [@grunch](https://github.com/grunch) in [#829](https://github.com/MostroP2P/mostro/pull/829)
* feat: cashu boot + run_cashu + dispatch seam — Cashu foundation CF-5 by [@grunch](https://github.com/grunch) in [#828](https://github.com/MostroP2P/mostro/pull/828)
* feat: cashu escrow DB helpers — Cashu foundation CF-4 by [@grunch](https://github.com/grunch) in [#797](https://github.com/MostroP2P/mostro/pull/797)
* feat: CashuClient mint library (cdk 0.17.2) — Cashu foundation CF-2 by [@grunch](https://github.com/grunch) in [#798](https://github.com/MostroP2P/mostro/pull/798)
* feat: cashu config + escrow mode — Cashu foundation CF-1 by [@grunch](https://github.com/grunch) in [#796](https://github.com/MostroP2P/mostro/pull/796)

### 🐛 Bug Fixes


* fix: resubscribe hold invoices correctly across a restart by [@grunch](https://github.com/grunch) in [#853](https://github.com/MostroP2P/mostro/pull/853)
* fix(deps): bump nostr to 0.44.6 for GHSA-hrqp-8w79-gwgw (NIP-44 DoS) by [@grunch](https://github.com/grunch) in [#846](https://github.com/MostroP2P/mostro/pull/846)
* fix(restore-session): scrub Nostr keys from log lines by [@ToRyVand](https://github.com/ToRyVand) in [#835](https://github.com/MostroP2P/mostro/pull/835)
* fix(lightning): don't panic the daemon on malformed preimage/hash (#804) by [@grunch](https://github.com/grunch) in [#821](https://github.com/MostroP2P/mostro/pull/821)

### 💼 Other


* Validate payout invoice network and bound final CLTV delta by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#861](https://github.com/MostroP2P/mostro/pull/861)
* Harden LNURL fetches against SSRF and hangs by [@arkanoider](https://github.com/arkanoider) in [#858](https://github.com/MostroP2P/mostro/pull/858)
* Harden nsec storage with SecretString and one-time key init by [@arkanoider](https://github.com/arkanoider) in [#772](https://github.com/MostroP2P/mostro/pull/772)
* Reject non-counterparty senders in cooperative cancel by [@arkanoider](https://github.com/arkanoider) in [#851](https://github.com/MostroP2P/mostro/pull/851)
* Add LUD-12 comment to dev fee LNURL-pay calls by [@ToRyVand](https://github.com/ToRyVand) in [#820](https://github.com/MostroP2P/mostro/pull/820)

### 🚜 Refactor


* refactor: EscrowBackend seam — Cashu foundation CF-0 by [@grunch](https://github.com/grunch) in [#795](https://github.com/MostroP2P/mostro/pull/795)

### 📚 Documentation


* docs: add security policy by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#850](https://github.com/MostroP2P/mostro/pull/850)
* docs: state the English-language convention explicitly by [@grunch](https://github.com/grunch) in [#822](https://github.com/MostroP2P/mostro/pull/822)

### 🧪 Testing


* test: raise line coverage 67%→93% + deep review findings by [@grunch](https://github.com/grunch) in [#803](https://github.com/MostroP2P/mostro/pull/803)

### ⚙️ Miscellaneous Tasks


* ci: cashu test-mint harness — Cashu foundation CF-3 by [@grunch](https://github.com/grunch) in [#799](https://github.com/MostroP2P/mostro/pull/799)
* ci: serve the coverage badge from our own Pages, drop Codecov by [@grunch](https://github.com/grunch) in [#824](https://github.com/MostroP2P/mostro/pull/824)
* ci: publish the llvm-cov HTML report to Pages and Codecov by [@grunch](https://github.com/grunch) in [#823](https://github.com/MostroP2P/mostro/pull/823)
* chore: add andreadiazcorreia to release verification instructions by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#802](https://github.com/MostroP2P/mostro/pull/802)

## Contributors
* [@grunch](https://github.com/grunch) made their contribution in [#853](https://github.com/MostroP2P/mostro/pull/853)
* [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) made their contribution in [#861](https://github.com/MostroP2P/mostro/pull/861)
* [@arkanoider](https://github.com/arkanoider) made their contribution in [#858](https://github.com/MostroP2P/mostro/pull/858)
* [@ToRyVand](https://github.com/ToRyVand) made their contribution in [#841](https://github.com/MostroP2P/mostro/pull/841)

**Full Changelog**: https://github.com/MostroP2P/mostro/compare/v0.18.0...0.18.1

<!-- generated by git-cliff -->
