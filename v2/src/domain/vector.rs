//! Vector comparison. Vectors are stored at unit length, so cosine similarity
//! is a plain dot product; nearest neighbours are an exact brute-force scan
//! (a personal knowledge base is small enough that exact beats approximate).

/// Scale `v` to unit length. `None` when it is empty, all zeros, or holds a
/// non-finite value — such a vector has no direction to compare.
pub fn unit(mut v: Vec<f32>) -> Option<Vec<f32>> {
    if v.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let norm = dot(&v, &v).sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return None;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    Some(v)
}

/// Dot product; equals cosine similarity for unit vectors. Panics when the
/// lengths differ (mixing vector spaces is a bug, never a ranking).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "vector length mismatch");
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// The `k` candidates most similar to `query`, best first, as `(item, score)`.
/// Ties keep the earlier candidate first (the sort is stable), so results
/// are deterministic.
pub fn top_k<'v, T>(
    query: &[f32],
    candidates: impl IntoIterator<Item = (T, &'v [f32])>,
    k: usize,
) -> Vec<(T, f32)> {
    let mut scored: Vec<(T, f32)> = candidates
        .into_iter()
        .map(|(item, v)| (item, dot(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn unit_scales_to_length_one() {
        let v = unit(vec![3.0, 4.0]).unwrap();
        assert!(close(v[0], 0.6) && close(v[1], 0.8));
        assert!(close(dot(&v, &v), 1.0));
    }

    #[test]
    fn unit_rejects_directionless_vectors() {
        assert_eq!(unit(vec![]), None);
        assert_eq!(unit(vec![0.0, 0.0]), None);
        assert_eq!(unit(vec![1.0, f32::NAN]), None);
        assert_eq!(unit(vec![f32::INFINITY, 1.0]), None);
    }

    #[test]
    fn dot_is_cosine_for_unit_vectors() {
        let a = unit(vec![1.0, 0.0]).unwrap();
        let b = unit(vec![1.0, 1.0]).unwrap();
        assert!(close(dot(&a, &b), std::f32::consts::FRAC_1_SQRT_2));
        assert!(close(dot(&a, &[-1.0, 0.0]), -1.0));
    }

    #[test]
    #[should_panic]
    fn dot_panics_on_length_mismatch() {
        dot(&[1.0], &[1.0, 0.0]);
    }

    #[test]
    fn top_k_orders_best_first_and_truncates() {
        let q = [1.0, 0.0];
        let rows: Vec<(&str, Vec<f32>)> = vec![
            ("far", vec![0.0, 1.0]),
            ("near", vec![0.9, 0.1]),
            ("exact", vec![1.0, 0.0]),
            ("opposite", vec![-1.0, 0.0]),
        ];
        let got = top_k(&q, rows.iter().map(|(n, v)| (*n, v.as_slice())), 2);
        let names: Vec<&str> = got.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["exact", "near"]);
        assert!(close(got[0].1, 1.0));
        assert!(got[0].1 >= got[1].1);
    }

    #[test]
    fn top_k_handles_small_inputs() {
        let q = [1.0, 0.0];
        let one = [vec![1.0, 0.0]];
        assert!(top_k(&q, one.iter().map(|v| (0, v.as_slice())), 0).is_empty());
        assert_eq!(top_k(&q, one.iter().map(|v| (7, v.as_slice())), 5).len(), 1);
        let none: [(u8, &[f32]); 0] = [];
        assert!(top_k(&q, none, 3).is_empty());
    }

    #[test]
    fn a_huge_k_returns_everything_without_reserving_k_slots() {
        let q = [1.0, 0.0];
        let rows = [vec![1.0, 0.0], vec![0.0, 1.0]];
        for k in [usize::MAX, 1 << 46] {
            let got = top_k(
                &q,
                rows.iter().enumerate().map(|(i, v)| (i, v.as_slice())),
                k,
            );
            assert_eq!(got.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1]);
        }
    }

    #[test]
    fn top_k_ties_keep_input_order() {
        let q = [1.0, 0.0];
        let rows = [vec![0.5, 0.5], vec![0.5, 0.5], vec![0.5, 0.5]];
        let got = top_k(
            &q,
            rows.iter().enumerate().map(|(i, v)| (i, v.as_slice())),
            2,
        );
        assert_eq!(got.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1]);
    }
}
