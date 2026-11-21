/// Solve a small weighted least-squares fit with partial pivoting.
pub(crate) fn weighted_fit<const N: usize>(
    samples: impl IntoIterator<Item = ([f64; N], f64, f64)>,
) -> Option<[f64; N]> {
    let mut matrix = [[0.; N]; N];
    let mut rhs = [0.; N];
    for (basis, value, weight) in samples {
        for (index, row) in matrix.iter_mut().enumerate() {
            for (entry, term) in row.iter_mut().zip(basis) {
                *entry += basis[index] * term * weight;
            }
            rhs[index] += basis[index] * value * weight;
        }
    }
    for column in 0..N {
        let pivot = (column..N)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))?;
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        let divisor = matrix[column][column];
        if divisor.abs() < 1e-8 {
            return None;
        }
        for value in &mut matrix[column][column..] {
            *value /= divisor;
        }
        rhs[column] /= divisor;
        let pivot_row = matrix[column];
        for (index, row) in matrix.iter_mut().enumerate() {
            if index != column {
                let factor = row[column];
                for (value, pivot) in row[column..].iter_mut().zip(&pivot_row[column..]) {
                    *value -= factor * pivot;
                }
                rhs[index] -= factor * rhs[column];
            }
        }
    }
    Some(rhs)
}
