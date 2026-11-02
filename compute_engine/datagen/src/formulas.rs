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
        let definition = MeasureDefinition {
            ulid: MeasureUlid(self.generator.ulid()),
            name: format!("計算{index}"),
            axes: self.generator.axes(self.dimensions),
        };
        let depth = self.generator.configuration.maximum_depth;
        let previous = (index > 0).then(|| self.references.len() - 1);
        let text = match previous {
            None => self.expression(&definition.axes, depth, false).text,
            Some(previous) if depth == 0 => self.reference(previous, &definition.axes).text,
            Some(previous) => {
                let link = self.reference(previous, &definition.axes).text;
                let other = self.expression(&definition.axes, depth - 1, false).text;
                let operator = ["+", "-", "*"]
                    .choose(&mut self.generator.random)
                    .expect("there are operators");
                if self.generator.random.random_bool(0.5) {
                    format!("({link} {operator} {other})")
                } else {
                    format!("({other} {operator} {link})")
                }
            }
        };
        self.references.push(Reference {
            name: definition.name.clone(),
            axes: definition.axes.clone(),
            exact: false,
        });
        (definition, text)
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
                    axes: operand.axes,
                }
            }
            Node::Binary(operator) => {
                let left = self.expression(allowed, depth - 1, exact);
                // shortcut: a denominator is built from input measures with + - and SUM only, so it stays an exact integer and the evaluator and the oracle agree on which cells are 0 whatever order they sum in; lift this once comparisons tolerate a cell that one side divides by a rounding residue and the other drops as division by 0.
                let right = self.expression(allowed, depth - 1, exact || operator == "/");
                let mut axes = left.axes;
                for axis in right.axes {
                    if !axes.contains(&axis) {
                        axes.push(axis);
                    }
                }
                Generated {
                    text: format!("({} {operator} {})", left.text, right.text),
                    axes,
                }
            }
            Node::Sum => {
                let value_axes = self.generator.axes(self.dimensions);
                let value = self.expression(&value_axes, depth - 1, exact);
                self.sum(value, allowed)
            }
        }
    }

    fn leaf(&mut self, allowed: &[DimensionUlid], exact: bool) -> Generated {
        let candidates: Vec<usize> = (0..self.references.len())
            .filter(|&reference| !exact || self.references[reference].exact)
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
            },
        }
    }

    fn reference(&mut self, reference: usize, allowed: &[DimensionUlid]) -> Generated {
        let Reference { name, axes, .. } = &self.references[reference];
        let generated = Generated {
            text: quote(name),
            axes: axes.clone(),
        };
        let fits = generated.axes.iter().all(|axis| allowed.contains(axis));
        if fits && self.generator.random.random_bool(0.8) {
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

fn quote(name: &str) -> String {
    format!("'{name}'")
}
