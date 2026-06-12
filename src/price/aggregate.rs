//! Pure aggregation core (spec §6). No I/O, no globals, no clock — every
//! function is a deterministic transform over in-memory inputs, which is
//! what makes the numeric heart of the feature exhaustively testable.

use std::collections::HashMap;

use super::provider::{ProviderId, ProviderQuotes, Quote};

/// One currency's aggregated result for a tick.
///
/// `sources` is the count of clean, candidate values that fed into
/// [`combine`] (the spec §6.4 "source_count" the store also stores).
/// `contributors` is the sorted, deduplicated list of providers whose
/// value actually **survived** [`combine`]'s outlier filter — what the
/// Nostr `source` tag really means. The two can differ: with three
/// providers and one outlier the count is 3 but contributors is 2.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateResult {
    pub value: f64,
    pub sources: u8,
    pub contributors: Vec<ProviderId>,
}

/// Combine a currency's candidate per-BTC prices into one figure (spec §6.2).
///
/// Inputs are first cleaned of non-finite and non-positive values (a
/// provider returning `NaN`, `inf`, `0`, or a negative is simply ignored).
/// Then, over the `n` remaining candidates:
///
/// - `n == 0` → `None` (no usable value),
/// - `n == 1` → that value,
/// - `n == 2` → their mean,
/// - `n >= 3` → the median, then the mean of the values within
///   `outlier_pct` percent of that median; if nothing falls within the band
///   (a widely-spread even-length input, where the median is a synthetic
///   midpoint that matches no value), fall back to the median itself.
///
/// The median anchors the truth; values too far from it are dropped before
/// the mean, so one corrupt/stale source cannot move the result while
/// genuine small spreads between honest sources are still averaged in.
pub fn combine(xs: &[f64], outlier_pct: f64) -> Option<f64> {
    let mut clean: Vec<f64> = xs
        .iter()
        .copied()
        .filter(|x| x.is_finite() && *x > 0.0)
        .collect();
    if clean.is_empty() {
        return None;
    }
    clean.sort_by(|a, b| a.partial_cmp(b).expect("finite values sort"));

    match clean.len() {
        1 => Some(clean[0]),
        2 => Some(mean(&clean)),
        _ => {
            let m = median_sorted(&clean);
            let tol = m * (outlier_pct / 100.0);
            let kept: Vec<f64> = clean
                .iter()
                .copied()
                .filter(|x| (x - m).abs() <= tol)
                .collect();
            // Odd `n`: the median is a member of `clean`, so it is always
            // kept. Even `n`: the median is the synthetic midpoint of the two
            // central values and may match nothing (widely-spread / bimodal
            // input) → `kept` is empty. Fall back to the median rather than
            // averaging an empty set, which would be NaN.
            if kept.is_empty() {
                Some(m)
            } else {
                Some(mean(&kept))
            }
        }
    }
}

/// Resolve fiat-cross quotes into per-BTC candidates (spec §6.3).
///
/// Each `(currency, base, value)` becomes `value × anchors[base]`, but only
/// when an anchor for `base` exists this tick (i.e. some direct quoter
/// reported `base`). A quote whose base has no anchor is dropped — the
/// currency then falls back to whatever direct candidates it has, or to
/// last-known-good.
///
/// Returns the resolved per-BTC candidates grouped by currency, ready to be
/// merged with that currency's direct candidates before [`combine`].
pub fn resolve_per_base(
    per_base: &[(String, String, f64)],
    anchors: &HashMap<String, f64>,
) -> HashMap<String, Vec<f64>> {
    let mut out: HashMap<String, Vec<f64>> = HashMap::new();
    for (currency, base, value) in per_base {
        if let Some(anchor) = anchors.get(base) {
            let candidate = value * anchor;
            if candidate.is_finite() && candidate > 0.0 {
                out.entry(currency.clone()).or_default().push(candidate);
            }
        }
    }
    out
}

