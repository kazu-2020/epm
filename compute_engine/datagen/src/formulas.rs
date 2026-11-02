use dimension::{Dimension, DimensionUlid};
use measure::{Measure, MeasureDefinition, MeasureUlid};
use rand::RngExt;
use rand::seq::IndexedRandom;

use crate::{Generator, dimension};

const NUMBERS: [&str; 7] = ["0", "1", "2", "3", "10", "0.5", "1.5"];

struct Reference {
    name: String,
    axes: Vec<DimensionUlid>,
    exact: bool,
}

struct Generated {
    text: String,
    axes: Vec<DimensionUlid>,
    exact: bool,
}

#[derive(Clone, Copy)]
enum Node {
    Negate,
    Binary(&'static str),
    Sum,
}

const NODES: [Node; 6] = [
    Node::Negate,
    Node::Binary("+"),
    Node::Binary("-"),
    Node::Binary("*"),
    Node::Binary("/"),
    Node::Sum,
];

const EXACT_NODES: [Node; 4] = [
    Node::Negate,
    Node::Binary("+"),
    Node::Binary("-"),
    Node::Sum,
];

struct Formulas<'g, 'c, 'd> {
    generator: &'g mut Generator<'c>,
    dimensions: &'d [Dimension],
    references: Vec<Reference>,
}

impl Generator<'_> {
    pub(crate) fn formulas(
        &mut self,
        dimensions: &[Dimension],
        inputs: &[Measure],
    ) -> Vec<(MeasureDefinition, String)> {
        let references = inputs
            .iter()
            .map(|input| Reference {
                name: input.definition().name.clone(),
                axes: input.definition().axes.clone(),
                exact: true,
            })
            .collect();
        let mut formulas = Formulas {
            generator: self,
            dimensions,
            references,
        };
        (0..formulas.generator.configuration.chain_length)
            .map(|index| formulas.formula(index))
            .collect()
    }
}

impl Formulas<'_, '_, '_> {
    fn formula(&mut self, index: usize) -> (MeasureDefinition, String) {
        let previous = (index > 0).then(|| self.references.len() - 1);
        let mut axes = self.generator.axes(self.dimensions);
        if let Some(previous) = previous
            && !self.references[previous].exact
        {
            axes = union(axes, self.references[previous].axes.clone());
        }
        let depth = self.generator.configuration.maximum_depth;
        let root = match previous {
            None => self.expression(&axes, depth, false),
            Some(previous) if depth == 0 => self.reference(previous, &axes),
            Some(previous) => {
                let link = self.reference(previous, &axes);
                let other = self.expression(&axes, depth - 1, false);
                let operator = *["+", "-", "*"]
                    .choose(&mut self.generator.random)
                    .expect("there are operators");
                if self.generator.random.random_bool(0.5) {
                    binary(link, operator, other)
                } else {
                    binary(other, operator, link)
                }
            }
        };
        let definition = MeasureDefinition {
            ulid: MeasureUlid(self.generator.ulid()),
            name: format!("計算{index}"),
            axes,
        };
        self.references.push(Reference {
            name: definition.name.clone(),
            axes: definition.axes.clone(),
            exact: root.exact,
        });
        (definition, root.text)
    }

    fn expression(&mut self, allowed: &[DimensionUlid], depth: usize, exact: bool) -> Generated {
        if depth == 0 || self.generator.random.random_bool(0.2) {
            return self.leaf(allowed, exact);
        }
        let nodes: &[Node] = if exact { &EXACT_NODES } else { &NODES };
        match *nodes
            .choose(&mut self.generator.random)
            .expect("there are nodes")
        {
            Node::Negate => {
                let operand = self.expression(allowed, depth - 1, exact);
                Generated {
                    text: format!("-{}", operand.text),
                    ..operand
                }
            }
            Node::Binary(operator) => {
                let left = self.expression(allowed, depth - 1, exact);
                let right = self.expression(allowed, depth - 1, exact);
                binary(left, operator, right)
            }
            Node::Sum => {
                let value_axes = self.generator.axes(self.dimensions);
                // Not any value under SUM: the evaluator and the oracle add in different orders, so a sum of rounded values can differ in the last bits and flip whether a later division sees 0.
                let value = self.expression(&value_axes, depth - 1, true);
                self.sum(value, allowed)
            }
        }
    }

    fn leaf(&mut self, allowed: &[DimensionUlid], exact: bool) -> Generated {
        let candidates: Vec<usize> = (0..self.references.len())
            .filter(|&index| {
                let reference = &self.references[index];
                reference.exact || (!exact && fits(&reference.axes, allowed))
            })
            .collect();
        match candidates.choose(&mut self.generator.random) {
            Some(&reference) if self.generator.random.random_bool(0.75) => {
                self.reference(reference, allowed)
            }
            _ => Generated {
                text: NUMBERS
                    .choose(&mut self.generator.random)
                    .expect("there are numbers")
                    .to_string(),
                axes: Vec::new(),
                exact: true,
            },
        }
    }

    fn reference(&mut self, reference: usize, allowed: &[DimensionUlid]) -> Generated {
        let Reference { name, axes, exact } = &self.references[reference];
        let generated = Generated {
            text: quote(name),
            axes: axes.clone(),
            exact: *exact,
        };
        if fits(&generated.axes, allowed)
            && (!generated.exact || self.generator.random.random_bool(0.8))
        {
            generated
        } else {
            self.sum(generated, allowed)
        }
    }

    fn sum(&mut self, value: Generated, allowed: &[DimensionUlid]) -> Generated {
        let mut outputs: Vec<DimensionUlid> = Vec::new();
        let mut by: Vec<String> = Vec::new();
        for &axis in &value.axes {
            let source = dimension(self.dimensions, axis);
            let mut options: Vec<(DimensionUlid, String)> = Vec::new();
            if allowed.contains(&axis) {
                options.push((axis, quote(source.name())));
            }
            for attribute in source.attributes() {
                if allowed.contains(&attribute.target()) {
                    options.push((
                        attribute.target(),
                        format!("{}.{}", quote(source.name()), quote(attribute.name())),
                    ));
                }
            }
            options.retain(|(output, _)| !outputs.contains(output));
            if options.is_empty() || self.generator.random.random_bool(0.4) {
                continue;
            }
            let (output, text) =
                options.swap_remove(self.generator.random.random_range(0..options.len()));
            outputs.push(output);
            by.push(text);
        }
        let text = if by.is_empty() {
            format!("SUM({})", value.text)
        } else {
            format!("SUM({}, BY {})", value.text, by.join(", "))
        };
        Generated {
            text,
            axes: outputs,
            exact: value.exact,
        }
    }
}

fn binary(left: Generated, operator: &str, right: Generated) -> Generated {
    Generated {
        text: format!("({} {operator} {})", left.text, right.text),
        exact: left.exact && right.exact && (operator == "+" || operator == "-"),
        axes: union(left.axes, right.axes),
    }
}

fn union(mut left: Vec<DimensionUlid>, right: Vec<DimensionUlid>) -> Vec<DimensionUlid> {
    for axis in right {
        if !left.contains(&axis) {
            left.push(axis);
        }
    }
    left
}

fn fits(axes: &[DimensionUlid], allowed: &[DimensionUlid]) -> bool {
    axes.iter().all(|axis| allowed.contains(axis))
}

fn quote(name: &str) -> String {
    format!("'{name}'")
}
