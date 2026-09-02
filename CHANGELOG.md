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


## What's Changed in 0.18.6

### 🚀 Features


* feat: boot node-identity guard for Lightning node changes (Phase 4) by [@grunch](https://github.com/grunch) in [#936](https://github.com/MostroP2P/mostro/pull/936)
* feat: advertise maintenance mode in the info event, republish on change (Phase 3) by [@grunch](https://github.com/grunch) in [#935](https://github.com/MostroP2P/mostro/pull/935)
* feat: admin RPC for maintenance mode — SetMaintenanceMode + GetMaintenanceStatus (Phase 2) by [@grunch](https://github.com/grunch) in [#934](https://github.com/MostroP2P/mostro/pull/934)
* feat: maintenance (drain) mode — persistent flag + escrow gate (Phase 1) by [@grunch](https://github.com/grunch) in [#933](https://github.com/MostroP2P/mostro/pull/933)
* feat(cashu): take flow escrow request + unblock creation — Track A TA-2 by [@grunch](https://github.com/grunch) in [#830](https://github.com/MostroP2P/mostro/pull/830)

### 🐛 Bug Fixes


* fix(ci): scope publishing to version tag pushes by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#909](https://github.com/MostroP2P/mostro/pull/909)
* fix: compare-and-swap the trade-pubkey rotation (#811) by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#903](https://github.com/MostroP2P/mostro/pull/903)
* fix: keep dev fee audit events (kind 8383) for one year by [@grunch](https://github.com/grunch) in [#924](https://github.com/MostroP2P/mostro/pull/924)
* fix: verify the event signature before the spam gate by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#892](https://github.com/MostroP2P/mostro/pull/892)
* fix(ci): make the release build toolchain verifiable and reproducible by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#905](https://github.com/MostroP2P/mostro/pull/905)
* fix: clear next-trade fields on sell child orders by [@21Mill](https://github.com/21Mill) in [#891](https://github.com/MostroP2P/mostro/pull/891)
* fix: stamp dispute events with a monotonic created_at by [@grunch](https://github.com/grunch) in [#899](https://github.com/MostroP2P/mostro/pull/899)
* fix: follow-ups to the payout dispatch queue (post-merge audit of #883) by [@Catrya](https://github.com/Catrya) in [#893](https://github.com/MostroP2P/mostro/pull/893)

### 🚜 Refactor


* refactor: removed dead code identified in issue by [@Arowolokehinde](https://github.com/Arowolokehinde) in [#848](https://github.com/MostroP2P/mostro/pull/848)

### 📚 Documentation


* docs: operator runbook for migrating to a different Lightning node (Phase 5) by [@grunch](https://github.com/grunch) in [#937](https://github.com/MostroP2P/mostro/pull/937)
* docs: maintenance mode / Lightning node migration spec by [@grunch](https://github.com/grunch) in [#932](https://github.com/MostroP2P/mostro/pull/932)
* docs: payment-account history / anti-triangulation implementation spec by [@grunch](https://github.com/grunch) in [#917](https://github.com/MostroP2P/mostro/pull/917)
* docs(cashu): specs for Tracks B/C/D (release, coop-cancel, dispute) by [@grunch](https://github.com/grunch) in [#833](https://github.com/MostroP2P/mostro/pull/833)

## Contributors
* [@grunch](https://github.com/grunch) made their contribution in [#937](https://github.com/MostroP2P/mostro/pull/937)
* [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) made their contribution in [#909](https://github.com/MostroP2P/mostro/pull/909)
* [@Arowolokehinde](https://github.com/Arowolokehinde) made their contribution in [#848](https://github.com/MostroP2P/mostro/pull/848)
* [@21Mill](https://github.com/21Mill) made their contribution in [#891](https://github.com/MostroP2P/mostro/pull/891)
* [@Catrya](https://github.com/Catrya) made their contribution in [#893](https://github.com/MostroP2P/mostro/pull/893)

**Full Changelog**: https://github.com/MostroP2P/mostro/compare/v0.18.5...0.18.6

<!-- generated by git-cliff -->
