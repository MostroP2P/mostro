//! Pure aggregation core (spec §6). No I/O, no globals, no clock — every
//! function is a deterministic transform over in-memory inputs, which is
//! what makes the numeric heart of the feature exhaustively testable.

use std::collections::HashMap;

use super::provider::{ProviderQuotes, Quote};

/// One currency's aggregated result for a tick: the price and how many
/// sources contributed it (before outlier removal).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AggregateResult {
    pub value: f64,
    pub sources: u8,
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
/// `provider_results` holds the `ProviderQuotes` of each provider that
/// succeeded this tick (failed providers contribute nothing — they are
/// simply absent). Currency codes are upper-cased so providers that
/// disagree on casing still combine (spec §6.6).
///
/// 1. Direct (`PerBtc`) quotes are grouped per currency.
/// 2. Per-currency **anchors** are the [`combine`]d direct quotes; fiat-cross
///    (`PerBase`) quotes are resolved against those anchors.
/// 3. Each currency's final value is the [`combine`] of its direct **and**
///    resolved candidates.
///
/// Returns the per-currency [`AggregateResult`] (value + contributing
/// source count). The caller stamps these into the store with the tick
/// timestamp.
pub fn aggregate_tick(
    provider_results: &[ProviderQuotes],
    outlier_pct: f64,
) -> HashMap<String, AggregateResult> {
    let mut direct: HashMap<String, Vec<f64>> = HashMap::new();
    let mut per_base: Vec<(String, String, f64)> = Vec::new();

    for quotes in provider_results {
        for (currency, quote) in quotes {
            let currency = currency.to_uppercase();
            match quote {
                Quote::PerBtc(v) => direct.entry(currency).or_default().push(*v),
                Quote::PerBase { base, value } => {
                    per_base.push((currency, base.to_uppercase(), *value))
                }
            }
        }
    }

    // Step 2: anchors = aggregated direct quotes, then resolve cross quotes.
    let mut anchors: HashMap<String, f64> = HashMap::new();
    for (currency, candidates) in &direct {
        if let Some(v) = combine(candidates, outlier_pct) {
            anchors.insert(currency.clone(), v);
        }
    }
    let resolved = resolve_per_base(&per_base, &anchors);

    // Step 3: combine direct + resolved candidates per currency.
    let mut out: HashMap<String, AggregateResult> = HashMap::new();
    let currencies: std::collections::HashSet<&String> =
        direct.keys().chain(resolved.keys()).collect();
    for currency in currencies {
        let mut candidates: Vec<f64> = Vec::new();
        if let Some(d) = direct.get(currency) {
            candidates.extend_from_slice(d);
        }
        if let Some(r) = resolved.get(currency) {
            candidates.extend_from_slice(r);
        }
        let sources = candidates
            .iter()
            .filter(|x| x.is_finite() && **x > 0.0)
            .count()
            .min(u8::MAX as usize) as u8;
        if let Some(value) = combine(&candidates, outlier_pct) {
            out.insert(currency.clone(), AggregateResult { value, sources });
        }
    }
    out
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

        let out = aggregate_tick(&[yadio, coingecko, eltoque], PCT);

        approx(out["USD"].value, 50_000.0);
        assert_eq!(out["USD"].sources, 2);
        approx(out["EUR"].value, 45_000.0);
        assert_eq!(out["EUR"].sources, 2);
        // CUP = combine(Yadio 20M [direct], El Toque 400×50_000 = 20M [resolved]).
        approx(out["CUP"].value, 20_000_000.0);
        assert_eq!(out["CUP"].sources, 2);
    }

    #[test]
    fn aggregate_tick_failed_provider_contributes_nothing() {
        // Only one provider's results are present (the other "failed" so it
        // was never added). USD has a single source.
        let mut yadio = ProviderQuotes::new();
        yadio.insert("USD".into(), Quote::PerBtc(50_000.0));
        let out = aggregate_tick(&[yadio], PCT);
        approx(out["USD"].value, 50_000.0);
        assert_eq!(out["USD"].sources, 1);
    }

    #[test]
    fn aggregate_tick_uppercases_currency_codes() {
        // currency-api ships lowercase; must combine with uppercase Yadio.
        let mut a = ProviderQuotes::new();
        a.insert("usd".into(), Quote::PerBtc(50_000.0));
        let mut b = ProviderQuotes::new();
        b.insert("USD".into(), Quote::PerBtc(50_200.0));
        let out = aggregate_tick(&[a, b], PCT);
        assert_eq!(out.len(), 1, "lowercase and uppercase must merge");
        approx(out["USD"].value, 50_100.0);
        assert_eq!(out["USD"].sources, 2);
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
        let out = aggregate_tick(&[eltoque], PCT);
        assert!(out.is_empty(), "no USD anchor → CUP cannot resolve");
    }
}
