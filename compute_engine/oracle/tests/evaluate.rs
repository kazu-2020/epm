#[path = "../../evaluator/tests/cases/mod.rs"]
mod cases;

use dimension::Dimension;
use measure::Measure;
use oracle::Oracle;

use cases::{CASES, Fixture};

#[test]
fn oracle_matches_every_hand_computed_case() {
    let fixture = Fixture::new();
    let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
    let inputs: Vec<&Measure> = fixture.inputs.iter().collect();
    for case in CASES {
        let oracle = Oracle::new(&dimensions, &inputs).unwrap();
        let mut rows = Vec::new();
        for &(result, source) in case.steps {
            rows = oracle
                .evaluate(&fixture.resolve(source), fixture.definition(result))
                .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        }
        let rows: Vec<(Vec<u32>, f64)> = rows
            .into_iter()
            .map(|(coordinate, value)| (coordinate.iter().map(|member| member.0).collect(), value))
            .collect();
        assert_eq!(cases::exact(&rows), cases::expected(case), "{}", case.name);
    }
}
