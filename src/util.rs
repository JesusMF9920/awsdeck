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
