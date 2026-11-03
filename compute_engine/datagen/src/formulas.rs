use dimension::{Dimension, DimensionUlid};
use formula::BinaryOperator;
use measure::{Measure, MeasureDefinition, MeasureUlid};
use rand::RngExt;
use rand::seq::IndexedRandom;

use crate::{Formula, Generator, dimension};

const NUMBERS: [&str; 7] = ["0", "1", "2", "3", "10", "0.5", "1.5"];

#[derive(Clone)]
struct Generated {
    text: String,
    axes: Vec<DimensionUlid>,
}

#[derive(Clone, Copy)]
enum Node {
    Negate,
    Binary(BinaryOperator),
    Sum,
}

const NODES: [Node; 6] = [
    Node::Negate,
    Node::Binary(BinaryOperator::Add),
    Node::Binary(BinaryOperator::Subtract),
    Node::Binary(BinaryOperator::Multiply),
    Node::Binary(BinaryOperator::Divide),
    Node::Sum,
];

struct Formulas<'g, 'c, 'd> {
    generator: &'g mut Generator<'c>,
    dimensions: &'d [Dimension],
    references: Vec<Generated>,
}

impl Generator<'_> {
    pub(crate) fn formulas(
        &mut self,
        dimensions: &[Dimension],
        inputs: &[Measure],
    ) -> Vec<Formula> {
        let references = inputs
            .iter()
            .map(|input| Generated {
                text: quote(&input.definition().name),
                axes: input.definition().axes.clone(),
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
    fn formula(&mut self, index: usize) -> Formula {
        let previous = (index > 0).then(|| self.references.len() - 1);
        let mut axes = self.generator.axes(self.dimensions);
        if let Some(previous) = previous
            && !fits(&self.references[previous].axes, &axes)
            && self.generator.random.random_bool(0.5)
        {
            axes = union(axes, self.references[previous].axes.clone());
        }
        let depth = self.generator.configuration.maximum_depth;
        let root = match previous {
            None => self.expression(&axes, depth),
            Some(previous) if depth == 0 => self.reference(previous, &axes),
            Some(previous) => {
                let link = self.reference(previous, &axes);
                let other = self.expression(&axes, depth - 1);
                let operator = *[
                    BinaryOperator::Add,
                    BinaryOperator::Subtract,
                    BinaryOperator::Multiply,
                ]
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
        self.references.push(Generated {
            text: quote(&definition.name),
            axes: definition.axes.clone(),
        });
        Formula {
            result: definition,
            text: root.text,
        }
    }

    fn expression(&mut self, allowed: &[DimensionUlid], depth: usize) -> Generated {
        if depth == 0 || self.generator.random.random_bool(0.2) {
            return self.leaf(allowed);
        }
        match *NODES
            .choose(&mut self.generator.random)
            .expect("there are nodes")
        {
            Node::Negate => {
                let operand = self.expression(allowed, depth - 1);
                Generated {
                    text: format!("-{}", operand.text),
                    ..operand
                }
            }
            Node::Binary(operator) => {
                let left = self.expression(allowed, depth - 1);
                let right = self.expression(allowed, depth - 1);
                binary(left, operator, right)
            }
            Node::Sum => {
                let value_axes = self.generator.axes(self.dimensions);
                let value = self.expression(&value_axes, depth - 1);
                self.sum(value, allowed)
            }
        }
    }

    fn leaf(&mut self, allowed: &[DimensionUlid]) -> Generated {
        if !self.references.is_empty() && self.generator.random.random_bool(0.75) {
            let reference = self.generator.random.random_range(0..self.references.len());
            return self.reference(reference, allowed);
        }
        Generated {
            text: NUMBERS
                .choose(&mut self.generator.random)
                .expect("there are numbers")
                .to_string(),
            axes: Vec::new(),
        }
    }

    fn reference(&mut self, reference: usize, allowed: &[DimensionUlid]) -> Generated {
        let generated = self.references[reference].clone();
        if fits(&generated.axes, allowed) && self.generator.random.random_bool(0.8) {
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
        }
    }
}

fn binary(left: Generated, operator: BinaryOperator, right: Generated) -> Generated {
    Generated {
        text: format!("({} {} {})", left.text, operator.symbol(), right.text),
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
