/// Random value generator for sampling data.
mod rvg;

pub use rvg::*;

#[macro_export]
macro_rules! assert_compile {
    ($program_closure:expr) => {
        match $crate::logic::builder::build($program_closure) {
            std::result::Result::Ok(v) => v,
            std::result::Result::Err(e) => {
                panic!("Failed to build program: {:?}", e);
            }
        }
    };
}

#[macro_export]
macro_rules! assert_compile_err {
    ($err:expr, $program_closure:expr) => {
        match $crate::logic::builder::build($program_closure) {
            std::result::Result::Ok(_) => {
                panic!("Expected an error, but compilation succeeded!");
            }
            std::result::Result::Err(e) => {
                pretty_assertions::assert_eq!(Some($err), e.downcast_ref());
            }
        };
    };
}

#[macro_export]
macro_rules! assert_derives {
    ($program_closure:expr, $expected:expr) => {
        assert_derives!(
            $program_closure,
            Vec::<$crate::tuple::InputTuple>::default(),
            $expected
        );
    };
    ($program_closure:expr, $edb:expr, $expected:expr) => {
        let program = match $crate::build($program_closure) {
            std::result::Result::Ok(v) => v,
            std::result::Result::Err(e) => {
                panic!("Failed to build program: {:?}", e);
            }
        };

        let mut b = Vec::default();
        $crate::pretty::Pretty::to_doc(&program)
            .render(80, &mut b)
            .unwrap();

        let pretty = String::from_utf8(b).unwrap();

        let mut bs = $crate::storage::memory::MemoryBlockstore::default();
        let mut vm = <$crate::runtime::vm::VM>::new(program);

        for input_fact in $edb {
            $crate::storage::blockstore::Blockstore::put_serializable(
                &mut bs,
                &input_fact,
                #[allow(unknown_lints, clippy::default_constructed_unit_structs)]
                $crate::storage::DefaultCodec::default(),
                $crate::storage::DEFAULT_MULTIHASH,
            )
            .unwrap();

            let cid = input_fact.cid().unwrap();
            let fact = $crate::tuple::Tuple::new(
                "evac",
                [
                    ("entity", input_fact.entity()),
                    ("attribute", input_fact.attr()),
                    ("value", input_fact.val()),
                ],
                Some(cid),
            );

            vm.push(fact).unwrap();

            for link in input_fact.links() {
                let fact = Tuple::new("links", [("from", cid), ("to", *link)], None);

                vm.push(fact).unwrap();
            }
        }

        match vm.step_epoch(&bs) {
            std::result::Result::Ok(v) => v,
            std::result::Result::Err(e) => {
                panic!("Failed to run program: {:?}", e);
            }
        };

        let mut facts = std::collections::BTreeMap::default();

        for (relation, _) in &$expected {
            facts.insert(
                $crate::id::RelationId::new(relation),
                std::collections::BTreeSet::default(),
            );
        }

        while let Ok(Some(fact)) = vm.pop() {
            if let Some(relation) = facts.get_mut(&fact.id()) {
                relation.insert(fact);
            }
        }

        for (relation, expected) in $expected {
            let actual = facts
                .get(&$crate::id::RelationId::new(relation))
                .unwrap()
                .clone();

            let expected = std::collections::BTreeSet::from_iter(expected.clone());

            pretty_assertions::assert_eq!(actual, expected, "program = \n{}", pretty);
        }
    };
}
