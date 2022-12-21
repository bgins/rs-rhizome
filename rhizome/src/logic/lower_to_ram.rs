use petgraph::{
    graph::{DiGraph, NodeIndex},
    visit::EdgeRef,
    Direction,
};
use std::collections::{BTreeMap, HashSet};

use crate::{
    error::Error,
    id::{AliasId, RelationId},
    ram,
};

use super::*;

pub fn lower_to_ram(program: &Program) -> Result<ram::Program, Error> {
    let mut statements: Vec<ram::Statement> = Vec::default();

    for stratum in &stratify(program)? {
        let mut lowered = lower_stratum_to_ram(stratum)?;

        statements.append(&mut lowered);
    }

    Ok(ram::Program::new(statements))
}

pub fn lower_stratum_to_ram(stratum: &Stratum) -> Result<Vec<ram::Statement>, Error> {
    let mut statements: Vec<ram::Statement> = Vec::default();

    if stratum.is_recursive {
        // Merge facts into delta
        for fact in &stratum.facts() {
            let lowered = lower_fact_to_ram(fact, ram::RelationVersion::Delta)?;

            statements.push(lowered);
        }

        // Partition the stratum's rules based on whether they depend on relations
        // that change during this stratum
        let (dynamic_rules, static_rules): (Vec<Rule>, Vec<Rule>) =
            stratum.rules().iter().cloned().partition(|r| {
                r.predicates()
                    .iter()
                    .any(|p| stratum.relations.contains(&p.id.clone()))
            });

        // Evaluate static rules out of the loop
        for rule in &static_rules {
            let mut lowered = lower_rule_to_ram(rule, stratum, ram::RelationVersion::Total)?;

            statements.append(&mut lowered);
        }

        // Merge the output of the static rules into delta, to be used in the loop
        for relation in
            HashSet::<RelationId>::from_iter(static_rules.iter().map(|r| r.head.clone()))
        {
            statements.push(ram::Statement::Merge {
                from: ram::Relation::new(relation.clone(), ram::RelationVersion::Total),
                into: ram::Relation::new(relation, ram::RelationVersion::Delta),
            });
        }

        let mut loop_body: Vec<ram::Statement> = Vec::default();

        // Purge new, computed during the last loop iteration
        for relation in &stratum.relations {
            loop_body.push(ram::Statement::Purge {
                relation: ram::Relation::new(relation.clone(), ram::RelationVersion::New),
            });
        }

        // Evaluate dynamic rules within the loop, inserting into new
        for rule in &dynamic_rules {
            let mut lowered = lower_rule_to_ram(rule, stratum, ram::RelationVersion::New)?;

            loop_body.append(&mut lowered);
        }

        // Exit the loop if all of the dynamic relations have reached a fixed point
        loop_body.push(ram::Statement::Exit {
            relations: stratum
                .relations
                .iter()
                .cloned()
                .map(|id| ram::Relation::new(id, ram::RelationVersion::New))
                .collect(),
        });

        // Merge new into total, then swap new and delta
        for relation in &stratum.relations {
            loop_body.push(ram::Statement::Merge {
                from: ram::Relation::new(relation.clone(), ram::RelationVersion::New),
                into: ram::Relation::new(relation.clone(), ram::RelationVersion::Total),
            });

            loop_body.push(ram::Statement::Swap {
                left: ram::Relation::new(relation.clone(), ram::RelationVersion::New),
                right: ram::Relation::new(relation.clone(), ram::RelationVersion::Delta),
            });
        }

        statements.push(ram::Statement::Loop { body: loop_body })
    } else {
        // Merge facts into total
        for fact in &stratum.facts() {
            let lowered = lower_fact_to_ram(fact, ram::RelationVersion::Total)?;

            statements.push(lowered);
        }

        // Evaluate all rules, inserting into total
        for rule in &stratum.rules() {
            let mut lowered = lower_rule_to_ram(rule, stratum, ram::RelationVersion::Total)?;

            statements.append(&mut lowered);
        }
    };

    Ok(statements)
}

pub fn lower_fact_to_ram(
    fact: &Fact,
    version: ram::RelationVersion,
) -> Result<ram::Statement, Error> {
    let attributes = fact
        .args
        .iter()
        .map(|(k, v)| (k.clone(), ram::Literal::new(v.datum.clone()).into()))
        .collect();

    Ok(ram::Statement::Insert {
        operation: ram::Operation::Project {
            attributes,
            into: ram::Relation::new(fact.head.clone(), version),
        },
    })
}

