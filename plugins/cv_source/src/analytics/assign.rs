//! Minimum-cost assignment (the Hungarian algorithm), O(n²m).

/// For each row of `cost`, the column it's assigned, or `None`. Costs that
/// aren't finite mean "can't be assigned". All rows have the same length.
pub fn assign(cost: &[Vec<f64>]) -> Vec<Option<usize>> {
    let rows = cost.len();
    let cols = cost.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return vec![None; rows];
    }
    // Forbidden pairs get a cost above any real total, and are dropped after.
    let finite_max = cost
        .iter()
        .flatten()
        .filter(|c| c.is_finite())
        .fold(0.0f64, |m, c| m.max(c.abs()));
    let forbidden = (finite_max + 1.0) * (rows.max(cols) as f64 + 1.0);
    let at = |r: usize, c: usize| {
        let v = cost[r][c];
        if v.is_finite() { v } else { forbidden }
    };
    // The algorithm wants rows ≤ columns.
    let swapped = rows > cols;
    let (n, m) = if swapped { (cols, rows) } else { (rows, cols) };
    let value = |i: usize, j: usize| if swapped { at(j, i) } else { at(i, j) };

    let (mut u, mut v) = (vec![0.0; n + 1], vec![0.0; m + 1]);
    // Column j (1-based) → its row (1-based; 0 = free).
    let mut matched = vec![0usize; m + 1];
    let mut way = vec![0usize; m + 1];
    for i in 1..=n {
        matched[0] = i;
        let mut j0 = 0;
        let mut min_v = vec![f64::INFINITY; m + 1];
        let mut used = vec![false; m + 1];
        loop {
            used[j0] = true;
            let i0 = matched[j0];
            let mut delta = f64::INFINITY;
            let mut j1 = 0;
            for j in 1..=m {
                if !used[j] {
                    let reduced = value(i0 - 1, j - 1) - u[i0] - v[j];
                    if reduced < min_v[j] {
                        min_v[j] = reduced;
                        way[j] = j0;
                    }
                    if min_v[j] < delta {
                        delta = min_v[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=m {
                if used[j] {
                    u[matched[j]] += delta;
                    v[j] -= delta;
                } else {
                    min_v[j] -= delta;
                }
            }
            j0 = j1;
            if matched[j0] == 0 {
                break;
            }
        }
        while j0 != 0 {
            let j1 = way[j0];
            matched[j0] = matched[j1];
            j0 = j1;
        }
    }
    let mut out = vec![None; rows];
    for (j, &row) in matched.iter().enumerate().skip(1) {
        if row == 0 {
            continue;
        }
        let (r, c) = if swapped {
            (j - 1, row - 1)
        } else {
            (row - 1, j - 1)
        };
        if cost[r][c].is_finite() {
            out[r] = Some(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO: f64 = f64::INFINITY;

    #[test]
    fn the_cheapest_total_wins_over_the_greedy_pick() {
        // Greedy takes (0, 0) at 1 and leaves row 1 with 10.
        let cost = vec![vec![1.0, 2.0], vec![1.5, 10.0]];
        assert_eq!(assign(&cost), [Some(1), Some(0)]);
    }

    #[test]
    fn more_rows_or_more_columns() {
        assert_eq!(assign(&[vec![5.0, 1.0, 3.0]]), [Some(1)]);
        assert_eq!(
            assign(&[vec![5.0], vec![1.0], vec![3.0]]),
            [None, Some(0), None]
        );
    }

    #[test]
    fn forbidden_pairs_stay_unassigned() {
        let cost = vec![vec![NO, NO], vec![NO, 0.5]];
        assert_eq!(assign(&cost), [None, Some(1)]);
        assert_eq!(assign(&[vec![NO]]), [None]);
    }

    #[test]
    fn a_forbidden_pair_never_beats_a_real_match() {
        let cost = vec![vec![0.9, NO], vec![0.1, NO]];
        let got = assign(&cost);
        assert_eq!(got.iter().flatten().count(), 1);
        assert_eq!(got[1], Some(0));
    }

    #[test]
    fn nothing_to_assign() {
        assert!(assign(&[]).is_empty());
        assert_eq!(assign(&[vec![], vec![]]), [None, None]);
    }
}