/// Run steps 1–3 of the §5.3 pipeline over one tick's provider results.
///
/// `provider_results` holds the `(ProviderId, ProviderQuotes)` pairs of
/// every provider that succeeded this tick (failed providers contribute
/// nothing — they are simply absent). Currency codes are upper-cased so
/// providers that disagree on casing still combine (spec §6.6).
///
/// 1. Direct (`PerBtc`) quotes are grouped per currency, **paired with
///    their provider id**.
/// 2. Per-currency **anchors** are the [`combine`]d direct quotes; fiat-cross
///    (`PerBase`) quotes are resolved against those anchors and attributed
///    to the fiat-cross provider (the anchor's own contributors are an
///    intermediate, not a contributor to the resolved currency).
/// 3. Each currency's final value is the [`combine`] of its direct **and**
///    resolved candidates. `contributors` lists the providers whose value
///    survived the outlier filter ([`kept_contributors`]) — what the Nostr
///    `source` tag actually represents (spec §9 Phase 1).
pub fn aggregate_tick(
    provider_results: &[(ProviderId, ProviderQuotes)],
    outlier_pct: f64,
) -> HashMap<String, AggregateResult> {
    // Per-currency direct (PerBtc) quotes paired with their source id.
    let mut direct: HashMap<String, Vec<(ProviderId, f64)>> = HashMap::new();
    // PerBase quotes paired with their source id; resolved in step 2.
    let mut per_base: Vec<(ProviderId, String, String, f64)> = Vec::new();

    for (id, quotes) in provider_results {
        for (currency, quote) in quotes {
            let currency = currency.to_uppercase();
            match quote {
                Quote::PerBtc(v) => direct.entry(currency).or_default().push((*id, *v)),
                Quote::PerBase { base, value } => {
                    per_base.push((*id, currency, base.to_uppercase(), *value))
                }
            }
        }
    }

    // Step 2: anchors = aggregated direct quotes (values only), then
    // resolve cross quotes — attributing each resolved candidate to the
    // fiat-cross provider that emitted it. Anchor contributors are *not*
    // propagated into resolved-currency contributors: they are an
    // intermediate of the cross math, not an upstream of the cross
    // currency.
    let mut anchors: HashMap<String, f64> = HashMap::new();
    for (currency, pairs) in &direct {
        let values: Vec<f64> = pairs.iter().map(|(_, v)| *v).collect();
        if let Some(v) = combine(&values, outlier_pct) {
            anchors.insert(currency.clone(), v);
        }
    }
    let mut resolved: HashMap<String, Vec<(ProviderId, f64)>> = HashMap::new();
    for (id, currency, base, value) in &per_base {
        if let Some(anchor) = anchors.get(base) {
            let candidate = value * anchor;
            if candidate.is_finite() && candidate > 0.0 {
                resolved
                    .entry(currency.clone())
                    .or_default()
                    .push((*id, candidate));
            }
        }
    }

    // Step 3: combine direct + resolved candidates per currency, and
    // attach the actual surviving contributors.
    let mut out: HashMap<String, AggregateResult> = HashMap::new();
    let currencies: std::collections::HashSet<&String> =
        direct.keys().chain(resolved.keys()).collect();
    for currency in currencies {
        let mut pairs: Vec<(ProviderId, f64)> = Vec::new();
        if let Some(d) = direct.get(currency) {
            pairs.extend_from_slice(d);
        }
        if let Some(r) = resolved.get(currency) {
            pairs.extend_from_slice(r);
        }
        let candidates: Vec<f64> = pairs.iter().map(|(_, v)| *v).collect();
        if let Some(value) = combine(&candidates, outlier_pct) {
            let contributors = kept_contributors(&pairs, outlier_pct);
            let sources = candidates
                .iter()
                .filter(|x| x.is_finite() && **x > 0.0)
                .count()
                .min(u8::MAX as usize) as u8;
            out.insert(
                currency.clone(),
                AggregateResult {
                    value,
                    sources,
                    contributors,
                },
            );
        }
    }
    out
}