struct TermMetadata {
    alias: Option<AliasId>,
    bindings: BTreeMap<Variable, ram::Term>,
    is_dynamic: bool,
}

impl TermMetadata {
    fn new(
        alias: Option<AliasId>,
        bindings: BTreeMap<Variable, ram::Term>,
        is_dynamic: bool,
    ) -> Self {
        Self {
            alias,
            bindings,
            is_dynamic,
        }
    }
}

pub fn lower_rule_to_ram(
    rule: &Rule,
    stratum: &Stratum,
    version: ram::RelationVersion,
) -> Result<Vec<ram::Statement>, Error> {
    let mut next_alias: BTreeMap<RelationId, Option<AliasId>> = BTreeMap::default();
    let mut bindings: BTreeMap<Variable, ram::Term> = BTreeMap::default();
    let mut term_metadata: Vec<(BodyTerm, TermMetadata)> = Vec::default();

    for body_term in &rule.body {
        match body_term {
            BodyTerm::Predicate(predicate) => {
                let alias = next_alias.entry(predicate.id.clone()).or_default().clone();

                // TODO: I truly, truly, hate this
                next_alias.entry(predicate.id.clone()).and_modify(|a| {
                    match a {
                        None => *a = Some(AliasId::new(0)),
                        Some(id) => *a = Some(id.next()),
                    };
                });

                for (attribute_id, attribute_value) in &predicate.args {
                    match attribute_value {
                        AttributeValue::Literal(_) => continue,
                        AttributeValue::Variable(variable) if !bindings.contains_key(variable) => {
                            bindings.insert(
                                variable.clone(),
                                ram::Attribute::new(
                                    attribute_id.clone(),
                                    predicate.id.clone(),
                                    alias.clone(),
                                )
                                .into(),
                            )
                        }
                        _ => continue,
                    };
                }

                term_metadata.push((
                    body_term.clone(),
                    TermMetadata::new(
                        alias.clone(),
                        bindings.clone(),
                        stratum.is_recursive && stratum.relations.contains(&predicate.id.clone()),
                    ),
                ));
            }
            BodyTerm::Negation(_) => continue,
        }
    }

    let projection_attributes = rule
        .args
        .iter()
        .map(|(k, v)| match v {
            AttributeValue::Literal(c) => (k.clone(), ram::Literal::new(c.datum.clone()).into()),
            AttributeValue::Variable(v) => (k.clone(), bindings.get(v).unwrap().clone()),
        })
        .collect();

    let projection = ram::Operation::Project {
        attributes: projection_attributes,
        into: ram::Relation::new(rule.head.clone(), version),
    };

    let mut statements: Vec<ram::Statement> = Vec::default();

    // We use a bitmask to represent all of the possible rewrites of the rule under
    // semi-naive evaluation, i.e. those where at least one dynamic predicate searches
    // against a delta relation, rather than total.
    //
    // TODO: Use Arc to share suffixes of a ram operation across overlapping insertions.
    // TODO: Decompose the rule into binary joins to reuse intermediate results.
    let count_of_dynamic = term_metadata
        .iter()
        .filter(|(_, metadata)| metadata.is_dynamic)
        .count();

    let rewrite_count = if count_of_dynamic == 0 {
        1
    } else {
        (1 << count_of_dynamic) - 1
    };

    for offset in 0..rewrite_count {
        // bitmask of dynamic predicate versions (1 => delta, 0 => total)
        let mask = (1 << count_of_dynamic) - 1 - offset;
        // Number of dynamic predicates handled so far
        let mut i = 0;

        let mut negations = rule.negations().clone();
        let mut previous = projection.clone();
        for (body_term, metadata) in term_metadata.iter().rev() {
            match body_term {
                BodyTerm::Predicate(predicate) => {
                    let mut formulae: Vec<ram::Formula> = Vec::default();
                    for (attribute_id, attribute_value) in &predicate.args {
                        match attribute_value {
                            AttributeValue::Literal(literal) => {
                                let formula = ram::Equality::new(
                                    ram::Attribute::new(
                                        attribute_id.clone(),
                                        predicate.id.clone(),
                                        metadata.alias.clone(),
                                    )
                                    .into(),
                                    ram::Literal::new(literal.datum.clone()).into(),
                                )
                                .into();

                                formulae.push(formula);
                            }
                            AttributeValue::Variable(variable) => {
                                match metadata.bindings.get(variable) {
                                    None => (),
                                    Some(ram::Term::Attribute(inner))
                                        if inner.relation() == predicate.id.clone()
                                            && inner.alias() == metadata.alias => {}
                                    Some(bound_value) => {
                                        let formula = ram::Equality::new(
                                            ram::Attribute::new(
                                                attribute_id.clone(),
                                                predicate.id.clone(),
                                                metadata.alias.clone(),
                                            )
                                            .into(),
                                            bound_value.clone(),
                                        )
                                        .into();

                                        formulae.push(formula);
                                    }
                                }
                            }
                        }
                    }

                    let (satisfied, unsatisfied): (Vec<_>, Vec<_>) =
                        negations.into_iter().partition(|n| {
                            n.variables()
                                .iter()
                                .all(|v| metadata.bindings.contains_key(v))
                        });

                    negations = unsatisfied;

                    for negation in satisfied {
                        let attributes = negation
                            .args
                            .iter()
                            .map(|(k, v)| match v {
                                AttributeValue::Literal(literal) => {
                                    (k.clone(), ram::Literal::new(literal.datum.clone()).into())
                                }
                                AttributeValue::Variable(variable) => {
                                    (k.clone(), metadata.bindings.get(variable).unwrap().clone())
                                }
                            })
                            .collect();

                        formulae.push(
                            ram::NotIn::new(
                                attributes,
                                ram::Relation::new(
                                    negation.id.clone(),
                                    ram::RelationVersion::Total,
                                ),
                            )
                            .into(),
                        )
                    }

                    let version = if metadata.is_dynamic && (mask & (1 << i) != 0) {
                        ram::RelationVersion::Delta
                    } else {
                        ram::RelationVersion::Total
                    };

                    previous = ram::Operation::Search {
                        // TODO: semi-naive
                        relation: ram::Relation::new(predicate.id.clone(), version),
                        alias: metadata.alias.clone(),
                        when: formulae,
                        operation: Box::new(previous),
                    };
                }
                BodyTerm::Negation(_) => unreachable!("Only iterating through positive terms"),
            };

            if metadata.is_dynamic {
                i += 1;
            }
        }

        statements.push(ram::Statement::Insert {
            operation: previous,
        });
    }

    Ok(statements)
}

