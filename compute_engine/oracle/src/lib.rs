use std::collections::BTreeSet;

use dimension::{AttributeUlid, Dimension, DimensionUlid, MemberId};
use duckdb::Connection;
use formula::{BinaryOperator, CheckedAxis, CheckedExpression, CheckedNode, Sign};
use measure::{Measure, MeasureUlid};

pub struct Oracle {
    connection: Connection,
}

struct Query {
    sql: String,
    axes: BTreeSet<DimensionUlid>,
}

impl Oracle {
    pub fn new(dimensions: &[&Dimension], measures: &[&Measure]) -> duckdb::Result<Self> {
        let connection = Connection::open_in_memory()?;
        for dimension in dimensions {
            connection.execute_batch(&format!(
                "CREATE TABLE {} AS SELECT range::UINTEGER AS member FROM range({})",
                member_table(dimension.ulid()),
                dimension.len()
            ))?;
            for attribute in dimension.attributes() {
                let table = attribute_table(attribute.ulid());
                connection.execute_batch(&format!(
                    "CREATE TABLE {table} (member UINTEGER NOT NULL, target UINTEGER)"
                ))?;
                let mut appender = connection.appender(&table)?;
                for member in 0..dimension.len() {
                    let target = attribute.value(MemberId(member)).map(|target| target.0);
                    appender.append_row(duckdb::params![member, target])?;
                }
                appender.flush()?;
            }
        }
        for measure in measures {
            let definition = measure.definition();
            let table = measure_table(definition.ulid);
            let mut columns: Vec<String> = definition
                .axes
                .iter()
                .map(|&axis| format!("{} UINTEGER NOT NULL", column(axis)))
                .collect();
            columns.push("value DOUBLE NOT NULL".into());
            connection.execute_batch(&format!("CREATE TABLE {table} ({})", columns.join(", ")))?;
            let mut appender = connection.appender(&table)?;
            appender.append_record_batch(measure.compact().base().clone())?;
            appender.flush()?;
        }
        Ok(Oracle { connection })
    }

    pub fn evaluate(
        &mut self,
        expression: &CheckedExpression,
        result: MeasureUlid,
    ) -> duckdb::Result<Vec<(Vec<MemberId>, f64)>> {
        let table = measure_table(result);
        self.connection.execute_batch(&format!(
            "CREATE TABLE {table} AS {}",
            expanded(&query(expression.root()), expression.result_axes())
        ))?;
        let mut statement = self
            .connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY ALL"))?;
        let width = expression.result_axes().len();
        let rows = statement.query_map([], |row| {
            let coordinate = (0..width)
                .map(|axis| row.get(axis).map(MemberId))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((coordinate, row.get(width)?))
        })?;
        rows.collect()
    }
}

fn member_table(dimension: DimensionUlid) -> String {
    format!("member_{}", dimension.0.0)
}

fn attribute_table(attribute: AttributeUlid) -> String {
    format!("attribute_{}", attribute.0.0)
}

fn measure_table(measure: MeasureUlid) -> String {
    format!("measure_{}", measure.0.0)
}

fn column(dimension: DimensionUlid) -> String {
    format!("axis_{}", dimension.0.0)
}

fn query(node: &CheckedNode) -> Query {
    let query = match node {
        CheckedNode::Number(value) => Query {
            sql: format!("SELECT CAST('{value:?}' AS DOUBLE) AS value"),
            axes: BTreeSet::new(),
        },
        CheckedNode::Measure { ulid, axes } => {
            let mut items: Vec<String> = axes.iter().map(|&axis| column(axis)).collect();
            items.push("value".into());
            Query {
                sql: format!("SELECT {} FROM {}", items.join(", "), measure_table(*ulid)),
                axes: axes.iter().copied().collect(),
            }
        }
        CheckedNode::Negate(operand) => {
            let operand = query(operand);
            let mut items: Vec<String> = operand.axes.iter().map(|&axis| column(axis)).collect();
            items.push("-value AS value".into());
            Query {
                sql: format!("SELECT {} FROM ({})", items.join(", "), operand.sql),
                axes: operand.axes,
            }
        }
        CheckedNode::Binary {
            operator: BinaryOperator::Add | BinaryOperator::Subtract,
            ..
        } => sum_of_terms(&node.terms()),
        CheckedNode::Binary {
            operator,
            left,
            right,
        } => product(*operator, &query(left), &query(right)),
        CheckedNode::Sum { value, by } => sum(&query(value), by),
    };
    let mut items: Vec<String> = query.axes.iter().map(|&axis| column(axis)).collect();
    items.push("CASE WHEN node.value = 0 THEN 0::DOUBLE ELSE node.value END AS value".into());
    Query {
        sql: format!(
            "SELECT {} FROM ({}) AS node WHERE isfinite(node.value)",
            items.join(", "),
            query.sql
        ),
        axes: query.axes,
    }
}

fn expanded(query: &Query, axes: &[DimensionUlid]) -> String {
    let mut items = Vec::new();
    let mut joins = String::new();
    for &axis in axes {
        if query.axes.contains(&axis) {
            items.push(format!("node.{}", column(axis)));
        } else {
            let table = member_table(axis);
            joins.push_str(&format!(" CROSS JOIN {table}"));
            items.push(format!("{table}.member AS {}", column(axis)));
        }
    }
    items.push("node.value".into());
    format!(
        "SELECT {} FROM ({}) AS node{joins}",
        items.join(", "),
        query.sql
    )
}

