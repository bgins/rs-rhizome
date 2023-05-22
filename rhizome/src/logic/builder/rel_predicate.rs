use anyhow::Result;
use cid::Cid;
use std::{cell::RefCell, collections::HashMap, sync::Arc};

use crate::{
    col_val::ColVal,
    error::{error, Error},
    id::{ColId, VarId},
    logic::ast::{CidValue, Declaration, RelPredicate},
    relation::Source,
    types::ColType,
};

use super::atom_binding::AtomBinding;

#[derive(Debug)]
pub struct RelPredicateBuilder {
    pub(super) cid: Option<CidValue>,
    pub(super) bindings: RefCell<Vec<(ColId, ColVal)>>,
}

impl RelPredicateBuilder {
    pub fn new(cid: Option<CidValue>) -> Self {
        Self {
            cid,
            bindings: Default::default(),
        }
    }

    pub fn finalize(
        self,
        relation: Arc<Declaration>,
        bound_vars: &mut HashMap<VarId, ColType>,
    ) -> Result<RelPredicate> {
        let mut cols = HashMap::default();

        if let Some(cid) = self.cid {
            if relation.source() != Source::Edb {
                return error(Error::ContentAddressedIDB(relation.id()));
            }

            if let CidValue::Var(var) = cid {
                bound_vars.insert(var.id(), ColType::new::<Cid>());
            }
        }

        for (col_id, col_val) in self.bindings.into_inner() {
            let schema = relation.schema();

            let Some(col) = schema.get_col(&col_id) else {
                return error(Error::UnrecognizedColumnBinding(relation.id(), col_id));
            };

            if cols.contains_key(&col_id) {
                return error(Error::ConflictingColumnBinding(relation.id(), col_id));
            }

            match &col_val {
                ColVal::Lit(val) => {
                    if col.col_type().check(val).is_err() {
                        return error(Error::ColumnValueTypeConflict(
                            relation.id(),
                            col_id,
                            col_val,
                            *col.col_type(),
                        ));
                    }
                }
                ColVal::Binding(var) => {
                    if let Ok(unified) = col.col_type().unify(&var.typ()) {
                        bound_vars.insert(var.id(), unified);
                    } else {
                        return error(Error::ColumnValueTypeConflict(
                            relation.id(),
                            col_id,
                            ColVal::Binding(*var),
                            *col.col_type(),
                        ));
                    }
                }
            }

            cols.insert(col_id, col_val);
        }

        let predicate = RelPredicate::new(relation, self.cid, cols);

        Ok(predicate)
    }

    pub fn bind_one<T>(&self, binding: T) -> Result<()>
    where
        T: AtomBinding,
    {
        let (id, val) = binding.into_pair();

        self.bindings.borrow_mut().push((id, val));

        Ok(())
    }
}