pub fn stratify(program: &Program) -> Result<Vec<Stratum>, Error> {
    let clauses_by_relation: BTreeMap<RelationId, Vec<Clause>> = program.clauses.iter().fold(
        BTreeMap::<RelationId, Vec<Clause>>::default(),
        |mut accum, clause| {
            accum
                .entry(clause.head())
                .and_modify(|clauses| clauses.push(clause.clone()))
                .or_insert_with(|| vec![clause.clone()]);
            accum
        },
    );

    let mut edg = DiGraph::<RelationId, BodyTermPolarity>::default();
    let mut nodes = BTreeMap::<RelationId, NodeIndex>::default();

    for clause in &program.clauses {
        nodes
            .entry(clause.head())
            .or_insert_with(|| edg.add_node(clause.head()));

        for dependency in clause.depends_on() {
            let to = *nodes
                .entry(dependency.to.clone())
                .or_insert_with(|| edg.add_node(dependency.to));

            let from = *nodes
                .entry(dependency.from.clone())
                .or_insert_with(|| edg.add_node(dependency.from));

            edg.add_edge(from, to, dependency.polarity);
        }
    }

    let sccs = petgraph::algo::kosaraju_scc(&edg);

    for scc in &sccs {
        for node in scc {
            for edge in edg.edges_directed(*node, Direction::Outgoing) {
                if edge.weight().is_negative() && scc.contains(&edge.target()) {
                    return Err(Error::ProgramUnstratifiable);
                }
            }
        }
    }

    Ok(sccs
        .iter()
        .map(|nodes| {
            Stratum {
                relations: nodes
                    .iter()
                    .map(|n| edg.node_weight(*n).unwrap())
                    .cloned()
                    .collect(),
                clauses: nodes
                    .iter()
                    .flat_map(|n| {
                        let weight = edg.node_weight(*n).unwrap();

                        clauses_by_relation.get(weight).cloned().unwrap_or_default()
                    })
                    .collect(),
                // TODO: is this sufficient?
                is_recursive: nodes.len() > 1 || edg.contains_edge(nodes[0], nodes[0]),
            }
        })
        .rev()
        .collect())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn stratify_tests() {
        let program = parser::parse(
            r#"
        v(v: X) :- r(r0: X, r1: Y).
        v(v: Y) :- r(r0: X, r1: Y).

        t(t0: X, t1: Y) :- r(r0: X, r1: Y).
        t(t0: X, t1: Y) :- t(t0: X, t1: Z), r(r0: Z, r1: Y).

        tc(tc0: X, tc1: Y):- v(v: X), v(v: Y), !t(t0: X, t1: Y).
        "#,
        )
        .unwrap();

        let stratification = stratify(&program);

        assert_eq!(
            Ok(vec![
                Stratum {
                    relations: vec!["r".into()],
                    clauses: vec![],
                    is_recursive: false,
                },
                Stratum {
                    relations: vec!["v".into()],
                    clauses: vec![
                        Rule::new(
                            "v".into(),
                            vec![("v".into(), Variable::new("X").into())],
                            vec![Predicate::new(
                                "r".into(),
                                vec![
                                    ("r0".into(), Variable::new("X").into()),
                                    ("r1".into(), Variable::new("Y").into()),
                                ],
                            )
                            .into()],
                        )
                        .unwrap()
                        .into(),
                        Rule::new(
                            "v".into(),
                            vec![("v".into(), Variable::new("Y").into())],
                            vec![Predicate::new(
                                "r".into(),
                                vec![
                                    ("r0".into(), Variable::new("X").into()),
                                    ("r1".into(), Variable::new("Y").into()),
                                ],
                            )
                            .into()],
                        )
                        .unwrap()
                        .into(),
                    ],
                    is_recursive: false,
                },
                Stratum {
                    relations: vec!["t".into()],
                    clauses: vec![
                        Rule::new(
                            "t".into(),
                            vec![
                                ("t0".into(), Variable::new("X").into()),
                                ("t1".into(), Variable::new("Y").into()),
                            ],
                            vec![Predicate::new(
                                "r".into(),
                                vec![
                                    ("r0".into(), Variable::new("X").into()),
                                    ("r1".into(), Variable::new("Y").into()),
                                ],
                            )
                            .into()],
                        )
                        .unwrap()
                        .into(),
                        Rule::new(
                            "t".into(),
                            vec![
                                ("t0".into(), Variable::new("X").into()),
                                ("t1".into(), Variable::new("Y").into()),
                            ],
                            vec![
                                Predicate::new(
                                    "t".into(),
                                    vec![
                                        ("t0".into(), Variable::new("X").into()),
                                        ("t1".into(), Variable::new("Z").into()),
                                    ],
                                )
                                .into(),
                                Predicate::new(
                                    "r".into(),
                                    vec![
                                        ("r0".into(), Variable::new("Z").into()),
                                        ("r1".into(), Variable::new("Y").into()),
                                    ],
                                )
                                .into(),
                            ],
                        )
                        .unwrap()
                        .into(),
                    ],
                    is_recursive: true,
                },
                Stratum {
                    relations: vec!["tc".into()],
                    clauses: vec![Rule::new(
                        "tc".into(),
                        vec![
                            ("tc0".into(), Variable::new("X").into()),
                            ("tc1".into(), Variable::new("Y").into()),
                        ],
                        vec![
                            Predicate::new(
                                "v".into(),
                                vec![("v".into(), Variable::new("X").into())],
                            )
                            .into(),
                            Predicate::new(
                                "v".into(),
                                vec![("v".into(), Variable::new("Y").into())],
                            )
                            .into(),
                            Negation::new(
                                "t".into(),
                                vec![
                                    ("t0".into(), Variable::new("X").into()),
                                    ("t1".into(), Variable::new("Y").into()),
                                ],
                            )
                            .into(),
                        ],
                    )
                    .unwrap()
                    .into(),],
                    is_recursive: false,
                }
            ]),
            stratification
        );
    }

    #[test]
    fn unstratifiable_tests() {
        let program = parser::parse(
            r#"
        p(p: X) :- t(t: X), !q(q: X).
        q(q: X) :- t(t: X), !p(p: X)."#,
        )
        .unwrap();

        let stratification = stratify(&program);

        assert_eq!(Err(Error::ProgramUnstratifiable), stratification);
    }
}