/// Return the provider ids whose value actually survives [`combine`]'s
/// "kept" predicate for one currency's candidate pairs — i.e. those
/// providers the Nostr `source` tag should advertise (spec §9 Phase 1
/// "contributing-source list"). The predicate mirrors [`combine`]:
///
/// - clean: drop non-finite and non-positive,
/// - `n <= 2`: every clean provider contributes,
/// - `n >= 3`: only providers whose value lies within
///   `outlier_pct` percent of the median contribute,
/// - `n >= 3` with no value inside the outlier band (the
///   even-length bimodal fallback in [`combine`]): every clean provider
///   contributes, since no single source is demonstrably the outlier and
///   `combine` falls back to the median itself.
///
/// Multiple paths from the same provider for the same currency (a direct
/// + a fiat-cross resolution, for example) are deduplicated.
fn kept_contributors(pairs: &[(ProviderId, f64)], outlier_pct: f64) -> Vec<ProviderId> {
    let clean: Vec<(ProviderId, f64)> = pairs
        .iter()
        .copied()
        .filter(|(_, x)| x.is_finite() && *x > 0.0)
        .collect();
    if clean.is_empty() {
        return Vec::new();
    }
    if clean.len() <= 2 {
        return dedup_sort(clean.into_iter().map(|(id, _)| id).collect());
    }
    let mut values: Vec<f64> = clean.iter().map(|(_, v)| *v).collect();
    values.sort_by(|a, b| a.partial_cmp(b).expect("finite values sort"));
    let m = median_sorted(&values);
    let tol = m * (outlier_pct / 100.0);
    let kept: Vec<ProviderId> = clean
        .iter()
        .copied()
        .filter(|(_, x)| (x - m).abs() <= tol)
        .map(|(id, _)| id)
        .collect();
    if kept.is_empty() {
        // Bimodal fallback: combine returns the median (a synthetic
        // midpoint matching no value). No single provider is the outlier
        // here, so every clean provider stays a contributor.
        return dedup_sort(clean.into_iter().map(|(id, _)| id).collect());
    }
    dedup_sort(kept)
}

fn dedup_sort(mut ids: Vec<ProviderId>) -> Vec<ProviderId> {
    ids.sort();
    ids.dedup();
    ids
}

