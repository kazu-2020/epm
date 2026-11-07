use bytes::Bytes;
use change_log::{Commit, RequestId, Sequence};
use datagen::Workspace;
use datagen::representative::{self, Size};
use dimension::MemberId;
use measure::{MeasureError, MeasureUlid};
use server::{DecodeError, Edit, Engine, EngineAlarm, FormulaError, Rejection};
use ulid::Ulid;

const TINY: Size = Size {
    products: 2,
    customers: 2,
    departments_per_kind: 1,
    quantity_cells: 50,
    account_fill: 0.02,
};

const QUANTITY: MeasureUlid = MeasureUlid(Ulid(100));
const SALES: MeasureUlid = MeasureUlid(Ulid(200));

fn workspace() -> Workspace {
    representative::generate(&TINY, 7)
}

fn engine() -> Engine {
    Engine::new(workspace()).expect("the representative formulas are valid")
}

fn quantity(coordinate: [u32; 5], value: Option<f64>) -> Edit {
    Edit {
        measure: QUANTITY,
        coordinate: coordinate.map(MemberId).to_vec(),
        value,
    }
}

fn commit(sequence: u64, edit: &Edit) -> Commit {
    Commit {
        request_id: RequestId(Bytes::from(format!("r{sequence}"))),
        sequence: Sequence(sequence),
        payload: edit.encode(),
    }
}

fn cell(engine: &Engine, measure: MeasureUlid, coordinate: [u32; 5]) -> Option<f64> {
    engine
        .measures()
        .iter()
        .find(|candidate| candidate.ulid() == measure)
        .expect("the engine holds the measure")
        .get(&coordinate.map(MemberId))
        .expect("the coordinate fits")
}

#[test]
fn a_formula_error_shows_before_any_request() {
    let mut broken = workspace();
    broken.formulas[0].text = "'存在しない'".into();
    let result = broken.formulas[0].result.ulid;
    assert!(matches!(
        Engine::new(broken),
        Err(FormulaError::Resolve { result: found, .. }) if found == result
    ));
}

#[test]
fn only_a_cell_of_an_input_measure_passes_the_check() {
    let engine = engine();
    assert_eq!(engine.check(&quantity([0, 1, 1, 0, 1], Some(2.5))), Ok(()));
    assert_eq!(engine.check(&quantity([0, 1, 1, 0, 1], None)), Ok(()));
    let mut result = quantity([0, 1, 1, 0, 1], Some(2.5));
    result.measure = SALES;
    assert_eq!(engine.check(&result), Err(Rejection::NotAnInput(SALES)));
    let mut short = quantity([0, 1, 1, 0, 1], Some(2.5));
    short.coordinate.pop();
    assert_eq!(
        engine.check(&short),
        Err(Rejection::Coordinate(MeasureError::CoordinateLength {
            expected: 5,
            found: 4
        }))
    );
    let versions = 3;
    assert!(matches!(
        engine.check(&quantity([0, versions, 1, 0, 1], Some(2.5))),
        Err(Rejection::Coordinate(MeasureError::MemberOutOfRange { member, .. }))
            if member == MemberId(versions)
    ));
}

#[test]
fn commits_apply_once_each_in_sequence_order() {
    let mut engine = engine();
    let cell_of_sales = [0, 1, 1, 0, 1];
    let set = quantity(cell_of_sales, Some(4.0));
    let clear = quantity(cell_of_sales, None);
    assert_eq!(engine.apply(&commit(0, &set)), Ok(()));
    assert_eq!(cell(&engine, QUANTITY, cell_of_sales), Some(4.0));
    assert!(
        cell(&engine, SALES, cell_of_sales).is_some(),
        "the sales formula sees the quantity"
    );
    let twice = engine.apply(&commit(0, &set));
    assert_eq!(
        twice,
        Err(EngineAlarm::OutOfOrder {
            expected: Sequence(1),
            found: Sequence(0)
        })
    );
    let skipped = engine.apply(&commit(2, &clear));
    assert_eq!(
        skipped,
        Err(EngineAlarm::OutOfOrder {
            expected: Sequence(1),
            found: Sequence(2)
        })
    );
    assert_eq!(cell(&engine, QUANTITY, cell_of_sales), Some(4.0));
    assert_eq!(engine.apply(&commit(1, &clear)), Ok(()));
    assert_eq!(cell(&engine, QUANTITY, cell_of_sales), None);
    assert_eq!(cell(&engine, SALES, cell_of_sales), None);
}

#[test]
fn a_commit_that_does_not_pass_the_check_is_an_alarm() {
    let mut engine = engine();
    let mut garbage = commit(0, &quantity([0, 0, 1, 0, 0], Some(1.0)));
    garbage.payload = Bytes::from_static(b"\x09");
    assert_eq!(
        engine.apply(&garbage),
        Err(EngineAlarm::Undecodable {
            sequence: Sequence(0),
            error: DecodeError::Version(9)
        })
    );
    let mut result = quantity([0, 0, 1, 0, 0], Some(1.0));
    result.measure = SALES;
    assert_eq!(
        engine.apply(&commit(0, &result)),
        Err(EngineAlarm::Rejected {
            sequence: Sequence(0),
            rejection: Rejection::NotAnInput(SALES)
        })
    );
}
