use std::cmp::Ordering;

/// Orders names so that `page2` comes before `page10`: ASCII letters compare
/// without case, and runs of digits compare by their value.
pub(super) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (start_a, start_b) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (number_a, number_b) = (trim_zeros(&a[start_a..i]), trim_zeros(&b[start_b..j]));
            let order = number_a.len().cmp(&number_b.len()).then(number_a.cmp(number_b));
            if order.is_ne() {
                return order;
            }
            continue;
        }
        let order = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
        if order.is_ne() {
            return order;
        }
        i += 1;
        j += 1;
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    &digits[digits.iter().take_while(|&&d| d == b'0').count()..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_runs_compare_by_value() {
        assert_eq!(natural_cmp("p2", "p10"), Ordering::Less);
        assert_eq!(natural_cmp("p000", "p1"), Ordering::Less);
        assert_eq!(natural_cmp("p0", "p000"), Ordering::Equal);
        assert_eq!(natural_cmp("Ch2/P1", "ch10/p1"), Ordering::Less);
        assert_eq!(natural_cmp("p1", "p1a"), Ordering::Less);
    }
}
