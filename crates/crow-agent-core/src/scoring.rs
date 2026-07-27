use crow_agent_protocol::{PenaltyRulesV1, ScoringWeightsV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PERCENTILE_SCALE: i64 = 1_000_000;
const MAX_SCORE_MILLIS: i64 = 100_000;
const RETURN_SCALE: i128 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub net_return_micros: i64,
    pub sortino_micros: i64,
    pub max_drawdown_micros: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunScoreInput {
    pub run_id: Uuid,
    pub net_return_micros: i64,
    pub sortino_micros: i64,
    pub max_drawdown_micros: i64,
    pub policy_rejections: u32,
    pub missed_cycles: u32,
    pub disqualified_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoredRun {
    pub run_id: Uuid,
    pub rank: Option<u32>,
    pub score_millis: i64,
    pub net_return_percentile_micros: i64,
    pub sortino_percentile_micros: i64,
    pub inverse_drawdown_percentile_micros: i64,
    pub penalty_millis: i64,
    pub disqualified_reason: Option<String>,
}

#[must_use]
pub fn score_runs(
    runs: &[RunScoreInput],
    weights: &ScoringWeightsV1,
    penalties: &PenaltyRulesV1,
) -> Vec<ScoredRun> {
    let eligible = runs
        .iter()
        .filter(|run| run.disqualified_reason.is_none())
        .collect::<Vec<_>>();
    let mut scored = runs
        .iter()
        .map(|run| {
            if let Some(reason) = &run.disqualified_reason {
                return ScoredRun {
                    run_id: run.run_id,
                    rank: None,
                    score_millis: 0,
                    net_return_percentile_micros: 0,
                    sortino_percentile_micros: 0,
                    inverse_drawdown_percentile_micros: 0,
                    penalty_millis: 0,
                    disqualified_reason: Some(reason.clone()),
                };
            }
            let net = percentile(
                &eligible
                    .iter()
                    .map(|candidate| candidate.net_return_micros)
                    .collect::<Vec<_>>(),
                run.net_return_micros,
            );
            let sortino = percentile(
                &eligible
                    .iter()
                    .map(|candidate| candidate.sortino_micros)
                    .collect::<Vec<_>>(),
                run.sortino_micros,
            );
            let inverse_drawdown = percentile(
                &eligible
                    .iter()
                    .map(|candidate| -candidate.max_drawdown_micros)
                    .collect::<Vec<_>>(),
                -run.max_drawdown_micros,
            );
            let weighted = (i64::from(weights.net_return) * net
                + i64::from(weights.sortino) * sortino
                + i64::from(weights.inverse_drawdown) * inverse_drawdown)
                / 1_000;
            let penalty = (i64::from(run.policy_rejections)
                * i64::from(penalties.policy_rejection_millis)
                + i64::from(run.missed_cycles) * i64::from(penalties.missed_cycle_millis))
            .min(i64::from(penalties.cap_millis));
            ScoredRun {
                run_id: run.run_id,
                rank: None,
                score_millis: (weighted - penalty).clamp(0, MAX_SCORE_MILLIS),
                net_return_percentile_micros: net,
                sortino_percentile_micros: sortino,
                inverse_drawdown_percentile_micros: inverse_drawdown,
                penalty_millis: penalty,
                disqualified_reason: None,
            }
        })
        .collect::<Vec<_>>();
    let inputs = runs
        .iter()
        .map(|run| (run.run_id, run))
        .collect::<std::collections::BTreeMap<_, _>>();
    scored.sort_by(|left, right| {
        match (
            left.disqualified_reason.is_some(),
            right.disqualified_reason.is_some(),
        ) {
            (false, true) => return std::cmp::Ordering::Less,
            (true, false) => return std::cmp::Ordering::Greater,
            (true, true) => return left.run_id.to_string().cmp(&right.run_id.to_string()),
            (false, false) => {}
        }
        let left_input = inputs[&left.run_id];
        let right_input = inputs[&right.run_id];
        right
            .score_millis
            .cmp(&left.score_millis)
            .then_with(|| {
                left_input
                    .max_drawdown_micros
                    .cmp(&right_input.max_drawdown_micros)
            })
            .then_with(|| {
                right_input
                    .net_return_micros
                    .cmp(&left_input.net_return_micros)
            })
            .then_with(|| left.run_id.to_string().cmp(&right.run_id.to_string()))
    });
    let mut rank = 1_u32;
    for run in &mut scored {
        if run.disqualified_reason.is_none() {
            run.rank = Some(rank);
            rank = rank.saturating_add(1);
        }
    }
    scored
}

#[must_use]
pub fn performance_metrics(equity: &[i64]) -> Option<PerformanceMetrics> {
    if equity.len() < 2 || equity.iter().any(|value| *value <= 0) {
        return None;
    }
    let start = i128::from(equity[0]);
    let end = i128::from(*equity.last()?);
    let net_return_micros = i64::try_from((end - start) * RETURN_SCALE / start).ok()?;
    let mut returns = Vec::with_capacity(equity.len() - 1);
    let mut peak = equity[0];
    let mut maximum_drawdown = 0_i128;
    for window in equity.windows(2) {
        returns.push(
            (i128::from(window[1]) - i128::from(window[0])) * RETURN_SCALE / i128::from(window[0]),
        );
        peak = peak.max(window[1]);
        if window[1] < peak {
            maximum_drawdown = maximum_drawdown
                .max((i128::from(peak) - i128::from(window[1])) * RETURN_SCALE / i128::from(peak));
        }
    }
    let count = i128::try_from(returns.len()).ok()?;
    let mean = returns.iter().sum::<i128>() / count;
    let downside_square_mean =
        returns
            .iter()
            .filter(|value| **value < 0)
            .try_fold(0_u128, |sum, value| {
                let magnitude = value.unsigned_abs();
                sum.checked_add(magnitude.checked_mul(magnitude)?)
            })?
            / u128::try_from(returns.len()).ok()?;
    let downside_deviation = integer_sqrt(downside_square_mean);
    let sortino_micros = if downside_deviation == 0 {
        if mean > 0 { i64::MAX } else { 0 }
    } else {
        i64::try_from(mean * RETURN_SCALE / i128::try_from(downside_deviation).ok()?).ok()?
    };
    Some(PerformanceMetrics {
        net_return_micros,
        sortino_micros,
        max_drawdown_micros: i64::try_from(maximum_drawdown).ok()?,
    })
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut left = 1_u128;
    let mut right = value.min(u128::from(u64::MAX));
    while left <= right {
        let middle = left + (right - left) / 2;
        match middle.checked_mul(middle) {
            Some(square) if square == value => return middle,
            Some(square) if square < value => left = middle + 1,
            _ => right = middle - 1,
        }
    }
    right
}

fn percentile(values: &[i64], target: i64) -> i64 {
    if values.len() <= 1 {
        return PERCENTILE_SCALE;
    }
    let lower =
        i64::try_from(values.iter().filter(|value| **value < target).count()).unwrap_or(i64::MAX);
    let equal =
        i64::try_from(values.iter().filter(|value| **value == target).count()).unwrap_or(i64::MAX);
    let value_count = i64::try_from(values.len()).unwrap_or(i64::MAX);
    let average_rank_twice = lower * 2 + equal - 1;
    average_rank_twice * PERCENTILE_SCALE / (2 * (value_count - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_are_deterministic_and_penalized() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let scored = score_runs(
            &[
                RunScoreInput {
                    run_id: first,
                    net_return_micros: 10,
                    sortino_micros: 10,
                    max_drawdown_micros: 5,
                    policy_rejections: 0,
                    missed_cycles: 0,
                    disqualified_reason: None,
                },
                RunScoreInput {
                    run_id: second,
                    net_return_micros: 0,
                    sortino_micros: 0,
                    max_drawdown_micros: 10,
                    policy_rejections: 1,
                    missed_cycles: 0,
                    disqualified_reason: None,
                },
            ],
            &ScoringWeightsV1::default(),
            &PenaltyRulesV1::default(),
        );
        assert_eq!(scored[0].score_millis, 100_000);
        assert_eq!(scored[0].rank, Some(1));
        assert_eq!(scored[1].score_millis, 0);
    }

    #[test]
    fn disqualified_run_is_visible_with_zero_score() {
        let run_id = Uuid::new_v4();
        let scored = score_runs(
            &[RunScoreInput {
                run_id,
                net_return_micros: 99,
                sortino_micros: 99,
                max_drawdown_micros: 0,
                policy_rejections: 0,
                missed_cycles: 0,
                disqualified_reason: Some("event chain gap".into()),
            }],
            &ScoringWeightsV1::default(),
            &PenaltyRulesV1::default(),
        );
        assert_eq!(scored[0].score_millis, 0);
        assert_eq!(scored[0].rank, None);
        assert_eq!(
            scored[0].disqualified_reason.as_deref(),
            Some("event chain gap")
        );
    }

    #[test]
    fn ties_use_drawdown_then_return_then_lexical_run_id() {
        let best_drawdown = Uuid::from_u128(3);
        let lexical_first = Uuid::from_u128(1);
        let lexical_second = Uuid::from_u128(2);
        let scored = score_runs(
            &[
                RunScoreInput {
                    run_id: lexical_second,
                    net_return_micros: 10,
                    sortino_micros: 10,
                    max_drawdown_micros: 10,
                    policy_rejections: 0,
                    missed_cycles: 0,
                    disqualified_reason: None,
                },
                RunScoreInput {
                    run_id: best_drawdown,
                    net_return_micros: 10,
                    sortino_micros: 10,
                    max_drawdown_micros: 5,
                    policy_rejections: 0,
                    missed_cycles: 0,
                    disqualified_reason: None,
                },
                RunScoreInput {
                    run_id: lexical_first,
                    net_return_micros: 10,
                    sortino_micros: 10,
                    max_drawdown_micros: 10,
                    policy_rejections: 0,
                    missed_cycles: 0,
                    disqualified_reason: None,
                },
            ],
            &ScoringWeightsV1::default(),
            &PenaltyRulesV1::default(),
        );
        assert_eq!(
            scored.iter().map(|run| run.run_id).collect::<Vec<_>>(),
            vec![best_drawdown, lexical_first, lexical_second]
        );
    }

    #[test]
    fn metrics_use_per_cycle_returns_and_zero_target_sortino() -> Result<(), &'static str> {
        let metrics = performance_metrics(&[1_000_000, 1_100_000, 990_000, 1_188_000])
            .ok_or("valid equity curve")?;
        assert_eq!(metrics.net_return_micros, 188_000);
        assert_eq!(metrics.max_drawdown_micros, 100_000);
        assert!(metrics.sortino_micros > 0);
        Ok(())
    }
}
