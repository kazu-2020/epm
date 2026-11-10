use bytes::Bytes;
use change_log::{Commit, RequestId, Sequence};
use datagen::Workspace;
use datagen::cases::exact;
use datagen::representative::{self, Size};
use dimension::MemberId;
use measure::{MeasureError, MeasureUlid};
use server::{DecodeError, Edit, Engine, EngineAlarm, FormulaError, Invalid};
use ulid::Ulid;

const TINY: Size = Size {
    products: 2,
    customers: 2,
    departments_per_kind: 1,
    quantity_cells: 50,
    account_fill: 0.02,
};

const QUANTITY: MeasureUlid = MeasureUlid(Ulid(100));
const PRICE: MeasureUlid = MeasureUlid(Ulid(101));
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

fn cell(engine: &Engine, measure: MeasureUlid, coordinate: &[u32]) -> Option<f64> {
    let coordinate: Vec<MemberId> = coordinate.iter().copied().map(MemberId).collect();
    engine
        .measures()
        .iter()
        .find(|candidate| candidate.ulid() == measure)
        .expect("the engine holds the measure")
        .get(&coordinate)
        .expect("the coordinate fits")
}

fn applied(engine: &Engine, commits: &[Commit]) -> Engine {
    engine
        .apply_batch(commits)
        .expect("the engine applies the batch")
}

fn cells(engine: &Engine) -> Vec<Vec<(Vec<u32>, String)>> {
    engine
        .measures()
        .iter()
        .map(|measure| exact(&measure.cells().collect::<Vec<_>>()))
        .collect()
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

fn check(payload: &Bytes) -> Result<(), Invalid> {
    engine().definitions().check(payload)
}

#[test]
fn only_a_cell_of_an_input_measure_passes_the_check() {
    assert_eq!(
        check(&quantity([0, 1, 1, 0, 1], Some(2.5)).encode()),
        Ok(())
    );
    assert_eq!(check(&quantity([0, 1, 1, 0, 1], None).encode()), Ok(()));
    assert_eq!(
        check(&Bytes::from_static(b"\x09")),
        Err(Invalid::Undecodable(DecodeError::Version(9)))
    );
    let mut result = quantity([0, 1, 1, 0, 1], Some(2.5));
    result.measure = SALES;
    assert_eq!(check(&result.encode()), Err(Invalid::NotAnInput(SALES)));
    let mut short = quantity([0, 1, 1, 0, 1], Some(2.5));
    short.coordinate.pop();
    assert_eq!(
        check(&short.encode()),
        Err(Invalid::Coordinate(MeasureError::CoordinateLength {
            expected: 5,
            found: 4
        }))
    );
    let versions = 3;
    assert!(matches!(
        check(&quantity([0, versions, 1, 0, 1], Some(2.5)).encode()),
        Err(Invalid::Coordinate(MeasureError::MemberOutOfRange { member, .. }))
            if member == MemberId(versions)
    ));
}

#[test]
fn commits_apply_once_each_in_sequence_order() {
    let engine = engine();
    let cell_of_sales = [0, 1, 1, 0, 1];
    let set = quantity(cell_of_sales, Some(4.0));
    let clear = quantity(cell_of_sales, None);
    let engine = applied(&engine, &[commit(0, &set)]);
    assert_eq!(engine.recalculated(), Some(Sequence(0)));
    assert_eq!(cell(&engine, QUANTITY, &cell_of_sales), Some(4.0));
    assert!(
        cell(&engine, SALES, &cell_of_sales).is_some(),
        "the sales formula sees the quantity"
    );
    assert_eq!(
        engine.apply_batch(&[commit(0, &set)]).err(),
        Some(EngineAlarm::OutOfOrder {
            expected: Sequence(1),
            found: Sequence(0)
        })
    );
    assert_eq!(
        engine
            .apply_batch(&[commit(1, &clear), commit(3, &set)])
            .err(),
        Some(EngineAlarm::OutOfOrder {
            expected: Sequence(2),
            found: Sequence(3)
        })
    );
    let cleared = applied(&engine, &[commit(1, &clear)]);
    assert_eq!(
        cell(&engine, QUANTITY, &cell_of_sales),
        Some(4.0),
        "unchanged"
    );
    assert_eq!(cell(&cleared, QUANTITY, &cell_of_sales), None);
    assert_eq!(cell(&cleared, SALES, &cell_of_sales), None);
}

#[test]
fn a_batch_gives_the_values_of_its_commits_one_by_one() {
    let first = [0, 1, 1, 0, 1];
    let second = [2, 1, 0, 1, 0];
    let price = Edit {
        measure: PRICE,
        coordinate: [0, 1, 1].map(MemberId).to_vec(),
        value: Some(9.0),
    };
    let edits = [
        quantity(first, Some(4.0)),
        price,
        quantity(second, Some(1.5)),
        quantity(first, Some(7.0)),
        quantity(second, None),
    ];
    let commits: Vec<Commit> = (0..)
        .zip(&edits)
        .map(|(sequence, edit)| commit(sequence, edit))
        .collect();
    let mut one_by_one = engine();
    for commit in &commits {
        one_by_one = applied(&one_by_one, std::slice::from_ref(commit));
    }
    let batch = applied(&engine(), &commits);
    assert_eq!(batch.recalculated(), Some(Sequence(4)));
    assert_eq!(
        cell(&batch, QUANTITY, &first),
        Some(7.0),
        "the last write wins"
    );
    assert_eq!(cell(&batch, QUANTITY, &second), None);
    assert_eq!(cells(&batch), cells(&one_by_one));
}

#[test]
fn a_commit_that_does_not_pass_the_check_is_an_alarm() {
    let engine = engine();
    let good = commit(0, &quantity([0, 0, 1, 0, 0], Some(1.0)));
    let mut garbage = commit(1, &quantity([0, 0, 1, 0, 0], Some(1.0)));
    garbage.payload = Bytes::from_static(b"\x09");
    assert_eq!(
        engine.apply_batch(&[good.clone(), garbage]).err(),
        Some(EngineAlarm::Invalid {
            sequence: Sequence(1),
            invalid: Invalid::Undecodable(DecodeError::Version(9))
        })
    );
    let mut result = quantity([0, 0, 1, 0, 0], Some(1.0));
    result.measure = SALES;
    assert_eq!(
        engine.apply_batch(&[good, commit(1, &result)]).err(),
        Some(EngineAlarm::Invalid {
            sequence: Sequence(1),
            invalid: Invalid::NotAnInput(SALES)
        })
    );
}
