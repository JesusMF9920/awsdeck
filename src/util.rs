//! Utilidades compartidas entre vistas, sin dependencias de UI ni del SDK.

/// Epoch en milisegundos -> `YYYY-MM-DD HH:MM:SSZ` (UTC), sin crate de fechas.
pub fn fmt_epoch_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Epoch en milisegundos -> `HH:MM:SS` (UTC). Compacto para listas de líneas de log,
/// donde la fecha gastaría ancho (casi siempre es "hoy").
pub fn fmt_clock_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{h:02}:{mi:02}:{s:02}")
}

/// Días desde 1970-01-01 -> (año, mes, día). Algoritmo de Howard Hinnant.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (año, mes, día) -> días desde 1970-01-01. Inverso de `civil_from_days` (Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parsea una duración relativa `<n><unidad>` a milisegundos. Unidades: `m` (minutos),
/// `h`, `d`, `w`. Ej.: `30m`, `6h`, `2d`, `1w`. `None` si no parsea o `n <= 0`.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let n: i64 = s[..split].trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    let unit_ms = match s[split..].trim() {
        "m" | "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return None,
    };
    Some(n * unit_ms)
}

/// Parsea una fecha/hora **UTC** a epoch millis. Acepta `YYYY-MM-DD` (medianoche) o
/// `YYYY-MM-DD` seguido de `T` o espacio + `HH:MM[:SS]`. `None` si no parsea o está
/// fuera de rango. Sin crate de fechas (usa `days_from_civil`).
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t.trim())),
        None => (s, None),
    };

    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    let (mut h, mut mi, mut se) = (0i64, 0i64, 0i64);
    if let Some(t) = time.filter(|t| !t.is_empty()) {
        let mut tp = t.split(':');
        h = tp.next()?.parse().ok()?;
        mi = tp.next()?.parse().ok()?;
        if let Some(sec) = tp.next() {
            se = sec.parse().ok()?;
        }
        if tp.next().is_some()
            || !(0..=23).contains(&h)
            || !(0..=59).contains(&mi)
            || !(0..=59).contains(&se)
        {
            return None;
        }
    }

    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + se) * 1000)
}

/// Match fuzzy por subsecuencia (case-insensitive). `None` si `needle` no es
/// subsecuencia de `haystack`. Score mayor = mejor: premia runs contiguos, matches
/// al inicio de un "segmento" (tras `-_/.: ` o al inicio) y posición temprana.
/// `needle` vacío puntúa 0 (matchea todo). Pensado para rankear nombres cortos
/// (log groups, colas), no para textos largos.
pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().collect();
    let mut needle_chars = needle.chars().map(|c| c.to_ascii_lowercase());
    let mut score: i32 = 0;
    let mut prev_match: Option<usize> = None;
    let mut next = needle_chars.next();

    for (i, &hc) in hay.iter().enumerate() {
        let Some(nc) = next else { break };
        if hc.to_ascii_lowercase() == nc {
            let boundary = i == 0 || matches!(hay[i - 1], '-' | '_' | '/' | '.' | ' ' | ':');
            if boundary {
                score += 15;
            }
            if prev_match == Some(i.wrapping_sub(1)) {
                score += 10; // run contiguo
            }
            score -= i as i32 / 4; // leve preferencia por matches tempranos
            prev_match = Some(i);
            next = needle_chars.next();
        }
    }

    // Si consumimos todo el needle, es subsecuencia.
    next.is_none().then_some(score)
}

/// Índices `[0, len)` filtrados y ordenados por `score`. Filtro vacío → todos en
/// orden original; con filtro → solo los `Some(score)`, ordenados por score desc
/// (sort estable: empates conservan el orden original). Helper compartido por las
/// vistas para rankear sus listas con `fuzzy_score`.
pub fn ranked(len: usize, filter: &str, score: impl Fn(usize) -> Option<i32>) -> Vec<usize> {
    if filter.is_empty() {
        return (0..len).collect();
    }
    let mut scored: Vec<(i32, usize)> = (0..len).filter_map(|i| score(i).map(|s| (s, i))).collect();
    scored.sort_by_key(|&(s, _)| std::cmp::Reverse(s)); // score desc, estable
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_millis_as_utc() {
        // 1700000000000 ms = 2023-11-14 22:13:20 UTC
        assert_eq!(fmt_epoch_millis(1_700_000_000_000), "2023-11-14 22:13:20Z");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30m"), Some(30 * 60_000));
        assert_eq!(parse_duration("6h"), Some(6 * 3_600_000));
        assert_eq!(parse_duration("2d"), Some(2 * 86_400_000));
        assert_eq!(parse_duration("1w"), Some(604_800_000));
        assert_eq!(parse_duration(" 12 h "), Some(12 * 3_600_000));
        assert_eq!(parse_duration("0h"), None, "n debe ser > 0");
        assert_eq!(parse_duration("h"), None, "falta el número");
        assert_eq!(parse_duration("12"), None, "falta la unidad");
        assert_eq!(parse_duration("3y"), None, "unidad no soportada");
    }

    #[test]
    fn parses_datetimes_utc() {
        // 2023-11-14 22:13:20 UTC == 1_700_000_000 s
        assert_eq!(
            parse_datetime("2023-11-14T22:13:20"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_datetime("2023-11-14 22:13:20"),
            Some(1_700_000_000_000)
        );
        // Solo fecha = medianoche UTC.
        assert_eq!(parse_datetime("2023-11-14"), Some(1_699_920_000_000));
        // Sin segundos.
        assert_eq!(parse_datetime("2023-11-14T22:13"), Some(1_699_999_980_000));
        assert!(parse_datetime("2023-13-01").is_none(), "mes inválido");
        assert!(
            parse_datetime("2023-11-14T25:00").is_none(),
            "hora inválida"
        );
        assert!(parse_datetime("not-a-date").is_none());
    }

    #[test]
    fn civil_days_roundtrip() {
        for &days in &[0i64, 1, -1, 19_876, 100_000, -3_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m as i64, d as i64), days);
        }
    }

    #[test]
    fn fuzzy_matches_subsequence_across_separators() {
        assert!(fuzzy_score("orders-api", "ordersapi").is_some());
        assert!(fuzzy_score("orders-api", "ordapi").is_some());
        assert!(fuzzy_score("/aws/lambda/Orders", "orders").is_some()); // case-insensitive
    }

    #[test]
    fn fuzzy_rejects_non_subsequence() {
        assert!(fuzzy_score("payments", "xyz").is_none());
        assert!(fuzzy_score("abc", "abcd").is_none());
    }

    #[test]
    fn fuzzy_empty_needle_matches_all() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn fuzzy_ranks_boundary_above_midword() {
        let boundary = fuzzy_score("orders-api", "ord").unwrap();
        let midword = fuzzy_score("reordered", "ord").unwrap();
        assert!(
            boundary > midword,
            "boundary {boundary} debe superar midword {midword}"
        );
    }
}