fn mean(xs: &[f64]) -> f64 {
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Median of an already-sorted, non-empty slice.
fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PCT: f64 = 5.0;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn combine_zero_sources_is_none() {
        assert!(combine(&[], PCT).is_none());
    }

    #[test]
    fn combine_one_two_sources() {
        approx(combine(&[100.0], PCT).unwrap(), 100.0);
        approx(combine(&[100.0, 102.0], PCT).unwrap(), 101.0); // mean of 2
    }

    #[test]
    fn combine_three_discards_outlier() {
        // median 101; 200 is >5% off → dropped; mean(100,101) = 100.5
        approx(combine(&[100.0, 101.0, 200.0], PCT).unwrap(), 100.5);
    }

    #[test]
    fn combine_all_equal() {
        approx(combine(&[50.0, 50.0, 50.0, 50.0], PCT).unwrap(), 50.0);
    }

    #[test]
    fn combine_outlier_boundary_inclusive() {
        // median 100, tol 5 → keep [95,105]. 105 is exactly on the bound → kept.
        approx(combine(&[100.0, 100.0, 105.0], PCT).unwrap(), 305.0 / 3.0);
        // 106 is just past the bound → dropped, mean(100,100) = 100.
        approx(combine(&[100.0, 100.0, 106.0], PCT).unwrap(), 100.0);
    }

    #[test]
    fn combine_even_length_no_value_near_median_falls_back_to_median() {
        // Even n, bimodal: median = (2+100)/2 = 51; tol = 2.55 → nothing in
        // the band. Must fall back to the median (finite), never NaN.
        let out = combine(&[1.0, 2.0, 100.0, 101.0], PCT).unwrap();
        assert!(out.is_finite(), "must not be NaN");
        approx(out, 51.0);
    }

    #[test]
    fn combine_rejects_non_finite_and_non_positive() {
        approx(combine(&[f64::NAN, 100.0, 100.0], PCT).unwrap(), 100.0);
        approx(combine(&[0.0, -5.0, 100.0], PCT).unwrap(), 100.0);
        approx(combine(&[f64::INFINITY, 100.0], PCT).unwrap(), 100.0);
        assert!(combine(&[f64::NAN, 0.0, -1.0], PCT).is_none());
    }

    #[test]
    fn resolve_per_base_with_and_without_anchor() {
        let mut anchors = HashMap::new();
        anchors.insert("USD".to_string(), 50_000.0);

        // El Toque worked example (spec §6.3): CUP per USD 400 × 50_000.
        let pb = vec![("CUP".to_string(), "USD".to_string(), 400.0)];
        let resolved = resolve_per_base(&pb, &anchors);
        approx(resolved["CUP"][0], 20_000_000.0);

        // Missing anchor → dropped.
        let pb_missing = vec![("CUP".to_string(), "EUR".to_string(), 400.0)];
        assert!(resolve_per_base(&pb_missing, &anchors).is_empty());
    }

    #[test]
    fn aggregate_tick_unions_partial_coverage() {
        // Yadio: USD, EUR, CUP (direct). CoinGecko: USD, EUR (no CUP).
        // El Toque: CUP via USD anchor (fiat-cross).
        let mut yadio = ProviderQuotes::new();
        yadio.insert("USD".into(), Quote::PerBtc(50_000.0));
        yadio.insert("EUR".into(), Quote::PerBtc(45_000.0));
        yadio.insert("CUP".into(), Quote::PerBtc(20_000_000.0));

        let mut coingecko = ProviderQuotes::new();
        coingecko.insert("USD".into(), Quote::PerBtc(50_000.0));
        coingecko.insert("EUR".into(), Quote::PerBtc(45_000.0));

        let mut eltoque = ProviderQuotes::new();
        eltoque.insert(
            "CUP".into(),
            Quote::PerBase {
                base: "USD".into(),
                value: 400.0,
            },
        );

        let out = aggregate_tick(
            &[
                (ProviderId::Yadio, yadio),
                (ProviderId::CoinGecko, coingecko),
                (ProviderId::ElToque, eltoque),
            ],
            PCT,
        );

        approx(out["USD"].value, 50_000.0);
        assert_eq!(out["USD"].sources, 2);
        assert_eq!(
            out["USD"].contributors,
            vec![ProviderId::Yadio, ProviderId::CoinGecko],
            "USD: both direct quoters survived"
        );
        approx(out["EUR"].value, 45_000.0);
        assert_eq!(out["EUR"].sources, 2);
        approx(out["CUP"].value, 20_000_000.0);
        assert_eq!(out["CUP"].sources, 2);
        // CUP contributors: Yadio (direct) + El Toque (fiat-cross
        // resolved via USD anchor). The USD anchor's own contributors
        // (Yadio, CoinGecko) are NOT propagated — they're an intermediate.
        assert_eq!(
            out["CUP"].contributors,
            vec![ProviderId::Yadio, ProviderId::ElToque]
        );
    }

    #[test]
    fn aggregate_tick_failed_provider_contributes_nothing() {
        // Only one provider's results are present (the other "failed" so it
        // was never added). USD has a single source.
        let mut yadio = ProviderQuotes::new();
        yadio.insert("USD".into(), Quote::PerBtc(50_000.0));
        let out = aggregate_tick(&[(ProviderId::Yadio, yadio)], PCT);
        approx(out["USD"].value, 50_000.0);
        assert_eq!(out["USD"].sources, 1);
        assert_eq!(out["USD"].contributors, vec![ProviderId::Yadio]);
    }

    #[test]
    fn aggregate_tick_uppercases_currency_codes() {
        // currency-api ships lowercase; must combine with uppercase Yadio.
        let mut a = ProviderQuotes::new();
        a.insert("usd".into(), Quote::PerBtc(50_000.0));
        let mut b = ProviderQuotes::new();
        b.insert("USD".into(), Quote::PerBtc(50_200.0));
        let out = aggregate_tick(&[(ProviderId::CurrencyApi, a), (ProviderId::Yadio, b)], PCT);
        assert_eq!(out.len(), 1, "lowercase and uppercase must merge");
        approx(out["USD"].value, 50_100.0);
        assert_eq!(out["USD"].sources, 2);
        assert_eq!(
            out["USD"].contributors,
            vec![ProviderId::Yadio, ProviderId::CurrencyApi],
            "n=2: both contributors keep their seat"
        );
    }

    #[test]
    fn aggregate_tick_cross_only_currency_without_anchor_is_absent() {
        // A fiat-cross CUP whose USD anchor is unavailable yields nothing.
        let mut eltoque = ProviderQuotes::new();
        eltoque.insert(
            "CUP".into(),
            Quote::PerBase {
                base: "USD".into(),
                value: 400.0,
            },
        );
        let out = aggregate_tick(&[(ProviderId::ElToque, eltoque)], PCT);
        assert!(out.is_empty(), "no USD anchor → CUP cannot resolve");
    }

    #[test]
    fn aggregate_tick_outlier_drops_provider_from_contributors() {
        // The motivating case for the review: three providers, one is a
        // wild outlier. `combine` drops it from the value, so the Nostr
        // `source` tag must NOT advertise it. With median = 50_100 and
        // tol = 5% = 2505, the outlier 75_000 sits way outside the band.
        let mut yadio = ProviderQuotes::new();
        yadio.insert("USD".into(), Quote::PerBtc(50_000.0));
        let mut coingecko = ProviderQuotes::new();
        coingecko.insert("USD".into(), Quote::PerBtc(50_200.0));
        let mut blockchain = ProviderQuotes::new();
        blockchain.insert("USD".into(), Quote::PerBtc(75_000.0));

        let out = aggregate_tick(
            &[
                (ProviderId::Yadio, yadio),
                (ProviderId::CoinGecko, coingecko),
                (ProviderId::Blockchain, blockchain),
            ],
            PCT,
        );

        // Value: mean of the in-band pair (Yadio 50_000 + CoinGecko 50_200).
        approx(out["USD"].value, 50_100.0);
        // `sources` still counts all three clean candidates (pre-outlier).
        assert_eq!(out["USD"].sources, 3);
        // `contributors` is the in-band set — Blockchain dropped.
        assert_eq!(
            out["USD"].contributors,
            vec![ProviderId::Yadio, ProviderId::CoinGecko],
            "outlier provider must not be advertised in the source tag"
        );
    }

    #[test]
    fn aggregate_tick_bimodal_fallback_keeps_all_clean_contributors() {
        // Even-length bimodal: combine falls back to the synthetic median.
        // No single provider is demonstrably the outlier, so every clean
        // provider stays a contributor — matching the comment in
        // `kept_contributors`.
        let mk = |v: f64| {
            let mut q = ProviderQuotes::new();
            q.insert("USD".into(), Quote::PerBtc(v));
            q
        };
        let out = aggregate_tick(
            &[
                (ProviderId::Yadio, mk(1.0)),
                (ProviderId::CoinGecko, mk(2.0)),
                (ProviderId::CurrencyApi, mk(100.0)),
                (ProviderId::Blockchain, mk(101.0)),
            ],
            PCT,
        );
        approx(out["USD"].value, 51.0); // (2+100)/2
                                        // All four clean providers survive the bimodal fallback.
                                        // `kept_contributors` -> `dedup_sort` sorts by the derived `Ord`
                                        // on `ProviderId`, which follows enum-variant declaration order
                                        // (Yadio, CoinGecko, CurrencyApi, Blockchain, ElToque). Pin the
                                        // exact list so a future refactor that perturbs ordering — or
                                        // accidentally drops a contributor — is caught by this test
                                        // rather than slipping past a loose `.len()` check.
        assert_eq!(
            out["USD"].contributors,
            vec![
                ProviderId::Yadio,
                ProviderId::CoinGecko,
                ProviderId::CurrencyApi,
                ProviderId::Blockchain,
            ]
        );
    }

    #[test]
    fn aggregate_tick_non_finite_value_drops_provider_from_contributors() {
        // A provider returning `0` or `NaN` for the only currency it
        // reports must not be claimed as a contributor.
        let mut bad = ProviderQuotes::new();
        bad.insert("USD".into(), Quote::PerBtc(f64::NAN));
        let mut good = ProviderQuotes::new();
        good.insert("USD".into(), Quote::PerBtc(50_000.0));
        let out = aggregate_tick(
            &[(ProviderId::Yadio, bad), (ProviderId::CoinGecko, good)],
            PCT,
        );
        approx(out["USD"].value, 50_000.0);
        assert_eq!(
            out["USD"].contributors,
            vec![ProviderId::CoinGecko],
            "NaN must drop the provider from contributors, not silently survive"
        );
    }
}
