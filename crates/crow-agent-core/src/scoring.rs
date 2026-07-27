use crow_agent_protocol::{PenaltyRulesV1, ScoringWeightsV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const PERCENTILE_SCALE: i64 = 1_000_000;
const MAX_SCORE_MILLIS: i64 = 100_000;

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
    runs.iter()
        .map(|run| {
            if let Some(reason) = &run.disqualified_reason {
                return ScoredRun {
                    run_id: run.run_id,
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
                score_millis: (weighted - penalty).clamp(0, MAX_SCORE_MILLIS),
                net_return_percentile_micros: net,
                sortino_percentile_micros: sortino,
                inverse_drawdown_percentile_micros: inverse_drawdown,
                penalty_millis: penalty,
                disqualified_reason: None,
            }
        })
        .collect()
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
        assert_eq!(
            scored[0].disqualified_reason.as_deref(),
            Some("event chain gap")
        );
    }
}
