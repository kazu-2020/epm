use dimension::Dimension;
use measure::Measure;
use oracle::Oracle;

use datagen::cases::{self, CASES, Fixture};

#[test]
fn oracle_matches_every_hand_computed_case() {
    let fixture = Fixture::default();
    let dimensions: Vec<&Dimension> = fixture.dimensions.iter().collect();
    let inputs: Vec<&Measure> = fixture.inputs.iter().collect();
    for case in CASES {
        let mut oracle = Oracle::new(&dimensions, &inputs).unwrap();
        let mut cells = Vec::new();
        for &(result, source) in case.steps {
            cells = oracle
                .evaluate(&fixture.resolve(source), fixture.definition(result))
                .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        }
        assert_eq!(cases::exact(&cells), cases::expected(case), "{}", case.name);
    }
}