fn sum_of_terms(terms: &[(Sign, &CheckedNode)]) -> Query {
    let terms: Vec<(Sign, Query)> = terms
        .iter()
        .map(|&(sign, term)| (sign, query(term)))
        .collect();
    let axes: BTreeSet<DimensionUlid> = terms
        .iter()
        .flat_map(|(_, term)| term.axes.iter().copied())
        .collect();
    let axes_list: Vec<DimensionUlid> = axes.iter().copied().collect();
    let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
    // Not only the full terms when none is full: spreading every term keeps '人数' + SUM('売上', BY '期間') defined on every pair.
    let creates_cells = |term: &Query| !any_full || term.axes.len() == axes.len();
    let columns = if axes.is_empty() {
        "1 AS unit".to_string()
    } else {
        axes_list
            .iter()
            .map(|&axis| column(axis))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let coordinates = terms
        .iter()
        .filter(|(_, term)| creates_cells(term))
        .map(|(_, term)| format!("SELECT {columns} FROM ({})", expanded(term, &axes_list)))
        .collect::<Vec<_>>()
        .join(" UNION ");
    let mut total = "0::DOUBLE".to_string();
    let mut joins = String::new();
    for (index, (sign, term)) in terms.iter().enumerate() {
        let alias = format!("term_{index}");
        let operator = match sign {
            Sign::Plus => "+",
            Sign::Minus => "-",
        };
        total.push_str(&format!(" {operator} COALESCE({alias}.value, 0)"));
        let on: Vec<String> = term
            .axes
            .iter()
            .map(|&axis| format!("{alias}.{0} = coordinate.{0}", column(axis)))
            .collect();
        joins.push_str(&format!(
            " LEFT JOIN ({}) AS {alias} ON {}",
            term.sql,
            if on.is_empty() {
                "TRUE".to_string()
            } else {
                on.join(" AND ")
            }
        ));
    }
    let mut items: Vec<String> = axes_list
        .iter()
        .map(|&axis| format!("coordinate.{}", column(axis)))
        .collect();
    items.push(format!("{total} AS value"));
    Query {
        sql: format!(
            "SELECT {} FROM ({coordinates}) AS coordinate{joins}",
            items.join(", ")
        ),
        axes,
    }
}

fn product(operator: BinaryOperator, left: &Query, right: &Query) -> Query {
    let axes: Vec<DimensionUlid> = left.axes.union(&right.axes).copied().collect();
    let condition = if axes.is_empty() {
        "TRUE".to_string()
    } else {
        axes.iter()
            .map(|&axis| format!("left_side.{0} = right_side.{0}", column(axis)))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let mut items: Vec<String> = axes
        .iter()
        .map(|&axis| format!("left_side.{}", column(axis)))
        .collect();
    items.push(format!(
        "left_side.value {} right_side.value AS value",
        operator.symbol()
    ));
    let filter = if operator == BinaryOperator::Divide {
        " WHERE right_side.value <> 0"
    } else {
        ""
    };
    Query {
        sql: format!(
            "SELECT {} FROM ({}) AS left_side JOIN ({}) AS right_side ON {condition}{filter}",
            items.join(", "),
            expanded(left, &axes),
            expanded(right, &axes)
        ),
        axes: axes.into_iter().collect(),
    }
}

fn sum(value: &Query, by: &[CheckedAxis]) -> Query {
    let mut axes = BTreeSet::new();
    let mut groups = Vec::new();
    let mut items = Vec::new();
    let mut joins = String::new();
    for (index, axis) in by.iter().enumerate() {
        match *axis {
            CheckedAxis::Dimension(dimension) => {
                axes.insert(dimension);
                groups.push(format!("node.{}", column(dimension)));
                items.push(format!("node.{0} AS {0}", column(dimension)));
            }
            CheckedAxis::Attribute {
                dimension,
                attribute,
                target,
            } => {
                let alias = format!("by_{index}");
                axes.insert(target);
                joins.push_str(&format!(
                    " JOIN {} AS {alias} ON {alias}.member = node.{} AND {alias}.target IS NOT NULL",
                    attribute_table(attribute),
                    column(dimension)
                ));
                groups.push(format!("{alias}.target"));
                items.push(format!("{alias}.target AS {}", column(target)));
            }
        }
    }
    let order: Vec<String> = value
        .axes
        .iter()
        .map(|&axis| format!("node.{}", column(axis)))
        .collect();
    // Not a bare SUM: DuckDB's parallel SUM adds in a different order on each run.
    items.push(if order.is_empty() {
        "SUM(node.value) AS value".into()
    } else {
        format!("SUM(node.value ORDER BY {}) AS value", order.join(", "))
    });
    let group_by = if groups.is_empty() {
        String::new()
    } else {
        format!(" GROUP BY {}", groups.join(", "))
    };
    Query {
        sql: format!(
            "SELECT {} FROM ({}) AS node{joins}{group_by} HAVING COUNT(*) > 0",
            items.join(", "),
            value.sql
        ),
        axes,
    }
}
