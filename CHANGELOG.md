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


## What's Changed in 0.19.0

### 🚀 Features


* feat!: remove protocol v1 (gift wrap) by [@grunch](https://github.com/grunch) in [#1004](https://github.com/MostroP2P/mostro/pull/1004)
* feat(disputes): rename the created_at tag to published_at by [@grunch](https://github.com/grunch) in [#1001](https://github.com/MostroP2P/mostro/pull/1001)
* feat(orders): rename the created_at tag to published_at by [@grunch](https://github.com/grunch) in [#1000](https://github.com/MostroP2P/mostro/pull/1000)
* feat: let the maker cancel before paying the bond by [@grunch](https://github.com/grunch) in [#996](https://github.com/MostroP2P/mostro/pull/996)
* feat(orders): publish a created_at tag on kind 38383 by [@grunch](https://github.com/grunch) in [#971](https://github.com/MostroP2P/mostro/pull/971)
* feat(dispute): close a dispute as cooperatively-canceled on a cooperative cancel by [@grunch](https://github.com/grunch) in [#968](https://github.com/MostroP2P/mostro/pull/968)
* feat(dispute): close a dispute as released when the seller releases by [@grunch](https://github.com/grunch) in [#967](https://github.com/MostroP2P/mostro/pull/967)
* feat(restore): return the counterparty's trade pubkey by [@grunch](https://github.com/grunch) in [#966](https://github.com/MostroP2P/mostro/pull/966)

### 🐛 Bug Fixes


* fix: recognize trade keys when create/take is accepted by [@grunch](https://github.com/grunch) in [#1006](https://github.com/MostroP2P/mostro/pull/1006)
* fix: notify solver when dispute closes after user resolution by [@SIDHARTH20K4](https://github.com/SIDHARTH20K4) in [#945](https://github.com/MostroP2P/mostro/pull/945)
* fix(price): bound relayed price staleness at one TTL, not two by [@ToRyVand](https://github.com/ToRyVand) in [#925](https://github.com/MostroP2P/mostro/pull/925)
* fix: report retry interval in seconds, not minutes by [@21Mill](https://github.com/21Mill) in [#889](https://github.com/MostroP2P/mostro/pull/889)
* fix: keep amt and fa tags consistent during the taker-bond window by [@arkanoider](https://github.com/arkanoider) in [#986](https://github.com/MostroP2P/mostro/pull/986)
* fix: expire a taker bond invoice with its window by [@grunch](https://github.com/grunch) in [#999](https://github.com/MostroP2P/mostro/pull/999)
* fix: give the maker a deadline to pay the maker bond by [@grunch](https://github.com/grunch) in [#994](https://github.com/MostroP2P/mostro/pull/994)
* fix: resolve a publish on the first relay's OK by [@grunch](https://github.com/grunch) in [#992](https://github.com/MostroP2P/mostro/pull/992)

### 📚 Documentation


* docs: plan the removal of protocol v1 for v0.19.0 by [@grunch](https://github.com/grunch) in [#1003](https://github.com/MostroP2P/mostro/pull/1003)
* docs: run the ortsom gate on fork pull requests by [@grunch](https://github.com/grunch) in [#981](https://github.com/MostroP2P/mostro/pull/981)
* docs: publish the contribution quality bar by [@grunch](https://github.com/grunch) in [#977](https://github.com/MostroP2P/mostro/pull/977)
* docs: spec a quality bar for pull requests by [@grunch](https://github.com/grunch) in [#976](https://github.com/MostroP2P/mostro/pull/976)
* docs: spec the ortsom e2e gate for pull requests by [@grunch](https://github.com/grunch) in [#975](https://github.com/MostroP2P/mostro/pull/975)

### 🧪 Testing


* test: cover the CantDo(DisputeCreationError) arm by [@21Mill](https://github.com/21Mill) in [#916](https://github.com/MostroP2P/mostro/pull/916)
* test: pin the v2 arm of gate_applies_to_v2_only by [@21Mill](https://github.com/21Mill) in [#915](https://github.com/MostroP2P/mostro/pull/915)

### ⚙️ Miscellaneous Tasks


* ci(ortsom): bump the pinned Ortsom to v0.3.2 by [@grunch](https://github.com/grunch) in [#1005](https://github.com/MostroP2P/mostro/pull/1005)
* ci(ortsom): run the gate's stack with anti-abuse bonds by [@grunch](https://github.com/grunch) in [#998](https://github.com/MostroP2P/mostro/pull/998)
* ci(ortsom): map src/publish.rs to core plumbing by [@grunch](https://github.com/grunch) in [#995](https://github.com/MostroP2P/mostro/pull/995)
* ci(ortsom): run nothing for a pull request that changes the gate by [@grunch](https://github.com/grunch) in [#989](https://github.com/MostroP2P/mostro/pull/989)
* ci(ortsom): warn when the changed code was not compared by [@grunch](https://github.com/grunch) in [#988](https://github.com/MostroP2P/mostro/pull/988)
* ci(ortsom): report the verdict on the pull request by [@grunch](https://github.com/grunch) in [#985](https://github.com/MostroP2P/mostro/pull/985)
* ci(ortsom): run the gate on pull requests, forks too by [@grunch](https://github.com/grunch) in [#983](https://github.com/MostroP2P/mostro/pull/983)
* ci(ortsom): build the baseline image in mostro by [@grunch](https://github.com/grunch) in [#982](https://github.com/MostroP2P/mostro/pull/982)
* ci(ortsom): fetch the private harness with a key by [@grunch](https://github.com/grunch) in [#980](https://github.com/MostroP2P/mostro/pull/980)
* ci(quality): add the triage bot in label-only mode by [@grunch](https://github.com/grunch) in [#979](https://github.com/MostroP2P/mostro/pull/979)
* ci(ortsom): select scenarios and measure main (gate Phase 2) by [@grunch](https://github.com/grunch) in [#978](https://github.com/MostroP2P/mostro/pull/978)

## Contributors
* [@grunch](https://github.com/grunch) made their contribution in [#1006](https://github.com/MostroP2P/mostro/pull/1006)
* [@SIDHARTH20K4](https://github.com/SIDHARTH20K4) made their contribution in [#945](https://github.com/MostroP2P/mostro/pull/945)
* [@ToRyVand](https://github.com/ToRyVand) made their contribution in [#925](https://github.com/MostroP2P/mostro/pull/925)
* [@21Mill](https://github.com/21Mill) made their contribution in [#889](https://github.com/MostroP2P/mostro/pull/889)
* [@arkanoider](https://github.com/arkanoider) made their contribution in [#986](https://github.com/MostroP2P/mostro/pull/986)

**Full Changelog**: https://github.com/MostroP2P/mostro/compare/v0.18.8...0.19.0

<!-- generated by git-cliff -->
