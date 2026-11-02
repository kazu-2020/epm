use std::collections::BTreeSet;
use std::fmt;

use dimension::{AttributeUlid, Dimension, DimensionUlid, MemberId};
use duckdb::Connection;
use formula::{
    BinaryOperator, ResolvedAxis, ResolvedAxisKind, ResolvedExpression, ResolvedExpressionKind,
};
use measure::{Measure, MeasureDefinition, MeasureUlid};

#[derive(Debug)]
pub enum OracleError {
    Unresolved,
    Database(duckdb::Error),
}

impl fmt::Display for OracleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for OracleError {}

impl From<duckdb::Error> for OracleError {
    fn from(error: duckdb::Error) -> Self {
        OracleError::Database(error)
    }
}

pub struct Oracle {
    connection: Connection,
}

struct Query {
    sql: String,
    axes: BTreeSet<DimensionUlid>,
}

impl Oracle {
    pub fn new(dimensions: &[&Dimension], measures: &[&Measure]) -> Result<Self, OracleError> {
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
        &self,
        expression: &ResolvedExpression,
        result: &MeasureDefinition,
    ) -> Result<Vec<(Vec<MemberId>, f64)>, OracleError> {
        let table = measure_table(result.ulid);
        self.connection.execute_batch(&format!(
            "CREATE OR REPLACE TABLE {table} AS {}",
            expanded(&query(expression)?, &result.axes)
        ))?;
        let mut statement = self
            .connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY ALL"))?;
        let width = result.axes.len();
        let rows = statement.query_map([], |row| {
            let coordinate = (0..width)
                .map(|axis| row.get(axis).map(MemberId))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((coordinate, row.get(width)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn member_table(dimension: DimensionUlid) -> String {
    format!("d_{}", dimension.0.0)
}

fn attribute_table(attribute: AttributeUlid) -> String {
    format!("a_{}", attribute.0.0)
}

fn measure_table(measure: MeasureUlid) -> String {
    format!("m_{}", measure.0.0)
}

fn column(dimension: DimensionUlid) -> String {
    format!("c_{}", dimension.0.0)
}

fn query(expression: &ResolvedExpression) -> Result<Query, OracleError> {
    let query = match &expression.kind {
        ResolvedExpressionKind::Number(value) => Query {
            sql: format!("SELECT CAST('{value:?}' AS DOUBLE) AS value"),
            axes: BTreeSet::new(),
        },
        ResolvedExpressionKind::Measure { ulid, axes } => {
            let mut items: Vec<String> = axes.iter().map(|&axis| column(axis)).collect();
            items.push("value".into());
            Query {
                sql: format!("SELECT {} FROM {}", items.join(", "), measure_table(*ulid)),
                axes: axes.iter().copied().collect(),
            }
        }
        ResolvedExpressionKind::Negate(operand) => {
            let operand = query(operand)?;
            let mut items: Vec<String> = operand.axes.iter().map(|&axis| column(axis)).collect();
            items.push("-value AS value".into());
            Query {
                sql: format!("SELECT {} FROM ({})", items.join(", "), operand.sql),
                axes: operand.axes,
            }
        }
        ResolvedExpressionKind::Binary {
            operator,
            left,
            right,
        } => binary(*operator, &query(left)?, &query(right)?),
        ResolvedExpressionKind::Sum { value, by } => sum(&query(value)?, by)?,
        ResolvedExpressionKind::Error => return Err(OracleError::Unresolved),
    };
    let mut items: Vec<String> = query.axes.iter().map(|&axis| column(axis)).collect();
    items.push("CASE WHEN node.value = 0 THEN 0::DOUBLE ELSE node.value END AS value".into());
    Ok(Query {
        sql: format!(
            "SELECT {} FROM ({}) AS node WHERE isfinite(node.value)",
            items.join(", "),
            query.sql
        ),
        axes: query.axes,
    })
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

fn binary(operator: BinaryOperator, left: &Query, right: &Query) -> Query {
    let axes: BTreeSet<DimensionUlid> = left.axes.union(&right.axes).copied().collect();
    let shared: Vec<DimensionUlid> = axes.iter().copied().collect();
    let left_sql = expanded(left, &shared);
    let right_sql = expanded(right, &shared);
    let condition = if shared.is_empty() {
        "TRUE".to_string()
    } else {
        shared
            .iter()
            .map(|&axis| format!("l.{0} = r.{0}", column(axis)))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let symbol = match operator {
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
    };
    let sql = match operator {
        BinaryOperator::Add | BinaryOperator::Subtract => {
            let mut items: Vec<String> = shared
                .iter()
                .map(|&axis| format!("COALESCE(l.{0}, r.{0}) AS {0}", column(axis)))
                .collect();
            items.push(format!(
                "COALESCE(l.value, 0) {symbol} COALESCE(r.value, 0) AS value"
            ));
            format!(
                "SELECT {} FROM ({left_sql}) AS l FULL OUTER JOIN ({right_sql}) AS r ON {condition}",
                items.join(", ")
            )
        }
        BinaryOperator::Multiply | BinaryOperator::Divide => {
            let mut items: Vec<String> = shared
                .iter()
                .map(|&axis| format!("l.{}", column(axis)))
                .collect();
            items.push(format!("l.value {symbol} r.value AS value"));
            let filter = if operator == BinaryOperator::Divide {
                " WHERE r.value <> 0"
            } else {
                ""
            };
            format!(
                "SELECT {} FROM ({left_sql}) AS l JOIN ({right_sql}) AS r ON {condition}{filter}",
                items.join(", ")
            )
        }
    };
    Query { sql, axes }
}

fn sum(value: &Query, by: &[ResolvedAxis]) -> Result<Query, OracleError> {
    let mut axes = BTreeSet::new();
    let mut groups = Vec::new();
    let mut items = Vec::new();
    let mut joins = String::new();
    for (index, axis) in by.iter().enumerate() {
        match axis.kind {
            ResolvedAxisKind::Dimension(dimension) => {
                axes.insert(dimension);
                groups.push(format!("node.{}", column(dimension)));
                items.push(format!("node.{0} AS {0}", column(dimension)));
            }
            ResolvedAxisKind::Attribute {
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
            ResolvedAxisKind::Unresolved => return Err(OracleError::Unresolved),
        }
    }
    items.push("SUM(node.value) AS value".into());
    let group_by = if groups.is_empty() {
        String::new()
    } else {
        format!(" GROUP BY {}", groups.join(", "))
    };
    Ok(Query {
        sql: format!(
            "SELECT {} FROM ({}) AS node{joins}{group_by} HAVING COUNT(*) > 0",
            items.join(", "),
            value.sql
        ),
        axes,
    })
}
