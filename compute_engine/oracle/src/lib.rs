use std::collections::{BTreeSet, HashMap};

use dimension::{AttributeGlobalId, Dimension, DimensionGlobalId, DimensionKind, MemberIndex};
use duckdb::Connection;
use formula::{CheckedAxis, CheckedExpression, CheckedNode, ProductOperator, Sign, Term};
use measure::{Measure, MeasureGlobalId};

pub struct Oracle {
    connection: Connection,
    packing_orders: PackingOrders,
}

type PackingOrders = HashMap<DimensionGlobalId, (DimensionKind, DimensionGlobalId)>;

struct Query {
    sql: String,
    axes: BTreeSet<DimensionGlobalId>,
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
            for attribute in dimension.reference_attributes() {
                let table = attribute_table(attribute.ulid());
                connection.execute_batch(&format!(
                    "CREATE TABLE {table} (member UINTEGER NOT NULL, target UINTEGER)"
                ))?;
                let mut appender = connection.appender(&table)?;
                for member in 0..dimension.len() {
                    let target = attribute.value(MemberIndex(member)).map(|target| target.0);
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
            appender.append_record_batch(measure.to_record_batch())?;
            appender.flush()?;
        }
        Ok(Oracle {
            connection,
            packing_orders: dimensions
                .iter()
                .map(|dimension| (dimension.ulid(), dimension.packing_order()))
                .collect(),
        })
    }

    pub fn evaluate(
        &mut self,
        expression: &CheckedExpression,
        result: MeasureGlobalId,
    ) -> duckdb::Result<Vec<(Vec<MemberIndex>, f64)>> {
        let table = measure_table(result);
        self.connection.execute_batch(&format!(
            "CREATE TABLE {table} AS {}",
            expanded(
                &query(expression.root(), &self.packing_orders),
                expression.result_axes()
            )
        ))?;
        let mut statement = self
            .connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY ALL"))?;
        let width = expression.result_axes().len();
        let rows = statement.query_map([], |row| {
            let coordinate = (0..width)
                .map(|axis| row.get(axis).map(MemberIndex))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((coordinate, row.get(width)?))
        })?;
        rows.collect()
    }
}

fn member_table(dimension: DimensionGlobalId) -> String {
    format!("member_{}", dimension.0.0)
}

fn attribute_table(attribute: AttributeGlobalId) -> String {
    format!("attribute_{}", attribute.0.0)
}

fn measure_table(measure: MeasureGlobalId) -> String {
    format!("measure_{}", measure.0.0)
}

fn column(dimension: DimensionGlobalId) -> String {
    format!("axis_{}", dimension.0.0)
}

fn query(node: &CheckedNode, packing_orders: &PackingOrders) -> Query {
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
        CheckedNode::Terms(terms) => sum_of_terms(terms, packing_orders),
        CheckedNode::Product {
            operator,
            left,
            right,
        } => product(
            *operator,
            &query(left, packing_orders),
            &query(right, packing_orders),
        ),
        CheckedNode::Sum { value, by } => sum(&query(value, packing_orders), by, packing_orders),
        CheckedNode::Filter {
            value,
            dimension,
            members,
            keeps_axis,
        } => filter(
            &query(value, packing_orders),
            *dimension,
            members,
            *keeps_axis,
        ),
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

fn expanded(query: &Query, axes: &[DimensionGlobalId]) -> String {
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

fn sum_of_terms(terms: &[Term], packing_orders: &PackingOrders) -> Query {
    let terms: Vec<(Sign, Query)> = terms
        .iter()
        .map(|term| (term.sign, query(&term.node, packing_orders)))
        .collect();
    let axes: BTreeSet<DimensionGlobalId> = terms
        .iter()
        .flat_map(|(_, term)| term.axes.iter().copied())
        .collect();
    let axes_list: Vec<DimensionGlobalId> = axes.iter().copied().collect();
    let any_full = terms.iter().any(|(_, term)| term.axes.len() == axes.len());
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

fn product(operator: ProductOperator, left: &Query, right: &Query) -> Query {
    let axes: Vec<DimensionGlobalId> = left.axes.union(&right.axes).copied().collect();
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
    let (symbol, filter) = match operator {
        ProductOperator::Multiply => ("*", ""),
        ProductOperator::Divide => ("/", " WHERE right_side.value <> 0"),
    };
    items.push(format!(
        "left_side.value {symbol} right_side.value AS value"
    ));
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

fn filter(
    value: &Query,
    dimension: DimensionGlobalId,
    members: &[MemberIndex],
    keeps_axis: bool,
) -> Query {
    let mut axes = value.axes.clone();
    if !keeps_axis {
        axes.remove(&dimension);
    }
    let mut items: Vec<String> = axes.iter().map(|&axis| column(axis)).collect();
    items.push("value".into());
    let members: Vec<String> = members.iter().map(|member| member.0.to_string()).collect();
    Query {
        sql: format!(
            "SELECT {} FROM ({}) WHERE {} IN ({})",
            items.join(", "),
            value.sql,
            column(dimension),
            members.join(", ")
        ),
        axes,
    }
}

fn sum(value: &Query, by: &[CheckedAxis], packing_orders: &PackingOrders) -> Query {
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
    let mut ordered: Vec<DimensionGlobalId> = value.axes.iter().copied().collect();
    ordered.sort_by_key(|axis| packing_orders[axis]);
    let order: Vec<String> = ordered
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
