//! Benchmark metrics. Human labels are ground truth; the same function evaluates every provider so
//! their numbers are directly comparable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfusionMatrix {
    pub tp: usize,
    pub fp: usize,
    pub tn: usize,
    pub fn_: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageMetrics {
    pub language: String,
    pub count: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    pub count: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub accuracy: f64,
    pub false_positive_rate: f64,
    pub false_negative_rate: f64,
    pub confusion_matrix: ConfusionMatrix,
    pub per_language: Vec<LanguageMetrics>,
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn precision(matrix: &ConfusionMatrix) -> f64 {
    ratio(matrix.tp, matrix.tp + matrix.fp)
}

fn recall(matrix: &ConfusionMatrix) -> f64 {
    ratio(matrix.tp, matrix.tp + matrix.fn_)
}

fn f1(precision: f64, recall: f64) -> f64 {
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// Evaluate predictions against labels. The three slices must have the same length.
pub fn evaluate(
    labels: &[bool],
    predictions: &[bool],
    languages: &[String],
) -> Result<BenchmarkMetrics, String> {
    if labels.len() != predictions.len() {
        return Err(format!(
            "labels ({}) and predictions ({}) must have the same length",
            labels.len(),
            predictions.len()
        ));
    }
    if !languages.is_empty() && languages.len() != labels.len() {
        return Err(format!(
            "languages ({}) must be empty or match the example count ({})",
            languages.len(),
            labels.len()
        ));
    }

    let mut matrix = ConfusionMatrix::default();
    let mut per_language: BTreeMap<String, ConfusionMatrix> = BTreeMap::new();
    for (index, (label, prediction)) in labels.iter().zip(predictions).enumerate() {
        let language = languages
            .get(index)
            .cloned()
            .unwrap_or_else(|| "und".to_string());
        let entry = per_language.entry(language).or_default();
        match (*label, *prediction) {
            (true, true) => {
                matrix.tp += 1;
                entry.tp += 1;
            }
            (false, true) => {
                matrix.fp += 1;
                entry.fp += 1;
            }
            (true, false) => {
                matrix.fn_ += 1;
                entry.fn_ += 1;
            }
            (false, false) => {
                matrix.tn += 1;
                entry.tn += 1;
            }
        }
    }

    let precision_value = precision(&matrix);
    let recall_value = recall(&matrix);
    let total = labels.len();

    Ok(BenchmarkMetrics {
        count: total,
        precision: precision_value,
        recall: recall_value,
        f1: f1(precision_value, recall_value),
        accuracy: ratio(matrix.tp + matrix.tn, total),
        false_positive_rate: ratio(matrix.fp, matrix.fp + matrix.tn),
        false_negative_rate: ratio(matrix.fn_, matrix.fn_ + matrix.tp),
        confusion_matrix: matrix,
        per_language: per_language
            .into_iter()
            .map(|(language, matrix)| {
                let precision = precision(&matrix);
                let recall = recall(&matrix);
                LanguageMetrics {
                    language,
                    count: matrix.tp + matrix.fp + matrix.tn + matrix.fn_,
                    precision,
                    recall,
                    f1: f1(precision, recall),
                }
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn languages(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn perfect_predictions() {
        let metrics = evaluate(
            &[true, false, true, false],
            &[true, false, true, false],
            &languages(&["en", "en", "es", "de"]),
        )
        .expect("metrics");
        assert_eq!(metrics.precision, 1.0);
        assert_eq!(metrics.recall, 1.0);
        assert_eq!(metrics.f1, 1.0);
        assert_eq!(metrics.accuracy, 1.0);
        assert_eq!(metrics.false_positive_rate, 0.0);
        assert_eq!(metrics.false_negative_rate, 0.0);
        assert_eq!(metrics.confusion_matrix.tp, 2);
        assert_eq!(metrics.confusion_matrix.tn, 2);
    }

    #[test]
    fn mixed_predictions_have_expected_rates() {
        // labels:  T T F F
        // preds:   T F T F
        let metrics = evaluate(
            &[true, true, false, false],
            &[true, false, true, false],
            &languages(&["en", "es", "en", "en"]),
        )
        .expect("metrics");
        assert_eq!(metrics.confusion_matrix.tp, 1);
        assert_eq!(metrics.confusion_matrix.fn_, 1);
        assert_eq!(metrics.confusion_matrix.fp, 1);
        assert_eq!(metrics.confusion_matrix.tn, 1);
        assert_eq!(metrics.precision, 0.5);
        assert_eq!(metrics.recall, 0.5);
        assert_eq!(metrics.f1, 0.5);
        assert_eq!(metrics.accuracy, 0.5);
        assert_eq!(metrics.false_positive_rate, 0.5);
        assert_eq!(metrics.false_negative_rate, 0.5);
        assert_eq!(metrics.per_language.len(), 2);
        let english = metrics
            .per_language
            .iter()
            .find(|entry| entry.language == "en")
            .expect("english");
        assert_eq!(english.count, 3);
        assert_eq!(english.precision, 0.5);
        assert_eq!(english.recall, 1.0);
    }

    #[test]
    fn empty_denominators_are_zero_not_nan() {
        let metrics =
            evaluate(&[false, false], &[false, false], &languages(&["en", "en"])).expect("metrics");
        assert_eq!(metrics.precision, 0.0);
        assert_eq!(metrics.recall, 0.0);
        assert_eq!(metrics.f1, 0.0);
        assert_eq!(metrics.accuracy, 1.0);
    }

    #[test]
    fn mismatched_lengths_are_an_error() {
        let error = evaluate(&[true], &[true, false], &languages(&["en", "en"]));
        assert!(error.is_err());
        let error = evaluate(&[true, false], &[true, false], &languages(&["en"]));
        assert!(error.is_err());
    }
}
