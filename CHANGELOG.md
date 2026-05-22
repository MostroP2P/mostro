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


## What's Changed in 0.17.4

### 🚀 Features


* feat(bond): Phase 3.5 — payout confirmation to the winner by [@grunch](https://github.com/grunch) in [#743](https://github.com/MostroP2P/mostro/pull/743)
* feat(bond): Phase 3 — payout flow for slashed bonds by [@grunch](https://github.com/grunch) in [#738](https://github.com/MostroP2P/mostro/pull/738)
* feat(bond): Phase 2 — solver-directed dispute slash by [@grunch](https://github.com/grunch) in [#737](https://github.com/MostroP2P/mostro/pull/737)
* feat: support MOSTRO_NSEC_PRIVKEY env var for Nostr private key by [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) in [#713](https://github.com/MostroP2P/mostro/pull/713)
* feat(bond): Phase 1.5 — dedicated PayBondInvoice action + WaitingTakerBond status by [@grunch](https://github.com/grunch) in [#736](https://github.com/MostroP2P/mostro/pull/736)
* feat(bond): concurrent taker bonds, first-to-lock wins (Phase 0+1) by [@grunch](https://github.com/grunch) in [#733](https://github.com/MostroP2P/mostro/pull/733)
* feat(bond): align AntiAbuseBondSettings with spec — slash split, claim window, drop unused dispute flag by [@grunch](https://github.com/grunch) in [#728](https://github.com/MostroP2P/mostro/pull/728)
* feat(bond): add Forfeited terminal state for long-stop bond payout by [@grunch](https://github.com/grunch) in [#727](https://github.com/MostroP2P/mostro/pull/727)
* feat(bond): add Phase 0 schema columns for split, forfeit window, and retry separation by [@grunch](https://github.com/grunch) in [#726](https://github.com/MostroP2P/mostro/pull/726)
* feat: added catrya key for manifest signature by [@Catrya](https://github.com/Catrya) in [#724](https://github.com/MostroP2P/mostro/pull/724)
* feat(bond): anti-abuse bond phase 1 — taker lifecycle (lock + always release) by [@grunch](https://github.com/grunch) in [#719](https://github.com/MostroP2P/mostro/pull/719)
* feat(nip59): adopt mostro-core 0.10.0 dual-key gift wrap transport by [@grunch](https://github.com/grunch) in [#718](https://github.com/MostroP2P/mostro/pull/718)
* feat(bond): anti-abuse bond phase 0 foundation by [@grunch](https://github.com/grunch) in [#712](https://github.com/MostroP2P/mostro/pull/712)

### 🐛 Bug Fixes


* fix(price): tolerate null rates in Yadio /exrates/BTC response by [@grunch](https://github.com/grunch) in [#748](https://github.com/MostroP2P/mostro/pull/748)
* fix: include created_at on AddInvoice SmallOrder by [@arkanoider](https://github.com/arkanoider) in [#739](https://github.com/MostroP2P/mostro/pull/739)
* fix(bond): align bond invoice memo with spec §6.1 by [@grunch](https://github.com/grunch) in [#735](https://github.com/MostroP2P/mostro/pull/735)
* fix(restore-session): re-send AddInvoice for failed payments on session restore by [@codaMW](https://github.com/codaMW) in [#721](https://github.com/MostroP2P/mostro/pull/721)

### 💼 Other


* Revert "fix(restore-session): re-send AddInvoice for failed payments on session restore" by [@grunch](https://github.com/grunch) in [#722](https://github.com/MostroP2P/mostro/pull/722)
* Add read and read-write dispute solver permissions by [@mostronatorcoder[bot]](https://github.com/mostronatorcoder[bot]) in [#708](https://github.com/MostroP2P/mostro/pull/708)

### 📚 Documentation


* docs: spec for multi-source price providers (remove Yadio single point of failure) by [@grunch](https://github.com/grunch) in [#745](https://github.com/MostroP2P/mostro/pull/745)
* docs(bond): add Phase 3.5 — payout confirmation to the winner by [@grunch](https://github.com/grunch) in [#742](https://github.com/MostroP2P/mostro/pull/742)
* docs(bond): fold taker_* columns into Phase 0 schema by [@grunch](https://github.com/grunch) in [#734](https://github.com/MostroP2P/mostro/pull/734)
* docs(bond): switch Phase 1.5 to concurrent taker bonds, first-to-lock wins by [@grunch](https://github.com/grunch) in [#732](https://github.com/MostroP2P/mostro/pull/732)
* docs(bond): spec cancel_action handling for WaitingTakerBond status by [@grunch](https://github.com/grunch) in [#730](https://github.com/MostroP2P/mostro/pull/730)
* docs(bond): note that mostro-core 0.11.0 ships Phase 1.5 + Phase 2 variants by [@grunch](https://github.com/grunch) in [#729](https://github.com/MostroP2P/mostro/pull/729)
* docs(bond): decouple slash from trade outcome; clarify maker/taker vs… by [@grunch](https://github.com/grunch) in [#725](https://github.com/MostroP2P/mostro/pull/725)

## Contributors
* [@grunch](https://github.com/grunch) made their contribution in [#748](https://github.com/MostroP2P/mostro/pull/748)
* [@arkanoider](https://github.com/arkanoider) made their contribution in [#739](https://github.com/MostroP2P/mostro/pull/739)
* [@AndreaDiazCorreia](https://github.com/AndreaDiazCorreia) made their contribution in [#713](https://github.com/MostroP2P/mostro/pull/713)
* [@Catrya](https://github.com/Catrya) made their contribution in [#724](https://github.com/MostroP2P/mostro/pull/724)
* [@codaMW](https://github.com/codaMW) made their contribution in [#721](https://github.com/MostroP2P/mostro/pull/721)
* [@mostronatorcoder[bot]](https://github.com/mostronatorcoder[bot]) made their contribution in [#708](https://github.com/MostroP2P/mostro/pull/708)

**Full Changelog**: https://github.com/MostroP2P/mostro/compare/v0.17.3...0.17.4

<!-- generated by git-cliff -->
