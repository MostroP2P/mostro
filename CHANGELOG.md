## Verifying the Release
In order to verify the release, you'll need to have gpg or gpg2 installed on your system. Once you've obtained a copy (and hopefully verified that as well), you'll first need to import the keys that have signed this release if you haven't done so already:
```bash
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/negrunch.asc | gpg --import
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/arkanoider.asc | gpg --import
curl https://raw.githubusercontent.com/MostroP2P/mostro/main/keys/catrya.asc | gpg --import
```
Once you have the required PGP keys, you can verify the release (assuming manifest.txt.sig.negrunch, manifest.txt.sig.arkanoider, manifest.txt.sig.catrya and manifest.txt are in the current directory) with:
```bash
gpg --verify manifest.txt.sig.negrunch manifest.txt
gpg --verify manifest.txt.sig.arkanoider manifest.txt
gpg --verify manifest.txt.sig.catrya manifest.txt

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 1E41631D137BA2ADE55344F73852B843679AD6F0
gpg: Good signature from "Francisco Calderón <fjcalderon@gmail.com>" [ultimate]

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 2E986CA1C5E7EA1635CD059C4989CC7415A43AEC
gpg: Good signature from "Arkanoider <github.913zc@simplelogin.com>" [ultimate]

gpg: Signature made fri 10 oct 2025 11:28:03 -03
gpg:                using RSA key 9A718444050F091D3D24CF6CE15E232F243D73E6
gpg: Good signature from "Catrya (github) <140891948+Catrya@users.noreply.github.com>" [ultimate]

```
That will verify the signature of the manifest file, which ensures integrity and authenticity of the archive you've downloaded locally containing the binaries. Next, depending on your operating system, you should then re-compute the sha256 hash of the archive with `shasum -a 256 <filename>`, compare it with the corresponding one in the manifest file, and ensure they match exactly.


## What's Changed in 0.18.0

### 🚀 Features


* feat(price): Phase 4 — unify live quote path on cache + enforce staleness by [@grunch](https://github.com/grunch) in [#783](https://github.com/MostroP2P/mostro/pull/783)
* feat(transport): make inner protocol version follow active transport by [@grunch](https://github.com/grunch) in [#785](https://github.com/MostroP2P/mostro/pull/785)
* feat(price): Phase 3 — El Toque fiat-cross provider (CUP/MLC) by [@grunch](https://github.com/grunch) in [#778](https://github.com/MostroP2P/mostro/pull/778)
* feat(transport): log active transport at mostrod startup by [@grunch](https://github.com/grunch) in [#781](https://github.com/MostroP2P/mostro/pull/781)
* feat(transport): Phase 2 — anti-spam gates for protocol v2 by [@grunch](https://github.com/grunch) in [#780](https://github.com/MostroP2P/mostro/pull/780)
* feat(bond): notify slashed party on dispute slash (#768) by [@Catrya](https://github.com/Catrya) in [#779](https://github.com/MostroP2P/mostro/pull/779)
* feat(transport): Phase 1 — wire protocol v2 (NIP-44 direct) into mostrod by [@grunch](https://github.com/grunch) in [#776](https://github.com/MostroP2P/mostro/pull/776)

### 🐛 Bug Fixes


* fix: surface InvalidOrderId to clients as CantDo(NotFound) by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#752](https://github.com/MostroP2P/mostro/pull/752)
* fix(nip33): rename info tag protocol_versions -> protocol_version by [@grunch](https://github.com/grunch) in [#782](https://github.com/MostroP2P/mostro/pull/782)

### 📚 Documentation


* docs: harden cashu spec after review (fee crash-safety, reuse guard, action ownership) by [@grunch](https://github.com/grunch) in [#794](https://github.com/MostroP2P/mostro/pull/794)
* docs(cashu): phased implementation spec series for Cashu escrow by [@grunch](https://github.com/grunch) in [#788](https://github.com/MostroP2P/mostro/pull/788)

### 🧪 Testing


* test: add smoke tests for runtime UPDATE helpers by [@grunch](https://github.com/grunch) in [#793](https://github.com/MostroP2P/mostro/pull/793)

### ⚙️ Miscellaneous Tasks


* chore: standardize v0.19.0 deprecation notices for the v1 transport by [@grunch](https://github.com/grunch) in [#800](https://github.com/MostroP2P/mostro/pull/800)
* chore: upgrade sqlx to 0.9 and drop sqlx-crud by [@arkanoider](https://github.com/arkanoider) in [#791](https://github.com/MostroP2P/mostro/pull/791)
* chore: drop sqlx-crud; use mostro_core::db::Crud by [@arkanoider](https://github.com/arkanoider) in [#789](https://github.com/MostroP2P/mostro/pull/789)
* ci(mutation): run as scheduled audit + opt-in instead of on every push to main by [@grunch](https://github.com/grunch) in [#787](https://github.com/MostroP2P/mostro/pull/787)

## Contributors
* [@grunch](https://github.com/grunch) made their contribution in [#800](https://github.com/MostroP2P/mostro/pull/800)
* [@arkanoider](https://github.com/arkanoider) made their contribution in [#791](https://github.com/MostroP2P/mostro/pull/791)
* [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) made their contribution in [#752](https://github.com/MostroP2P/mostro/pull/752)
* [@Catrya](https://github.com/Catrya) made their contribution in [#779](https://github.com/MostroP2P/mostro/pull/779)

**Full Changelog**: https://github.com/MostroP2P/mostro/compare/v0.17.5...0.18.0

<!-- generated by git-cliff -->
